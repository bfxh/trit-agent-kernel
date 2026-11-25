//! AI 状态层：从 session 事件重建当前状态、进行中的 turn、压缩后的状态并做校验。
//!
//! 这是纯派生视图：不改变会话、不写日志；宿主可直接展示或用于迁移校验。
//! 校验只检查事件序列的一致性，不猜测业务语义。

use serde::{Deserialize, Serialize};

use trit_orch::{Ternary, TokenUsage};

use crate::{SessionEvent, SessionRecord};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Idle,
    /// 已开始 turn，等待模型提议或准备下一步。
    Planning,
    /// 有工具调用在途。
    ToolsRunning,
    /// 刚发生过上下文压缩。
    Compacting,
    /// 模型已提交完成声明，等待宿主裁决。
    Judging,
    /// 最近一个 turn 已完成。
    Finished,
    /// 最近一个 turn 未完成/失败。
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgressState {
    pub turn: usize,
    pub step: usize,
    pub state: AgentState,
    pub pending_tools: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionState {
    pub step: usize,
    pub dropped: usize,
    pub truncated: usize,
    #[serde(default)]
    pub retained: Option<usize>,
    #[serde(default)]
    pub dropped_chars: usize,
    #[serde(default)]
    pub truncated_chars: usize,
    #[serde(default)]
    pub epoch_before: Option<u64>,
    #[serde(default)]
    pub epoch_after: Option<u64>,
    #[serde(default)]
    pub freshness: Option<Ternary>,
    pub digest: Option<String>,
    pub state_before: AgentState,
    /// 压缩完成、回到可继续执行的状态。
    pub state_after: AgentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateIssueSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateIssueCode {
    EventOutsideTurn,
    TurnStartedBeforeFinished,
    FinishedWithoutStart,
    StepRegression,
    CompactionMissingDigest,
    DigestWithoutCompaction,
    UnfinishedTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateIssue {
    pub code: StateIssueCode,
    pub severity: StateIssueSeverity,
    pub seq: u64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateValidation {
    /// True = 无问题；Unknown = 只有警告；False = 有错误。
    pub verdict: Ternary,
    pub issues: Vec<StateIssue>,
}

impl Default for StateValidation {
    fn default() -> Self {
        Self {
            verdict: Ternary::True,
            issues: Vec::new(),
        }
    }
}

impl StateValidation {
    fn push(
        &mut self,
        code: StateIssueCode,
        severity: StateIssueSeverity,
        seq: u64,
        detail: String,
    ) {
        self.issues.push(StateIssue {
            code,
            severity,
            seq,
            detail,
        });
        if severity == StateIssueSeverity::Error {
            self.verdict = Ternary::False;
        } else if self.verdict == Ternary::True {
            self.verdict = Ternary::Unknown;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateLayer {
    pub state: AgentState,
    pub turns: usize,
    pub completed_turns: usize,
    pub in_progress: Option<InProgressState>,
    /// 每次压缩的状态快照，按事件顺序保留。
    pub compactions: Vec<CompactionState>,
    /// 最近一次投影快照播报的 epoch / freshness；旧日志为 None。
    #[serde(default)]
    pub projection_epoch: Option<u64>,
    #[serde(default)]
    pub projection_freshness: Option<Ternary>,
    pub usage: TokenUsage,
    pub validation: StateValidation,
}

impl StateLayer {
    pub fn last_compaction(&self) -> Option<&CompactionState> {
        self.compactions.last()
    }

    /// 被压缩后的 agent 状态；没有压缩时 None。
    pub fn compressed_state(&self) -> Option<AgentState> {
        self.last_compaction()
            .map(|compaction| compaction.state_after)
    }
}

pub fn state_layer(records: &[SessionRecord]) -> StateLayer {
    let mut layer = StateLayer::default();
    let mut current: Option<InProgressState> = None;
    let mut last_step = 0usize;
    let mut validation = StateValidation::default();

    for record in records {
        match &record.event {
            SessionEvent::TurnStarted { .. } => {
                if current.is_some() {
                    validation.push(
                        StateIssueCode::TurnStartedBeforeFinished,
                        StateIssueSeverity::Warning,
                        record.seq,
                        "new turn started before previous turn finished".into(),
                    );
                }
                layer.turns += 1;
                last_step = 0;
                current = Some(InProgressState {
                    turn: layer.turns - 1,
                    step: 0,
                    state: AgentState::Planning,
                    pending_tools: 0,
                });
                layer.state = AgentState::Planning;
            }
            SessionEvent::Proposal {
                step,
                tool_calls,
                completion,
                usage,
                ..
            } => {
                let Some(progress) = current.as_mut() else {
                    validation.push(
                        StateIssueCode::EventOutsideTurn,
                        StateIssueSeverity::Error,
                        record.seq,
                        "proposal outside a turn".into(),
                    );
                    continue;
                };
                check_step(&mut validation, &mut last_step, *step, record.seq);
                progress.step = *step;
                progress.state = if *completion {
                    AgentState::Judging
                } else if *tool_calls > 0 {
                    AgentState::ToolsRunning
                } else {
                    AgentState::Planning
                };
                layer.state = progress.state;
                let usage = usage.into_token_usage();
                add_usage(&mut layer.usage, usage);
            }
            SessionEvent::ToolRequested { step, .. } => {
                let Some(progress) = current.as_mut() else {
                    validation.push(
                        StateIssueCode::EventOutsideTurn,
                        StateIssueSeverity::Error,
                        record.seq,
                        "tool request outside a turn".into(),
                    );
                    continue;
                };
                check_step(&mut validation, &mut last_step, *step, record.seq);
                progress.step = *step;
                progress.pending_tools = progress.pending_tools.saturating_add(1);
                progress.state = AgentState::ToolsRunning;
                layer.state = AgentState::ToolsRunning;
            }
            SessionEvent::ToolCompleted { step, .. } => {
                let Some(progress) = current.as_mut() else {
                    validation.push(
                        StateIssueCode::EventOutsideTurn,
                        StateIssueSeverity::Error,
                        record.seq,
                        "tool completion outside a turn".into(),
                    );
                    continue;
                };
                check_step(&mut validation, &mut last_step, *step, record.seq);
                progress.step = *step;
                progress.pending_tools = progress.pending_tools.saturating_sub(1);
                progress.state = if progress.pending_tools == 0 {
                    AgentState::Planning
                } else {
                    AgentState::ToolsRunning
                };
                layer.state = progress.state;
            }
            SessionEvent::PlanPatched { .. } => {
                if let Some(progress) = current.as_mut() {
                    progress.state = AgentState::Planning;
                    layer.state = AgentState::Planning;
                }
            }
            SessionEvent::ContextCompacted {
                step,
                dropped,
                truncated,
                retained,
                dropped_chars,
                truncated_chars,
                epoch_before,
                epoch_after,
                freshness,
                digest,
            } => {
                let Some(progress) = current.as_mut() else {
                    validation.push(
                        StateIssueCode::EventOutsideTurn,
                        StateIssueSeverity::Error,
                        record.seq,
                        "compaction outside a turn".into(),
                    );
                    continue;
                };
                check_step(&mut validation, &mut last_step, *step, record.seq);
                if *dropped > 0 && digest.is_none() {
                    validation.push(
                        StateIssueCode::CompactionMissingDigest,
                        StateIssueSeverity::Error,
                        record.seq,
                        "compaction dropped receipts without digest".into(),
                    );
                }
                if digest.is_some() && *dropped == 0 && *truncated == 0 {
                    validation.push(
                        StateIssueCode::DigestWithoutCompaction,
                        StateIssueSeverity::Warning,
                        record.seq,
                        "digest present without dropped/truncated receipts".into(),
                    );
                }
                let state_before = progress.state;
                progress.step = *step;
                progress.state = AgentState::Compacting;
                layer.state = AgentState::Compacting;
                layer.compactions.push(CompactionState {
                    step: *step,
                    dropped: *dropped,
                    truncated: *truncated,
                    retained: *retained,
                    dropped_chars: *dropped_chars,
                    truncated_chars: *truncated_chars,
                    epoch_before: *epoch_before,
                    epoch_after: *epoch_after,
                    freshness: *freshness,
                    digest: digest.clone(),
                    state_before,
                    state_after: AgentState::Planning,
                });
                if let Some(epoch) = epoch_after {
                    layer.projection_epoch = Some(*epoch);
                }
                if let Some(freshness) = freshness {
                    layer.projection_freshness = Some(*freshness);
                }
            }
            SessionEvent::CompletionClaimed { step, .. } => {
                if let Some(progress) = current.as_mut() {
                    check_step(&mut validation, &mut last_step, *step, record.seq);
                    progress.step = *step;
                    progress.state = AgentState::Judging;
                    layer.state = AgentState::Judging;
                } else {
                    validation.push(
                        StateIssueCode::EventOutsideTurn,
                        StateIssueSeverity::Error,
                        record.seq,
                        "completion claim outside a turn".into(),
                    );
                }
            }
            SessionEvent::Judgment { .. } => {
                if let Some(progress) = current.as_mut() {
                    progress.state = AgentState::Judging;
                    layer.state = AgentState::Judging;
                }
            }
            SessionEvent::TurnFinished { complete, .. } => {
                if current.is_none() {
                    validation.push(
                        StateIssueCode::FinishedWithoutStart,
                        StateIssueSeverity::Error,
                        record.seq,
                        "turn finished without a start".into(),
                    );
                    continue;
                }
                if *complete {
                    layer.completed_turns += 1;
                    layer.state = AgentState::Finished;
                } else {
                    layer.state = AgentState::Failed;
                }
                current = None;
            }
            SessionEvent::PrefixChanged { .. } => {}
            SessionEvent::ContextFreshnessRecovered {
                epoch, freshness, ..
            } => {
                layer.projection_epoch = Some(*epoch);
                layer.projection_freshness = Some(*freshness);
            }
            // 门链结论不改变 turn 状态机；它只作为审计事件留在日志里。
            SessionEvent::GateChain { .. } => {}
        }
    }

    if let Some(progress) = current.as_ref() {
        validation.push(
            StateIssueCode::UnfinishedTurn,
            StateIssueSeverity::Warning,
            0,
            format!(
                "turn {} still in progress at step {} ({:?})",
                progress.turn, progress.step, progress.state
            ),
        );
    }

    layer.in_progress = current;
    layer.validation = validation;
    layer
}

fn check_step(validation: &mut StateValidation, last_step: &mut usize, step: usize, seq: u64) {
    if step < *last_step {
        validation.push(
            StateIssueCode::StepRegression,
            StateIssueSeverity::Warning,
            seq,
            format!("step {step} regressed from {last_step}"),
        );
    }
    *last_step = step;
}

fn add_usage(into: &mut TokenUsage, usage: TokenUsage) {
    into.fresh = into.fresh.saturating_add(usage.fresh);
    into.cached = into.cached.saturating_add(usage.cached);
    into.output = into.output.saturating_add(usage.output);
    into.estimated |= usage.estimated;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolOutcome, ToolRequest};
    use trit_orch::EvidenceVerdict;
    use trit_reasonix::{CriterionRequest, HostUsage};

    fn record(seq: u64, event: SessionEvent) -> SessionRecord {
        SessionRecord {
            schema: 1,
            seq,
            event,
        }
    }

    fn started(seq: u64) -> SessionRecord {
        record(
            seq,
            SessionEvent::TurnStarted {
                task: "t".into(),
                objective: "o".into(),
                max_steps: 8,
            },
        )
    }

    fn proposal(seq: u64, step: usize, tools: usize, completion: bool) -> SessionRecord {
        record(
            seq,
            SessionEvent::Proposal {
                step,
                tool_calls: tools,
                completion,
                usage: HostUsage::default(),
                context: None,
            },
        )
    }

    fn tool_requested(seq: u64, step: usize) -> SessionRecord {
        record(
            seq,
            SessionEvent::ToolRequested {
                step,
                request: ToolRequest::read("c1", "a.txt"),
            },
        )
    }

    fn tool_completed(seq: u64, step: usize) -> SessionRecord {
        record(
            seq,
            SessionEvent::ToolCompleted {
                step,
                outcome: ToolOutcome {
                    call_id: "c1".into(),
                    tool: "read_file".into(),
                    ..ToolOutcome::default()
                },
            },
        )
    }

    #[test]
    fn empty_log_is_idle_and_valid() {
        let state = state_layer(&[]);
        assert_eq!(state.state, AgentState::Idle);
        assert!(state.in_progress.is_none());
        assert_eq!(state.validation.verdict, Ternary::True);
    }

    #[test]
    fn turn_lifecycle_tracks_pending_tools_and_completion() {
        let records = vec![
            started(1),
            proposal(2, 0, 1, false),
            tool_requested(3, 0),
            tool_completed(4, 0),
            proposal(5, 1, 0, true),
            record(
                6,
                SessionEvent::CompletionClaimed {
                    step: 1,
                    criteria: vec![CriterionRequest {
                        id: "c1".into(),
                        kind: trit_reasonix::CriterionKind::Manual,
                    }],
                },
            ),
            record(
                7,
                SessionEvent::Judgment {
                    verdict: EvidenceVerdict::Proved,
                    ternary: Ternary::True,
                    violations: 0,
                    complete: true,
                },
            ),
            record(
                8,
                SessionEvent::TurnFinished {
                    steps: 2,
                    complete: true,
                },
            ),
        ];
        let state = state_layer(&records);
        assert_eq!(state.state, AgentState::Finished);
        assert_eq!(state.turns, 1);
        assert_eq!(state.completed_turns, 1);
        assert!(state.in_progress.is_none());
        assert_eq!(state.validation.verdict, Ternary::True);
    }

    #[test]
    fn compaction_records_state_after_and_digest() {
        let records = vec![
            started(1),
            record(
                2,
                SessionEvent::ContextCompacted {
                    step: 0,
                    dropped: 2,
                    truncated: 1,
                    dropped_chars: 200,
                    truncated_chars: 50,
                    retained: Some(1),
                    epoch_before: Some(0),
                    epoch_after: Some(1),
                    freshness: Some(Ternary::Unknown),
                    digest: Some("digest".into()),
                },
            ),
        ];
        let state = state_layer(&records);
        assert_eq!(state.state, AgentState::Compacting);
        assert_eq!(state.compressed_state(), Some(AgentState::Planning));
        assert_eq!(
            state.last_compaction().unwrap().state_before,
            AgentState::Planning
        );
        assert_eq!(state.last_compaction().unwrap().dropped, 2);
        assert_eq!(state.last_compaction().unwrap().retained, Some(1));
        assert_eq!(state.last_compaction().unwrap().epoch_before, Some(0));
        assert_eq!(state.last_compaction().unwrap().epoch_after, Some(1));
        assert_eq!(
            state.last_compaction().unwrap().freshness,
            Some(Ternary::Unknown)
        );
        assert_eq!(state.projection_epoch, Some(1));
        assert_eq!(state.projection_freshness, Some(Ternary::Unknown));
        assert_eq!(state.last_compaction().unwrap().dropped_chars, 200);
        assert_eq!(state.last_compaction().unwrap().truncated_chars, 50);
        assert_eq!(
            state.last_compaction().unwrap().digest.as_deref(),
            Some("digest")
        );
        assert_eq!(state.validation.verdict, Ternary::Unknown);
    }

    #[test]
    fn unfinished_turn_is_visible_and_warned() {
        let state = state_layer(&[started(1), proposal(2, 0, 1, false)]);
        assert_eq!(state.in_progress.as_ref().unwrap().turn, 0);
        assert_eq!(state.in_progress.as_ref().unwrap().pending_tools, 0);
        assert_eq!(state.state, AgentState::ToolsRunning);
        assert_eq!(state.validation.verdict, Ternary::Unknown);
        assert!(
            state
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::UnfinishedTurn)
        );
    }

    #[test]
    fn missing_digest_is_an_error() {
        let state = state_layer(&[
            started(1),
            record(
                2,
                SessionEvent::ContextCompacted {
                    step: 0,
                    dropped: 1,
                    truncated: 0,
                    dropped_chars: 100,
                    truncated_chars: 0,
                    retained: None,
                    epoch_before: None,
                    epoch_after: None,
                    freshness: None,
                    digest: None,
                },
            ),
        ]);
        assert_eq!(state.validation.verdict, Ternary::False);
        assert!(
            state
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::CompactionMissingDigest)
        );
    }

    #[test]
    fn out_of_turn_events_are_errors() {
        let state = state_layer(&[proposal(1, 0, 0, false)]);
        assert_eq!(state.validation.verdict, Ternary::False);
        assert!(
            state
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::EventOutsideTurn)
        );
    }

    #[test]
    fn step_regression_is_a_warning() {
        let records = vec![
            started(1),
            proposal(2, 2, 0, false),
            proposal(3, 1, 0, false),
        ];
        let state = state_layer(&records);
        assert_eq!(state.validation.verdict, Ternary::Unknown);
        assert!(
            state
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::StepRegression)
        );
    }

    #[test]
    fn digest_without_drops_is_only_a_warning() {
        let state = state_layer(&[
            started(1),
            record(
                2,
                SessionEvent::ContextCompacted {
                    step: 0,
                    dropped: 0,
                    truncated: 0,
                    dropped_chars: 0,
                    truncated_chars: 0,
                    retained: None,
                    epoch_before: None,
                    epoch_after: None,
                    freshness: None,
                    digest: Some("digest".into()),
                },
            ),
        ]);
        assert_eq!(state.validation.verdict, Ternary::Unknown);
        assert!(
            state
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::DigestWithoutCompaction)
        );
    }

    #[test]
    fn multiple_turns_and_multiple_compactions_are_visible() {
        let records = vec![
            started(1),
            record(
                2,
                SessionEvent::ContextCompacted {
                    step: 0,
                    dropped: 1,
                    truncated: 0,
                    dropped_chars: 100,
                    truncated_chars: 0,
                    retained: None,
                    epoch_before: None,
                    epoch_after: None,
                    freshness: None,
                    digest: Some("d1".into()),
                },
            ),
            record(
                3,
                SessionEvent::TurnFinished {
                    steps: 0,
                    complete: true,
                },
            ),
            started(4),
            record(
                5,
                SessionEvent::ContextCompacted {
                    step: 1,
                    dropped: 2,
                    truncated: 0,
                    dropped_chars: 200,
                    truncated_chars: 0,
                    retained: None,
                    epoch_before: None,
                    epoch_after: None,
                    freshness: None,
                    digest: Some("d2".into()),
                },
            ),
        ];
        let state = state_layer(&records);
        assert_eq!(state.turns, 2);
        assert_eq!(state.completed_turns, 1);
        assert_eq!(state.compactions.len(), 2);
        assert_eq!(
            state.last_compaction().unwrap().digest.as_deref(),
            Some("d2")
        );
        assert_eq!(state.in_progress.as_ref().unwrap().turn, 1);
    }

    #[test]
    fn usage_is_aggregated_and_estimated_flag_is_sticky() {
        let records = vec![
            started(1),
            record(
                2,
                SessionEvent::Proposal {
                    step: 0,
                    tool_calls: 0,
                    completion: false,
                    usage: HostUsage {
                        prompt_tokens: 10,
                        completion_tokens: 2,
                        cache_hit_tokens: 8,
                        cache_miss_tokens: 2,
                        estimated: false,
                    },
                    context: None,
                },
            ),
            record(
                3,
                SessionEvent::Proposal {
                    step: 1,
                    tool_calls: 0,
                    completion: false,
                    usage: HostUsage {
                        prompt_tokens: 5,
                        completion_tokens: 1,
                        cache_hit_tokens: 0,
                        cache_miss_tokens: 5,
                        estimated: true,
                    },
                    context: None,
                },
            ),
        ];
        let state = state_layer(&records);
        assert_eq!(state.usage.fresh, 7);
        assert_eq!(state.usage.cached, 8);
        assert_eq!(state.usage.output, 3);
        assert!(state.usage.estimated);
    }

    #[test]
    fn nested_turn_start_is_a_warning_and_finish_without_start_is_an_error() {
        let nested = state_layer(&[started(1), started(2)]);
        assert!(
            nested
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::TurnStartedBeforeFinished)
        );

        let orphan = state_layer(&[record(
            1,
            SessionEvent::TurnFinished {
                steps: 0,
                complete: true,
            },
        )]);
        assert_eq!(orphan.validation.verdict, Ternary::False);
        assert!(
            orphan
                .validation
                .issues
                .iter()
                .any(|issue| issue.code == StateIssueCode::FinishedWithoutStart)
        );
    }

    #[test]
    fn compressed_state_is_absent_before_any_compaction() {
        let state = state_layer(&[started(1)]);
        assert!(state.compressed_state().is_none());
        assert!(state.last_compaction().is_none());
    }

    #[test]
    fn failed_turn_is_failed_and_not_completed() {
        let records = vec![
            started(1),
            record(
                2,
                SessionEvent::TurnFinished {
                    steps: 0,
                    complete: false,
                },
            ),
        ];
        let state = state_layer(&records);
        assert_eq!(state.state, AgentState::Failed);
        assert_eq!(state.completed_turns, 0);
        assert!(state.in_progress.is_none());
    }
}
