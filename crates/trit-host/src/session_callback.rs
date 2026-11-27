//! 子会话结果回填：把 child 的完成状态写成 parent 目录下的 `delegations.jsonl` 侧车。
//!
//! 物理门：
//! - 只处理 parent/child 都存在、且两者都是 Trit 格式的链接。
//! - 默认 dry-run；`--apply` 才追加一行 JSON。
//! - 不改 parent/child 的原始会话日志；侧车文件可单独审计/删除。
//! - 幂等：同一 child 的 `records + modified_ms` 已记录时跳过。

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::session_scan::{LinkKind, SessionEntry, SessionFormat, SessionScanReport};

pub const SESSION_CALLBACK_SCHEMA: u32 = 1;
pub const DELEGATIONS_FILE: &str = "delegations.jsonl";
/// 回填侧车读取上限；超过时 fail closed，避免恶意放大文件拖垮扫描。
const MAX_DELEGATIONS_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackOptions {
    /// 默认 true；只有显式 `--apply` 才追加结果。
    pub dry_run: bool,
}

impl Default for CallbackOptions {
    fn default() -> Self {
        Self { dry_run: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackStatus {
    Completed,
    Failed,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationResult {
    pub parent: String,
    pub child: String,
    #[serde(default)]
    pub task: Option<String>,
    pub status: CallbackStatus,
    pub steps: usize,
    /// 幂等键的一部分：child 的日志记录数与修改时间。
    pub child_records: usize,
    pub child_modified_ms: u64,
    /// 前一条记录的 `record_hash`；第一条为 None。
    #[serde(default)]
    pub prev_hash: Option<String>,
    /// 本条逻辑内容 + `prev_hash` 的 FNV-1a 指纹。
    #[serde(default)]
    pub record_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackItem {
    pub parent: String,
    pub child: String,
    #[serde(default)]
    pub task: Option<String>,
    pub status: CallbackStatus,
    pub target_path: String,
    pub written: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackReport {
    pub schema: u32,
    pub dry_run: bool,
    pub items: Vec<CallbackItem>,
    #[serde(default)]
    pub skipped: Vec<String>,
}

/// 扫描 parent/child 链接并生成（可选执行）结果回填。
pub fn callback_sessions(
    scan: &SessionScanReport,
    options: &CallbackOptions,
) -> Result<CallbackReport, AgentError> {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = BTreeSet::new();
    for link in &scan.links {
        if link.kind != LinkKind::ParentChild {
            continue;
        }
        let Some(parent) = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == link.from)
        else {
            skipped.push(format!("{} -> {}: parent missing", link.from, link.to));
            continue;
        };
        let Some(child) = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == link.to)
        else {
            skipped.push(format!("{} -> {}: child missing", link.from, link.to));
            continue;
        };
        if parent.format != SessionFormat::Trit || child.format != SessionFormat::Trit {
            skipped.push(format!("{} -> {}: non-trit format", link.from, link.to));
            continue;
        }
        let Some(task) = child.tasks.last().map(|task| task.task.clone()) else {
            skipped.push(format!("{} -> {}: child has no tasks", link.from, link.to));
            continue;
        };
        let key = format!(
            "{}|{}|{}|{}",
            parent.session_id, child.session_id, child.records, child.modified_ms
        );
        if !seen.insert(key) {
            continue;
        }
        let target = Path::new(&parent.path)
            .parent()
            .unwrap_or(Path::new("."))
            .join(DELEGATIONS_FILE);
        let result = DelegationResult {
            parent: parent.session_id.clone(),
            child: child.session_id.clone(),
            task: Some(task.clone()),
            status: classify(child),
            steps: child.tasks.last().map_or(0, |task| task.steps),
            child_records: child.records,
            child_modified_ms: child.modified_ms,
            prev_hash: None,
            record_hash: String::new(),
        };
        let written = if already_written(&target, &result)? {
            skipped.push(format!("{} -> {}: already recorded", link.from, link.to));
            false
        } else if options.dry_run {
            false
        } else {
            append_result(&target, &result)?;
            true
        };
        items.push(CallbackItem {
            parent: parent.session_id.clone(),
            child: child.session_id.clone(),
            task: Some(task),
            status: result.status,
            target_path: target.display().to_string(),
            written,
        });
    }
    Ok(CallbackReport {
        schema: SESSION_CALLBACK_SCHEMA,
        dry_run: options.dry_run,
        items,
        skipped,
    })
}

pub(crate) fn classify(child: &SessionEntry) -> CallbackStatus {
    if child.in_doubt > 0 || child.open_turn {
        return CallbackStatus::Incomplete;
    }
    if let Some(gate) = &child.last_gate
        && !gate.passed
    {
        return CallbackStatus::Failed;
    }
    if !child.tasks.is_empty()
        && child
            .tasks
            .iter()
            .all(|task| task.finished && task.complete)
    {
        CallbackStatus::Completed
    } else {
        CallbackStatus::Incomplete
    }
}

fn already_written(target: &Path, result: &DelegationResult) -> Result<bool, AgentError> {
    if fs::metadata(target).is_ok_and(|metadata| metadata.len() > MAX_DELEGATIONS_BYTES) {
        return Err(AgentError::Io(format!(
            "delegations file too large: {}",
            target.display()
        )));
    }
    let Ok(text) = fs::read_to_string(target) else {
        return Ok(false);
    };
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("parent").and_then(serde_json::Value::as_str) == Some(&result.parent)
            && value.get("child").and_then(serde_json::Value::as_str) == Some(&result.child)
            && value
                .get("child_records")
                .and_then(serde_json::Value::as_u64)
                == Some(result.child_records as u64)
            && value
                .get("child_modified_ms")
                .and_then(serde_json::Value::as_u64)
                == Some(result.child_modified_ms)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn append_result(target: &Path, result: &DelegationResult) -> Result<(), AgentError> {
    if let Ok(metadata) = fs::symlink_metadata(target)
        && metadata.file_type().is_symlink()
    {
        return Err(AgentError::Io(format!(
            "refusing symlink delegation target {}",
            target.display()
        )));
    }
    let mut record = result.clone();
    record.prev_hash = last_record_hash(target)?;
    record.record_hash = record_hash(&record);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(target)
        .map_err(|error| AgentError::Io(format!("append {}: {error}", target.display())))?;
    serde_json::to_writer(&mut file, &record)
        .map_err(|error| AgentError::SessionState(format!("encode delegation: {error}")))?;
    file.write_all(b"\n")
        .map_err(|error| AgentError::Io(format!("append {}: {error}", target.display())))?;
    Ok(())
}

fn record_hash(record: &DelegationResult) -> String {
    let payload = format!(
        "{}|{}|{}|{:?}|{}|{}|{}|{}",
        record.parent,
        record.child,
        record.task.as_deref().unwrap_or_default(),
        record.status,
        record.steps,
        record.child_records,
        record.child_modified_ms,
        record.prev_hash.as_deref().unwrap_or_default()
    );
    use std::fmt::Write as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, payload.as_bytes());
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for byte in digest.as_ref() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn last_record_hash(target: &Path) -> Result<Option<String>, AgentError> {
    if fs::metadata(target).is_ok_and(|metadata| metadata.len() > MAX_DELEGATIONS_BYTES) {
        return Err(AgentError::Io(format!(
            "delegations file too large: {}",
            target.display()
        )));
    }
    let Ok(text) = fs::read_to_string(target) else {
        return Ok(None);
    };
    let mut last = None;
    for line in text.lines() {
        if let Ok(record) = serde_json::from_str::<DelegationResult>(line)
            && !record.record_hash.is_empty()
        {
            last = Some(record.record_hash);
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_migration::SessionRole;
    use crate::session_scan::{
        SessionEntry, SessionFormat, SessionGate, SessionLink, SessionScanReport, SessionTask,
    };

    fn temp(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "trit-session-callback-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn entry(root: &Path, id: &str, finished: bool) -> SessionEntry {
        SessionEntry {
            path: root.join(format!("{id}.jsonl")).display().to_string(),
            session_id: id.into(),
            project: None,
            role: SessionRole::Primary,
            format: SessionFormat::Trit,
            meta: None,
            bytes: 10,
            modified_ms: 7,
            records: 3,
            corrupt_lines: 0,
            tasks: vec![SessionTask {
                index: 1,
                task: "work".into(),
                objective: String::new(),
                steps: 2,
                complete: finished,
                finished,
            }],
            open_turn: !finished,
            in_doubt: 0,
            last_gate: None,
        }
    }

    fn report(root: &Path, child: SessionEntry) -> SessionScanReport {
        SessionScanReport {
            schema: 1,
            root: root.display().to_string(),
            roots: vec![root.display().to_string()],
            scanned: 2,
            sessions: vec![entry(root, "parent", true), child],
            links: vec![SessionLink {
                kind: LinkKind::ParentChild,
                from: "parent".into(),
                to: "child".into(),
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn completed_child_is_dry_run_by_default() {
        let root = temp("dry");
        let scan = report(&root, entry(&root, "child", true));
        let report = callback_sessions(&scan, &CallbackOptions::default()).unwrap();
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].status, CallbackStatus::Completed);
        assert!(!report.items[0].written);
        assert!(!root.join(DELEGATIONS_FILE).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_appends_once_and_is_idempotent() {
        let root = temp("apply");
        let scan = report(&root, entry(&root, "child", true));
        let first = callback_sessions(&scan, &CallbackOptions { dry_run: false }).unwrap();
        assert!(first.items[0].written);
        let target = root.join(DELEGATIONS_FILE);
        let text = fs::read_to_string(&target).unwrap();
        assert_eq!(text.lines().count(), 1);
        let record: DelegationResult = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(record.parent, "parent");
        assert_eq!(record.child, "child");
        assert_eq!(record.status, CallbackStatus::Completed);

        let second = callback_sessions(&scan, &CallbackOptions { dry_run: false }).unwrap();
        assert!(!second.items[0].written);
        assert!(
            second
                .skipped
                .iter()
                .any(|line| line.contains("already recorded"))
        );
        assert_eq!(fs::read_to_string(&target).unwrap().lines().count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn blocked_child_is_failed() {
        let root = temp("failed");
        let mut child = entry(&root, "child", true);
        child.last_gate = Some(SessionGate {
            status: "failed".into(),
            exit: 1,
            passed: false,
        });
        let scan = report(&root, child);
        let report = callback_sessions(&scan, &CallbackOptions::default()).unwrap();
        assert_eq!(report.items[0].status, CallbackStatus::Failed);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn open_child_is_incomplete() {
        let root = temp("open");
        let scan = report(&root, entry(&root, "child", false));
        let report = callback_sessions(&scan, &CallbackOptions::default()).unwrap();
        assert_eq!(report.items[0].status, CallbackStatus::Incomplete);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delegation_records_form_a_hash_chain() {
        let root = temp("chain");
        let target = root.join(DELEGATIONS_FILE);
        let mut first = DelegationResult {
            parent: "parent".into(),
            child: "a".into(),
            task: Some("a".into()),
            status: CallbackStatus::Completed,
            steps: 1,
            child_records: 1,
            child_modified_ms: 1,
            prev_hash: None,
            record_hash: String::new(),
        };
        append_result(&target, &first).unwrap();
        first.child = "b".into();
        append_result(&target, &first).unwrap();

        let text = fs::read_to_string(&target).unwrap();
        let records: Vec<DelegationResult> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert!(!records[0].record_hash.is_empty());
        assert_eq!(
            records[1].prev_hash.as_deref(),
            Some(records[0].record_hash.as_str())
        );
        assert!(!records[1].record_hash.is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
