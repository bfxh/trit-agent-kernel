//! 模型协议适配：同一份 `ProviderRequest` / `ProviderResponse`，按协议构造请求与解析响应。
//!
//! 协议差异只集中在这里：端点、认证头、请求体、响应解析、流式事件。为了不让三份
//! 解析逻辑各自漂移，请求体以 OpenAI Chat 为中间态再转换；响应把各协议的 tool 调用
//! 归一成 OpenAI 的 `message.tool_calls`，复用 `openai::parse_message` 解释工具语义。
//!
//! 已支持：`openai-chat`、`anthropic-messages`、`gemini-generate-content`。

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::openai::{self, OpenAiConfig};
use crate::provider_stream::{ProviderStreamEvent, SseEvent};
use crate::{AgentError, ProviderRequest, ProviderResponse, ToolSpec};
use trit_reasonix::HostUsage;

/// Anthropic Messages API 版本头。变更协议版本必须显式改这里，不读环境变量。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic 未配置输出上限时的默认 `max_tokens`（该协议必填）。
pub const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 4_096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Protocol {
    #[default]
    OpenAiChat,
    /// DeepSeek：OpenAI Chat 兼容的传输，独立的默认端点与凭据变量。
    DeepSeekChat,
    AnthropicMessages,
    GeminiGenerateContent,
}

impl Protocol {
    pub const ALL: [Protocol; 4] = [
        Protocol::OpenAiChat,
        Protocol::DeepSeekChat,
        Protocol::AnthropicMessages,
        Protocol::GeminiGenerateContent,
    ];

    /// 解析协议名；别名覆盖常见写法，未知名字 fail closed 并列出支持项。
    pub fn parse(name: &str) -> Result<Self, AgentError> {
        let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "" | "openai" | "openai-chat" | "chat-completions" => Ok(Self::OpenAiChat),
            "deepseek" | "deepseek-chat" | "deepseek-reasoner" => Ok(Self::DeepSeekChat),
            "anthropic" | "anthropic-messages" | "claude" => Ok(Self::AnthropicMessages),
            "gemini" | "google" | "gemini-generate-content" | "generate-content" => {
                Ok(Self::GeminiGenerateContent)
            }
            other => Err(AgentError::Provider(format!(
                "unknown protocol {other:?}; supported: openai-chat, anthropic-messages, gemini-generate-content"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::DeepSeekChat => "deepseek-chat",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GeminiGenerateContent => "gemini-generate-content",
        }
    }

    /// 未显式指定协议时按 base_url 推断；未知主机保留 OpenAI Chat（兼容自建网关）。
    pub fn from_base_url(base_url: &str) -> Self {
        let lower = base_url.to_ascii_lowercase();
        if lower.contains("deepseek") {
            Self::DeepSeekChat
        } else if lower.contains("anthropic") {
            Self::AnthropicMessages
        } else if lower.contains("generativelanguage.googleapis.com") {
            Self::GeminiGenerateContent
        } else {
            Self::OpenAiChat
        }
    }

    /// 可用环境变量名；用于报错时提示该配哪个变量。
    pub fn api_key_env(&self) -> &'static str {
        match self {
            Self::OpenAiChat => "OPENAI_API_KEY",
            Self::DeepSeekChat => "DEEPSEEK_API_KEY",
            Self::AnthropicMessages => "ANTHROPIC_API_KEY",
            Self::GeminiGenerateContent => "GEMINI_API_KEY",
        }
    }

    pub fn endpoint(&self, base_url: &str, model: &str, stream: bool) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Self::OpenAiChat | Self::DeepSeekChat => join(base, "chat/completions"),
            Self::AnthropicMessages => join_versioned(base, "v1/messages"),
            Self::GeminiGenerateContent => format!(
                "{}/v1beta/models/{}:{}",
                base,
                model,
                if stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                }
            ),
        }
    }

    pub fn headers(&self, api_key: &str) -> Vec<(&'static str, String)> {
        match self {
            Self::OpenAiChat | Self::DeepSeekChat => {
                vec![("Authorization", format!("Bearer {api_key}"))]
            }
            Self::AnthropicMessages => vec![
                ("x-api-key", api_key.to_owned()),
                ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
            ],
            Self::GeminiGenerateContent => vec![("x-goog-api-key", api_key.to_owned())],
        }
    }

    /// 构造请求体；`stream` 只影响 OpenAI 与 Gemini 的字段（Anthropic 用端点区分）。
    pub fn build_body(&self, config: &OpenAiConfig, request: &ProviderRequest) -> Value {
        let base = openai::build_request_body(config, request);
        match self {
            Self::OpenAiChat | Self::DeepSeekChat => base,
            Self::AnthropicMessages => anthropic_body(&base, config, request),
            Self::GeminiGenerateContent => gemini_body(&base, config, request),
        }
    }

    pub fn parse_body(
        &self,
        body: &str,
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse, AgentError> {
        match self {
            Self::OpenAiChat | Self::DeepSeekChat => openai::parse_response(body, tools),
            Self::AnthropicMessages => anthropic_parse(body, tools),
            Self::GeminiGenerateContent => gemini_parse(body, tools),
        }
    }

    /// 流式解码器；三种协议的 SSE 事件形状不同，统一产出 `ProviderStreamEvent`。
    pub fn stream_decoder(&self) -> Box<dyn ProtocolStream> {
        match self {
            Self::OpenAiChat | Self::DeepSeekChat => Box::new(OpenAiProtocolStream::default()),
            Self::AnthropicMessages => Box::new(AnthropicProtocolStream::default()),
            Self::GeminiGenerateContent => Box::new(GeminiProtocolStream::default()),
        }
    }
}

/// `{base}/{suffix}`；`base` 已带 `/v1` 时不再重复。
fn join_versioned(base: &str, suffix: &str) -> String {
    if let Some(rest) = base.strip_suffix("/v1")
        && let Some(tail) = suffix.strip_prefix("v1/")
    {
        return format!("{rest}/v1/{tail}");
    }
    join(base, suffix)
}

fn join(base: &str, suffix: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), suffix)
}

/// 流式解码器：把协议原生事件转成内核事件，最后组装成 `ProviderResponse`。
pub trait ProtocolStream {
    fn push(&mut self, event: &SseEvent) -> Result<Vec<ProviderStreamEvent>, AgentError>;
    fn finish(self: Box<Self>, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError>;
}

// ---------------------------------------------------------------- OpenAI Chat

/// 复用 `provider_stream::OpenAiStreamAssembler`，保持既有行为不变。
#[derive(Default)]
pub struct OpenAiProtocolStream {
    assembler: crate::provider_stream::OpenAiStreamAssembler,
}

impl ProtocolStream for OpenAiProtocolStream {
    fn push(&mut self, event: &SseEvent) -> Result<Vec<ProviderStreamEvent>, AgentError> {
        self.assembler.push_sse(event)
    }

    fn finish(self: Box<Self>, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
        self.assembler.finish(tools)
    }
}

// ---------------------------------------------------------------- Anthropic

#[derive(Debug, Clone, Default)]
struct ToolBlock {
    id: String,
    name: String,
    json: String,
}

#[derive(Debug, Default)]
pub struct AnthropicProtocolStream {
    text: String,
    blocks: BTreeMap<usize, ToolBlock>,
    usage: Option<HostUsage>,
    stop_reason: Option<String>,
    saw_stop: bool,
}

impl ProtocolStream for AnthropicProtocolStream {
    fn push(&mut self, event: &SseEvent) -> Result<Vec<ProviderStreamEvent>, AgentError> {
        let mut out = Vec::new();
        if event.data.trim().is_empty() {
            return Ok(out);
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|error| AgentError::Provider(format!("bad anthropic sse json: {error}")))?;
        let kind = event
            .event
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    let usage = anthropic_usage(Some(usage));
                    self.usage = Some(usage);
                    out.push(ProviderStreamEvent::Usage(usage));
                }
            }
            "content_block_start" => {
                let index = block_index(&value);
                let block = &value["content_block"];
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let tool = ToolBlock {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("toolu-unknown")
                            .to_owned(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        json: String::new(),
                    };
                    out.push(ProviderStreamEvent::ToolCallDelta {
                        index,
                        id: Some(tool.id.clone()),
                        name: Some(tool.name.clone()),
                        arguments: String::new(),
                    });
                    self.blocks.insert(index, tool);
                }
            }
            "content_block_delta" => {
                let index = block_index(&value);
                let delta = &value["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if !text.is_empty() {
                            self.text.push_str(&text);
                            out.push(ProviderStreamEvent::TextDelta(text));
                        }
                    }
                    Some("thinking_delta") => {
                        let text = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if !text.is_empty() {
                            out.push(ProviderStreamEvent::ReasoningDelta(text));
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if let Some(block) = self.blocks.get_mut(&index) {
                            block.json.push_str(&partial);
                        }
                        if !partial.is_empty() {
                            out.push(ProviderStreamEvent::ToolCallDelta {
                                index,
                                id: None,
                                name: None,
                                arguments: partial,
                            });
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                    out.push(ProviderStreamEvent::Finish {
                        reason: Some(reason.to_owned()),
                    });
                }
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    && let Some(usage) = self.usage.as_mut()
                {
                    usage.completion_tokens = output;
                    out.push(ProviderStreamEvent::Usage(*usage));
                }
            }
            "message_stop" => {
                self.saw_stop = true;
                out.push(ProviderStreamEvent::Done);
            }
            "error" => {
                return Err(AgentError::Provider(format!(
                    "anthropic stream error: {}",
                    value.get("error").cloned().unwrap_or(Value::Null)
                )));
            }
            _ => {}
        }
        Ok(out)
    }

    fn finish(self: Box<Self>, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
        if !self.saw_stop
            && self.stop_reason.is_none()
            && self.blocks.is_empty()
            && self.text.is_empty()
        {
            return Err(AgentError::Provider(
                "anthropic stream ended without any content".into(),
            ));
        }
        let mut calls = Vec::new();
        for (_, block) in self.blocks {
            calls.push(json!({
                "id": block.id,
                "type": "function",
                "function": {
                    "name": block.name,
                    "arguments": if block.json.trim().is_empty() { "{}".to_owned() } else { block.json },
                }
            }));
        }
        let (tool_calls, completion) =
            openai::parse_message(&json!({ "tool_calls": calls }), tools)?;
        Ok(ProviderResponse {
            tool_calls,
            completion,
            usage: self.usage.unwrap_or_else(host_usage_unknown),
            prefix: None,
            context: None,
        })
    }
}

fn block_index(value: &Value) -> usize {
    value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
}

// ---------------------------------------------------------------- Gemini

#[derive(Debug, Default)]
pub struct GeminiProtocolStream {
    text: String,
    calls: Vec<(String, String, String)>, // (id, name, arguments json)
    usage: Option<HostUsage>,
    saw_finish: bool,
}

impl ProtocolStream for GeminiProtocolStream {
    fn push(&mut self, event: &SseEvent) -> Result<Vec<ProviderStreamEvent>, AgentError> {
        let mut out = Vec::new();
        if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
            if event.data.trim() == "[DONE]" {
                self.saw_finish = true;
                out.push(ProviderStreamEvent::Done);
            }
            return Ok(out);
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|error| AgentError::Provider(format!("bad gemini sse json: {error}")))?;
        if let Some(error) = value.get("error") {
            return Err(AgentError::Provider(format!(
                "gemini stream error: {error}"
            )));
        }
        if let Some(usage) = value.get("usageMetadata") {
            let usage = gemini_usage(Some(usage));
            self.usage = Some(usage);
            out.push(ProviderStreamEvent::Usage(usage));
        }
        let parts = value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.text.push_str(text);
                    out.push(ProviderStreamEvent::TextDelta(text.to_owned()));
                }
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = call.get("args").cloned().unwrap_or(json!({})).to_string();
                let id = format!("gemini-call-{}", self.calls.len());
                out.push(ProviderStreamEvent::ToolCallDelta {
                    index: self.calls.len(),
                    id: Some(id.clone()),
                    name: Some(name.clone()),
                    arguments: arguments.clone(),
                });
                self.calls.push((id, name, arguments));
            }
        }
        if value
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            .is_some()
        {
            self.saw_finish = true;
            out.push(ProviderStreamEvent::Finish {
                reason: value
                    .pointer("/candidates/0/finishReason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        Ok(out)
    }

    fn finish(self: Box<Self>, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
        if !self.saw_finish && self.calls.is_empty() && self.text.is_empty() {
            return Err(AgentError::Provider(
                "gemini stream ended without any content".into(),
            ));
        }
        let calls = self
            .calls
            .into_iter()
            .map(|(id, name, arguments)| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                })
            })
            .collect::<Vec<_>>();
        let (tool_calls, completion) =
            openai::parse_message(&json!({ "tool_calls": calls }), tools)?;
        Ok(ProviderResponse {
            tool_calls,
            completion,
            usage: self.usage.unwrap_or_else(host_usage_unknown),
            prefix: None,
            context: None,
        })
    }
}

// ---------------------------------------------------------------- 请求体

fn system_and_user(base: &Value) -> (String, String) {
    let system = base
        .pointer("/messages/0/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let user = base
        .pointer("/messages/1/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (system, user)
}

fn anthropic_body(base: &Value, config: &OpenAiConfig, request: &ProviderRequest) -> Value {
    let (system, user) = system_and_user(base);
    json!({
        "model": config.model,
        "max_tokens": config.max_output_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS),
        "system": system,
        "messages": [{"role": "user", "content": [{"type": "text", "text": user}]}],
        "tools": anthropic_tools(&request.tools),
        "tool_choice": {"type": "auto"},
    })
}

fn gemini_body(base: &Value, config: &OpenAiConfig, request: &ProviderRequest) -> Value {
    let (system, user) = system_and_user(base);
    let mut body = json!({
        "systemInstruction": {"parts": [{"text": system}]},
        "contents": [{"role": "user", "parts": [{"text": user}]}],
        "tools": [{"functionDeclarations": gemini_tools(&request.tools)}],
        "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
    });
    if let Some(max_output_tokens) = config.max_output_tokens {
        body["generationConfig"] = json!({"maxOutputTokens": max_output_tokens});
    }
    body
}

/// 从 OpenAI function 声明转成 Anthropic `tools`（`input_schema`）。
fn anthropic_tools(tools: &[ToolSpec]) -> Vec<Value> {
    openai::tool_schemas(tools)
        .into_iter()
        .filter_map(|schema| {
            let function = schema.get("function")?;
            Some(json!({
                "name": function.get("name")?.clone(),
                "description": function.get("description").cloned().unwrap_or(json!("")),
                "input_schema": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or(json!({"type": "object"})),
            }))
        })
        .collect()
}

/// 从 OpenAI function 声明转成 Gemini `functionDeclarations`。
fn gemini_tools(tools: &[ToolSpec]) -> Vec<Value> {
    openai::tool_schemas(tools)
        .into_iter()
        .filter_map(|schema| {
            let function = schema.get("function")?;
            Some(json!({
                "name": function.get("name")?.clone(),
                "description": function.get("description").cloned().unwrap_or(json!("")),
                "parameters": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or(json!({"type": "object"})),
            }))
        })
        .collect()
}

fn anthropic_parse(body: &str, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| AgentError::Provider(format!("bad json: {error}")))?;
    if let Some(error) = value.get("error") {
        return Err(AgentError::Provider(format!("provider error: {error}")));
    }
    let mut calls = Vec::new();
    if let Some(blocks) = value.get("content").and_then(Value::as_array) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu-unknown");
                let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
                    AgentError::Provider("anthropic tool_use missing name".into())
                })?;
                let input = block.get("input").cloned().unwrap_or(json!({}));
                calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": input.to_string()}
                }));
            }
        }
    }
    let (tool_calls, completion) = openai::parse_message(&json!({ "tool_calls": calls }), tools)?;
    Ok(ProviderResponse {
        tool_calls,
        completion,
        usage: anthropic_usage(value.get("usage")),
        prefix: None,
        context: None,
    })
}

fn anthropic_usage(usage: Option<&Value>) -> HostUsage {
    let Some(usage) = usage else {
        return host_usage_unknown();
    };
    let read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let estimated =
        input == 0 && output == 0 && read == 0 && write == 0 && usage.get("input_tokens").is_none();
    HostUsage {
        prompt_tokens: input.saturating_add(read).saturating_add(write),
        completion_tokens: output,
        cache_hit_tokens: read,
        cache_miss_tokens: input.saturating_add(write),
        estimated,
    }
}

fn gemini_parse(body: &str, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| AgentError::Provider(format!("bad json: {error}")))?;
    if let Some(error) = value.get("error") {
        return Err(AgentError::Provider(format!("provider error: {error}")));
    }
    let mut calls = Vec::new();
    let mut index = 0usize;
    if let Some(parts) = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            let Some(call) = part.get("functionCall") else {
                continue;
            };
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::Provider("gemini functionCall missing name".into()))?;
            let arguments = call.get("args").cloned().unwrap_or(json!({}));
            calls.push(json!({
                "id": format!("gemini-call-{index}"),
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }));
            index += 1;
        }
    }
    let (tool_calls, completion) = openai::parse_message(&json!({ "tool_calls": calls }), tools)?;
    Ok(ProviderResponse {
        tool_calls,
        completion,
        usage: gemini_usage(value.get("usageMetadata")),
        prefix: None,
        context: None,
    })
}

fn gemini_usage(usage: Option<&Value>) -> HostUsage {
    let Some(usage) = usage else {
        return host_usage_unknown();
    };
    let prompt = usage
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    HostUsage {
        prompt_tokens: prompt,
        completion_tokens: output,
        cache_hit_tokens: cached,
        cache_miss_tokens: prompt.saturating_sub(cached),
        estimated: usage.get("promptTokenCount").is_none(),
    }
}

fn host_usage_unknown() -> HostUsage {
    HostUsage {
        estimated: true,
        ..HostUsage::default()
    }
}
