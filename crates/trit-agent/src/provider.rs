//! Provider 契约：把宿主模型调用与具体 SDK / HTTP 解耦。
//!
//! 这里只有同步、纯数据的接口；真实 OpenAI-compatible 传输层由宿主实现。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{AgentError, Model, PrefixSignal, Proposal, ToolOutcome, ToolRequest, TurnContext};
use trit_orch::{TaskBrief, TokenUsage};
use trit_reasonix::{ClaimRequest, HostUsage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub brief: TaskBrief,
    pub tools: Vec<ToolSpec>,
    pub receipts: Vec<ToolOutcome>,
    pub step: usize,
    pub usage: TokenUsage,
    pub context_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub tool_calls: Vec<ToolRequest>,
    pub completion: Option<ClaimRequest>,
    pub usage: HostUsage,
    /// 可选：上下文 prefix 变化（Reasonix/宿主诊断）。
    pub prefix: Option<PrefixSignal>,
}

pub trait Provider {
    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, AgentError>;
}

pub struct ProviderModel<P> {
    provider: P,
    tools: Vec<ToolSpec>,
}

impl<P: Provider> ProviderModel<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            tools: default_tools(),
        }
    }

    pub fn with_tools(provider: P, tools: Vec<ToolSpec>) -> Self {
        Self { provider, tools }
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
        let response = self.provider.complete(&ProviderRequest {
            brief: brief.clone(),
            tools: self.tools.clone(),
            receipts: context.receipts.clone(),
            step: context.step,
            usage: context.usage,
            context_digest: context.digest.clone(),
        })?;
        Ok(Proposal {
            tool_calls: response.tool_calls,
            completion: response.completion,
            usage: response.usage,
            prefix: response.prefix,
        })
    }
}

pub fn default_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 file inside the workspace.".into(),
            read_only: true,
        },
        ToolSpec {
            name: "write_file".into(),
            description: "Write a UTF-8 file inside the workspace; requires allow-write.".into(),
            read_only: false,
        },
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the workspace; requires allow-shell.".into(),
            read_only: false,
        },
        ToolSpec {
            name: "complete_subtask".into(),
            description: "Submit a completion claim with criteria backed by tool receipts.".into(),
            read_only: true,
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
