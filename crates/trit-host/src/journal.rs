//! 写入 mutation journal 与崩溃后对账。
//!
//! 每次经由 [`crate::permissions::ClaimingTools`] 放行的非只读调用都会追加两条
//! 记录：`begin` 与 `finish`。进程在两者之间崩溃时，`begin` 没有对应的 `finish`，
//! [`MutationJournal::report`] 会把它列为 `in_doubt`，供宿主决定重做、回滚或人工
//! 复核。
//!
//! 账本是 append-only JSONL：`<root>/.agent/write-journal.jsonl`。默认不做 `fsync`，
//! 崩溃可能丢掉最后一行；解析失败的行如果位于末尾会按“崩溃尾部”忽略，中间的坏行
//! 仍然报错。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{ToolOutcome, ToolRequest};

/// 账本 schema。
pub const JOURNAL_SCHEMA: u32 = 1;
/// 账本相对 root 的路径。
pub const JOURNAL_PATH: &str = ".agent/write-journal.jsonl";
/// 单条记录里保留的工具输出上限（字符）；账本不复制完整日志。
const MAX_OUTPUT: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalError {
    Io(String),
    Parse(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "write journal io: {message}"),
            Self::Parse(message) => write!(formatter, "write journal parse: {message}"),
        }
    }
}

impl std::error::Error for JournalError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    Begin,
    Finish,
    Rollback,
}

/// 一次写操作涉及的文件快照；`backup` 指向 `.agent/journal-backups/<op_id>/` 下的副本。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupEntry {
    /// workspace 相对路径（`/` 分隔）。
    pub path: String,
    /// 操作前文件是否存在。
    pub existed: bool,
    /// 备份文件名；`None` 表示没有备份（超限或被跳过）。
    #[serde(default)]
    pub backup: Option<String>,
    /// 超过备份上限或路径不可归一化，回滚时无法恢复。
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalRecord {
    pub schema: u32,
    pub seq: u64,
    pub op_id: u64,
    pub phase: JournalPhase,
    pub tool: String,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub lease_epoch: Option<u64>,
    /// `begin` 时实际持有的锁文件（相对 workspace）；恢复者据此判断租约是否仍被占。
    #[serde(default)]
    pub lease_locks: Vec<String>,
    #[serde(default)]
    pub passed: bool,
    #[serde(default)]
    pub in_doubt: bool,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub backups: Vec<BackupEntry>,
    pub at: u64,
}

/// `begin` 之后、`finish` 之前的操作句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalEntry {
    pub op_id: u64,
}

/// 对账报告。
#[derive(Debug, Clone, Default, Serialize)]
pub struct JournalReport {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub rolled_back: usize,
    pub orphans: usize,
    pub in_doubt: Vec<JournalRecord>,
}

impl JournalReport {
    /// 是否有未闭合的 `begin`。
    pub fn has_in_doubt(&self) -> bool {
        !self.in_doubt.is_empty()
    }
}

#[derive(Debug)]
pub struct MutationJournal {
    root: PathBuf,
    path: PathBuf,
    seq: u64,
}

impl MutationJournal {
    /// 打开（或创建）账本；已有记录会校验 schema 并续上 seq。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, JournalError> {
        let root = root.as_ref().to_path_buf();
        let path = root.join(JOURNAL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                JournalError::Io(format!("create {}: {error}", parent.display()))
            })?;
        }
        let records = read_records(&path)?;
        let seq = records.last().map_or(0, |record| record.seq);
        Ok(Self { root, path, seq })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 记录一次写操作的开始；返回的 entry 必须传给 `record_finish`。
    pub fn record_begin(
        &mut self,
        request: &ToolRequest,
        lease_epoch: Option<u64>,
    ) -> Result<JournalEntry, JournalError> {
        self.record_begin_with_lease(request, lease_epoch, &[])
    }

    /// 同 [`Self::record_begin`]，额外记录本次实际持有的锁文件，供崩溃后判断
    /// 是否可以安全回滚（租约仍被活进程持有时不能动工作区）。
    pub fn record_begin_with_lease(
        &mut self,
        request: &ToolRequest,
        lease_epoch: Option<u64>,
        lease_locks: &[String],
    ) -> Result<JournalEntry, JournalError> {
        self.seq = self.seq.saturating_add(1);
        let op_id = self.seq;
        let record = JournalRecord {
            schema: JOURNAL_SCHEMA,
            seq: op_id,
            op_id,
            phase: JournalPhase::Begin,
            tool: request.tool.clone(),
            call_id: request.call_id.clone(),
            paths: request.paths.clone(),
            lease_epoch,
            lease_locks: lease_locks.to_vec(),
            passed: false,
            in_doubt: false,
            output: String::new(),
            backups: self.backup_paths(op_id, &request.paths)?,
            at: now_secs(),
        };
        self.append(&record)?;
        Ok(JournalEntry { op_id })
    }

    /// 记录一次写操作的结束（成功或失败）。
    pub fn record_finish(
        &mut self,
        entry: JournalEntry,
        outcome: &ToolOutcome,
    ) -> Result<(), JournalError> {
        self.seq = self.seq.saturating_add(1);
        let record = JournalRecord {
            schema: JOURNAL_SCHEMA,
            seq: self.seq,
            op_id: entry.op_id,
            phase: JournalPhase::Finish,
            tool: outcome.tool.clone(),
            call_id: outcome.call_id.clone(),
            paths: outcome.written_paths.clone(),
            lease_epoch: None,
            lease_locks: Vec::new(),
            passed: outcome.passed,
            in_doubt: outcome.in_doubt,
            output: truncate(&outcome.output, MAX_OUTPUT),
            backups: Vec::new(),
            at: now_secs(),
        };
        self.append(&record)?;
        // 成功的写已经把新内容落盘，pre-image 不再需要；失败/存疑保留给人工复核或回滚。
        if record.passed && !record.in_doubt {
            self.discard_backups(entry.op_id);
        }
        Ok(())
    }

    /// 记录一次工具错误（执行器返回 `Err`，没有 `ToolOutcome`）。
    pub fn record_error(
        &mut self,
        entry: JournalEntry,
        request: &ToolRequest,
        error: &str,
    ) -> Result<(), JournalError> {
        self.seq = self.seq.saturating_add(1);
        let record = JournalRecord {
            schema: JOURNAL_SCHEMA,
            seq: self.seq,
            op_id: entry.op_id,
            phase: JournalPhase::Finish,
            tool: request.tool.clone(),
            call_id: request.call_id.clone(),
            paths: request.paths.clone(),
            lease_epoch: None,
            lease_locks: Vec::new(),
            passed: false,
            in_doubt: true,
            output: truncate(error, MAX_OUTPUT),
            backups: Vec::new(),
            at: now_secs(),
        };
        self.append(&record)
    }

    /// 对账：找到没有 `finish` 的 `begin`。
    pub fn report(&self) -> Result<JournalReport, JournalError> {
        report_records(read_records(&self.path)?)
    }

    /// 记录一次回滚结果，闭合对应的 `begin`。
    pub fn record_rollback(
        &mut self,
        op_id: u64,
        outcome: &RollbackOutcome,
    ) -> Result<(), JournalError> {
        self.seq = self.seq.saturating_add(1);
        let record = JournalRecord {
            schema: JOURNAL_SCHEMA,
            seq: self.seq,
            op_id,
            phase: JournalPhase::Rollback,
            tool: String::new(),
            call_id: String::new(),
            paths: Vec::new(),
            lease_epoch: None,
            lease_locks: Vec::new(),
            passed: outcome.ok(),
            in_doubt: false,
            output: format!(
                "restored={} removed={} skipped={} failed={}",
                outcome.restored.len(),
                outcome.removed.len(),
                outcome.skipped.len(),
                outcome.failed.len()
            ),
            backups: Vec::new(),
            at: now_secs(),
        };
        self.append(&record)?;
        if outcome.ok() {
            self.discard_backups(op_id);
        }
        Ok(())
    }

    fn backup_dir(&self, op_id: u64) -> PathBuf {
        self.root
            .join(".agent/journal-backups")
            .join(op_id.to_string())
    }

    /// 尽力删除一次操作留下的 pre-image 备份；删不掉不影响主流程，
    /// 后续可用 [`Self::prune_backups`] 再清。
    fn discard_backups(&self, op_id: u64) {
        let _ = std::fs::remove_dir_all(self.backup_dir(op_id));
    }

    /// 清理没有任何未闭合/失败操作引用的备份目录。
    ///
    /// 保留：仍未闭合的 `begin`、`finish` 失败、`rollback` 失败（这些是复核证据）。
    /// 删除：成功闭合的操作、以及账本里完全没有记录的孤儿目录。
    pub fn prune_backups(&self) -> Result<PruneReport, JournalError> {
        let mut report = PruneReport::default();
        let mut keep = BTreeSet::new();
        for record in read_records(&self.path)? {
            match record.phase {
                JournalPhase::Begin => {
                    keep.insert(record.op_id);
                }
                JournalPhase::Finish | JournalPhase::Rollback => {
                    if record.passed {
                        keep.remove(&record.op_id);
                    } else {
                        keep.insert(record.op_id);
                    }
                }
            }
        }
        let dir = self.root.join(".agent/journal-backups");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(error) => {
                return Err(JournalError::Io(format!("read {}: {error}", dir.display())));
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|error| JournalError::Io(format!("read {}: {error}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(op_id) = name.parse::<u64>() else {
                // 不认识的目录不动，交给人工处理。
                report.kept += 1;
                continue;
            };
            if keep.contains(&op_id) {
                report.kept += 1;
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => report.removed += 1,
                Err(error) => report.failures.push(format!("{name}: {error}")),
            }
        }
        Ok(report)
    }
    pub fn rollback(&self, record: &JournalRecord) -> RollbackOutcome {
        let mut outcome = RollbackOutcome {
            op_id: record.op_id,
            ..RollbackOutcome::default()
        };
        let dir = self.backup_dir(record.op_id);
        for entry in &record.backups {
            if entry.skipped {
                outcome.skipped.push(entry.path.clone());
                continue;
            }
            let Some(relative) = backup_relative(&entry.path) else {
                outcome.skipped.push(entry.path.clone());
                continue;
            };
            let target = self.root.join(&relative);
            if entry.existed {
                let Some(name) = &entry.backup else {
                    outcome.skipped.push(entry.path.clone());
                    continue;
                };
                match std::fs::copy(dir.join(name), &target) {
                    Ok(_) => outcome.restored.push(entry.path.clone()),
                    Err(error) => outcome.failed.push(format!("{}: {error}", entry.path)),
                }
            } else {
                match std::fs::remove_file(&target) {
                    Ok(()) => outcome.removed.push(entry.path.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => outcome.failed.push(format!("{}: {error}", entry.path)),
                }
            }
        }
        outcome
    }

    fn backup_paths(&self, op_id: u64, paths: &[String]) -> Result<Vec<BackupEntry>, JournalError> {
        let dir = self.backup_dir(op_id);
        let mut entries = Vec::with_capacity(paths.len());
        for (index, raw) in paths.iter().enumerate() {
            let Some(relative) = backup_relative(raw) else {
                entries.push(BackupEntry {
                    path: raw.clone(),
                    skipped: true,
                    ..BackupEntry::default()
                });
                continue;
            };
            let target = self.root.join(&relative);
            match std::fs::metadata(&target) {
                Ok(metadata) if metadata.is_file() => {
                    if metadata.len() > MAX_BACKUP_BYTES {
                        entries.push(BackupEntry {
                            path: relative,
                            existed: true,
                            backup: None,
                            skipped: true,
                            bytes: metadata.len(),
                        });
                        continue;
                    }
                    std::fs::create_dir_all(&dir).map_err(|error| {
                        JournalError::Io(format!("create {}: {error}", dir.display()))
                    })?;
                    let name = index.to_string();
                    std::fs::copy(&target, dir.join(&name)).map_err(|error| {
                        JournalError::Io(format!("backup {}: {error}", target.display()))
                    })?;
                    entries.push(BackupEntry {
                        path: relative,
                        existed: true,
                        backup: Some(name),
                        skipped: false,
                        bytes: metadata.len(),
                    });
                }
                Ok(_) => entries.push(BackupEntry {
                    path: relative,
                    existed: true,
                    backup: None,
                    skipped: true,
                    bytes: 0,
                }),
                Err(_) => entries.push(BackupEntry {
                    path: relative,
                    existed: false,
                    backup: None,
                    skipped: false,
                    bytes: 0,
                }),
            }
        }
        Ok(entries)
    }

    fn append(&self, record: &JournalRecord) -> Result<(), JournalError> {
        let mut line = serde_json::to_string(record)
            .map_err(|error| JournalError::Io(format!("encode journal: {error}")))?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| JournalError::Io(format!("open {}: {error}", self.path.display())))?;
        file.write_all(line.as_bytes())
            .map_err(|error| JournalError::Io(format!("write {}: {error}", self.path.display())))
    }
}

/// 只读对账：账本不存在时返回空报告，也不创建 `.agent` 目录。
///
/// 完成裁决用它确认本轮声明路径上是否还压着上次崩溃留下的未闭合写操作。
pub fn inspect(root: impl AsRef<Path>) -> Result<JournalReport, JournalError> {
    let path = root.as_ref().join(JOURNAL_PATH);
    report_records(read_records(&path)?)
}

fn report_records(records: Vec<JournalRecord>) -> Result<JournalReport, JournalError> {
    let mut report = JournalReport::default();
    let mut open: BTreeMap<u64, JournalRecord> = BTreeMap::new();
    for record in records {
        report.total += 1;
        match record.phase {
            JournalPhase::Begin => {
                open.insert(record.op_id, record);
            }
            JournalPhase::Finish => match open.remove(&record.op_id) {
                Some(_) => {
                    if record.passed {
                        report.completed += 1;
                    } else {
                        report.failed += 1;
                    }
                }
                None => report.orphans += 1,
            },
            JournalPhase::Rollback => {
                open.remove(&record.op_id);
                report.rolled_back += 1;
            }
        }
    }
    report.in_doubt = open.into_values().collect();
    Ok(report)
}

/// 备份目录清理结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneReport {
    pub removed: usize,
    pub kept: usize,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RollbackOutcome {
    pub op_id: u64,
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
}

impl RollbackOutcome {
    pub fn ok(&self) -> bool {
        self.skipped.is_empty() && self.failed.is_empty()
    }
}

/// 单个文件备份上限；超过只记录、不回滚。
const MAX_BACKUP_BYTES: u64 = 8 * 1024 * 1024;

fn backup_relative(raw: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(&raw.replace('\\', "/")).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn read_records(path: &Path) -> Result<Vec<JournalRecord>, JournalError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(JournalError::Io(format!(
                "open {}: {error}",
                path.display()
            )));
        }
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| JournalError::Io(format!("read {}: {error}", path.display())))?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(record) => {
                if record.schema != JOURNAL_SCHEMA {
                    return Err(JournalError::Parse(format!(
                        "{}:{}: schema {} != {}",
                        path.display(),
                        index + 1,
                        record.schema,
                        JOURNAL_SCHEMA
                    )));
                }
                records.push(record);
            }
            Err(error) => {
                // 崩溃可能留下半行；只容忍最后一行。
                if index + 1 == lines.len() {
                    break;
                }
                return Err(JournalError::Parse(format!(
                    "{}:{}: {error}",
                    path.display(),
                    index + 1
                )));
            }
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("trit-journal-{tag}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn request() -> ToolRequest {
        ToolRequest::write("call-1", "write_file", "src/a.py", "x")
    }

    fn outcome(passed: bool) -> ToolOutcome {
        ToolOutcome {
            call_id: "call-1".into(),
            tool: "write_file".into(),
            command: None,
            passed,
            assertions: 0,
            written_paths: vec!["src/a.py".into()],
            in_doubt: false,
            output: "ok".into(),
        }
    }

    #[test]
    fn records_begin_and_finish() {
        let root = root("pair");
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), Some(3)).unwrap();
        journal.record_finish(entry, &outcome(true)).unwrap();
        let report = journal.report().unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.completed, 1);
        assert_eq!(report.failed, 0);
        assert!(!report.has_in_doubt());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unclosed_begin_is_in_doubt() {
        let root = root("doubt");
        let mut journal = MutationJournal::open(&root).unwrap();
        let _ = journal.record_begin(&request(), None).unwrap();
        let report = journal.report().unwrap();
        assert_eq!(report.in_doubt.len(), 1);
        assert!(report.has_in_doubt());
        assert_eq!(report.in_doubt[0].tool, "write_file");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_finish_is_counted_and_seq_continues() {
        let root = root("failed");
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), None).unwrap();
        journal.record_finish(entry, &outcome(false)).unwrap();
        let report = journal.report().unwrap();
        assert_eq!(report.failed, 1);

        drop(journal);
        let mut reopened = MutationJournal::open(&root).unwrap();
        let entry = reopened.record_begin(&request(), None).unwrap();
        reopened.record_error(entry, &request(), "boom").unwrap();
        let report = reopened.report().unwrap();
        assert_eq!(report.total, 4);
        assert_eq!(report.failed, 2);
        assert!(!report.has_in_doubt());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn trailing_partial_line_is_ignored() {
        let root = root("partial");
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), None).unwrap();
        journal.record_finish(entry, &outcome(true)).unwrap();
        let path = journal.path().to_path_buf();
        drop(journal);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"schema\":1,\"seq\":");
        std::fs::write(&path, text).unwrap();

        let reopened = MutationJournal::open(&root).unwrap();
        let report = reopened.report().unwrap();
        assert_eq!(report.completed, 1);
        assert!(!report.has_in_doubt());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_restores_existing_file() {
        let root = root("rollback");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "old").unwrap();
        let mut journal = MutationJournal::open(&root).unwrap();
        let _ = journal.record_begin(&request(), None).unwrap();
        // 模拟半完成写。
        std::fs::write(root.join("src/a.py"), "new").unwrap();

        let report = journal.report().unwrap();
        assert_eq!(report.in_doubt.len(), 1);
        let record = report.in_doubt[0].clone();
        let outcome = journal.rollback(&record);
        assert_eq!(outcome.restored, ["src/a.py"]);
        assert!(outcome.ok(), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.py")).unwrap(),
            "old"
        );

        journal.record_rollback(record.op_id, &outcome).unwrap();
        let report = journal.report().unwrap();
        assert_eq!(report.rolled_back, 1);
        assert!(!report.has_in_doubt());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_removes_new_file() {
        let root = root("rollback-new");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut journal = MutationJournal::open(&root).unwrap();
        let _ = journal.record_begin(&request(), None).unwrap();
        std::fs::write(root.join("src/a.py"), "new").unwrap();

        let report = journal.report().unwrap();
        let outcome = journal.rollback(&report.in_doubt[0]);
        assert_eq!(outcome.removed, ["src/a.py"]);
        assert!(outcome.ok(), "{outcome:?}");
        assert!(!root.join("src/a.py").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn successful_finish_discards_backups() {
        let root = root("discard-ok");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "old").unwrap();
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), None).unwrap();
        let backups = root
            .join(".agent/journal-backups")
            .join(entry.op_id.to_string());
        assert!(backups.is_dir());

        journal.record_finish(entry, &outcome(true)).unwrap();
        assert!(!backups.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_finish_keeps_backups_and_prune_preserves_them() {
        let root = root("keep-failed");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "old").unwrap();
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), None).unwrap();
        let backups = root
            .join(".agent/journal-backups")
            .join(entry.op_id.to_string());

        journal.record_finish(entry, &outcome(false)).unwrap();
        assert!(backups.is_dir());

        let pruned = journal.prune_backups().unwrap();
        assert_eq!(pruned.removed, 0);
        assert_eq!(pruned.kept, 1);
        assert!(pruned.failures.is_empty());
        assert!(backups.is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prune_removes_orphans_but_keeps_open_begins() {
        let root = root("prune");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "old").unwrap();
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), None).unwrap();
        let open_dir = root
            .join(".agent/journal-backups")
            .join(entry.op_id.to_string());
        let orphan = root.join(".agent/journal-backups/99");
        std::fs::create_dir_all(&orphan).unwrap();

        let pruned = journal.prune_backups().unwrap();
        assert_eq!(pruned.removed, 1);
        assert_eq!(pruned.kept, 1);
        assert!(open_dir.is_dir());
        assert!(!orphan.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn successful_rollback_discards_backups() {
        let root = root("discard-rollback");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "old").unwrap();
        let mut journal = MutationJournal::open(&root).unwrap();
        let entry = journal.record_begin(&request(), None).unwrap();
        let backups = root
            .join(".agent/journal-backups")
            .join(entry.op_id.to_string());
        std::fs::write(root.join("src/a.py"), "new").unwrap();

        let report = journal.report().unwrap();
        let outcome = journal.rollback(&report.in_doubt[0]);
        assert!(outcome.ok(), "{outcome:?}");
        journal.record_rollback(entry.op_id, &outcome).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.py")).unwrap(),
            "old"
        );
        assert!(!backups.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
