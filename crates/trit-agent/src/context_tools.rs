//! 智能体上下文工具：`context_search` / `context_add` / `context_read`。
//!
//! 这是“智能体自主”的执行面：agent 自己搜索、加入动态上下文、按句柄读取；
//! 宿主只做 ACL/预算/secret/前缀保护。新增内容永远进动态尾段，不改稳定前缀。

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::context_layers::{
    AgentContext, CacheLedger, CacheWindow, ContextCatalog, ContextDetail, ContextPlan, ContextSpan,
};
use crate::provider::ContextPlanner;
use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest, TurnContext};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReadRecord {
    pub handle: String,
    pub id: String,
    pub chars: usize,
    #[serde(default)]
    pub span: Option<ContextSpan>,
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
    read_budget_chars: usize,
    read_used: usize,
    reads: Vec<ContextReadRecord>,
}

impl<T, R> ContextReadTools<T, R> {
    pub fn new(inner: T, plan: ContextPlan, resolver: R) -> Self {
        Self {
            inner,
            plan,
            resolver,
            max_chars: 8 * 1024,
            read_budget_chars: 16 * 1024,
            read_used: 0,
            reads: Vec::new(),
        }
    }

    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars.max(1);
        self
    }

    pub fn with_read_budget(mut self, read_budget_chars: usize) -> Self {
        self.read_budget_chars = read_budget_chars.max(1);
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
        let remaining = self.read_budget_chars.saturating_sub(self.read_used);
        let (text, chars) = read_slice(
            resolved.document.full,
            request.span,
            self.max_chars,
            remaining,
        )?;
        self.read_used = self.read_used.saturating_add(chars);
        self.reads.push(ContextReadRecord {
            handle: handle.clone(),
            id: item.id.clone(),
            chars,
            span: request.span,
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
        self.read_used = 0;
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
    read_budget_chars: usize,
    read_used: usize,
    reads: Vec<ContextReadRecord>,
    adds: Vec<ContextAddRecord>,
}

impl<T, R> AgentContextTools<T, R> {
    pub fn new(inner: T, context: Arc<Mutex<AgentContext<R>>>) -> Self {
        Self {
            inner,
            context,
            max_chars: 8 * 1024,
            read_budget_chars: 16 * 1024,
            read_used: 0,
            reads: Vec::new(),
            adds: Vec::new(),
        }
    }

    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars.max(1);
        self
    }

    pub fn with_read_budget(mut self, read_budget_chars: usize) -> Self {
        self.read_budget_chars = read_budget_chars.max(1);
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
                let remaining = self.read_budget_chars.saturating_sub(self.read_used);
                let (text, chars) = read_slice(full, request.span, self.max_chars, remaining)?;
                self.read_used = self.read_used.saturating_add(chars);
                self.reads.push(ContextReadRecord {
                    handle: handle.clone(),
                    id: item.id,
                    chars,
                    span: request.span,
                });
                Ok(outcome(request, text))
            }
            _ => self.inner.execute(request),
        }
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.reads.clear();
        self.adds.clear();
        self.read_used = 0;
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

/// 把共享的 `AgentContext` 每轮快照成 `ContextPlan`，供 ProviderModel 使用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LowHitPolicy {
    #[default]
    Keep,
    DemotePrefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePressurePolicy {
    /// 低命中时动态添加/选批预算的缩放（基点）；10000 表示不缩放。
    pub dynamic_budget_scale_bp: u32,
    /// 低命中时 context_search 候选数的缩放（基点）。
    pub search_limit_scale_bp: u32,
}

impl Default for CachePressurePolicy {
    fn default() -> Self {
        Self {
            dynamic_budget_scale_bp: 10_000,
            search_limit_scale_bp: 10_000,
        }
    }
}

impl CachePressurePolicy {
    pub fn enabled(self) -> bool {
        self.dynamic_budget_scale_bp < 10_000 || self.search_limit_scale_bp < 10_000
    }
}

pub struct AgentContextPlanner<R> {
    context: Arc<Mutex<AgentContext<R>>>,
    cache_ledger: Option<Arc<Mutex<CacheLedger>>>,
    hit_threshold_bp: u32,
    low_hit_policy: LowHitPolicy,
    cache_window: CacheWindow,
    cache_pressure: CachePressurePolicy,
}

impl<R> AgentContextPlanner<R> {
    pub fn new(context: Arc<Mutex<AgentContext<R>>>) -> Self {
        Self {
            context,
            cache_ledger: None,
            hit_threshold_bp: 5_000,
            low_hit_policy: LowHitPolicy::Keep,
            cache_window: CacheWindow::default(),
            cache_pressure: CachePressurePolicy::default(),
        }
    }

    pub fn with_cache_ledger(mut self, cache_ledger: Arc<Mutex<CacheLedger>>) -> Self {
        self.cache_ledger = Some(cache_ledger);
        self
    }

    pub fn with_hit_threshold_bp(mut self, hit_threshold_bp: u32) -> Self {
        self.hit_threshold_bp = hit_threshold_bp;
        self
    }

    pub fn with_low_hit_policy(mut self, low_hit_policy: LowHitPolicy) -> Self {
        self.low_hit_policy = low_hit_policy;
        self
    }

    pub fn with_cache_window(mut self, cache_window: CacheWindow) -> Self {
        self.cache_window = cache_window;
        self
    }

    pub fn with_cache_pressure_policy(mut self, cache_pressure: CachePressurePolicy) -> Self {
        self.cache_pressure = cache_pressure;
        self
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
        let mut plan = context.plan().clone();
        drop(context);
        let mut pressure_scale_bp = 10_000;
        let mut search_scale_bp = 10_000;
        if let Some(cache_ledger) = &self.cache_ledger {
            let cache_ledger = cache_ledger
                .lock()
                .map_err(|_| AgentError::Tool("cache ledger poisoned".into()))?;
            if let Some(stat) =
                cache_ledger.stat_for(&plan.provider, &plan.model, &plan.cache.prefix_fingerprint)
            {
                let window = stat.windowed(now_ms(), self.cache_window);
                plan.cache.observed_hit_rate_bp = Some(window.calibrated_hit_rate_bp());
                plan.cache.calibration = window.confidence();
                if window.confidence() == trit_orch::Ternary::True {
                    plan.cache.hit_potential = trit_orch::Ternary::from_bool(
                        window.calibrated_hit_rate_bp() >= self.hit_threshold_bp,
                    );
                    if plan.cache.hit_potential == trit_orch::Ternary::False {
                        if self.low_hit_policy == LowHitPolicy::DemotePrefix {
                            plan.demote_stable_prefix();
                        }
                        if self.cache_pressure.enabled() {
                            pressure_scale_bp = self.cache_pressure.dynamic_budget_scale_bp;
                            search_scale_bp = self.cache_pressure.search_limit_scale_bp;
                        }
                    }
                }
            }
        }
        let mut context = self
            .context
            .lock()
            .map_err(|_| AgentError::Tool("context state poisoned".into()))?;
        context.set_dynamic_budget_scale_bp(pressure_scale_bp);
        context.set_search_limit_scale_bp(search_scale_bp);
        drop(context);
        Ok(Some(plan))
    }
}

fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
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

fn read_slice(
    text: String,
    span: Option<ContextSpan>,
    max_chars: usize,
    remaining_budget: usize,
) -> Result<(String, usize), AgentError> {
    if remaining_budget == 0 {
        return Err(AgentError::Tool("context read budget exhausted".into()));
    }
    let chars: Vec<char> = text.chars().collect();
    let offset = span.map_or(0, |span| span.offset);
    if offset > chars.len() {
        return Err(AgentError::Tool(format!(
            "context span offset {offset} exceeds {len} chars",
            len = chars.len()
        )));
    }
    let end = span
        .map(|span| offset.saturating_add(span.length))
        .unwrap_or(chars.len())
        .min(chars.len());
    let mut text: String = chars[offset..end].iter().collect();
    let cap = max_chars.min(remaining_budget);
    let mut used = text.chars().count();
    if used > cap {
        text = text.chars().take(cap).collect();
        text.push_str("...(truncated)");
        used = cap;
    }
    Ok((text, used))
}
