//! 回填对账：重新扫描 child 当前状态，与 parent 目录下 `delegations.jsonl` 的记录比对。
//!
//! 只读、不写文件。用于防止“child 假完成”：记录写的是 completed，但 child 现在
//! 已变成 open/incomplete，或日志在回填后继续增长。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::session_callback::{CallbackStatus, DELEGATIONS_FILE, DelegationResult, classify};
use crate::session_scan::{LinkKind, SessionScanReport};

pub const SESSION_VERIFY_SCHEMA: u32 = 1;
/// 与 `session_callback` 保持一致的回填侧车读取上限。
const MAX_DELEGATIONS_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVerdict {
    /// 记录与当前 child 完全一致。
    Ok,
    /// 状态一致，但 child 的日志在回填后增长/变化，需要重新回填。
    Stale,
    /// 记录状态与当前 child 状态不一致。
    Mismatch,
    /// 记录存在，但 child 已不在扫描结果里。
    MissingChild,
    /// 没有找到 parent 目录下的回填记录。
    MissingDelegation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationCheck {
    pub parent: String,
    pub child: String,
    #[serde(default)]
    pub recorded_status: Option<CallbackStatus>,
    #[serde(default)]
    pub current_status: Option<CallbackStatus>,
    #[serde(default)]
    pub recorded_records: Option<usize>,
    #[serde(default)]
    pub current_records: Option<usize>,
    pub verdict: CheckVerdict,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackVerifyReport {
    pub schema: u32,
    pub checks: Vec<DelegationCheck>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 对账所有 parent_child 链接；缺失 parent/child 只记 warning，不中断。
pub fn verify_callbacks(scan: &SessionScanReport) -> Result<CallbackVerifyReport, AgentError> {
    let mut checks = Vec::new();
    let mut warnings = Vec::new();
    let mut checked = BTreeSet::new();
    for link in &scan.links {
        if link.kind != LinkKind::ParentChild {
            continue;
        }
        let Some(parent) = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == link.from)
        else {
            warnings.push(format!("{}: parent missing", link.from));
            continue;
        };
        let target = Path::new(&parent.path)
            .parent()
            .unwrap_or(Path::new("."))
            .join(DELEGATIONS_FILE);
        if checked.insert(target.clone())
            && let Some(warning) = verify_chain(&target)
        {
            warnings.push(warning);
        }
        let recorded = last_record(&target, &link.from, &link.to)?;
        let child = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == link.to);

        let recorded_status = recorded.as_ref().map(|record| record.status);
        let recorded_records = recorded.as_ref().map(|record| record.child_records);
        let current_status = child.map(classify);
        let current_records = child.map(|entry| entry.records);

        let (verdict, reason) = match (&recorded, child) {
            (None, _) => (
                CheckVerdict::MissingDelegation,
                "no delegation record for this parent/child".to_owned(),
            ),
            (Some(_), None) => (
                CheckVerdict::MissingChild,
                "child session is no longer present".to_owned(),
            ),
            (Some(record), Some(child)) => {
                let current = classify(child);
                if current != record.status {
                    (
                        CheckVerdict::Mismatch,
                        format!("recorded {:?}, current {:?}", record.status, current),
                    )
                } else if child.records != record.child_records
                    || child.modified_ms != record.child_modified_ms
                {
                    (
                        CheckVerdict::Stale,
                        format!(
                            "child grew from {} to {} records",
                            record.child_records, child.records
                        ),
                    )
                } else {
                    (CheckVerdict::Ok, "record matches current child".to_owned())
                }
            }
        };
        checks.push(DelegationCheck {
            parent: link.from.clone(),
            child: link.to.clone(),
            recorded_status,
            current_status,
            recorded_records,
            current_records,
            verdict,
            reason,
        });
    }
    Ok(CallbackVerifyReport {
        schema: SESSION_VERIFY_SCHEMA,
        checks,
        warnings,
    })
}

fn verify_chain(path: &Path) -> Option<String> {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_DELEGATIONS_BYTES) {
        return Some(format!("delegations file too large: {}", path.display()));
    }
    let Ok(text) = fs::read_to_string(path) else {
        return None;
    };
    let mut prev: Option<String> = None;
    for (index, line) in text.lines().enumerate() {
        let Ok(record) = serde_json::from_str::<DelegationResult>(line) else {
            continue;
        };
        if record.record_hash.is_empty() {
            continue;
        }
        if record.prev_hash != prev {
            return Some(format!(
                "delegations chain broken at line {} ({})",
                index + 1,
                path.display()
            ));
        }
        prev = Some(record.record_hash);
    }
    None
}

fn last_record(
    path: &Path,
    parent: &str,
    child: &str,
) -> Result<Option<DelegationResult>, AgentError> {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_DELEGATIONS_BYTES) {
        return Err(AgentError::Io(format!(
            "delegations file too large: {}",
            path.display()
        )));
    }
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut found = None;
    for line in text.lines() {
        if let Ok(record) = serde_json::from_str::<DelegationResult>(line)
            && record.parent == parent
            && record.child == child
        {
            found = Some(record);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_callback::DelegationResult;
    use crate::session_migration::SessionRole;
    use crate::session_scan::{
        SessionEntry, SessionFormat, SessionLink, SessionScanReport, SessionTask,
    };
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "trit-session-verify-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn entry(root: &Path, id: &str, records: usize, finished: bool) -> SessionEntry {
        SessionEntry {
            path: root.join(format!("{id}.jsonl")).display().to_string(),
            session_id: id.into(),
            project: None,
            role: SessionRole::Primary,
            format: SessionFormat::Trit,
            meta: None,
            bytes: 10,
            modified_ms: 7,
            records,
            corrupt_lines: 0,
            tasks: vec![SessionTask {
                index: 1,
                task: "work".into(),
                objective: String::new(),
                steps: 1,
                complete: finished,
                finished,
            }],
            open_turn: !finished,
            in_doubt: 0,
            last_gate: None,
        }
    }

    fn scan(root: &Path, child: SessionEntry) -> SessionScanReport {
        SessionScanReport {
            schema: 1,
            root: root.display().to_string(),
            roots: vec![root.display().to_string()],
            scanned: 2,
            sessions: vec![entry(root, "parent", 2, true), child],
            links: vec![SessionLink {
                kind: LinkKind::ParentChild,
                from: "parent".into(),
                to: "child".into(),
            }],
            warnings: Vec::new(),
        }
    }

    fn write_record(root: &Path, status: CallbackStatus, records: usize) {
        let record = DelegationResult {
            parent: "parent".into(),
            child: "child".into(),
            task: Some("work".into()),
            status,
            steps: 1,
            child_records: records,
            child_modified_ms: 7,
            prev_hash: None,
            record_hash: String::new(),
        };
        let mut line = serde_json::to_string(&record).unwrap();
        line.push('\n');
        fs::write(root.join(DELEGATIONS_FILE), line).unwrap();
    }

    #[test]
    fn matching_record_is_ok() {
        let root = temp("ok");
        write_record(&root, CallbackStatus::Completed, 3);
        let report = verify_callbacks(&scan(&root, entry(&root, "child", 3, true))).unwrap();
        assert_eq!(report.checks[0].verdict, CheckVerdict::Ok);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn completed_record_but_open_child_is_mismatch() {
        let root = temp("mismatch");
        write_record(&root, CallbackStatus::Completed, 3);
        let report = verify_callbacks(&scan(&root, entry(&root, "child", 3, false))).unwrap();
        assert_eq!(report.checks[0].verdict, CheckVerdict::Mismatch);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn child_growth_makes_the_record_stale() {
        let root = temp("stale");
        write_record(&root, CallbackStatus::Completed, 3);
        let report = verify_callbacks(&scan(&root, entry(&root, "child", 4, true))).unwrap();
        assert_eq!(report.checks[0].verdict, CheckVerdict::Stale);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_delegation_is_reported() {
        let root = temp("missing");
        let report = verify_callbacks(&scan(&root, entry(&root, "child", 3, true))).unwrap();
        assert_eq!(report.checks[0].verdict, CheckVerdict::MissingDelegation);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_hash_chain_is_reported() {
        let root = temp("chain");
        let record = DelegationResult {
            parent: "parent".into(),
            child: "child".into(),
            task: Some("work".into()),
            status: CallbackStatus::Completed,
            steps: 1,
            child_records: 3,
            child_modified_ms: 7,
            prev_hash: Some("fnv1a64:deadbeef".into()),
            record_hash: "fnv1a64:cafebabe".into(),
        };
        let mut line = serde_json::to_string(&record).unwrap();
        line.push('\n');
        fs::write(root.join(DELEGATIONS_FILE), line).unwrap();

        let report = verify_callbacks(&scan(&root, entry(&root, "child", 3, true))).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("chain broken")),
            "{:?}",
            report.warnings
        );
        let _ = fs::remove_dir_all(&root);
    }
}
