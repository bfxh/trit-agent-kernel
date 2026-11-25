//! Rust 宿主骨架：把模型提议、工具执行、效果台账与完成裁决串成一个 turn。
//!
//! 这是 Reasonix Rust 化的最小可运行内核：不绑定具体 provider / UI / 异步运行时，
//! 宿主实现 [`Model`] 与 [`ToolExecutor`] 即可。内核仍是纯库，I/O 只发生在工具实现里。

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use trit_orch::{
    AdmissionObligation, Effect, InterventionPolicy, Ledger, Limits, ObligationKind, Observation,
    Plan, PlanPatch, Resources, Snapshot, TaskBrief, Ternary, TickInput, TokenUsage, task_brief,
    tick,
};
use trit_reasonix::{
    AdapterError, ClaimRequest, CompletionClaim, CompletionJudgment, CriterionClaim, CriterionKind,
    HostToolCall, HostUsage, TurnSpec, judge_completion,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("model script exhausted")]
    ModelExhausted,
    #[error("tool script exhausted")]
    ToolExhausted,
    #[error("tool result {actual} does not match request {expected}")]
    ToolMismatch { expected: String, actual: String },
    #[error("model returned neither tool calls nor a completion claim")]
    EmptyProposal,
    #[error("turn exceeded {limit} steps without a completion claim")]
    StepLimit { limit: usize },
    #[error("kernel produced no brief for task {task}")]
    NoBrief { task: String },
    #[error("tool error: {0}")]
    Tool(String),
    #[error("tool {tool} denied: {reason}")]
    ToolDenied { tool: String, reason: String },
    #[error("plan protocol: {message}")]
    PlanProtocol { message: String },
    #[error("extension: {message}")]
    Extension { message: String },
    #[error("memory: {message}")]
    Memory { message: String },
    #[error("io error: {0}")]
    Io(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("session io at {path}: {message}")]
    SessionIo { path: String, message: String },
    #[error("session parse at {path}:{line}: {message}")]
    SessionParse {
        path: String,
        line: usize,
        message: String,
    },
    #[error("session schema {found} (expected {expected}) at {path}")]
    SessionSchema {
        path: String,
        found: u32,
        expected: u32,
    },
    #[error("session state: {0}")]
    SessionState(String),
    #[error("rewind cannot undo shell side effects: {0:?}")]
    Unrewindable(Vec<String>),
    #[error("rewind failed: {original}; rollback failed: {failures:?}")]
    RewindRollback {
        original: String,
        failures: Vec<String>,
    },
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error(transparent)]
    Kernel(#[from] trit_orch::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call_id: String,
    pub tool: String,
    pub read_only: bool,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<crate::context_layers::ContextSpan>,
    /// MCP 等动态工具的原始参数对象；本地工具不使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

impl ToolRequest {
    pub fn verification(call_id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "bash".into(),
            read_only: true,
            command: Some(command.into()),
            ..Self::default()
        }
    }

    pub fn read(call_id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "read_file".into(),
            read_only: true,
            paths: vec![path.into()],
            ..Self::default()
        }
    }

    pub fn shell(call_id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "bash".into(),
            read_only: false,
            command: Some(command.into()),
            ..Self::default()
        }
    }

    pub fn context_read(call_id: impl Into<String>, handle: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "context_read".into(),
            read_only: true,
            paths: vec![handle.into()],
            ..Self::default()
        }
    }

    pub fn context_read_span(
        call_id: impl Into<String>,
        handle: impl Into<String>,
        span: crate::context_layers::ContextSpan,
    ) -> Self {
        let mut request = Self::context_read(call_id, handle);
        request.span = Some(span);
        request
    }

    pub fn context_search(
        call_id: impl Into<String>,
        query: impl Into<String>,
        limit: Option<usize>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "context_search".into(),
            read_only: true,
            command: Some(query.into()),
            content: limit.map(|limit| limit.to_string()),
            ..Self::default()
        }
    }

    pub fn context_add(call_id: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "context_add".into(),
            read_only: false,
            paths: vec![id.into()],
            ..Self::default()
        }
    }

    pub fn write(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        path: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            read_only: false,
            paths: vec![path.into()],
            content: Some(content.into()),
            ..Self::default()
        }
    }

    /// 移动：`paths` 固定为 `[from, to]`，两端都在 claim 校验范围内。
    pub fn move_path(
        call_id: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "move_path".into(),
            read_only: false,
            paths: vec![from.into(), to.into()],
            ..Self::default()
        }
    }

    pub fn host_call(&self, task: &str) -> HostToolCall {
        HostToolCall {
            task: task.to_owned(),
            call_id: self.call_id.clone(),
            tool: self.tool.clone(),
            read_only: self.read_only,
            paths: self.paths.clone(),
            operation: self.operation.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub call_id: String,
    pub tool: String,
    #[serde(default)]
    pub command: Option<String>,
    pub passed: bool,
    #[serde(default)]
    pub assertions: u32,
    #[serde(default)]
    pub written_paths: Vec<String>,
    /// 执行结果不明（超时 / 断流）：既不能算成功，也不能算未发生。
    #[serde(default)]
    pub in_doubt: bool,
    /// 供模型/审计查看的输出摘要；过大时由工具层截断。
    #[serde(default)]
    pub output: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default)]
    pub tool_calls: Vec<ToolRequest>,
    #[serde(default)]
    pub completion: Option<ClaimRequest>,
    #[serde(default)]
    pub usage: HostUsage,
    #[serde(default)]
    pub prefix: Option<PrefixSignal>,
    /// 本轮上下文 inclusion receipt + 实际 cache 观察。
    #[serde(default)]
    pub context_receipt: Option<crate::context_layers::ContextReceipt>,
}

/// Provider 报告的 prompt prefix 变化；用于产生 Rebasis 义务。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixSignal {
    pub changed: bool,
    #[serde(default)]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnContext {
    pub task: String,
    pub step: usize,
    pub usage: TokenUsage,
    pub receipts: Vec<ToolOutcome>,
    /// 被压缩掉的旧回执的确定性摘要。
    pub digest: Option<String>,
    /// 当前 append-only 投影的模型可见视图。
    pub projection: Option<crate::context_projection::ProjectionView>,
}

pub trait Model {
    fn proposal(
        &mut self,
        brief: &TaskBrief,
        context: &TurnContext,
    ) -> Result<Proposal, AgentError>;
}

impl<T: Model + ?Sized> Model for Box<T> {
    fn proposal(
        &mut self,
        brief: &TaskBrief,
        context: &TurnContext,
    ) -> Result<Proposal, AgentError> {
        (**self).proposal(brief, context)
    }
}

pub trait ToolExecutor {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError>;

    /// 新一轮开始；检查点/审计工具可在这里准备状态。
    fn begin_turn(&mut self, _turn: usize, _objective: &str) -> Result<(), AgentError> {
        Ok(())
    }

    /// 一轮结束；检查点工具在这里落盘。
    fn end_turn(&mut self, _complete: bool) -> Result<(), AgentError> {
        Ok(())
    }
}

impl<T: ToolExecutor + ?Sized> ToolExecutor for Box<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        (**self).execute(request)
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        (**self).begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        (**self).end_turn(complete)
    }
}

/// 测试/演示用：按顺序返回预置提议。
#[derive(Debug, Clone, Default)]
pub struct ScriptedModel {
    queue: VecDeque<Proposal>,
}

impl ScriptedModel {
    pub fn new(proposals: Vec<Proposal>) -> Self {
        Self {
            queue: proposals.into(),
        }
    }
}

impl Model for ScriptedModel {
    fn proposal(
        &mut self,
        _brief: &TaskBrief,
        _context: &TurnContext,
    ) -> Result<Proposal, AgentError> {
        self.queue.pop_front().ok_or(AgentError::ModelExhausted)
    }
}

/// 测试/演示用：按顺序返回预置工具结果。
#[derive(Debug, Clone, Default)]
pub struct ScriptedTools {
    queue: VecDeque<ToolOutcome>,
}

impl ScriptedTools {
    pub fn new(outcomes: Vec<ToolOutcome>) -> Self {
        Self {
            queue: outcomes.into(),
        }
    }
}

impl ToolExecutor for ScriptedTools {
    fn execute(&mut self, _request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        self.queue.pop_front().ok_or(AgentError::ToolExhausted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub task: String,
    pub steps: usize,
    pub brief: TaskBrief,
    pub receipts: Vec<ToolOutcome>,
    pub usage: TokenUsage,
    pub judgment: CompletionJudgment,
    pub effects: Ledger,
    pub rebasis: Vec<AdmissionObligation>,
}

pub struct AgentHost<M, T> {
    model: M,
    tools: T,
    max_steps: usize,
    context: ContextPolicy,
    cache_ledger: Arc<Mutex<crate::context_layers::CacheLedger>>,
    last_prefix_fingerprint: Option<String>,
    context_projection: crate::context_projection::ContextProjection,
    receipt_epochs: BTreeMap<String, u64>,
    freshness_recovery: crate::context_projection::FreshnessRecovery,
}

impl<M: Model, T: ToolExecutor> AgentHost<M, T> {
    pub fn new(model: M, tools: T) -> Self {
        Self {
            model,
            tools,
            max_steps: 8,
            context: ContextPolicy::default(),
            cache_ledger: Arc::new(Mutex::new(crate::context_layers::CacheLedger::default())),
            last_prefix_fingerprint: None,
            context_projection: crate::context_projection::ContextProjection::new(),
            receipt_epochs: BTreeMap::new(),
            freshness_recovery: crate::context_projection::FreshnessRecovery::default(),
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    pub fn with_context(mut self, context: ContextPolicy) -> Self {
        self.context = context;
        self
    }

    pub fn with_freshness_recovery(
        mut self,
        freshness_recovery: crate::context_projection::FreshnessRecovery,
    ) -> Self {
        self.freshness_recovery = freshness_recovery;
        self
    }

    pub fn model(&self) -> &M {
        &self.model
    }

    pub fn cache_ledger(&self) -> Arc<Mutex<crate::context_layers::CacheLedger>> {
        Arc::clone(&self.cache_ledger)
    }

    pub fn context_projection(&self) -> &crate::context_projection::ContextProjection {
        &self.context_projection
    }

    /// 手动恢复投影 freshness；下一条 proposal / 下一个 turn 会看到新值。
    pub fn mark_context_freshness(&mut self, freshness: Ternary) {
        self.context_projection.mark_freshness(freshness);
    }

    pub fn with_cache_ledger(
        mut self,
        cache_ledger: Arc<Mutex<crate::context_layers::CacheLedger>>,
    ) -> Self {
        self.cache_ledger = cache_ledger;
        self
    }

    pub fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.tools.begin_turn(turn, objective)
    }

    pub fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.tools.end_turn(complete)
    }

    pub fn run_turn(&mut self, turn: &TurnSpec) -> Result<TurnOutcome, AgentError> {
        self.run_turn_inner(turn, None)
    }

    pub fn run_turn_with_session(
        &mut self,
        turn: &TurnSpec,
        session: &mut SessionLog,
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_inner(turn, Some(session))
    }

    fn run_turn_inner(
        &mut self,
        turn: &TurnSpec,
        mut session: Option<&mut SessionLog>,
    ) -> Result<TurnOutcome, AgentError> {
        let mut spec = turn.task_spec();
        spec.epoch = self.context_projection.epoch();
        let task_epoch = spec.epoch;
        let mut plan = Plan::new(1, vec![spec])?;
        let mut snapshot = Snapshot {
            revision: 1,
            observations: BTreeMap::from([(
                turn.task.clone(),
                Observation {
                    epoch: task_epoch,
                    authorization: Ternary::True,
                    freshness: self.context_projection.freshness(),
                    ..Observation::default()
                },
            )]),
        };
        let limits = Limits {
            workers: 1,
            capacity: Resources::new(1, 1),
            tokens: turn.max_tokens.max(1),
        };
        let policy = InterventionPolicy::default();
        let initial = tick(TickInput {
            plan: &plan,
            snapshot: &snapshot,
            limits: &limits,
            ledger: &Ledger::new(),
            claims: None,
            briefed: &[],
            trace: None,
            since_seq: 0,
            intervention: &policy,
        })?;
        let mut brief = initial
            .briefs
            .into_iter()
            .next()
            .ok_or_else(|| AgentError::NoBrief {
                task: turn.task.clone(),
            })?;

        log_session(
            &mut session,
            SessionEvent::TurnStarted {
                task: turn.task.clone(),
                objective: turn.objective.clone(),
                max_steps: self.max_steps,
            },
        )?;
        self.context_projection.begin_turn();
        self.receipt_epochs.clear();
        let mut ledger = Ledger::new();
        let mut receipts = Vec::new();
        let mut usage = TokenUsage::default();
        let mut rebasis: Vec<AdmissionObligation> = Vec::new();
        let mut context = TurnContext {
            task: turn.task.clone(),
            step: 0,
            usage,
            receipts: Vec::new(),
            digest: None,
            projection: None,
        };

        for step in 0..self.max_steps {
            context.step = step;
            context.usage = usage;
            let compacted = compact_receipts(&receipts, &self.context);
            if compacted.dropped > 0 || compacted.truncated > 0 {
                let boundary = if compacted.dropped > 0 {
                    Some(
                        self.context_projection
                            .compact(crate::context_projection::CompactionInput {
                                step,
                                retained: compacted.receipts.len(),
                                dropped: compacted.dropped,
                                truncated: compacted.truncated,
                                dropped_chars: compacted.dropped_chars,
                                truncated_chars: compacted.truncated_chars,
                                digest: compacted.digest.clone(),
                            })
                            .map_err(|error| {
                                AgentError::SessionState(format!("context projection: {error}"))
                            })?,
                    )
                } else {
                    None
                };
                log_session(
                    &mut session,
                    SessionEvent::ContextCompacted {
                        step,
                        dropped: compacted.dropped,
                        truncated: compacted.truncated,
                        retained: Some(compacted.receipts.len()),
                        dropped_chars: compacted.dropped_chars,
                        truncated_chars: compacted.truncated_chars,
                        epoch_before: boundary.as_ref().map(|boundary| boundary.epoch_before),
                        epoch_after: boundary.as_ref().map(|boundary| boundary.epoch_after),
                        freshness: boundary.as_ref().map(|boundary| boundary.freshness),
                        digest: compacted.digest.clone(),
                    },
                )?;
                if let Some(boundary) = boundary {
                    plan = plan.rebase_epoch()?;
                    snapshot.revision = plan.revision();
                    if let Some(observation) = snapshot.observations.get_mut(&turn.task) {
                        observation.epoch = plan
                            .task(&turn.task)
                            .map(|task| task.epoch)
                            .unwrap_or(observation.epoch);
                        observation.freshness = Ternary::Unknown;
                    }
                    brief = brief_for(&plan, &snapshot, &limits, &turn.task)?;
                    let needs_obligation = !rebasis.iter().any(|existing| {
                        existing.kind == ObligationKind::Rebasis && existing.subject == "context"
                    });
                    if needs_obligation {
                        let detail = format!(
                            "context compaction epoch {} -> {}; generation {}; freshness Unknown",
                            boundary.epoch_before, boundary.epoch_after, boundary.generation
                        );
                        rebasis.push(AdmissionObligation::new(
                            ObligationKind::Rebasis,
                            &turn.task,
                            "context",
                            &detail,
                        ));
                    }
                }
            }
            context.receipts = compacted.receipts;
            context.digest = compacted.digest;
            context.projection = Some(self.context_projection.view());
            let proposal = self.model.proposal(&brief, &context)?;
            log_session(
                &mut session,
                SessionEvent::Proposal {
                    step,
                    tool_calls: proposal.tool_calls.len(),
                    completion: proposal.completion.is_some(),
                    usage: proposal.usage,
                    context: proposal.context_receipt.clone(),
                },
            )?;
            if let Some(cache) = proposal
                .context_receipt
                .as_ref()
                .and_then(|receipt| receipt.cache.as_ref())
            {
                let mut ledger = self
                    .cache_ledger
                    .lock()
                    .map_err(|_| AgentError::SessionState("cache ledger is poisoned".into()))?;
                ledger.observe(cache);
            }
            let mut prefix_changed = proposal
                .prefix
                .as_ref()
                .is_some_and(|prefix| prefix.changed);
            let mut prefix_reasons = proposal
                .prefix
                .as_ref()
                .filter(|prefix| prefix.changed)
                .map(|prefix| prefix.reasons.clone())
                .unwrap_or_default();
            if let Some(inclusion) = proposal
                .context_receipt
                .as_ref()
                .and_then(|receipt| receipt.inclusion.as_ref())
            {
                let current = (!inclusion.prefix_fingerprint.is_empty())
                    .then_some(inclusion.prefix_fingerprint.as_str());
                let local_changed = match (self.last_prefix_fingerprint.as_deref(), current) {
                    (Some(previous), Some(current)) => previous != current,
                    (Some(_), None) => true,
                    _ => false,
                };
                if local_changed {
                    prefix_changed = true;
                    prefix_reasons.push("context_plan".into());
                }
                self.last_prefix_fingerprint = current.map(str::to_owned);
            }
            if prefix_changed {
                if prefix_reasons.is_empty() {
                    prefix_reasons.push("prefix_signal".into());
                }
                prefix_reasons.sort();
                prefix_reasons.dedup();
                log_session(
                    &mut session,
                    SessionEvent::PrefixChanged {
                        step,
                        reasons: prefix_reasons.clone(),
                        rebasis: true,
                    },
                )?;
                let detail = format!("prompt prefix changed: {prefix_reasons:?}");
                let obligation = AdmissionObligation::new(
                    ObligationKind::Rebasis,
                    &turn.task,
                    "prefix",
                    &detail,
                );
                if !rebasis.iter().any(|existing| existing.id == obligation.id) {
                    rebasis.push(obligation);
                }
            }
            merge_usage(&mut usage, proposal.usage.into_token_usage());
            if proposal.tool_calls.is_empty() && proposal.completion.is_none() {
                return Err(AgentError::EmptyProposal);
            }

            for request in &proposal.tool_calls {
                log_session(
                    &mut session,
                    SessionEvent::ToolRequested {
                        step,
                        request: request.clone(),
                    },
                )?;
                if let Some((next_plan, next_snapshot, operation)) =
                    declare_effect(&plan, &snapshot, &turn.task, request)?
                {
                    plan = next_plan;
                    snapshot = next_snapshot;
                    if let Some(observation) = snapshot.observations.get_mut(&turn.task) {
                        observation.epoch = plan
                            .task(&turn.task)
                            .map(|task| task.epoch)
                            .unwrap_or(observation.epoch);
                    }
                    log_session(
                        &mut session,
                        SessionEvent::PlanPatched {
                            revision: plan.revision(),
                            task: turn.task.clone(),
                            operation,
                        },
                    )?;
                    brief = brief_for(&plan, &snapshot, &limits, &turn.task)?;
                }
                let host_call = request.host_call(&turn.task);
                let attempt = match host_call.effect_spec() {
                    Some(effect) => {
                        let attempt = ledger.prepare(&turn.task, 0, &effect.operation, 1)?;
                        ledger.mark_sending(&attempt, 1)?;
                        Some(attempt)
                    }
                    None => None,
                };
                let outcome = self.tools.execute(request)?;
                if outcome.call_id != request.call_id {
                    return Err(AgentError::ToolMismatch {
                        expected: request.call_id.clone(),
                        actual: outcome.call_id,
                    });
                }
                if let Some(attempt) = attempt {
                    if outcome.in_doubt {
                        ledger.mark_unknown(&attempt)?;
                    } else {
                        ledger.record_result(&attempt, outcome.passed)?;
                    }
                }
                log_session(
                    &mut session,
                    SessionEvent::ToolCompleted {
                        step,
                        outcome: outcome.clone(),
                    },
                )?;
                self.context_projection.append(step, &outcome);
                self.receipt_epochs
                    .insert(outcome.call_id.clone(), self.context_projection.epoch());
                receipts.push(outcome);
                if matches!(
                    self.freshness_recovery,
                    crate::context_projection::FreshnessRecovery::OnVerifiedEvidence
                ) && self.context_projection.freshness() == Ternary::Unknown
                    && receipts.last().is_some_and(|outcome| {
                        outcome.passed && !outcome.in_doubt && outcome.assertions > 0
                    })
                {
                    self.context_projection.mark_freshness(Ternary::True);
                    log_session(
                        &mut session,
                        SessionEvent::ContextFreshnessRecovered {
                            step,
                            epoch: self.context_projection.epoch(),
                            freshness: Ternary::True,
                        },
                    )?;
                    if let Some(observation) = snapshot.observations.get_mut(&turn.task) {
                        observation.freshness = Ternary::True;
                    }
                    brief = brief_for(&plan, &snapshot, &limits, &turn.task)?;
                }
            }

            if let Some(claim) = &proposal.completion {
                log_session(
                    &mut session,
                    SessionEvent::CompletionClaimed {
                        step,
                        criteria: claim.criteria.clone(),
                    },
                )?;
                let active_epoch = self.context_projection.epoch();
                let evidence: Vec<ToolOutcome> =
                    if self.context_projection.freshness() != Ternary::True {
                        Vec::new()
                    } else if active_epoch == 0 {
                        receipts.clone()
                    } else {
                        receipts
                            .iter()
                            .filter(|outcome| {
                                self.receipt_epochs.get(&outcome.call_id) == Some(&active_epoch)
                            })
                            .cloned()
                            .collect()
                    };
                let backed = back_claim(claim, &turn.task, &evidence);
                let judgment = judge_completion(&brief, &backed)?;
                let complete = judgment.is_complete();
                log_session(
                    &mut session,
                    SessionEvent::Judgment {
                        verdict: judgment.verdict,
                        ternary: judgment.ternary,
                        violations: judgment.adherence.violations.len(),
                        complete,
                    },
                )?;
                log_session(
                    &mut session,
                    SessionEvent::TurnFinished {
                        steps: step + 1,
                        complete,
                    },
                )?;
                return Ok(TurnOutcome {
                    task: turn.task.clone(),
                    steps: step + 1,
                    brief,
                    receipts,
                    usage,
                    judgment,
                    effects: ledger,
                    rebasis,
                });
            }
        }

        Err(AgentError::StepLimit {
            limit: self.max_steps,
        })
    }
}

fn merge_usage(into: &mut TokenUsage, other: TokenUsage) {
    into.fresh = into.fresh.saturating_add(other.fresh);
    into.cached = into.cached.saturating_add(other.cached);
    into.output = into.output.saturating_add(other.output);
    into.estimated |= other.estimated;
}

/// 宿主用回执给 criterion 背书；模型不能自己声明 backed/passed。
fn back_claim(claim: &ClaimRequest, task: &str, receipts: &[ToolOutcome]) -> CompletionClaim {
    CompletionClaim {
        task: task.to_owned(),
        criteria: claim
            .criteria
            .iter()
            .map(|criterion| {
                let (backed, passed) = match &criterion.kind {
                    CriterionKind::Manual => (false, false),
                    CriterionKind::Verification { command } => receipts
                        .iter()
                        .find(|outcome| {
                            !outcome.in_doubt
                                && outcome.command.as_deref() == Some(command.as_str())
                        })
                        .map_or((false, false), |outcome| (true, outcome.passed)),
                    CriterionKind::Diff { paths } => {
                        let in_doubt = receipts.iter().any(|outcome| {
                            outcome.in_doubt
                                && outcome
                                    .written_paths
                                    .iter()
                                    .any(|path| paths.contains(path))
                        });
                        let backed = !in_doubt
                            && paths.iter().all(|path| {
                                receipts
                                    .iter()
                                    .any(|outcome| outcome.written_paths.contains(path))
                            });
                        (backed, backed)
                    }
                };
                CriterionClaim {
                    id: criterion.id.clone(),
                    kind: criterion.kind.clone(),
                    backed,
                    passed,
                }
            })
            .collect(),
    }
}

fn declare_effect(
    plan: &Plan,
    snapshot: &Snapshot,
    task: &str,
    request: &ToolRequest,
) -> Result<Option<(Plan, Snapshot, String)>, AgentError> {
    if request.read_only {
        return Ok(None);
    }
    let Some(effect) = request.host_call(task).effect_spec() else {
        return Ok(None);
    };
    let Some(current) = plan.task(task) else {
        return Err(AgentError::NoBrief {
            task: task.to_owned(),
        });
    };
    let operation = effect.operation_for(task).to_owned();
    if current
        .effects
        .iter()
        .any(|existing| existing.operation_for(task) == operation)
    {
        return Ok(None);
    }
    let mut spec = current.clone();
    spec.effects.push(effect);
    let patch = PlanPatch {
        base_revision: plan.revision(),
        add: Vec::new(),
        remove: Vec::new(),
        replace: vec![spec],
    };
    let next = plan.patch(&patch, snapshot)?;
    let mut next_snapshot = snapshot.clone();
    next_snapshot.revision = next.revision();
    Ok(Some((next, next_snapshot, operation)))
}

fn brief_for(
    plan: &Plan,
    snapshot: &Snapshot,
    limits: &Limits,
    task: &str,
) -> Result<TaskBrief, AgentError> {
    let selection = plan.select_with_effects(snapshot, limits, &Ledger::new())?;
    Ok(task_brief(plan, &selection, task)?)
}

/// 供宿主检查的便捷方法：效果是否已确认。
pub fn effect_confirmed(ledger: &Ledger, operation: &str) -> bool {
    ledger.state(operation) == Some(Effect::Confirmed)
}

fn log_session(
    session: &mut Option<&mut SessionLog>,
    event: SessionEvent,
) -> Result<(), AgentError> {
    if let Some(log) = session.as_deref_mut() {
        log.append(event)?;
    }
    Ok(())
}

/// 无头运行配置：工作区权限与步数上限。
#[derive(Debug)]
pub struct HeadlessConfig {
    pub workspace: crate::tools::WorkspaceConfig,
    pub max_steps: usize,
    pub session: Option<std::path::PathBuf>,
    pub context: ContextPolicy,
    /// 可选的写入守卫（路径 claim + 跨进程租约）。
    pub write_guard: Option<crate::permissions::WriteGuard>,
}

/// 无头运行：本地工具 + 模型提议 + 内核裁决。
pub fn run_headless<M: Model>(
    turn: TurnSpec,
    model: M,
    config: HeadlessConfig,
) -> Result<TurnOutcome, AgentError> {
    let tools = crate::tools::LocalTools::new(config.workspace)?;
    let tools = crate::permissions::wrap_claim(tools, config.write_guard);
    let mut host = AgentHost::new(model, tools)
        .with_max_steps(config.max_steps)
        .with_context(config.context);
    if let Some(path) = config.session {
        let mut log = SessionLog::open(path)?;
        host.run_turn_with_session(&turn, &mut log)
    } else {
        host.run_turn(&turn)
    }
}

#[cfg(windows)]
pub mod appcontainer;
pub mod checkpoint;
pub mod context;
pub mod context_layers;
pub mod context_projection;
pub mod context_tools;
pub mod extensions;
pub mod fleet_runner;
pub mod frontend_compat;
pub mod git_checkpoint;
pub mod journal;
pub mod lease;
pub mod mcp_tools;
pub mod memory;
pub mod openai;
pub mod permissions;
pub mod planner;
pub mod port_bridge;
pub mod provider;
pub mod provider_health;
pub mod provider_stream;
pub mod reasonix_wire;
pub mod remote_fleet;
pub mod remote_ledger;
pub mod remote_serve;
pub mod remote_subagent;
pub mod sampling;
pub mod separation;
pub mod serve;
pub mod session;
pub mod session_migration;
pub mod state_layer;
pub mod subagent_process;
pub mod supervisor;
pub mod tools;
pub use trit_orch::SelectionPressure;

pub use checkpoint::{
    CHECKPOINT_SCHEMA, Checkpoint, CheckpointRecorder, CheckpointStore, CheckpointingTools,
    RewindReport, RewindScope, rewind_session, rewind_session_with_policy,
};
pub use context::{CompactedContext, ContextPolicy, compact_receipts};
pub use context_layers::{
    AgentContext, AutonomyPolicy, CacheClass, CacheLedger, CacheObservation, CacheReport,
    CacheSample, CacheStat, CacheTier, CacheTiers, CacheWindow, CacheWindowStats,
    CalibrationReport, CompressedContext, ContextCatalog, ContextDegradation, ContextDetail,
    ContextDocument, ContextDropReason, ContextError, ContextItem, ContextKind, ContextPlan,
    ContextReceipt, ContextRef, ContextRequest, ContextResolver, ContextScope, ContextSelection,
    ContextSpan, DroppedContext, ImpressionDelta, InMemoryContext, InclusionReceipt, ModelProfile,
    PriorImpression, ReasoningEffort, ResolvedContext, SearchOrder, Sensitivity, UsageCalibration,
    assemble_context, estimate_tokens,
};
pub use context_projection::{
    CompactionBoundary, CompactionInput, ContextProjection, FreshnessRecovery, ProjectionEntry,
    ProjectionError, ProjectionView, replay as replay_projection,
};
pub use context_tools::{
    AgentContextPlanner, AgentContextTools, CachePressurePolicy, ContextAddRecord,
    ContextReadRecord, ContextReadTools, LowHitPolicy,
};
pub use extensions::{
    CommandRegistry, CommandSpec, HookEvent, HookRegistry, HookSpec, ParsedCommand, SkillIndex,
    SkillSpec,
};
pub use fleet_runner::{
    FleetJob, HeadlessFleetRunner, run_fleet, session_path, turn_spec_from_task,
};
pub use frontend_compat::{
    FeatureDecision, FeatureGate, FeatureGroupDecision, FeatureGroupSpec, FeatureNotice,
    FeatureSpec, FeatureState, FrontendCapabilities, GateReport, GateRequest, LockReason,
};
pub use git_checkpoint::{
    GitCheckpointStore, GitCheckpointingTools, rewind_session_git, rewind_session_git_with_policy,
};
pub use journal::{
    BackupEntry, JOURNAL_PATH, JOURNAL_SCHEMA, JournalEntry, JournalError, JournalPhase,
    JournalRecord, JournalReport, MutationJournal, RollbackOutcome,
};
pub use lease::{LEASE_PATH, LeaseError, OperationLease};
pub use mcp_tools::{McpToolClient, McpToolExecutor, McpToolRoute};
pub use memory::{MemoryEntry, MemoryHit, MemoryStore};
pub use openai::{OpenAiConfig, OpenAiProvider};
pub use permissions::{
    ClaimingTools, PermissionApprover, PermissionDecision, PermissionGate, PermissionMode,
    PermissionPolicy, PermissionRule, WriteClaim, WriteGuard, wrap_claim,
};
pub use planner::{PlanCollectingTools, plan_from_capture, planner_catalog_specs, planner_tools};
pub use port_bridge::{PortBridge, PortBridgeSpec};
pub use provider::{
    ContextPlanner, Provider, ProviderModel, ProviderRequest, ProviderResponse, ScriptedProvider,
    ToolCatalog, ToolSpec, default_tools,
};
pub use provider_health::{
    ConnectionFailure, ConnectionFailureKind, ConnectionHealth, ConnectionHealthPolicy,
    ConnectionState,
};
pub use provider_stream::{
    OpenAiStreamAssembler, ProviderStreamEvent, SseEvent, StreamingProvider, read_sse_events,
};
pub use reasonix_wire::{
    REASONIX_EVENT_KINDS, REASONIX_WIRE_VERSION, WireCacheDiagnostics, WireContractError,
    WireEvent, WireFixture, WireTool, WireUsage, parse_fixture,
};
pub use remote_fleet::{RemoteAttempt, RemotePlanConfig, RemotePlanReport, run_remote_plan};
pub use remote_ledger::{Claim, LedgerServer, LedgerServerConfig};
pub use remote_serve::{RemoteMode, RemoteServer, RemoteServerConfig};
pub use remote_subagent::{
    FRAME_CONTENT_TYPE, IDEMPOTENCY_HEADER, MAX_RESPONSE_BYTES, RemoteReply, run_remote_subagent,
    run_remote_subagent_reply,
};
pub use sampling::OpenAiSamplingHandler;
pub use separation::{
    PLAN_TOOL, READ_ONLY_TOOLS, ReadOnlyCatalog, ReadOnlyTools, SessionPaths, is_read_only_tool,
    plan_handoff_from_proposal, planner_tool_specs,
};
pub use serve::{Route, ServeConfig, ServeError, ServeHeaders, authorize, route};
pub use session::{
    GateChainPhase, GateChainRecord, SESSION_SCHEMA, Session, SessionEvent, SessionLog,
    SessionRecord, SessionState, TurnRecord, recover, recovery_context_for_session,
};
pub use session_migration::{
    CacheDropReason, ContextConflictPolicy, ContextVersion, HookDescriptor, MigrationPlan,
    MigrationPolicy, MigrationReport, MigrationTarget, SESSION_MIGRATION_SCHEMA,
    SessionMigrationBundle, SessionRole, plan_migration,
};
pub use state_layer::{
    AgentState, CompactionState, InProgressState, StateIssue, StateIssueCode, StateIssueSeverity,
    StateLayer, StateValidation, state_layer,
};
pub use subagent_process::{SubagentProcessConfig, run_subagent};
pub use supervisor::{
    GatewaySupervisor, McpCapabilities, McpHandshake, McpLogLevel, McpNotification, McpPrompt,
    McpPromptArgument, McpPromptsCapability, McpResource, McpResourceContent, McpResourceRead,
    McpResourceTemplate, McpResourcesCapability, McpRoot, McpServerInfo, McpStatus, McpSupervisor,
    McpTool, McpToolsCapability, ProcessSpec, ProcessSupervisor, RecoveryContext, RestartPolicy,
    SamplingHandler, SupervisorConfig, SupervisorError, SupervisorLoop, SupervisorRuntime,
    SupervisorStatus,
};
