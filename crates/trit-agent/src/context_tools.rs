//! 智能体上下文工具：`context_search` / `context_add` / `context_read`。
//!
//! 这是“智能体自主”的执行面：agent 自己搜索、加入动态上下文、按句柄读取；
//! 宿主只做 ACL/预算/secret/前缀保护。新增内容永远进动态尾段，不改稳定前缀。

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::context_layers::{AgentContext, ContextCatalog, ContextDetail, ContextPlan};
use crate::provider::ContextPlanner;
use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest, TurnContext};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReadRecord {
    pub handle: String,
    pub id: String,
    pub chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAddRecord {
    pub handle: String,
    pub id: String,
    pub detail: ContextDetail,
    pub tokens: u64,
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

impl<T: ToolExecutor, R: crate::context_layers::ContextResolver> ToolExecutor
    for ContextReadTools<T, R>
{
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

/// 智能体自主上下文工具；与 `AgentContextPlanner` 共享同一个 `AgentContext`。
pub struct AgentContextTools<T, R> {
    inner: T,
    context: Arc<Mutex<AgentContext<R>>>,
    max_chars: usize,
    reads: Vec<ContextReadRecord>,
    adds: Vec<ContextAddRecord>,
}

impl<T, R> AgentContextTools<T, R> {
    pub fn new(inner: T, context: Arc<Mutex<AgentContext<R>>>) -> Self {
        Self {
            inner,
            context,
            max_chars: 8 * 1024,
            reads: Vec::new(),
            adds: Vec::new(),
        }
    }

    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars.max(1);
        self
    }

    pub fn context(&self) -> Arc<Mutex<AgentContext<R>>> {
        Arc::clone(&self.context)
    }

    pub fn reads(&self) -> &[ContextReadRecord] {
        &self.reads
    }

    pub fn adds(&self) -> &[ContextAddRecord] {
        &self.adds
    }

    fn lock(&self) -> Result<MutexGuard<'_, AgentContext<R>>, AgentError> {
        self.context
            .lock()
            .map_err(|_| AgentError::Tool("context state poisoned".into()))
    }
}

impl<T: ToolExecutor, R: ContextCatalog> ToolExecutor for AgentContextTools<T, R> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        match request.tool.as_str() {
            "context_search" => {
                let query = request
                    .command
                    .as_deref()
                    .or_else(|| request.paths.first().map(String::as_str))
                    .unwrap_or_default();
                let limit = request
                    .content
                    .as_deref()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(5);
                let hits = self.lock()?.search(query, limit);
                let output = hits
                    .iter()
                    .map(|reference| format!("{}\t{}", reference.id, reference.label))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(outcome(request, output))
            }
            "context_add" => {
                let id = request
                    .paths
                    .first()
                    .ok_or_else(|| AgentError::Tool("context_add requires an id".into()))?;
                let item = self
                    .lock()?
                    .add(id)
                    .map_err(|error| AgentError::Tool(error.to_string()))?;
                self.adds.push(ContextAddRecord {
                    handle: item.handle.clone(),
                    id: item.id.clone(),
                    detail: item.detail,
                    tokens: item.tokens,
                });
                Ok(outcome(
                    request,
                    format!("added {} {}", item.handle, item.id),
                ))
            }
            "context_read" => {
                let handle = request
                    .paths
                    .first()
                    .ok_or_else(|| AgentError::Tool("context_read requires a handle".into()))?;
                let (item, full) = self
                    .lock()?
                    .read(handle)
                    .map_err(|error| AgentError::Tool(error.to_string()))?;
                let text = truncate(full, self.max_chars);
                let chars = text.chars().count();
                self.reads.push(ContextReadRecord {
                    handle: handle.clone(),
                    id: item.id,
                    chars,
                });
                Ok(outcome(request, text))
            }
            _ => self.inner.execute(request),
        }
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.reads.clear();
        self.adds.clear();
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

/// 把共享的 `AgentContext` 每轮快照成 `ContextPlan`，供 ProviderModel 使用。
pub struct AgentContextPlanner<R> {
    context: Arc<Mutex<AgentContext<R>>>,
}

impl<R> AgentContextPlanner<R> {
    pub fn new(context: Arc<Mutex<AgentContext<R>>>) -> Self {
        Self { context }
    }
}

impl<R: ContextCatalog> ContextPlanner for AgentContextPlanner<R> {
    fn plan(
        &mut self,
        _brief: &trit_orch::TaskBrief,
        _context: &TurnContext,
    ) -> Result<Option<ContextPlan>, AgentError> {
        let context = self
            .context
            .lock()
            .map_err(|_| AgentError::Tool("context state poisoned".into()))?;
        Ok(Some(context.plan().clone()))
    }
}

fn outcome(request: &ToolRequest, output: String) -> ToolOutcome {
    ToolOutcome {
        call_id: request.call_id.clone(),
        tool: request.tool.clone(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: Vec::new(),
        in_doubt: false,
        output,
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
