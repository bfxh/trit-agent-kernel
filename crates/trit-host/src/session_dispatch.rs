//! 会话派发：把 plan 里的 `resume` 项落成新的内核会话文件。
//!
//! 物理门：
//! - 只处理 `Resume` 且 `format == Trit` 的会话；Reasonix/Unknown/Inspect 一律跳过。
//! - 默认 dry-run，只输出计划，不创建任何文件。
//! - `--apply` 才写；目标已存在时拒绝覆盖，改用带序号的新文件名。
//! - 新会话只写一条 `TurnStarted`，`objective` 记录来源会话，避免把未完成的旧 turn 弄脏。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::session::{SessionEvent, SessionLog};
use crate::session_plan::{SessionAction, SessionPlanReport};
use crate::session_scan::{SessionFormat, SessionScanReport};

pub const SESSION_DISPATCH_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchOptions {
    /// 默认 true；只有显式 `--apply` 才写文件。
    pub dry_run: bool,
    pub max_steps: usize,
}

impl Default for DispatchOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            max_steps: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchAction {
    CreateSession,
    /// 目标路径被占用或写前校验失败；不覆盖。
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchItem {
    pub source_session: String,
    pub task: String,
    pub target_path: String,
    pub action: DispatchAction,
    /// 是否真的写了文件；dry-run 时恒为 false。
    pub applied: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReport {
    pub schema: u32,
    pub dry_run: bool,
    pub items: Vec<DispatchItem>,
    #[serde(default)]
    pub skipped: Vec<String>,
}

/// 生成（并在 `--apply` 时执行）派发计划。
pub fn dispatch_sessions(
    scan: &SessionScanReport,
    plan: &SessionPlanReport,
    options: &DispatchOptions,
) -> Result<DispatchReport, AgentError> {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for item in &plan.items {
        if item.action != SessionAction::Resume {
            skipped.push(format!("{}: {:?}", item.session_id, item.action));
            continue;
        }
        let Some(entry) = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == item.session_id)
        else {
            skipped.push(format!("{}: source missing", item.session_id));
            continue;
        };
        if entry.format != SessionFormat::Trit {
            skipped.push(format!(
                "{}: format {:?} is read-only",
                entry.session_id, entry.format
            ));
            continue;
        }
        let Some(task) = item.task.as_deref().filter(|task| !task.is_empty()) else {
            skipped.push(format!("{}: no task", entry.session_id));
            continue;
        };

        let source = Path::new(&entry.path);
        let dir = source.parent().unwrap_or(Path::new("."));
        let target = unique_target(dir, &entry.session_id)?;
        let (action, reason, applied) = if target.exists() {
            (
                DispatchAction::Blocked,
                Some("target exists; refusing to overwrite".to_owned()),
                false,
            )
        } else if options.dry_run {
            (DispatchAction::CreateSession, None, false)
        } else {
            write_resume_session(&target, task, &entry.session_id, options.max_steps)?;
            (DispatchAction::CreateSession, None, true)
        };
        items.push(DispatchItem {
            source_session: entry.session_id.clone(),
            task: task.to_owned(),
            target_path: target.display().to_string(),
            action,
            applied,
            reason,
        });
    }
    Ok(DispatchReport {
        schema: SESSION_DISPATCH_SCHEMA,
        dry_run: options.dry_run,
        items,
        skipped,
    })
}

fn unique_target(dir: &Path, session_id: &str) -> Result<PathBuf, AgentError> {
    let safe: String = session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let base = dir.join(format!("{safe}.resume.jsonl"));
    if !base.exists() {
        return Ok(base);
    }
    for index in 1..1000 {
        let candidate = dir.join(format!("{safe}.resume.{index}.jsonl"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(AgentError::SessionState(format!(
        "no free resume target for {session_id}"
    )))
}

fn write_resume_session(
    target: &Path,
    task: &str,
    source: &str,
    max_steps: usize,
) -> Result<(), AgentError> {
    if target.exists() {
        return Err(AgentError::SessionState(format!(
            "refusing to overwrite {}",
            target.display()
        )));
    }
    let mut log = SessionLog::open(target)?;
    log.append(SessionEvent::TurnStarted {
        task: task.to_owned(),
        objective: format!("resume {source}"),
        max_steps,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_migration::SessionRole;
    use crate::session_plan::{SessionPlanItem, plan_sessions};
    use crate::session_scan::{SessionEntry, SessionFormat, SessionScanReport, SessionTask};
    use std::fs;

    fn temp(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "trit-session-dispatch-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn open_session(root: &Path, name: &str) -> PathBuf {
        let path = root.join(format!("{name}.jsonl"));
        let mut log = SessionLog::open(&path).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "unfinished".into(),
            objective: "original".into(),
            max_steps: 2,
        })
        .unwrap();
        path
    }

    fn report(root: &Path, path: &Path) -> (SessionScanReport, SessionPlanReport) {
        let scan = SessionScanReport {
            schema: 1,
            root: root.display().to_string(),
            scanned: 1,
            sessions: vec![SessionEntry {
                path: path.display().to_string(),
                session_id: "open".into(),
                project: None,
                role: SessionRole::Primary,
                format: SessionFormat::Trit,
                meta: None,
                bytes: 1,
                modified_ms: 0,
                records: 1,
                corrupt_lines: 0,
                tasks: vec![SessionTask {
                    index: 1,
                    task: "unfinished".into(),
                    objective: "original".into(),
                    steps: 0,
                    complete: false,
                    finished: false,
                }],
                open_turn: true,
                in_doubt: 0,
                last_gate: None,
            }],
            links: Vec::new(),
            warnings: Vec::new(),
        };
        (scan.clone(), plan_sessions(&scan))
    }

    #[test]
    fn dry_run_never_writes() {
        let root = temp("dry");
        let path = open_session(&root, "open");
        let (scan, plan) = report(&root, &path);
        let report = dispatch_sessions(&scan, &plan, &DispatchOptions::default()).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.items.len(), 1);
        assert!(!report.items[0].applied);
        assert_eq!(report.items[0].action, DispatchAction::CreateSession);
        assert!(!Path::new(&report.items[0].target_path).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_writes_a_resumable_trit_session() {
        let root = temp("apply");
        let path = open_session(&root, "open");
        let (scan, plan) = report(&root, &path);
        let report = dispatch_sessions(
            &scan,
            &plan,
            &DispatchOptions {
                dry_run: false,
                max_steps: 8,
            },
        )
        .unwrap();
        assert!(report.items[0].applied);
        let target = Path::new(&report.items[0].target_path);
        assert!(target.exists());
        let records = SessionLog::open(target).unwrap().load().unwrap();
        assert!(matches!(
            &records[0].event,
            SessionEvent::TurnStarted { task, .. } if task == "unfinished"
        ));
        let ended = crate::session::recover(&records).unwrap();
        assert_eq!(ended.turns.len(), 1);
        assert!(!ended.turns[0].finished);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_resume_items_are_skipped() {
        let root = temp("skip");
        let scan = SessionScanReport {
            schema: 1,
            root: root.display().to_string(),
            scanned: 1,
            sessions: vec![SessionEntry {
                path: root.join("chat.jsonl").display().to_string(),
                session_id: "chat".into(),
                project: None,
                role: SessionRole::Primary,
                format: SessionFormat::Reasonix,
                meta: None,
                bytes: 1,
                modified_ms: 0,
                records: 1,
                corrupt_lines: 0,
                tasks: Vec::new(),
                open_turn: true,
                in_doubt: 0,
                last_gate: None,
            }],
            links: Vec::new(),
            warnings: Vec::new(),
        };
        let plan = SessionPlanReport {
            schema: 1,
            root: root.display().to_string(),
            items: vec![SessionPlanItem {
                session_id: "chat".into(),
                path: root.join("chat.jsonl").display().to_string(),
                action: SessionAction::Inspect,
                priority: 50,
                reason: "read-only".into(),
                task: Some("t".into()),
                links: Vec::new(),
            }],
            unassigned: Vec::new(),
        };
        let report = dispatch_sessions(&scan, &plan, &DispatchOptions::default()).unwrap();
        assert!(report.items.is_empty());
        assert_eq!(report.skipped.len(), 1);
    }
}
