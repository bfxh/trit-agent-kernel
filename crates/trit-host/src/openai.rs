//! OpenAI-compatible Chat Completions provider（阻塞式）。
//!
//! 只负责 HTTP 传输、工具 schema 与响应解析；不让内核接触网络。
//! API key 只在 Authorization 头里出现，Debug 输出会脱敏。

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufReader, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::provider_health::{ConnectionFailureKind, ConnectionHealth, ConnectionHealthPolicy};
use crate::provider_stream::{
    OpenAiStreamAssembler, ProviderStreamEvent, StreamingProvider, read_sse_events,
};
use crate::{
    AgentError, CacheObservation, ContextReceipt, PrefixSignal, Provider, ProviderRequest,
    ProviderResponse, ToolRequest, ToolSpec,
};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage};

#[derive(Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    /// 建连超时：DNS/TCP/TLS 握手；与整次请求超时分开。
    pub connect_timeout: Duration,
    pub max_output_tokens: Option<u64>,
    /// 流式请求是否发送 `stream_options.include_usage`；不支持该字段的兼容网关可关闭。
    pub stream_include_usage: bool,
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("stream_include_usage", &self.stream_include_usage)
            .finish()
    }
}

impl OpenAiConfig {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(10),
            max_output_tokens: None,
            stream_include_usage: true,
        }
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn with_stream_include_usage(mut self, include: bool) -> Self {
        self.stream_include_usage = include;
        self
    }

    pub fn from_env() -> Result<Self, AgentError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| AgentError::Provider("OPENAI_API_KEY is not set".into()))?;
        let model = std::env::var("OPENAI_MODEL")
            .map_err(|_| AgentError::Provider("OPENAI_MODEL is not set".into()))?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let connect_timeout = std::env::var("TRIT_OPENAI_CONNECT_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(10));
        let model = Self::resolve_model(model);
        Ok(Self::new(base_url, api_key, model).with_connect_timeout(connect_timeout))
    }

    /// `TRIT_MODEL_ALIASES`（JSON 对象）命中时替换模型名；非法 JSON 保持原名。
    fn resolve_model(model: String) -> String {
        let Ok(raw) = std::env::var("TRIT_MODEL_ALIASES") else {
            return model;
        };
        let Ok(aliases) = serde_json::from_str::<BTreeMap<String, String>>(&raw) else {
            return model;
        };
        apply_model_alias(&aliases, &model)
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiConfig,
    agent: ureq::Agent,
    health: Arc<Mutex<ConnectionHealth>>,
    health_policy: ConnectionHealthPolicy,
}

/// 单次 HTTP 响应的读取上限；超过时 fail closed，避免恶意/异常网关拖垮进程。
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

fn apply_model_alias(aliases: &BTreeMap<String, String>, model: &str) -> String {
    aliases
        .get(model)
        .cloned()
        .unwrap_or_else(|| model.to_owned())
}

/// 把 HTTP 状态码映射到连接失败类型；配额文案优先按 Quota 处理。
fn classify_status(code: u16, body: &str) -> ConnectionFailureKind {
    match code {
        429 => ConnectionFailureKind::RateLimit,
        401 | 403 => ConnectionFailureKind::Auth,
        402 => ConnectionFailureKind::Quota,
        500..=599 => ConnectionFailureKind::Server,
        400..=499 => {
            if body.to_ascii_lowercase().contains("quota") {
                ConnectionFailureKind::Quota
            } else {
                ConnectionFailureKind::Protocol
            }
        }
        _ => ConnectionFailureKind::Unknown,
    }
}

fn read_response_text(response: ureq::Response) -> Result<String, AgentError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| AgentError::Provider(format!("read response: {error}")))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(AgentError::Provider(format!(
            "response exceeds {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(config.timeout)
            .timeout_connect(config.connect_timeout)
            .build();
        Self {
            config,
            agent,
            health: Arc::new(Mutex::new(ConnectionHealth::new())),
            health_policy: ConnectionHealthPolicy::default(),
        }
    }

    /// 当前连接健康快照。
    pub fn health(&self) -> ConnectionHealth {
        self.health
            .lock()
            .map(|health| health.clone())
            .unwrap_or_default()
    }

    fn record_success(&self) {
        if let Ok(mut health) = self.health.lock() {
            health.record_success();
        }
    }

    fn record_failure(&self, kind: ConnectionFailureKind) {
        if let Ok(mut health) = self.health.lock() {
            health.record_failure(kind, self.health_policy);
        }
    }

    fn post_chat(&self, body: &Value) -> Result<String, AgentError> {
        let _permit = crate::concurrency_limit::acquire();
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        match self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(response) => {
                self.record_success();
                read_response_text(response)
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = read_response_text(response).unwrap_or_default();
                self.record_failure(classify_status(code, &text));
                Err(AgentError::Provider(format!("HTTP {code}: {text}")))
            }
            Err(error) => {
                self.record_failure(ConnectionFailureKind::Transport);
                Err(AgentError::Provider(format!("transport: {error}")))
            }
        }
    }

    /// 原始 chat completion：供 MCP sampling 等非工具循环调用。
    pub fn complete_chat(&self, body: &Value) -> Result<Value, AgentError> {
        let text = self.post_chat(body)?;
        serde_json::from_str(&text)
            .map_err(|error| AgentError::Provider(format!("bad json: {error}")))
    }
}

impl Provider for OpenAiProvider {
    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, AgentError> {
        let body = build_request_body(&self.config, request);
        let text = self.post_chat(&body)?;
        let mut response = parse_response(&text, &request.tools)?;
        attach_context_receipt(&mut response, request);
        Ok(response)
    }
}

impl StreamingProvider for OpenAiProvider {
    fn complete_stream(
        &mut self,
        request: &ProviderRequest,
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> Result<(), AgentError>,
    ) -> Result<ProviderResponse, AgentError> {
        let _permit = crate::concurrency_limit::acquire();
        let mut body = build_request_body(&self.config, request);
        body["stream"] = json!(true);
        if self.config.stream_include_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream")
            .send_string(&body.to_string());
        let response = match response {
            Ok(response) => {
                self.record_success();
                response
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = read_response_text(response).unwrap_or_default();
                self.record_failure(classify_status(code, &text));
                return Err(AgentError::Provider(format!("HTTP {code}: {text}")));
            }
            Err(error) => {
                self.record_failure(ConnectionFailureKind::Transport);
                return Err(AgentError::Provider(format!("transport: {error}")));
            }
        };

        let mut assembler = OpenAiStreamAssembler::new();
        read_sse_events(BufReader::new(response.into_reader()), |event| {
            for delta in assembler.push_sse(&event)? {
                on_event(delta)?;
            }
            Ok(())
        })?;
        let mut response = assembler.finish(&request.tools)?;
        attach_context_receipt(&mut response, request);
        Ok(response)
    }
}

fn build_request_body(config: &OpenAiConfig, request: &ProviderRequest) -> Value {
    let mut system = format!(
        "{}\n\nYou are a headless coding agent. Use the provided tools. \
         Only claim completion with complete_subtask. Every criterion must be backed \
         by a tool result (a command that ran or paths that were written). \
         Manual notes are never evidence.",
        request.brief.render()
    );
    let mut context_tail = String::new();
    let mut context_fingerprint = String::new();
    if let Some(plan) = &request.context {
        if plan.cache.prefix_items > 0 {
            system.push_str("\n\n");
            system.push_str(&plan.render_prefix());
        }
        context_tail = plan.render_tail();
        context_fingerprint = plan.cache.prefix_fingerprint.clone();
    }
    let mut user = json!({
        "step": request.step,
        "usage": request.usage,
        "receipts": request.receipts,
        "context_digest": request.context_digest,
        "context_tail": context_tail,
        "context_fingerprint": context_fingerprint,
        "instruction": "Continue the task. Call tools as needed. When finished, call complete_subtask."
    });
    if let Some(projection) = &request.projection {
        user["projection"] = serde_json::to_value(projection).unwrap_or(serde_json::Value::Null);
    }
    let mut body = json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user.to_string()}
        ],
        "tools": tool_schemas(&request.tools),
        "tool_choice": "auto"
    });
    if let Some(max_output_tokens) = config.max_output_tokens {
        body["max_tokens"] = json!(max_output_tokens);
    }
    body
}

fn tool_schemas(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let parameters = if let Some(schema) = &tool.input_schema {
                schema.clone()
            } else {
                match tool.name.as_str() {
                    "read_file" => json!({
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }),
                    "write_file" => json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["path", "content"]
                    }),
                    "bash" => json!({
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"]
                    }),
                    "context_search" => json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["query"]
                    }),
                    "context_add" => json!({
                        "type": "object",
                        "properties": {"id": {"type": "string"}},
                        "required": ["id"]
                    }),
                    "context_read" => json!({
                        "type": "object",
                        "properties": {
                            "handle": {"type": "string"},
                            "offset": {"type": "integer"},
                            "length": {"type": "integer"}
                        },
                        "required": ["handle"]
                    }),
                    "complete_subtask" => json!({
                        "type": "object",
                        "properties": {
                            "criteria": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string"},
                                        "kind": {
                                            "type": "string",
                                            "enum": ["verification", "diff", "manual"]
                                        },
                                        "command": {"type": "string"},
                                        "paths": {
                                            "type": "array",
                                            "items": {"type": "string"}
                                        }
                                    },
                                    "required": ["id", "kind"]
                                }
                            }
                        },
                        "required": ["criteria"]
                    }),
                    _ => json!({"type": "object"}),
                }
            };
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters
                }
            })
        })
        .collect()
}

fn parse_response(body: &str, tools: &[ToolSpec]) -> Result<ProviderResponse, AgentError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| AgentError::Provider(format!("bad json: {error}")))?;
    if let Some(error) = value.get("error") {
        return Err(AgentError::Provider(format!("provider error: {error}")));
    }
    let message = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| AgentError::Provider("missing choices[0].message".into()))?;
    let (tool_calls, completion) = parse_message(message, tools)?;
    Ok(ProviderResponse {
        tool_calls,
        completion,
        usage: parse_usage(value.get("usage")),
        prefix: parse_prefix(&value),
        context: None,
    })
}

pub(crate) fn parse_message(
    message: &Value,
    tools: &[ToolSpec],
) -> Result<(Vec<ToolRequest>, Option<ClaimRequest>), AgentError> {
    let mut tool_calls = Vec::new();
    let mut completion = None;
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call-{index}"));
            let function = call
                .get("function")
                .ok_or_else(|| AgentError::Provider("tool call missing function".into()))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::Provider("tool call missing name".into()))?;
            let arguments = function
                .get("arguments")
                .map(parse_arguments)
                .transpose()?
                .unwrap_or(Value::Null);
            match name {
                "read_file" => {
                    let path = string_arg(&arguments, "path")?;
                    tool_calls.push(ToolRequest::read(call_id, path));
                }
                "write_file" => {
                    let path = string_arg(&arguments, "path")?;
                    let content = arguments
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    tool_calls.push(ToolRequest::write(call_id, name, path, content));
                }
                "bash" => {
                    let command = string_arg(&arguments, "command")?;
                    // `read_only` 只表示工具自身的静态属性，不能由模型参数自报：
                    // 否则一条写命令只要附上 {"read_only": true} 就能绕过审批与写声明。
                    tool_calls.push(ToolRequest::shell(call_id, command));
                }
                "context_search" => {
                    let query = string_arg(&arguments, "query")?;
                    let limit = arguments
                        .get("limit")
                        .and_then(Value::as_u64)
                        .map(|limit| limit as usize);
                    tool_calls.push(ToolRequest::context_search(call_id, query, limit));
                }
                "context_add" => {
                    let id = string_arg(&arguments, "id")?;
                    tool_calls.push(ToolRequest::context_add(call_id, id));
                }
                "context_read" => {
                    let handle = string_arg(&arguments, "handle")?;
                    let span = match (
                        arguments.get("offset").and_then(Value::as_u64),
                        arguments.get("length").and_then(Value::as_u64),
                    ) {
                        (Some(offset), Some(length)) => Some(crate::ContextSpan {
                            offset: offset as usize,
                            length: length as usize,
                        }),
                        (None, None) => None,
                        _ => {
                            return Err(AgentError::Provider(
                                "context_read span needs both offset and length".into(),
                            ));
                        }
                    };
                    let mut request = ToolRequest::context_read(call_id, handle);
                    request.span = span;
                    tool_calls.push(request);
                }
                "complete_subtask" => {
                    completion = Some(parse_claim(&arguments, &call_id)?);
                }
                other => {
                    if let Some(spec) = tools.iter().find(|spec| spec.name == other) {
                        tool_calls.push(ToolRequest {
                            call_id,
                            tool: other.to_owned(),
                            read_only: spec.read_only,
                            arguments: Some(arguments),
                            ..ToolRequest::default()
                        });
                    } else {
                        return Err(AgentError::Provider(format!("unknown tool: {other}")));
                    }
                }
            }
        }
    }
    Ok((tool_calls, completion))
}

fn attach_context_receipt(response: &mut ProviderResponse, request: &ProviderRequest) {
    let Some(plan) = &request.context else {
        return;
    };
    let cache = CacheObservation {
        provider: plan.provider.clone(),
        model: plan.model.clone(),
        prefix_fingerprint: plan.cache.prefix_fingerprint.clone(),
        prompt_tokens: response.usage.prompt_tokens,
        cache_hit_tokens: response.usage.cache_hit_tokens,
        cache_miss_tokens: response.usage.cache_miss_tokens,
        estimated: response.usage.estimated,
        observed_at_ms: now_ms(),
    };
    response.context = Some(ContextReceipt {
        inclusion: Some(plan.inclusion_receipt()),
        cache: Some(cache),
    });
}

fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

pub(crate) fn parse_prefix(value: &Value) -> Option<PrefixSignal> {
    let diagnostics = value.get("cache_diagnostics")?;
    let changed = diagnostics
        .get("prefix_changed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reasons: Vec<String> = diagnostics
        .get("prefix_change_reasons")
        .and_then(Value::as_array)
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(PrefixSignal {
        changed: changed || !reasons.is_empty(),
        reasons,
    })
}

fn parse_arguments(value: &Value) -> Result<Value, AgentError> {
    match value {
        Value::String(text) => serde_json::from_str(text)
            .map_err(|error| AgentError::Provider(format!("bad tool arguments: {error}"))),
        other => Ok(other.clone()),
    }
}

fn string_arg(arguments: &Value, key: &str) -> Result<String, AgentError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AgentError::Provider(format!("missing string argument: {key}")))
}

fn parse_claim(arguments: &Value, call_id: &str) -> Result<ClaimRequest, AgentError> {
    let criteria = arguments
        .get("criteria")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Provider("complete_subtask missing criteria".into()))?;
    let criteria = criteria
        .iter()
        .enumerate()
        .map(|(index, criterion)| {
            let id = criterion
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{call_id}:{index}"));
            let kind = criterion
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::Provider("criterion missing kind".into()))?;
            let kind = match kind {
                "verification" => CriterionKind::Verification {
                    command: string_arg(criterion, "command")?,
                },
                "diff" => {
                    let paths = criterion
                        .get("paths")
                        .and_then(Value::as_array)
                        .ok_or_else(|| AgentError::Provider("diff criterion missing paths".into()))?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    CriterionKind::Diff { paths }
                }
                "manual" => CriterionKind::Manual,
                other => {
                    return Err(AgentError::Provider(format!(
                        "unknown criterion kind: {other}"
                    )));
                }
            };
            Ok(CriterionRequest { id, kind })
        })
        .collect::<Result<Vec<_>, AgentError>>()?;
    Ok(ClaimRequest { criteria })
}

pub(crate) fn parse_usage(usage: Option<&Value>) -> HostUsage {
    let Some(usage) = usage else {
        return HostUsage {
            estimated: true,
            ..HostUsage::default()
        };
    };
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("prompt_cache_hit_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let miss = usage
        .get("prompt_cache_miss_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("cache_miss_tokens").and_then(Value::as_u64))
        .unwrap_or_else(|| prompt_tokens.saturating_sub(cached));
    HostUsage {
        prompt_tokens,
        completion_tokens,
        cache_hit_tokens: cached,
        cache_miss_tokens: miss,
        estimated: usage
            .get("estimated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::{
        CacheClass, CacheReport, ContextDetail, ContextItem, ContextKind, ContextPlan,
        ContextScope, ReasoningEffort,
    };
    use trit_orch::{
        Limits, Observation, Plan, Resources, Snapshot, TaskBrief, Ternary, TokenUsage, task_brief,
    };
    use trit_reasonix::TurnSpec;

    fn brief() -> TaskBrief {
        let spec = TurnSpec {
            task: "turn-1".into(),
            objective: "fix bug".into(),
            constraints: vec!["do not touch schema".into()],
            stop_when: vec![],
            min_assertions: 1,
            max_tokens: 1000,
        }
        .task_spec();
        let tasks = vec![spec];
        let plan = Plan::new(1, tasks.clone()).unwrap();
        let snapshot = Snapshot {
            revision: 1,
            observations: BTreeMap::from([(
                "turn-1".to_owned(),
                Observation {
                    authorization: Ternary::True,
                    freshness: Ternary::True,
                    ..Observation::default()
                },
            )]),
        };
        let limits = Limits {
            workers: 1,
            capacity: Resources::new(1, 1),
            tokens: 1000,
        };
        let selection = plan.select(&snapshot, &limits).unwrap();
        task_brief(&plan, &selection, "turn-1").unwrap()
    }

    fn context_plan() -> ContextPlan {
        ContextPlan {
            project: "p1".into(),
            session: "s1".into(),
            model: "model-x".into(),
            provider: "https://example.test/v1".into(),
            reasoning: ReasoningEffort::Off,
            items: vec![
                ContextItem {
                    handle: "C1".into(),
                    id: "stable".into(),
                    label: "stable doc".into(),
                    kind: ContextKind::Doc,
                    scope: ContextScope::Project,
                    revision: "rev1".into(),
                    required: false,
                    cache: CacheClass::StablePrefix,
                    score: None,
                    content_hash: Some("stable-hash".into()),
                    detail: ContextDetail::Full,
                    text: Some("stable text".into()),
                    tokens: 10,
                },
                ContextItem {
                    handle: "C2".into(),
                    id: "dynamic".into(),
                    label: "dynamic doc".into(),
                    kind: ContextKind::Doc,
                    scope: ContextScope::Project,
                    revision: "rev2".into(),
                    required: false,
                    cache: CacheClass::DynamicTail,
                    score: None,
                    content_hash: None,
                    detail: ContextDetail::Full,
                    text: Some("dynamic text".into()),
                    tokens: 10,
                },
            ],
            dropped: Vec::new(),
            degradations: Vec::new(),
            budget_tokens: 100,
            estimated_tokens: 20,
            cache: CacheReport {
                prefix_items: 1,
                dynamic_items: 1,
                prefix_tokens: 10,
                dynamic_tokens: 10,
                observed_hit_rate_bp: None,
                calibration: Ternary::Unknown,
                prefix_fingerprint: "fnv1a64:abc".into(),
                hit_potential: Ternary::True,
                task_selection: trit_orch::SelectionPressure::Normal,
                invalidated: Vec::new(),
            },
            autonomy: Default::default(),
            calibration: None,
        }
    }

    fn provider_request() -> ProviderRequest {
        ProviderRequest {
            brief: brief(),
            tools: crate::default_tools(),
            receipts: vec![crate::ToolOutcome {
                call_id: "w1".into(),
                tool: "write_file".into(),
                command: None,
                passed: true,
                assertions: 0,
                written_paths: vec!["a.txt".into()],
                in_doubt: false,
                output: "wrote 5 bytes".into(),
            }],
            step: 1,
            usage: TokenUsage {
                fresh: 10,
                cached: 20,
                output: 3,
                estimated: false,
            },
            context_digest: None,
            context: None,
            projection: None,
        }
    }

    #[test]
    fn builds_openai_request_with_brief_tools_and_receipts() {
        let mut request = provider_request();
        request.context = Some(context_plan());
        let body = build_request_body(
            &OpenAiConfig::new("https://example.test/v1", "sk-test", "model-x"),
            &request,
        );
        assert_eq!(body["model"], "model-x");
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("Objective"));
        assert!(system.contains("do not touch schema"));
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("wrote 5 bytes"));
        assert!(system.contains("stable text"));
        assert!(user.contains("dynamic text"));
        assert!(user.contains("fnv1a64:abc"));
        let tools: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            tools,
            [
                "read_file",
                "write_file",
                "bash",
                "search_code",
                "glob",
                "browser",
                "download",
                "tools_batch",
                "context_search",
                "context_add",
                "context_read",
                "complete_subtask"
            ]
        );
    }

    #[test]
    fn attaches_context_inclusion_and_cache_observation() {
        let mut request = provider_request();
        request.context = Some(context_plan());
        let mut response = ProviderResponse {
            tool_calls: Vec::new(),
            completion: None,
            usage: HostUsage {
                prompt_tokens: 100,
                completion_tokens: 5,
                cache_hit_tokens: 80,
                cache_miss_tokens: 20,
                estimated: false,
            },
            prefix: None,
            context: None,
        };
        attach_context_receipt(&mut response, &request);
        let receipt = response.context.unwrap();
        let inclusion = receipt.inclusion.unwrap();
        assert_eq!(inclusion.prefix_fingerprint, "fnv1a64:abc");
        assert_eq!(inclusion.handles, ["C1", "C2"]);
        let cache = receipt.cache.unwrap();
        assert_eq!(cache.cache_hit_tokens, 80);
        assert_eq!(cache.cache_miss_tokens, 20);
        assert_eq!(cache.cache_hit_rate_bp(), 8_000);
    }

    #[test]
    fn builds_projection_view_into_user_prompt() {
        let mut request = provider_request();
        request.projection = Some(crate::context_projection::ProjectionView {
            epoch: 1,
            generation: 2,
            freshness: trit_orch::Ternary::Unknown,
            visible: vec![crate::context_projection::ProjectionEntry {
                call_id: "call-1".into(),
                tool: "bash".into(),
                step: 1,
                epoch: 1,
                passed: true,
                in_doubt: false,
                written_paths: vec!["a.txt".into()],
                chars: 5,
            }],
            last_compaction: None,
        });
        let body = build_request_body(
            &OpenAiConfig::new("https://example.test/v1", "sk-test", "model-x"),
            &request,
        );
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("\"epoch\":1"));
        assert!(user.contains("\"generation\":2"));
        assert!(user.contains("call-1"));
        assert!(user.contains("Unknown"));
    }

    #[test]
    fn parses_context_read_tool_call() {
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {"name": "context_read", "arguments": "{\"handle\":\"C1\",\"offset\":2,\"length\":3}"}}
            ]}}]
        }"#;
        let response = parse_response(body, &[]).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].tool, "context_read");
        assert_eq!(response.tool_calls[0].paths, ["C1"]);
        assert_eq!(
            response.tool_calls[0].span,
            Some(crate::ContextSpan {
                offset: 2,
                length: 3
            })
        );
        assert!(response.tool_calls[0].read_only);
    }

    #[test]
    fn parses_tool_calls_and_cache_usage() {
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}},
                {"id": "call-2", "function": {"name": "write_file", "arguments": "{\"path\":\"b.rs\",\"content\":\"x\"}"}}
            ]}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5,
                      "prompt_tokens_details": {"cached_tokens": 80}}
        }"#;
        let response = parse_response(body, &[]).unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].tool, "read_file");
        assert!(response.tool_calls[0].read_only);
        assert_eq!(response.tool_calls[1].content.as_deref(), Some("x"));
        assert_eq!(response.usage.cache_hit_tokens, 80);
        assert_eq!(response.usage.cache_miss_tokens, 20);
        assert_eq!(response.usage.completion_tokens, 5);
        assert!(!response.usage.estimated);
    }

    #[test]
    fn parses_deepseek_prompt_cache_usage_fields() {
        let body = r#"{
            "choices": [{"message": {}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 5,
                "prompt_cache_hit_tokens": 70,
                "prompt_cache_miss_tokens": 30
            }
        }"#;
        let response = parse_response(body, &[]).unwrap();
        assert_eq!(response.usage.cache_hit_tokens, 70);
        assert_eq!(response.usage.cache_miss_tokens, 30);
        assert!(!response.usage.estimated);
    }

    #[test]
    fn parses_complete_subtask() {
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {
                    "name": "complete_subtask",
                    "arguments": "{\"criteria\":[{\"id\":\"c1\",\"kind\":\"verification\",\"command\":\"cargo test\"},{\"id\":\"c2\",\"kind\":\"diff\",\"paths\":[\"a.rs\"]},{\"id\":\"c3\",\"kind\":\"manual\"}]}"
                }}
            ]}}]
        }"#;
        let response = parse_response(body, &[]).unwrap();
        let completion = response.completion.unwrap();
        assert_eq!(completion.criteria.len(), 3);
        assert!(matches!(
            completion.criteria[0].kind,
            CriterionKind::Verification { .. }
        ));
        assert!(response.usage.estimated, "missing usage is estimated");
    }

    #[test]
    fn parses_prefix_diagnostics() {
        let body = r#"{
            "choices": [{"message": {}}],
            "cache_diagnostics": {
                "prefix_changed": true,
                "prefix_change_reasons": ["compaction", "tool schema"]
            }
        }"#;
        let response = parse_response(body, &[]).unwrap();
        let prefix = response.prefix.unwrap();
        assert!(prefix.changed);
        assert_eq!(prefix.reasons, ["compaction", "tool schema"]);
    }

    #[test]
    fn dynamic_tool_schema_and_arguments_round_trip() {
        let spec = ToolSpec {
            name: "mcp__stub__echo".into(),
            description: "Echo".into(),
            read_only: true,
            input_schema: Some(json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            })),
        };
        let schemas = tool_schemas(std::slice::from_ref(&spec));
        assert_eq!(
            schemas[0]["function"]["parameters"]["properties"]["text"]["type"],
            "string"
        );
        assert_eq!(schemas[0]["function"]["name"], "mcp__stub__echo");

        let body = json!({
            "choices": [{"message": {"tool_calls": [{
                "id": "call-1",
                "function": {
                    "name": "mcp__stub__echo",
                    "arguments": "{\"text\":\"hi\"}"
                }
            }]}}]
        });
        let response = parse_response(&body.to_string(), &[spec]).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].tool, "mcp__stub__echo");
        assert_eq!(
            response.tool_calls[0].arguments,
            Some(json!({"text": "hi"}))
        );
        assert!(response.tool_calls[0].read_only);
    }

    #[test]
    fn bash_read_only_argument_is_ignored() {
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {
                    "name": "bash",
                    "arguments": "{\"command\":\"echo pwn > hooks/pre-commit\",\"read_only\":true}"
                }}
            ]}}]
        }"#;
        let response = parse_response(body, &[]).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].tool, "bash");
        assert!(
            !response.tool_calls[0].read_only,
            "model-declared read_only must never reach the permission layer"
        );
    }

    #[test]
    fn malformed_or_unknown_tools_fail_closed() {
        assert!(matches!(
            parse_response("not-json", &[]),
            Err(AgentError::Provider(_))
        ));
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {"name": "danger", "arguments": "{}"}}
            ]}}]
        }"#;
        assert!(matches!(
            parse_response(body, &[]),
            Err(AgentError::Provider(message)) if message.contains("unknown tool")
        ));
    }

    #[test]
    fn config_has_separate_connect_timeout() {
        let config = OpenAiConfig::new("http://127.0.0.1:9/v1", "k", "m");
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.timeout, Duration::from_secs(120));
    }

    fn mock_status_server(
        status: u16,
        body: &'static str,
        count: usize,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut request_body = vec![0u8; content_length];
                reader.read_exact(&mut request_body).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        (port, handle)
    }

    #[test]
    fn server_errors_degrade_health() {
        use crate::provider_health::ConnectionState;
        let (port, handle) = mock_status_server(500, "{}", 3);
        let provider = OpenAiProvider::new(OpenAiConfig::new(
            format!("http://127.0.0.1:{port}/v1"),
            "k",
            "m",
        ));
        for _ in 0..3 {
            assert!(provider.complete_chat(&json!({})).is_err());
        }
        assert_eq!(provider.health().state, ConnectionState::Unavailable);
        handle.join().unwrap();
    }

    #[test]
    fn auth_error_is_immediately_unavailable() {
        use crate::provider_health::ConnectionState;
        let (port, handle) = mock_status_server(401, "{}", 1);
        let provider = OpenAiProvider::new(OpenAiConfig::new(
            format!("http://127.0.0.1:{port}/v1"),
            "k",
            "m",
        ));
        assert!(provider.complete_chat(&json!({})).is_err());
        assert_eq!(provider.health().state, ConnectionState::Unavailable);
        handle.join().unwrap();
    }

    #[test]
    fn model_alias_maps_and_falls_back() {
        let aliases = BTreeMap::from([
            ("fast".to_owned(), "gpt-4o-mini".to_owned()),
            ("smart".to_owned(), "gpt-4o".to_owned()),
        ]);
        assert_eq!(apply_model_alias(&aliases, "fast"), "gpt-4o-mini");
        assert_eq!(apply_model_alias(&aliases, "gpt-4o"), "gpt-4o");
    }
}
