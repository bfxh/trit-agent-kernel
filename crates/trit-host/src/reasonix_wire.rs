//! Reasonix wire 兼容契约（B7.1）。
//!
//! 这里只做版本化解析与映射，不接入现有 provider / 工具执行路径。字段名、
//! 省略规则以 `1012905/DeepSeek-Reasonix-rs@5edbda1` 的 `eventwire` 为基线；
//! 未消费的字段放进 `extra` 原样保留，避免适配层吃掉上游新增字段。

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{PrefixSignal, ToolRequest};
use trit_orch::TokenUsage;
use trit_reasonix::HostUsage;

/// 当前适配器理解的 Reasonix wire 版本。
pub const REASONIX_WIRE_VERSION: u32 = 1;

/// `eventwire.Event.kind` 的已知取值，顺序与上游一致。
pub const REASONIX_EVENT_KINDS: &[&str] = &[
    "turn_started",
    "reasoning",
    "text",
    "message",
    "tool_dispatch",
    "tool_result",
    "usage",
    "notice",
    "phase",
    "approval_request",
    "ask_request",
    "turn_done",
    "compaction_started",
    "compaction_progress",
    "compaction_done",
    "tool_progress",
    "mcp_surface_ready",
    "retrying",
    "steer",
    "guardian_assessment",
    "extension_surface",
    "extension_status",
    "stream_attempt",
    "context_maintenance",
    "todo_progress",
    "workspace_lease",
    "workspace_changed",
    "turn_phase",
    "completion_summary",
    "adjudications_changed",
    "browser_tabs_changed",
    "inbox_changed",
    "rewind_changed",
    "graph_delta",
    "progress_watch",
    "ask_engaged",
];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireContractError {
    #[error("unsupported wire version {found}, expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("field {field} is negative: {value}")]
    NegativeToken { field: &'static str, value: i64 },
    #[error("event seq is negative: {0}")]
    NegativeSeq(i64),
    #[error("tool {tool} has negative duration: {value}")]
    NegativeDuration { tool: String, value: i64 },
    #[error("invalid {context} JSON: {message}")]
    InvalidJson { context: String, message: String },
    #[error("tool {tool} arguments are not valid JSON: {message}")]
    InvalidToolArguments { tool: String, message: String },
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}

fn is_empty_string(value: &str) -> bool {
    value.is_empty()
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 带来源与版本的 fixture 外壳；payload 本身保持上游字段形状。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFixture<T> {
    pub version: u32,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub name: String,
    pub payload: T,
}

impl<T> WireFixture<T> {
    pub fn validate(&self) -> Result<(), WireContractError> {
        if self.version != REASONIX_WIRE_VERSION {
            return Err(WireContractError::UnsupportedVersion {
                found: self.version,
                expected: REASONIX_WIRE_VERSION,
            });
        }
        Ok(())
    }
}

pub fn parse_fixture<T: DeserializeOwned>(json: &str) -> Result<WireFixture<T>, WireContractError> {
    let fixture: WireFixture<T> =
        serde_json::from_str(json).map_err(|error| WireContractError::InvalidJson {
            context: "fixture".into(),
            message: error.to_string(),
        })?;
    fixture.validate()?;
    Ok(fixture)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireCacheDiagnostics {
    #[serde(rename = "prefixHash", default)]
    pub prefix_hash: String,
    #[serde(rename = "prefixChanged", default)]
    pub prefix_changed: bool,
    #[serde(
        rename = "prefixChangeReasons",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub prefix_change_reasons: Vec<String>,
    #[serde(rename = "systemHash", default)]
    pub system_hash: String,
    #[serde(rename = "toolsHash", default)]
    pub tools_hash: String,
    #[serde(rename = "logRewriteVersion", default)]
    pub log_rewrite_version: i64,
    #[serde(rename = "toolSchemaTokens", default)]
    pub tool_schema_tokens: i64,
    #[serde(rename = "cacheMissTokens", default)]
    pub cache_miss_tokens: i64,
    #[serde(rename = "cacheHitTokens", default)]
    pub cache_hit_tokens: i64,
    #[serde(rename = "carriedMessages", default)]
    pub carried_messages: i64,
    #[serde(rename = "bodyChanged", default)]
    pub body_changed: bool,
    #[serde(rename = "bodyHash", default)]
    pub body_hash: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl WireCacheDiagnostics {
    fn validate(&self) -> Result<(), WireContractError> {
        for (field, value) in [
            ("cacheDiagnostics.cacheHitTokens", self.cache_hit_tokens),
            ("cacheDiagnostics.cacheMissTokens", self.cache_miss_tokens),
            ("cacheDiagnostics.toolSchemaTokens", self.tool_schema_tokens),
        ] {
            if value < 0 {
                return Err(WireContractError::NegativeToken { field, value });
            }
        }
        Ok(())
    }

    pub fn prefix_signal(&self) -> PrefixSignal {
        PrefixSignal {
            changed: self.prefix_changed,
            reasons: self.prefix_change_reasons.clone(),
        }
    }
}

/// Reasonix `eventwire/usage.Usage` 的兼容子集。
///
/// 计费明细对象保留在 `extra` / `Value` 中，不让适配层丢失上游字段；本层只
/// 映射内核真正消费的 token、estimated 与 cache diagnostics。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireUsage {
    #[serde(rename = "promptTokens", default)]
    pub prompt_tokens: i64,
    #[serde(rename = "completionTokens", default)]
    pub completion_tokens: i64,
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: i64,
    #[serde(rename = "cacheHitTokens", default)]
    pub cache_hit_tokens: i64,
    #[serde(rename = "cacheMissTokens", default)]
    pub cache_miss_tokens: i64,
    #[serde(
        rename = "reasoningTokens",
        default,
        skip_serializing_if = "is_zero_i64"
    )]
    pub reasoning_tokens: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub source: String,
    #[serde(rename = "attemptId", default, skip_serializing_if = "is_empty_string")]
    pub attempt_id: String,
    #[serde(
        rename = "cacheDiagnostics",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_diagnostics: Option<WireCacheDiagnostics>,
    #[serde(rename = "sessionCacheHitTokens", default)]
    pub session_cache_hit_tokens: i64,
    #[serde(rename = "sessionCacheMissTokens", default)]
    pub session_cache_miss_tokens: i64,
    #[serde(
        rename = "contextPromptTokens",
        default,
        skip_serializing_if = "is_zero_i64"
    )]
    pub context_prompt_tokens: i64,
    #[serde(
        rename = "contextCompletionTokens",
        default,
        skip_serializing_if = "is_zero_i64"
    )]
    pub context_completion_tokens: i64,
    #[serde(
        rename = "contextReasoningTokens",
        default,
        skip_serializing_if = "is_zero_i64"
    )]
    pub context_reasoning_tokens: i64,
    #[serde(
        rename = "contextCacheHitTokens",
        default,
        skip_serializing_if = "is_zero_i64"
    )]
    pub context_cache_hit_tokens: i64,
    #[serde(
        rename = "contextCacheMissTokens",
        default,
        skip_serializing_if = "is_zero_i64"
    )]
    pub context_cache_miss_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub cost: f64,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub currency: String,
    #[serde(
        rename = "currencyCode",
        default,
        skip_serializing_if = "is_empty_string"
    )]
    pub currency_code: String,
    #[serde(rename = "costUsd", default, skip_serializing_if = "is_zero_f64")]
    pub cost_usd: f64,
    #[serde(rename = "costQuote", default, skip_serializing_if = "Option::is_none")]
    pub cost_quote: Option<Value>,
    #[serde(rename = "costComplete", default, skip_serializing_if = "is_false")]
    pub cost_complete: bool,
    #[serde(rename = "displayComplete", default, skip_serializing_if = "is_false")]
    pub display_complete: bool,
    #[serde(
        rename = "displayStatus",
        default,
        skip_serializing_if = "is_empty_string"
    )]
    pub display_status: String,
    #[serde(
        rename = "aggregateMode",
        default,
        skip_serializing_if = "is_empty_string"
    )]
    pub aggregate_mode: String,
    #[serde(
        rename = "originalTotals",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub original_totals: Vec<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl WireUsage {
    pub fn validate(&self) -> Result<(), WireContractError> {
        for (field, value) in [
            ("promptTokens", self.prompt_tokens),
            ("completionTokens", self.completion_tokens),
            ("totalTokens", self.total_tokens),
            ("cacheHitTokens", self.cache_hit_tokens),
            ("cacheMissTokens", self.cache_miss_tokens),
            ("reasoningTokens", self.reasoning_tokens),
            ("sessionCacheHitTokens", self.session_cache_hit_tokens),
            ("sessionCacheMissTokens", self.session_cache_miss_tokens),
            ("contextPromptTokens", self.context_prompt_tokens),
            ("contextCompletionTokens", self.context_completion_tokens),
            ("contextReasoningTokens", self.context_reasoning_tokens),
            ("contextCacheHitTokens", self.context_cache_hit_tokens),
            ("contextCacheMissTokens", self.context_cache_miss_tokens),
        ] {
            if value < 0 {
                return Err(WireContractError::NegativeToken { field, value });
            }
        }
        if let Some(diagnostics) = &self.cache_diagnostics {
            diagnostics.validate()?;
        }
        Ok(())
    }

    pub fn to_host_usage(&self) -> Result<HostUsage, WireContractError> {
        self.validate()?;
        Ok(HostUsage {
            prompt_tokens: self.prompt_tokens as u64,
            completion_tokens: self.completion_tokens as u64,
            cache_hit_tokens: self.cache_hit_tokens as u64,
            cache_miss_tokens: self.cache_miss_tokens as u64,
            estimated: self.estimated,
        })
    }

    pub fn to_token_usage(&self) -> Result<TokenUsage, WireContractError> {
        Ok(self.to_host_usage()?.into_token_usage())
    }

    pub fn prefix_signal(&self) -> Option<PrefixSignal> {
        self.cache_diagnostics
            .as_ref()
            .map(WireCacheDiagnostics::prefix_signal)
    }
}

/// Reasonix `eventwire/tool.Tool` 的兼容子集；dispatch 与 result 共用该帧。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub args: String,
    #[serde(
        rename = "resolvedName",
        default,
        skip_serializing_if = "is_empty_string"
    )]
    pub resolved_name: String,
    #[serde(
        rename = "capabilityId",
        default,
        skip_serializing_if = "is_empty_string"
    )]
    pub capability_id: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub output: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub err: String,
    #[serde(
        rename = "refusalCode",
        default,
        skip_serializing_if = "is_empty_string"
    )]
    pub refusal_code: String,
    #[serde(rename = "readOnly", default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(rename = "durationMs", default, skip_serializing_if = "is_zero_i64")]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    #[serde(rename = "parentId", default, skip_serializing_if = "is_empty_string")]
    pub parent_id: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub issuer: String,
    #[serde(rename = "attemptId", default, skip_serializing_if = "is_empty_string")]
    pub attempt_id: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl WireTool {
    pub fn validate(&self) -> Result<(), WireContractError> {
        if self.duration_ms < 0 {
            return Err(WireContractError::NegativeDuration {
                tool: self.id.clone(),
                value: self.duration_ms,
            });
        }
        Ok(())
    }

    pub fn arguments(&self) -> Result<Option<Value>, WireContractError> {
        if self.args.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&self.args).map(Some).map_err(|error| {
            WireContractError::InvalidToolArguments {
                tool: self.id.clone(),
                message: error.to_string(),
            }
        })
    }

    pub fn passed(&self) -> bool {
        self.err.is_empty() && self.refusal_code.is_empty()
    }

    pub fn in_doubt(&self) -> bool {
        self.partial
    }

    /// 转成工具循环请求；本地工具的 command/paths 仍由后续适配层补齐。
    pub fn to_tool_request(&self) -> Result<ToolRequest, WireContractError> {
        self.validate()?;
        Ok(ToolRequest {
            call_id: self.id.clone(),
            tool: if self.resolved_name.is_empty() {
                self.name.clone()
            } else {
                self.resolved_name.clone()
            },
            read_only: self.read_only,
            arguments: self.arguments()?,
            ..ToolRequest::default()
        })
    }
}

/// Reasonix `eventwire/event.Event` 的兼容子集。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireEvent {
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub seq: i64,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub code: String,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<WireTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<WireUsage>,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub err: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "is_empty_string")]
    pub outcome: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl WireEvent {
    pub fn validate(&self) -> Result<(), WireContractError> {
        if self.seq < 0 {
            return Err(WireContractError::NegativeSeq(self.seq));
        }
        if let Some(tool) = &self.tool {
            tool.validate()?;
        }
        if let Some(usage) = &self.usage {
            usage.validate()?;
        }
        Ok(())
    }

    pub fn is_known_kind(&self) -> bool {
        REASONIX_EVENT_KINDS.contains(&self.kind.as_str())
    }

    pub fn token_usage(&self) -> Result<Option<TokenUsage>, WireContractError> {
        self.usage
            .as_ref()
            .map(WireUsage::to_token_usage)
            .transpose()
    }

    pub fn prefix_signal(&self) -> Option<PrefixSignal> {
        self.usage.as_ref().and_then(WireUsage::prefix_signal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> WireUsage {
        WireUsage {
            prompt_tokens: 1_200,
            completion_tokens: 300,
            total_tokens: 1_500,
            cache_hit_tokens: 900,
            cache_miss_tokens: 300,
            estimated: false,
            ..WireUsage::default()
        }
    }

    #[test]
    fn usage_maps_hit_miss_and_preserves_estimated() {
        let host = usage().to_host_usage().unwrap();
        assert_eq!(host.prompt_tokens, 1_200);
        assert_eq!(host.completion_tokens, 300);
        assert_eq!(host.cache_hit_tokens, 900);
        assert_eq!(host.cache_miss_tokens, 300);
        assert!(!host.estimated);

        let token = usage().to_token_usage().unwrap();
        assert_eq!(token.fresh, 300);
        assert_eq!(token.cached, 900);
        assert_eq!(token.output, 300);
        assert!(token.is_exact());
    }

    #[test]
    fn negative_token_fields_fail_closed() {
        let mut usage = usage();
        usage.prompt_tokens = -1;
        assert!(matches!(
            usage.to_host_usage(),
            Err(WireContractError::NegativeToken {
                field: "promptTokens",
                value: -1
            })
        ));
    }

    #[test]
    fn tool_arguments_parse_and_invalid_json_is_typed() {
        let mut tool = WireTool {
            id: "call-1".into(),
            name: "shell".into(),
            resolved_name: "bash".into(),
            args: "{\"command\":\"cargo test\"}".into(),
            ..WireTool::default()
        };
        let request = tool.to_tool_request().unwrap();
        assert_eq!(request.call_id, "call-1");
        assert_eq!(request.tool, "bash");
        assert!(!request.read_only);
        assert_eq!(request.arguments.unwrap()["command"], "cargo test");

        tool.args = "{".into();
        assert!(matches!(
            tool.arguments(),
            Err(WireContractError::InvalidToolArguments { .. })
        ));
    }

    #[test]
    fn event_keeps_unknown_upstream_fields() {
        let event: WireEvent = serde_json::from_str(
            r#"{"kind":"turn_done","seq":7,"retryAttempt":2,"nonPersistable":true}"#,
        )
        .unwrap();
        event.validate().unwrap();
        assert_eq!(event.extra["retryAttempt"], 2);
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("\"retryAttempt\":2"));
        assert!(encoded.contains("\"nonPersistable\":true"));
        assert!(event.is_known_kind());
    }

    #[test]
    fn negative_seq_and_duration_fail_closed() {
        let event = WireEvent {
            seq: -1,
            ..WireEvent::default()
        };
        assert!(matches!(
            event.validate(),
            Err(WireContractError::NegativeSeq(-1))
        ));

        let tool = WireTool {
            id: "call-1".into(),
            duration_ms: -1,
            ..WireTool::default()
        };
        assert!(matches!(
            tool.validate(),
            Err(WireContractError::NegativeDuration { .. })
        ));
    }
}
