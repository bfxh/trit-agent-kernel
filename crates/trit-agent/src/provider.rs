//! Provider 契约：把宿主模型调用与具体 SDK / HTTP 解耦。
//!
//! 这里只有同步、纯数据的接口；真实 OpenAI-compatible 传输层由宿主实现。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{
    AgentError, ContextPlan, ContextReceipt, Model, PrefixSignal, Proposal, ToolOutcome,
    ToolRequest, TurnContext,
};
use trit_orch::{TaskBrief, TokenUsage};
use trit_reasonix::{ClaimRequest, HostUsage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub read_only: bool,
    /// 动态工具（如 MCP）的 JSON Schema；内置工具为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

/// 每轮由宿主提供的上下文计划；ProviderModel 不知道 catalog/ACL。
/// 启用后宿主还必须用 `ContextReadTools` 包装 ToolExecutor，否则模型调用
/// `context_read` 时执行器会报 unknown tool。
pub trait ContextPlanner {
    fn plan(
        &mut self,
        brief: &TaskBrief,
        context: &TurnContext,
    ) -> Result<Option<ContextPlan>, AgentError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub brief: TaskBrief,
    pub tools: Vec<ToolSpec>,
    pub receipts: Vec<ToolOutcome>,
    pub step: usize,
    pub usage: TokenUsage,
    pub context_digest: Option<String>,
    pub context: Option<ContextPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub tool_calls: Vec<ToolRequest>,
    pub completion: Option<ClaimRequest>,
    pub usage: HostUsage,
    /// 可选：上下文 prefix 变化（Reasonix/宿主诊断）。
    pub prefix: Option<PrefixSignal>,
    /// 可选：本轮 inclusion receipt 与实际 cache hit/miss 对照。
    pub context: Option<ContextReceipt>,
}

pub trait Provider {
    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, AgentError>;
}

pub struct ProviderModel<P> {
    provider: P,
    tools: Vec<ToolSpec>,
    context_planner: Option<Box<dyn ContextPlanner>>,
}

impl<P: Provider> ProviderModel<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            tools: default_tools(),
            context_planner: None,
        }
    }

    pub fn with_tools(provider: P, tools: Vec<ToolSpec>) -> Self {
        Self {
            provider,
            tools,
            context_planner: None,
        }
    }

    pub fn with_context_planner(mut self, planner: Box<dyn ContextPlanner>) -> Self {
        self.context_planner = Some(planner);
        self
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }
}

impl<P: Provider> Model for ProviderModel<P> {
    fn proposal(
        &mut self,
        brief: &TaskBrief,
        context: &TurnContext,
    ) -> Result<Proposal, AgentError> {
        let plan = match self.context_planner.as_mut() {
            Some(planner) => planner.plan(brief, context)?,
            None => None,
        };
        // 没有上下文计划时不要向模型暴露 context_read：CLI 默认没有 catalog。
        let tools = if plan.is_some() {
            self.tools.clone()
        } else {
            self.tools
                .iter()
                .filter(|tool| !tool.name.starts_with("context_"))
                .cloned()
                .collect()
        };
        let response = self.provider.complete(&ProviderRequest {
            brief: brief.clone(),
            tools,
            receipts: context.receipts.clone(),
            step: context.step,
            usage: context.usage,
            context_digest: context.digest.clone(),
            context: plan,
        })?;
        Ok(Proposal {
            tool_calls: response.tool_calls,
            completion: response.completion,
            usage: response.usage,
            prefix: response.prefix,
            context_receipt: response.context,
        })
    }
}

pub fn default_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 file inside the workspace.".into(),
            read_only: true,
            input_schema: None,
        },
        ToolSpec {
            name: "write_file".into(),
            description: "Write a UTF-8 file inside the workspace; requires allow-write.".into(),
            read_only: false,
            input_schema: None,
        },
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the workspace; requires allow-shell.".into(),
            read_only: false,
            input_schema: None,
        },
        ToolSpec {
            name: "context_search".into(),
            description: "Search the context catalog and return candidate ids/labels.".into(),
            read_only: true,
            input_schema: None,
        },
        ToolSpec {
            name: "context_add".into(),
            description: "Add a searched context ref to the dynamic tail and return its handle."
                .into(),
            read_only: false,
            input_schema: None,
        },
        ToolSpec {
            name: "context_read".into(),
            description: "Expand a context handle (C1, C2, ...) into its full text.".into(),
            read_only: true,
            input_schema: None,
        },
        ToolSpec {
            name: "complete_subtask".into(),
            description: "Submit a completion claim with criteria backed by tool receipts.".into(),
            read_only: true,
            input_schema: None,
        },
    ]
}

/// 测试/回放用：按顺序返回预置响应，并记录收到的请求。
#[derive(Debug, Clone, Default)]
pub struct ScriptedProvider {
    queue: VecDeque<ProviderResponse>,
    requests: Vec<ProviderRequest>,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            queue: responses.into(),
            requests: Vec::new(),
        }
    }

    pub fn requests(&self) -> &[ProviderRequest] {
        &self.requests
    }
}

impl Provider for ScriptedProvider {
    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, AgentError> {
        self.requests.push(request.clone());
        self.queue.pop_front().ok_or(AgentError::ModelExhausted)
    }
}
