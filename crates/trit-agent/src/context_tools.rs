//! `context_read` 工具：把模型可见句柄展开成宿主侧正文。
//!
//! 它只读上下文存储，不触碰工作区；句柄必须已在 `ContextPlan` 里，
//! 未知句柄 fail-closed。每次展开记录一条本地 receipt，便于审计。

use serde::{Deserialize, Serialize};

use crate::context_layers::{ContextPlan, ContextResolver};
use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReadRecord {
    pub handle: String,
    pub id: String,
    pub chars: usize,
}

pub struct ContextReadTools<T, R> {
    inner: T,
    plan: ContextPlan,
    resolver: R,
    max_chars: usize,
    reads: Vec<ContextReadRecord>,
}

impl<T, R> ContextReadTools<T, R> {
    pub fn new(inner: T, plan: ContextPlan, resolver: R) -> Self {
        Self {
            inner,
            plan,
            resolver,
            max_chars: 8 * 1024,
            reads: Vec::new(),
        }
    }

    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars.max(1);
        self
    }

    pub fn reads(&self) -> &[ContextReadRecord] {
        &self.reads
    }
}

impl<T: ToolExecutor, R: ContextResolver> ToolExecutor for ContextReadTools<T, R> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if request.tool != "context_read" {
            return self.inner.execute(request);
        }
        let handle = request
            .paths
            .first()
            .ok_or_else(|| AgentError::Tool("context_read requires a handle".into()))?;
        let item = self
            .plan
            .items
            .iter()
            .find(|item| item.handle == *handle)
            .ok_or_else(|| AgentError::Tool(format!("unknown context handle: {handle}")))?;
        let resolved = self
            .resolver
            .resolve(&item.id)
            .map_err(|error| AgentError::Tool(format!("context read {}: {error}", item.id)))?
            .ok_or_else(|| AgentError::Tool(format!("context ref disappeared: {}", item.id)))?;
        if resolved.document.id != item.id {
            return Err(AgentError::Tool(format!(
                "context document mismatch: {} != {}",
                resolved.document.id, item.id
            )));
        }
        let text = truncate(resolved.document.full, self.max_chars);
        let chars = text.chars().count();
        self.reads.push(ContextReadRecord {
            handle: handle.clone(),
            id: item.id.clone(),
            chars,
        });
        Ok(ToolOutcome {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: text,
        })
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.reads.clear();
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

fn truncate(mut text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let end = text
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(end);
    text.push_str("...(truncated)");
    text
}
