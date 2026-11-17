//! 无头会话的 JSONL 事件日志：追加、加载、续写。
//!
//! 一行一个 `SessionRecord`；schema 固定，seq 单调。日志可能包含工具输出，
//! 宿主应把它当作敏感数据管理（脱敏/保留策略不在本层）。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{AgentError, AgentHost, Model, ToolExecutor, ToolOutcome, ToolRequest, TurnOutcome};
use trit_orch::{EvidenceVerdict, Ternary, TokenUsage};
use trit_reasonix::{CriterionRequest, HostUsage, TurnSpec};

pub const SESSION_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema: u32,
    pub seq: u64,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TurnStarted {
        task: String,
        objective: String,
        max_steps: usize,
    },
    Proposal {
        step: usize,
        tool_calls: usize,
        completion: bool,
        usage: HostUsage,
        #[serde(default)]
        context: Option<crate::context_layers::ContextReceipt>,
    },
    ToolRequested {
        step: usize,
        request: ToolRequest,
    },
    PlanPatched {
        revision: u64,
        task: String,
        operation: String,
    },
    ToolCompleted {
        step: usize,
        outcome: ToolOutcome,
    },
    PrefixChanged {
        step: usize,
        reasons: Vec<String>,
        rebasis: bool,
    },
    ContextCompacted {
        step: usize,
        dropped: usize,
        truncated: usize,
        #[serde(default)]
        retained: Option<usize>,
        #[serde(default)]
        dropped_chars: usize,
        #[serde(default)]
        truncated_chars: usize,
        #[serde(default)]
        epoch_before: Option<u64>,
        #[serde(default)]
        epoch_after: Option<u64>,
        #[serde(default)]
        freshness: Option<Ternary>,
        digest: Option<String>,
    },
    CompletionClaimed {
        step: usize,
        criteria: Vec<CriterionRequest>,
    },
    Judgment {
        verdict: EvidenceVerdict,
        ternary: Ternary,
        violations: usize,
        complete: bool,
    },
    TurnFinished {
        steps: usize,
        complete: bool,
    },
}

#[derive(Debug, Clone)]
pub struct SessionLog {
    path: PathBuf,
    seq: u64,
}

impl SessionLog {
    /// 打开或创建日志；已有日志会校验 schema 并续上 seq。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let path = path.into();
        let records = read_records(&path)?;
        let seq = records.last().map_or(0, |record| record.seq);
        Ok(Self { path, seq })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, event: SessionEvent) -> Result<u64, AgentError> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| self.io(error))?;
        }
        self.seq = self.seq.saturating_add(1);
        let record = SessionRecord {
            schema: SESSION_SCHEMA,
            seq: self.seq,
            event,
        };
        let line = serde_json::to_string(&record).map_err(|error| AgentError::SessionIo {
            path: self.path.display().to_string(),
            message: error.to_string(),
        })?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| self.io(error))?;
        writeln!(file, "{line}").map_err(|error| self.io(error))?;
        Ok(self.seq)
    }

    pub fn load(&self) -> Result<Vec<SessionRecord>, AgentError> {
        read_records(&self.path)
    }

    /// 截断到 `turn` 之前；保留前面完整的 turn，返回删除的事件数。
    pub fn truncate_before_turn(&mut self, turn: usize) -> Result<usize, AgentError> {
        let records = self.load()?;
        let mut seen = 0;
        let mut cut = None;
        for (index, record) in records.iter().enumerate() {
            if matches!(record.event, SessionEvent::TurnStarted { .. }) {
                if seen == turn {
                    cut = Some(index);
                    break;
                }
                seen += 1;
            }
        }
        let cut = cut
            .ok_or_else(|| AgentError::SessionState(format!("rewind turn {turn} out of range")))?;
        let kept = &records[..cut];
        let mut text = String::new();
        for record in kept {
            let line = serde_json::to_string(record)
                .map_err(|error| AgentError::SessionState(format!("session encode: {error}")))?;
            text.push_str(&line);
            text.push('\n');
        }
        let temporary =
            self.path
                .with_extension(format!("tmp.{}.{}", std::process::id(), self.seq));
        std::fs::write(&temporary, text).map_err(|error| self.io(error))?;
        match std::fs::rename(&temporary, &self.path) {
            Ok(()) => {}
            Err(_) => {
                let _ = std::fs::remove_file(&self.path);
                std::fs::rename(&temporary, &self.path).map_err(|error| self.io(error))?;
            }
        }
        self.seq = kept.last().map_or(0, |record| record.seq);
        Ok(records.len() - kept.len())
    }

    fn io(&self, error: std::io::Error) -> AgentError {
        AgentError::SessionIo {
            path: self.path.display().to_string(),
            message: error.to_string(),
        }
    }
}

/// 从 session log 重建 supervisor 恢复包（状态层 + 日志中的 cache 观测）。
pub fn recovery_context_for_session(
    path: &Path,
) -> Result<crate::supervisor::RecoveryContext, AgentError> {
    let records = read_records(path)?;
    Ok(crate::supervisor::RecoveryContext {
        session_id: Some(path.display().to_string()),
        state: Some(crate::state_layer::state_layer(&records)),
        hooks: Vec::new(),
        cache: Some(cache_ledger_from_records(&records)),
    })
}

fn read_records(path: &Path) -> Result<Vec<SessionRecord>, AgentError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AgentError::SessionIo {
                path: path.display().to_string(),
                message: error.to_string(),
            });
        }
    };
    let mut records: Vec<SessionRecord> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: SessionRecord =
            serde_json::from_str(line).map_err(|error| AgentError::SessionParse {
                path: path.display().to_string(),
                line: index + 1,
                message: error.to_string(),
            })?;
        if record.schema != SESSION_SCHEMA {
            return Err(AgentError::SessionSchema {
                path: path.display().to_string(),
                found: record.schema,
                expected: SESSION_SCHEMA,
            });
        }
        if let Some(previous) = records.last()
            && record.seq <= previous.seq
        {
            return Err(AgentError::SessionParse {
                path: path.display().to_string(),
                line: index + 1,
                message: format!("seq {} is not greater than {}", record.seq, previous.seq),
            });
        }
        records.push(record);
    }
    Ok(records)
}

/// 从已有事件恢复出的多轮会话状态。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub turns: Vec<TurnRecord>,
    pub usage: TokenUsage,
}

impl SessionState {
    pub fn next_turn(&self) -> usize {
        self.turns.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnRecord {
    pub task: String,
    pub objective: String,
    pub steps: usize,
    pub verdict: Option<EvidenceVerdict>,
    pub ternary: Option<Ternary>,
    pub complete: bool,
    pub finished: bool,
    pub usage: TokenUsage,
    pub tool_calls: usize,
    pub criteria: usize,
}

/// 从 JSONL 事件重建会话；未收到 `TurnFinished` 的 turn 标记为未完成。
pub fn recover(records: &[SessionRecord]) -> Result<SessionState, AgentError> {
    let mut state = SessionState::default();
    let mut current: Option<TurnRecord> = None;
    for record in records {
        match &record.event {
            SessionEvent::TurnStarted {
                task, objective, ..
            } => {
                if let Some(previous) = current.take() {
                    state.turns.push(previous);
                }
                current = Some(TurnRecord {
                    task: task.clone(),
                    objective: objective.clone(),
                    ..TurnRecord::default()
                });
            }
            SessionEvent::Proposal {
                tool_calls, usage, ..
            } => {
                let Some(turn) = current.as_mut() else {
                    return Err(AgentError::SessionState("proposal outside a turn".into()));
                };
                turn.tool_calls += tool_calls;
                let usage = usage.into_token_usage();
                add_usage(&mut turn.usage, usage);
                add_usage(&mut state.usage, usage);
            }
            SessionEvent::CompletionClaimed { criteria, .. } => {
                if let Some(turn) = current.as_mut() {
                    turn.criteria = criteria.len();
                }
            }
            SessionEvent::Judgment {
                verdict,
                ternary,
                complete,
                ..
            } => {
                if let Some(turn) = current.as_mut() {
                    turn.verdict = Some(*verdict);
                    turn.ternary = Some(*ternary);
                    turn.complete = *complete;
                }
            }
            SessionEvent::TurnFinished { steps, complete } => {
                let Some(mut turn) = current.take() else {
                    return Err(AgentError::SessionState(
                        "turn finished without a start".into(),
                    ));
                };
                turn.steps = *steps;
                turn.complete = *complete;
                turn.finished = true;
                state.turns.push(turn);
            }
            _ => {}
        }
    }
    if let Some(turn) = current {
        state.turns.push(turn);
    }
    Ok(state)
}

/// 文件支撑的多轮会话：自动恢复历史，并在同一 log 上续写。
pub struct Session<M, T> {
    host: AgentHost<M, T>,
    log: SessionLog,
    state: SessionState,
}

impl<M: Model, T: ToolExecutor> Session<M, T> {
    pub fn open(model: M, tools: T, path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let log = SessionLog::open(path)?;
        let records = log.load()?;
        let state = recover(&records)?;
        let cache_ledger =
            std::sync::Arc::new(std::sync::Mutex::new(cache_ledger_from_records(&records)));
        Ok(Self {
            host: AgentHost::new(model, tools).with_cache_ledger(cache_ledger),
            log,
            state,
        })
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.host = self.host.with_max_steps(max_steps);
        self
    }

    pub fn with_context(mut self, context: crate::ContextPolicy) -> Self {
        self.host = self.host.with_context(context);
        self
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn log(&self) -> &SessionLog {
        &self.log
    }

    pub fn state_layer(&self) -> Result<crate::state_layer::StateLayer, AgentError> {
        Ok(crate::state_layer::state_layer(&self.log.load()?))
    }

    pub fn validate_state(&self) -> Result<crate::state_layer::StateValidation, AgentError> {
        Ok(self.state_layer()?.validation)
    }

    /// supervisor 重连时可发送的恢复包；hooks 由宿主从迁移 bundle 补充。
    pub fn recovery_context(&self) -> Result<crate::supervisor::RecoveryContext, AgentError> {
        let cache = Some(
            self.cache_ledger()
                .lock()
                .map_err(|_| AgentError::SessionState("cache ledger is poisoned".into()))?
                .clone(),
        );
        Ok(crate::supervisor::RecoveryContext {
            session_id: Some(self.log.path().display().to_string()),
            state: Some(self.state_layer()?),
            hooks: Vec::new(),
            cache,
        })
    }

    pub fn cache_ledger(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::context_layers::CacheLedger>> {
        self.host.cache_ledger()
    }

    pub fn with_cache_ledger(
        mut self,
        cache_ledger: std::sync::Arc<std::sync::Mutex<crate::context_layers::CacheLedger>>,
    ) -> Self {
        let replay = self.host.cache_ledger();
        if let (Ok(replay), Ok(mut target)) = (replay.lock(), cache_ledger.lock()) {
            target.merge(&replay);
        }
        self.host = self.host.with_cache_ledger(cache_ledger);
        self
    }

    pub fn run_turn(&mut self, turn: &TurnSpec) -> Result<TurnOutcome, AgentError> {
        let index = self.state.next_turn();
        self.host.begin_turn(index, &turn.objective)?;
        let result = self.host.run_turn_with_session(turn, &mut self.log);
        let complete = result
            .as_ref()
            .map(|outcome| outcome.judgment.is_complete())
            .unwrap_or(false);
        let end = self.host.end_turn(complete);
        self.state = recover(&self.log.load()?)?;
        let outcome = result?;
        end?;
        Ok(outcome)
    }

    /// 删除 `turn` 及之后的会话事件；`turn` 是 0-based 的已存在 turn。
    pub fn truncate_before_turn(&mut self, turn: usize) -> Result<usize, AgentError> {
        self.log.truncate_before_turn(turn)?;
        let records = self.log.load()?;
        self.state = recover(&records)?;
        let rebuilt = cache_ledger_from_records(&records);
        if let Ok(mut ledger) = self.host.cache_ledger().lock() {
            *ledger = rebuilt;
        }
        Ok(self.state.turns.len())
    }
}

fn cache_ledger_from_records(records: &[SessionRecord]) -> crate::context_layers::CacheLedger {
    let mut ledger = crate::context_layers::CacheLedger::default();
    for record in records {
        if let SessionEvent::Proposal {
            context: Some(receipt),
            ..
        } = &record.event
            && let Some(cache) = &receipt.cache
        {
            ledger.observe(cache);
        }
    }
    ledger
}

fn add_usage(into: &mut TokenUsage, usage: TokenUsage) {
    into.fresh = into.fresh.saturating_add(usage.fresh);
    into.cached = into.cached.saturating_add(usage.cached);
    into.output = into.output.saturating_add(usage.output);
    into.estimated |= usage.estimated;
}
