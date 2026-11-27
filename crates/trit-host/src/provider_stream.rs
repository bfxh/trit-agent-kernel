//! OpenAI-compatible 流式解析与工具增量累积。
//!
//! 这是新增能力，不改变 `Provider::complete` 的默认同步路径。流式 SSE、
//! tool-call 增量与最终 `ProviderResponse` 组装在这里完成；真正发 HTTP 的
//! `OpenAiProvider::complete_stream` 也必须显式调用才会走到这里。

use std::collections::BTreeMap;
use std::io::BufRead;

use serde_json::{Value, json};

use crate::openai::parse_message;
use crate::{AgentError, PrefixSignal, Provider, ProviderRequest, ProviderResponse, ToolSpec};
use trit_reasonix::HostUsage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub id: Option<String>,
    pub data: String,
}

/// 逐行读取 SSE；空行 dispatch，`: comment` 忽略，多行 data 用换行拼接。
pub fn read_sse_events<R, F>(mut reader: R, mut on_event: F) -> Result<(), AgentError>
where
    R: BufRead,
    F: FnMut(SseEvent) -> Result<(), AgentError>,
{
    const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
    const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;
    let mut data = String::new();
    let mut event = None;
    let mut id = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| AgentError::Provider(format!("read SSE: {error}")))?;
        if read == 0 {
            dispatch_sse(&mut data, &mut event, &mut id, &mut on_event)?;
            return Ok(());
        }
        if line.len() > MAX_SSE_LINE_BYTES {
            return Err(AgentError::Provider(format!(
                "SSE line exceeds {MAX_SSE_LINE_BYTES} bytes"
            )));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            dispatch_sse(&mut data, &mut event, &mut id, &mut on_event)?;
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        let (field, raw) = trimmed.split_once(':').unwrap_or((trimmed, ""));
        let value = raw.strip_prefix(' ').unwrap_or(raw);
        match field {
            "data" => {
                if data.len().saturating_add(value.len()) > MAX_SSE_EVENT_BYTES {
                    return Err(AgentError::Provider(format!(
                        "SSE event exceeds {MAX_SSE_EVENT_BYTES} bytes"
                    )));
                }
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            "event" => event = Some(value.to_owned()),
            "id" => id = Some(value.to_owned()),
            _ => {}
        }
    }
}

fn dispatch_sse<F>(
    data: &mut String,
    event: &mut Option<String>,
    id: &mut Option<String>,
    on_event: &mut F,
) -> Result<(), AgentError>
where
    F: FnMut(SseEvent) -> Result<(), AgentError>,
{
    if data.is_empty() && event.is_none() {
        return Ok(());
    }
    let payload = SseEvent {
        event: event.take(),
        id: id.take(),
        data: std::mem::take(data),
    };
    on_event(payload)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Usage(HostUsage),
    Finish {
        reason: Option<String>,
    },
    Done,
}

/// 流式 provider 的可选扩展；同步 `Provider` 不变。
pub trait StreamingProvider: Provider {
    fn complete_stream(
        &mut self,
        request: &ProviderRequest,
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> Result<(), AgentError>,
    ) -> Result<ProviderResponse, AgentError>;
}

#[derive(Debug, Clone, Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct OpenAiStreamAssembler {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PendingToolCall>,
    usage: Option<HostUsage>,
    finish_reason: Option<String>,
    prefix: Option<PrefixSignal>,
    saw_done: bool,
    chunks: usize,
}

impl OpenAiStreamAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    pub fn finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    pub fn saw_done(&self) -> bool {
        self.saw_done
    }

    pub fn push_sse(&mut self, event: &SseEvent) -> Result<Vec<ProviderStreamEvent>, AgentError> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            if self.saw_done {
                return Ok(Vec::new());
            }
            self.saw_done = true;
            return Ok(vec![ProviderStreamEvent::Done]);
        }
        let value: Value = serde_json::from_str(event.data.as_str())
            .map_err(|error| AgentError::Provider(format!("bad SSE json: {error}")))?;
        self.push_chunk(&value)
    }

    pub fn push_chunk(&mut self, value: &Value) -> Result<Vec<ProviderStreamEvent>, AgentError> {
        if let Some(error) = value.get("error") {
            return Err(AgentError::Provider(format!("stream error: {error}")));
        }
        self.chunks = self.chunks.saturating_add(1);
        let mut events = Vec::new();
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        if let Some(delta) = choice.and_then(|choice| choice.get("delta")) {
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                self.text.push_str(content);
                events.push(ProviderStreamEvent::TextDelta(content.to_owned()));
            }
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                self.reasoning.push_str(reasoning);
                events.push(ProviderStreamEvent::ReasoningDelta(reasoning.to_owned()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    events.extend(self.push_tool_delta(call)?);
                }
            }
        }
        if let Some(reason) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            && !reason.is_empty()
        {
            self.finish_reason = Some(reason.to_owned());
            events.push(ProviderStreamEvent::Finish {
                reason: Some(reason.to_owned()),
            });
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let usage = crate::openai::parse_usage(Some(usage));
            self.usage = Some(usage);
            events.push(ProviderStreamEvent::Usage(usage));
        }
        if let Some(prefix) = crate::openai::parse_prefix(value) {
            self.prefix = Some(prefix);
        }
        Ok(events)
    }

    fn push_tool_delta(&mut self, call: &Value) -> Result<Vec<ProviderStreamEvent>, AgentError> {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| AgentError::Provider("stream tool call missing index".into()))?
            as usize;
        let function = call
            .get("function")
            .ok_or_else(|| AgentError::Provider("stream tool call missing function".into()))?;
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let arguments = match function.get("arguments") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Null) | None => None,
            Some(other) => Some(other.to_string()),
        };

        let pending = self.tool_calls.entry(index).or_default();
        if let Some(id) = &id {
            if pending.id.is_empty() {
                pending.id = id.clone();
            } else if pending.id != *id {
                return Err(AgentError::Provider(format!(
                    "stream tool call {index} changed id from {} to {id}",
                    pending.id
                )));
            }
        }
        if let Some(name) = &name {
            if pending.name.is_empty() {
                pending.name = name.clone();
            } else if pending.name != *name {
                return Err(AgentError::Provider(format!(
                    "stream tool call {index} changed name from {} to {name}",
                    pending.name
                )));
            }
        }
        if let Some(arguments) = &arguments {
            pending.arguments.push_str(arguments);
        }
        Ok(vec![ProviderStreamEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments: arguments.unwrap_or_default(),
        }])
    }

    pub fn finish(self, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
        if self.chunks == 0 {
            return Err(AgentError::Provider("empty provider stream".into()));
        }
        if !self.saw_done && self.finish_reason.is_none() {
            return Err(AgentError::Provider(
                "provider stream ended without finish_reason or [DONE]".into(),
            ));
        }
        let mut calls = Vec::with_capacity(self.tool_calls.len());
        for (index, pending) in self.tool_calls {
            if pending.name.is_empty() {
                return Err(AgentError::Provider(format!(
                    "stream tool call {index} missing name"
                )));
            }
            let id = if pending.id.is_empty() {
                format!("call-{index}")
            } else {
                pending.id
            };
            let arguments = if pending.arguments.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&pending.arguments).map_err(|error| {
                    AgentError::Provider(format!("bad streamed tool arguments: {error}"))
                })?
            };
            calls.push(json!({
                "id": id,
                "function": {"name": pending.name, "arguments": arguments}
            }));
        }
        let message = json!({"tool_calls": calls});
        let (tool_calls, completion) = parse_message(&message, tools)?;
        Ok(ProviderResponse {
            tool_calls,
            completion,
            usage: self.usage.unwrap_or_else(|| HostUsage {
                estimated: true,
                ..HostUsage::default()
            }),
            prefix: self.prefix,
            context: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn sse_decoder_handles_comments_multiline_and_eof() {
        let mut events = Vec::new();
        read_sse_events(
            Cursor::new(": comment\nevent: delta\ndata: a\ndata: b\n\ndata: tail"),
            |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data, "a\nb");
        assert_eq!(events[1].data, "tail");
    }

    #[test]
    fn tool_identity_conflict_fails_closed() {
        let mut assembler = OpenAiStreamAssembler::new();
        assembler
            .push_chunk(&json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "call-1", "function": {"name": "bash", "arguments": ""}}
                ]}}]
            }))
            .unwrap();
        assert!(
            assembler
                .push_chunk(&json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 0, "id": "call-2", "function": {"arguments": ""}}
                    ]}}]
                }))
                .is_err()
        );
    }

    #[test]
    fn interleaved_tool_deltas_stay_separate() {
        let mut assembler = OpenAiStreamAssembler::new();
        for chunk in [
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call-0", "function": {"name": "bash", "arguments": "{\"command\":\"ca"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": "call-1", "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "rgo test\"}"}}
            ]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ] {
            assembler.push_chunk(&chunk).unwrap();
        }
        let response = assembler.finish(&crate::default_tools()).unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].call_id, "call-0");
        assert_eq!(response.tool_calls[0].tool, "bash");
        assert_eq!(
            response.tool_calls[0].command.as_deref(),
            Some("cargo test")
        );
        assert_eq!(response.tool_calls[1].call_id, "call-1");
        assert_eq!(response.tool_calls[1].tool, "read_file");
        assert_eq!(response.tool_calls[1].paths, ["a.rs"]);
    }

    #[test]
    fn empty_stream_fails_closed() {
        assert!(OpenAiStreamAssembler::new().finish(&[]).is_err());
    }

    #[test]
    fn real_sse_tool_call_fixture_assembles() {
        // 真实录制形态：多事件、tool args 分片、usage 与 [DONE]。
        let fixture = r#": keep-alive
data: {"id":"chatcmpl-1","choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"read_file","arguments":""}}]}}]}

data: {"id":"chatcmpl-1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}

data: {"id":"chatcmpl-1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/main.rs\"}"}}]}}]}

data: {"id":"chatcmpl-1","choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}}

data: [DONE]

"#;
        let mut assembler = OpenAiStreamAssembler::new();
        read_sse_events(Cursor::new(fixture), |event| {
            assembler.push_sse(&event)?;
            Ok(())
        })
        .unwrap();
        let response = assembler.finish(&crate::default_tools()).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].call_id, "call_abc");
        assert_eq!(response.tool_calls[0].tool, "read_file");
        assert_eq!(response.tool_calls[0].paths, ["src/main.rs"]);
        assert_eq!(response.usage.prompt_tokens, 12);
        assert_eq!(response.usage.completion_tokens, 8);
    }
}
