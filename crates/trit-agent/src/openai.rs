//! OpenAI-compatible Chat Completions provider（阻塞式）。
//!
//! 只负责 HTTP 传输、工具 schema 与响应解析；不让内核接触网络。
//! API key 只在 Authorization 头里出现，Debug 输出会脱敏。

use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};

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
    pub max_output_tokens: Option<u64>,
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("max_output_tokens", &self.max_output_tokens)
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
            max_output_tokens: None,
        }
    }

    pub fn from_env() -> Result<Self, AgentError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| AgentError::Provider("OPENAI_API_KEY is not set".into()))?;
        let model = std::env::var("OPENAI_MODEL")
            .map_err(|_| AgentError::Provider("OPENAI_MODEL is not set".into()))?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        Ok(Self::new(base_url, api_key, model))
    }
}

pub struct OpenAiProvider {
    config: OpenAiConfig,
    agent: ureq::Agent,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(config.timeout).build();
        Self { config, agent }
    }
}

impl Provider for OpenAiProvider {
    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, AgentError> {
        let body = build_request_body(&self.config, request);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match response {
            Ok(response) => {
                let text = response
                    .into_string()
                    .map_err(|error| AgentError::Provider(format!("read response: {error}")))?;
                let mut response = parse_response(&text)?;
                attach_context_receipt(&mut response, request);
                Ok(response)
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                Err(AgentError::Provider(format!("HTTP {code}: {text}")))
            }
            Err(error) => Err(AgentError::Provider(format!("transport: {error}"))),
        }
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
    let user = json!({
        "step": request.step,
        "usage": request.usage,
        "receipts": request.receipts,
        "context_digest": request.context_digest,
        "context_tail": context_tail,
        "context_fingerprint": context_fingerprint,
        "instruction": "Continue the task. Call tools as needed. When finished, call complete_subtask."
    });
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
            let parameters = match tool.name.as_str() {
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
                    "properties": {
                        "command": {"type": "string"},
                        "read_only": {"type": "boolean"}
                    },
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

fn parse_response(body: &str) -> Result<ProviderResponse, AgentError> {
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
    let usage = parse_usage(value.get("usage"));
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
                    let read_only = arguments
                        .get("read_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let mut request = ToolRequest::shell(call_id, command);
                    request.read_only = read_only;
                    tool_calls.push(request);
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
                    return Err(AgentError::Provider(format!("unknown tool: {other}")));
                }
            }
        }
    }
    Ok(ProviderResponse {
        tool_calls,
        completion,
        usage,
        prefix: parse_prefix(&value),
        context: None,
    })
}

fn attach_context_receipt(response: &mut ProviderResponse, request: &ProviderRequest) {
    let Some(plan) = &request.context else {
        return;
    };
    let cache = CacheObservation {
        model: plan.model.clone(),
        prefix_fingerprint: plan.cache.prefix_fingerprint.clone(),
        prompt_tokens: response.usage.prompt_tokens,
        cache_hit_tokens: response.usage.cache_hit_tokens,
        cache_miss_tokens: response.usage.cache_miss_tokens,
        estimated: response.usage.estimated,
    };
    response.context = Some(ContextReceipt {
        inclusion: Some(plan.inclusion_receipt()),
        cache: Some(cache),
    });
}

fn parse_prefix(value: &Value) -> Option<PrefixSignal> {
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

fn parse_usage(usage: Option<&Value>) -> HostUsage {
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
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let miss = usage
        .get("cache_miss_tokens")
        .and_then(Value::as_u64)
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
    fn parses_context_read_tool_call() {
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {"name": "context_read", "arguments": "{\"handle\":\"C1\",\"offset\":2,\"length\":3}"}}
            ]}}]
        }"#;
        let response = parse_response(body).unwrap();
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
        let response = parse_response(body).unwrap();
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
    fn parses_complete_subtask() {
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {
                    "name": "complete_subtask",
                    "arguments": "{\"criteria\":[{\"id\":\"c1\",\"kind\":\"verification\",\"command\":\"cargo test\"},{\"id\":\"c2\",\"kind\":\"diff\",\"paths\":[\"a.rs\"]},{\"id\":\"c3\",\"kind\":\"manual\"}]}"
                }}
            ]}}]
        }"#;
        let response = parse_response(body).unwrap();
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
        let response = parse_response(body).unwrap();
        let prefix = response.prefix.unwrap();
        assert!(prefix.changed);
        assert_eq!(prefix.reasons, ["compaction", "tool schema"]);
    }

    #[test]
    fn malformed_or_unknown_tools_fail_closed() {
        assert!(matches!(
            parse_response("not-json"),
            Err(AgentError::Provider(_))
        ));
        let body = r#"{
            "choices": [{"message": {"tool_calls": [
                {"id": "call-1", "function": {"name": "danger", "arguments": "{}"}}
            ]}}]
        }"#;
        assert!(matches!(
            parse_response(body),
            Err(AgentError::Provider(message)) if message.contains("unknown tool")
        ));
    }
}
