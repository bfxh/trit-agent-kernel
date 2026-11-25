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

use std::collections::BTreeMap;
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
    #[serde(default)]
    pub passed: bool,
    #[serde(default)]
    pub in_doubt: bool,
    #[serde(default)]
    pub output: String,
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
    path: PathBuf,
    seq: u64,
}

impl MutationJournal {
    /// 打开（或创建）账本；已有记录会校验 schema 并续上 seq。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = root.as_ref().join(JOURNAL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                JournalError::Io(format!("create {}: {error}", parent.display()))
            })?;
        }
        let records = read_records(&path)?;
        let seq = records.last().map_or(0, |record| record.seq);
        Ok(Self { path, seq })
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
            passed: false,
            in_doubt: false,
            output: String::new(),
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
            passed: outcome.passed,
            in_doubt: outcome.in_doubt,
            output: truncate(&outcome.output, MAX_OUTPUT),
            at: now_secs(),
        };
        self.append(&record)
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
            passed: false,
            in_doubt: true,
            output: truncate(error, MAX_OUTPUT),
            at: now_secs(),
        };
        self.append(&record)
    }

    /// 对账：找到没有 `finish` 的 `begin`。
    pub fn report(&self) -> Result<JournalReport, JournalError> {
        let records = read_records(&self.path)?;
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
            }
        }
        report.in_doubt = open.into_values().collect();
        Ok(report)
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
}
