//! 文件检查点与回退。
//!
//! 每个 turn 一份 checkpoint，只记录写工具触碰到的文件在 turn 开始前的内容；
//! `rewind_code` 恢复最早快照（None 表示当时不存在，恢复时删除）。
//! shell 命令无法由文件快照回退，会在 checkpoint 中记录，并在回退时报告。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};

use crate::{
    AgentError, SessionEvent, SessionLog, SessionRecord, ToolExecutor, ToolOutcome, ToolRequest,
};

pub const CHECKPOINT_SCHEMA: u32 = 1;
/// 单个 checkpoint / rewind 日志读取上限；超过时 fail closed。
const MAX_STATE_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const REWIND_SCHEMA: u32 = 1;
pub const REWIND_JOURNAL: &str = "rewind-journal.jsonl";
pub const REWIND_BACKUP_DIR: &str = "rewind-backups";
/// 单个文件快照上限；超过即 fail-closed，避免把大文件读进内存/检查点。
const MAX_CHECKPOINT_FILE_BYTES: u64 = 8 * 1024 * 1024;
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema: u32,
    pub turn: usize,
    pub objective: String,
    /// path -> 内容；None 表示该路径在 turn 开始时不存在。
    pub files: BTreeMap<String, Option<String>>,
    /// 本轮执行过的 shell 命令；无法由文件快照回退。
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindScope {
    Code,
    Conversation,
    Both,
}

impl RewindScope {
    pub fn includes_code(self) -> bool {
        matches!(self, Self::Code | Self::Both)
    }

    pub fn includes_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindReport {
    pub turn: usize,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub conversation_truncated: bool,
    /// 被回退 turn 中执行过、但无法撤销的 shell 命令。
    #[serde(default)]
    pub unrewindable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointStore {
    dir: PathBuf,
}

impl CheckpointStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|error| {
            AgentError::Io(format!("checkpoint dir {}: {error}", dir.display()))
        })?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn list(&self) -> Result<Vec<Checkpoint>, AgentError> {
        let mut checkpoints = BTreeMap::new();
        let entries = std::fs::read_dir(&self.dir).map_err(|error| {
            AgentError::Io(format!("checkpoint dir {}: {error}", self.dir.display()))
        })?;
        for entry in entries {
            let entry =
                entry.map_err(|error| AgentError::Io(format!("checkpoint dir: {error}")))?;
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("checkpoint-")
                || !name.to_string_lossy().ends_with(".json")
            {
                continue;
            }
            if std::fs::metadata(entry.path())
                .is_ok_and(|metadata| metadata.len() > MAX_STATE_FILE_BYTES)
            {
                return Err(AgentError::SessionState(format!(
                    "checkpoint too large: {}",
                    entry.path().display()
                )));
            }
            let text = std::fs::read_to_string(entry.path()).map_err(|error| {
                AgentError::Io(format!("checkpoint {}: {error}", entry.path().display()))
            })?;
            let checkpoint: Checkpoint = serde_json::from_str(&text).map_err(|error| {
                AgentError::SessionState(format!("checkpoint {}: {error}", entry.path().display()))
            })?;
            if checkpoint.schema != CHECKPOINT_SCHEMA {
                return Err(AgentError::SessionState(format!(
                    "checkpoint schema {} (expected {})",
                    checkpoint.schema, CHECKPOINT_SCHEMA
                )));
            }
            if checkpoints.insert(checkpoint.turn, checkpoint).is_some() {
                return Err(AgentError::SessionState("duplicate checkpoint turn".into()));
            }
        }
        Ok(checkpoints.into_values().collect())
    }

    pub fn begin(&self, turn: usize, objective: &str) -> CheckpointRecorder {
        CheckpointRecorder {
            store: self.clone(),
            checkpoint: Checkpoint {
                schema: CHECKPOINT_SCHEMA,
                turn,
                objective: objective.to_owned(),
                files: BTreeMap::new(),
                commands: Vec::new(),
            },
        }
    }

    /// 只读检查回退 journal：未完成/回滚失败的 rewind 会让新回退 fail-closed。
    pub fn rewind_status(&self) -> Result<RewindJournalReport, AgentError> {
        RewindJournal::open(&self.dir)?.report()
    }

    /// 把未完成的 rewind 恢复到 pre-rewind 状态。
    ///
    /// 只处理已知 `applying` 的路径（按逆序恢复）；成功后在 journal 落 `recover`，
    /// 失败明细原样带回。恢复失败的操作会继续留在 `in_doubt`，不会被静默清掉。
    pub fn recover_rewind(&self, root: &Path) -> Result<RewindRecoveryReport, AgentError> {
        let mut journal = RewindJournal::open(&self.dir)?;
        let status = journal.report()?;
        if status.in_doubt.iter().any(|doubt| doubt.backend == "git") {
            return Err(AgentError::SessionState(
                "rewind journal contains git-backend operation(s); use rewind-status --checkpoint-backend git"
                    .into(),
            ));
        }
        let mut report = RewindRecoveryReport::default();
        for doubt in status.in_doubt {
            report.attempted += 1;
            let failures = if doubt.applied.is_empty() {
                Vec::new()
            } else {
                restore_rewind_backups(&journal, root, &doubt)
            };
            journal.recover(doubt.op_id, doubt.turn, "files", "", &failures)?;
            if failures.is_empty() {
                journal.cleanup(doubt.op_id);
                report.recovered += 1;
            } else {
                report.failed.push(RewindRecoveryFailure {
                    op_id: doubt.op_id,
                    failures,
                });
            }
        }
        Ok(report)
    }

    /// 恢复 `turn` 及之后所有 checkpoint 中每个路径的最早快照。
    ///
    /// 先完成所有路径的归一化、symlink/类型检查和当前内容捕获，再开始写入；
    /// 应用中途失败会按逆序回滚，避免留下半恢复的工作区。
    /// 目标 turn 的 checkpoint 不存在时 fail-closed（可能被保留策略清理）。
    pub fn rewind_code(&self, root: &Path, turn: usize) -> Result<RewindReport, AgentError> {
        let checkpoints = self.list()?;
        if !checkpoints.iter().any(|checkpoint| checkpoint.turn == turn) {
            return Err(AgentError::SessionState(format!(
                "checkpoint for turn {turn} is missing"
            )));
        }
        let mut journal = RewindJournal::open(&self.dir)?;
        let status = journal.report()?;
        if status.has_in_doubt() {
            return Err(AgentError::SessionState(format!(
                "rewind journal has {} unfinished operation(s); inspect rewind-status before retrying",
                status.in_doubt.len()
            )));
        }
        let root = std::fs::canonicalize(root)
            .map_err(|error| AgentError::Io(format!("workspace root: {error}")))?;
        let dir = Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|error| AgentError::Io(format!("open workspace: {error}")))?;
        let mut earliest: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut unrewindable = Vec::new();
        for checkpoint in checkpoints {
            if checkpoint.turn < turn {
                continue;
            }
            for command in checkpoint.commands {
                if !unrewindable.contains(&command) {
                    unrewindable.push(command);
                }
            }
            for (path, content) in checkpoint.files {
                earliest.entry(path).or_insert(content);
            }
        }

        let mut by_path: BTreeMap<PathBuf, RewindEntry> = BTreeMap::new();
        for (raw, content) in earliest {
            let Some(relative) = normalize_relative(Path::new(&raw)) else {
                return Err(AgentError::SessionState(format!(
                    "checkpoint path escapes workspace: {raw}"
                )));
            };
            let entry = RewindEntry {
                raw,
                content: content.map(String::into_bytes),
            };
            if let Some(existing) = by_path.get(&relative) {
                if existing.content != entry.content {
                    return Err(AgentError::SessionState(format!(
                        "checkpoint paths collide after normalization: {} vs {}",
                        existing.raw, entry.raw
                    )));
                }
                continue;
            }
            by_path.insert(relative, entry);
        }
        let entries: Vec<(PathBuf, RewindEntry)> = by_path.into_iter().collect();

        let mut previous: BTreeMap<PathBuf, Option<Vec<u8>>> = BTreeMap::new();
        for (relative, entry) in &entries {
            let current = capture_current(&dir, relative, &entry.raw)?;
            previous.insert(relative.clone(), current);
        }

        // 任何路径被改写前先落 pre-rewind 备份与 `begin`：进程若在下面任何一步崩溃，
        // 账本会留下未闭合记录，`recover_rewind` 可据此把半写工作区恢复回去。
        let backup_entries: Vec<(String, Option<Vec<u8>>)> = entries
            .iter()
            .map(|(relative, entry)| (entry.raw.clone(), previous.get(relative).cloned().flatten()))
            .collect();
        let op_id = journal.begin(turn, &backup_entries)?;

        let mut created_dirs = Vec::new();
        let mut attempted = Vec::new();
        let applied = (|| -> Result<(), AgentError> {
            for (relative, entry) in &entries {
                journal.applying(op_id, turn, &entry.raw)?;
                attempted.push(relative.clone());
                if let Some(content) = &entry.content {
                    ensure_parents(&dir, relative, &entry.raw, &mut created_dirs)?;
                    write_guarded(&dir, relative, content, &entry.raw)?;
                } else {
                    match dir.remove_file(relative) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(AgentError::Io(format!(
                                "rewind delete {}: {error}",
                                entry.raw
                            )));
                        }
                    }
                }
            }
            Ok(())
        })();

        if let Err(original) = applied {
            let failures = rollback(&dir, &entries, &previous, &attempted, &created_dirs);
            journal.rollback(op_id, turn, &failures)?;
            if failures.is_empty() {
                journal.cleanup(op_id);
                return Err(original);
            }
            return Err(AgentError::RewindRollback {
                original: original.to_string(),
                failures,
            });
        }

        let mut report = RewindReport {
            turn,
            unrewindable,
            ..RewindReport::default()
        };
        for (relative, entry) in &entries {
            if entry.content.is_some() {
                report.restored.push(entry.raw.clone());
            } else if matches!(previous.get(relative), Some(Some(_))) {
                report.deleted.push(entry.raw.clone());
            }
        }
        journal.finish(op_id, turn, &report.restored, &report.deleted)?;
        journal.cleanup(op_id);
        Ok(report)
    }

    /// 只读列出 `turn` 及之后的 shell 命令，供调用方在恢复前做 fail-closed 判断。
    pub fn commands_since(&self, turn: usize) -> Result<Vec<String>, AgentError> {
        let checkpoints = self.list()?;
        if !checkpoints.iter().any(|checkpoint| checkpoint.turn == turn) {
            return Err(AgentError::SessionState(format!(
                "checkpoint for turn {turn} is missing"
            )));
        }
        let mut commands = Vec::new();
        for checkpoint in checkpoints {
            if checkpoint.turn < turn {
                continue;
            }
            for command in checkpoint.commands {
                if !commands.contains(&command) {
                    commands.push(command);
                }
            }
        }
        Ok(commands)
    }

    /// 只保留最近 `keep_last` 个 checkpoint。
    pub fn prune(&self, keep_last: usize) -> Result<usize, AgentError> {
        let checkpoints = self.list()?;
        let keep_last = keep_last.max(1);
        if checkpoints.len() <= keep_last {
            return Ok(0);
        }
        let remove = checkpoints.len() - keep_last;
        for checkpoint in checkpoints.iter().take(remove) {
            let path = self.checkpoint_path(checkpoint.turn);
            std::fs::remove_file(&path)
                .map_err(|error| AgentError::Io(format!("prune {}: {error}", path.display())))?;
        }
        Ok(remove)
    }

    fn checkpoint_path(&self, turn: usize) -> PathBuf {
        self.dir.join(format!("checkpoint-{turn:08}.json"))
    }

    fn save(&self, checkpoint: &Checkpoint) -> Result<(), AgentError> {
        let path = self.checkpoint_path(checkpoint.turn);
        let text = serde_json::to_string_pretty(checkpoint)
            .map_err(|error| AgentError::SessionState(format!("checkpoint encode: {error}")))?;
        let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
        let temporary = self.dir.join(format!(
            "checkpoint-{:08}.{}.{}.tmp",
            checkpoint.turn,
            std::process::id(),
            seq
        ));
        std::fs::write(&temporary, text)
            .map_err(|error| AgentError::Io(format!("checkpoint write: {error}")))?;
        match std::fs::rename(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                std::fs::rename(&temporary, &path)
                    .map_err(|error| AgentError::Io(format!("checkpoint rename: {error}")))
            }
        }
    }
}

/// 回退 journal 的单个阶段。`begin` 之后没有 `finish`/`rollback`/`recover` 即
/// 「未完成回退」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindPhase {
    Begin,
    Applying,
    Finish,
    Rollback,
    Recover,
}

/// `begin` 时为每个路径留下的 pre-rewind 快照；`recover` 靠它恢复半写工作区。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RewindBackup {
    pub path: String,
    pub existed: bool,
    /// `rewind-backups/<op_id>/<index>` 下的文件名；`None` 表示当时不存在。
    #[serde(default)]
    pub backup: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindRecord {
    pub schema: u32,
    pub seq: u64,
    pub op_id: u64,
    pub phase: RewindPhase,
    pub turn: usize,
    /// `begin`：本次回退计划触碰的全部路径。
    #[serde(default)]
    pub paths: Vec<String>,
    /// `begin`：每个路径的 pre-rewind 快照。
    #[serde(default)]
    pub backups: Vec<RewindBackup>,
    /// 后端标识：`files`（逐路径备份）或 `git`（整树 backup commit）。
    #[serde(default)]
    pub backend: String,
    /// `git` 后端的整树 pre-rewind commit；`files` 为空。
    #[serde(default)]
    pub backup: String,
    /// `applying`：即将（或可能已经）被改写的单个路径。
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub restored: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
    pub at: u64,
}

/// 未完成或回滚失败的回退操作。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RewindDoubt {
    pub op_id: u64,
    pub turn: usize,
    /// 计划触碰的路径。
    pub planned: Vec<String>,
    /// 已经进入 `applying` 的路径（崩溃时可能处于半写状态）。
    pub applied: Vec<String>,
    /// 进程内回滚失败明细；为空表示只是崩溃导致未闭合。
    #[serde(default)]
    pub rollback_failures: Vec<String>,
    /// 恢复所需的 pre-rewind 快照。
    #[serde(default)]
    pub backups: Vec<RewindBackup>,
    /// 后端标识；旧记录为空时按 `files` 处理。
    #[serde(default)]
    pub backend: String,
    /// `git` 后端的整树 pre-rewind commit。
    #[serde(default)]
    pub backup: String,
    pub at: u64,
}

impl RewindDoubt {
    /// 是否有路径可能已被改写。
    pub fn touched_paths(&self) -> &[String] {
        &self.applied
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RewindJournalReport {
    pub total: usize,
    pub completed: usize,
    pub rolled_back: usize,
    pub recovered: usize,
    /// 未闭合的 `begin` 与回滚失败的操作；非空时新回退必须 fail-closed。
    pub in_doubt: Vec<RewindDoubt>,
}

impl RewindJournalReport {
    pub fn has_in_doubt(&self) -> bool {
        !self.in_doubt.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RewindRecoveryFailure {
    pub op_id: u64,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RewindRecoveryReport {
    pub attempted: usize,
    pub recovered: usize,
    pub failed: Vec<RewindRecoveryFailure>,
}

impl RewindRecoveryReport {
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }
}

pub(crate) struct RewindJournal {
    dir: PathBuf,
    path: PathBuf,
    seq: u64,
}

impl RewindJournal {
    pub(crate) fn open(dir: &Path) -> Result<Self, AgentError> {
        std::fs::create_dir_all(dir)
            .map_err(|error| AgentError::Io(format!("rewind journal dir: {error}")))?;
        let path = dir.join(REWIND_JOURNAL);
        let records = read_rewind_records(&path)?;
        let seq = records.last().map_or(0, |record| record.seq);
        Ok(Self {
            dir: dir.to_path_buf(),
            path,
            seq,
        })
    }

    /// 先写 pre-rewind 备份，再落 `begin`；任何备份失败都发生在工作区被改写之前。
    fn begin(
        &mut self,
        turn: usize,
        entries: &[(String, Option<Vec<u8>>)],
    ) -> Result<u64, AgentError> {
        self.seq = self.seq.saturating_add(1);
        let op_id = self.seq;
        let backup_dir = self.backup_dir(op_id);
        let mut backups = Vec::with_capacity(entries.len());
        for (index, (path, content)) in entries.iter().enumerate() {
            match content {
                Some(bytes) => {
                    std::fs::create_dir_all(&backup_dir)
                        .map_err(|error| AgentError::Io(format!("rewind backup dir: {error}")))?;
                    let name = index.to_string();
                    std::fs::write(backup_dir.join(&name), bytes).map_err(|error| {
                        AgentError::Io(format!("rewind backup {path}: {error}"))
                    })?;
                    backups.push(RewindBackup {
                        path: path.clone(),
                        existed: true,
                        backup: Some(name),
                    });
                }
                None => backups.push(RewindBackup {
                    path: path.clone(),
                    existed: false,
                    backup: None,
                }),
            }
        }
        self.append(RewindRecord {
            schema: REWIND_SCHEMA,
            seq: op_id,
            op_id,
            phase: RewindPhase::Begin,
            turn,
            paths: entries.iter().map(|(path, _)| path.clone()).collect(),
            backups,
            backend: "files".into(),
            backup: String::new(),
            path: String::new(),
            restored: Vec::new(),
            deleted: Vec::new(),
            failures: Vec::new(),
            at: now_secs(),
        })?;
        Ok(op_id)
    }

    /// git 后端的 `begin`：整树 backup commit 已在工作区改写前创建。
    pub(crate) fn begin_git(&mut self, turn: usize, backup: &str) -> Result<u64, AgentError> {
        self.seq = self.seq.saturating_add(1);
        let op_id = self.seq;
        self.append(RewindRecord {
            schema: REWIND_SCHEMA,
            seq: op_id,
            op_id,
            phase: RewindPhase::Begin,
            turn,
            paths: Vec::new(),
            backups: Vec::new(),
            backend: "git".into(),
            backup: backup.to_owned(),
            path: String::new(),
            restored: Vec::new(),
            deleted: Vec::new(),
            failures: Vec::new(),
            at: now_secs(),
        })?;
        Ok(op_id)
    }

    pub(crate) fn applying(
        &mut self,
        op_id: u64,
        turn: usize,
        path: &str,
    ) -> Result<(), AgentError> {
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        self.append(RewindRecord {
            schema: REWIND_SCHEMA,
            seq,
            op_id,
            phase: RewindPhase::Applying,
            turn,
            paths: Vec::new(),
            backups: Vec::new(),
            backend: String::new(),
            backup: String::new(),
            path: path.to_owned(),
            restored: Vec::new(),
            deleted: Vec::new(),
            failures: Vec::new(),
            at: now_secs(),
        })
    }

    pub(crate) fn finish(
        &mut self,
        op_id: u64,
        turn: usize,
        restored: &[String],
        deleted: &[String],
    ) -> Result<(), AgentError> {
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        self.append(RewindRecord {
            schema: REWIND_SCHEMA,
            seq,
            op_id,
            phase: RewindPhase::Finish,
            turn,
            paths: Vec::new(),
            backups: Vec::new(),
            backend: String::new(),
            backup: String::new(),
            path: String::new(),
            restored: restored.to_vec(),
            deleted: deleted.to_vec(),
            failures: Vec::new(),
            at: now_secs(),
        })
    }

    pub(crate) fn rollback(
        &mut self,
        op_id: u64,
        turn: usize,
        failures: &[String],
    ) -> Result<(), AgentError> {
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        self.append(RewindRecord {
            schema: REWIND_SCHEMA,
            seq,
            op_id,
            phase: RewindPhase::Rollback,
            turn,
            paths: Vec::new(),
            backups: Vec::new(),
            backend: String::new(),
            backup: String::new(),
            path: String::new(),
            restored: Vec::new(),
            deleted: Vec::new(),
            failures: failures.to_vec(),
            at: now_secs(),
        })
    }

    pub(crate) fn recover(
        &mut self,
        op_id: u64,
        turn: usize,
        backend: &str,
        backup: &str,
        failures: &[String],
    ) -> Result<(), AgentError> {
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        self.append(RewindRecord {
            schema: REWIND_SCHEMA,
            seq,
            op_id,
            phase: RewindPhase::Recover,
            turn,
            paths: Vec::new(),
            backups: Vec::new(),
            backend: backend.to_owned(),
            backup: backup.to_owned(),
            path: String::new(),
            restored: Vec::new(),
            deleted: Vec::new(),
            failures: failures.to_vec(),
            at: now_secs(),
        })
    }

    fn backup_dir(&self, op_id: u64) -> PathBuf {
        self.dir.join(REWIND_BACKUP_DIR).join(op_id.to_string())
    }

    fn cleanup(&self, op_id: u64) {
        let _ = std::fs::remove_dir_all(self.backup_dir(op_id));
    }

    pub(crate) fn report(&self) -> Result<RewindJournalReport, AgentError> {
        rewind_report(read_rewind_records(&self.path)?)
    }

    fn append(&self, record: RewindRecord) -> Result<(), AgentError> {
        let mut line = serde_json::to_string(&record)
            .map_err(|error| AgentError::SessionState(format!("rewind journal encode: {error}")))?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| AgentError::Io(format!("open {}: {error}", self.path.display())))?;
        file.write_all(line.as_bytes())
            .map_err(|error| AgentError::Io(format!("write rewind journal: {error}")))
    }
}

fn read_rewind_records(path: &Path) -> Result<Vec<RewindRecord>, AgentError> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AgentError::Io(format!("open {}: {error}", path.display())));
        }
    };
    if let Ok(metadata) = file.metadata()
        && metadata.len() > MAX_STATE_FILE_BYTES
    {
        return Err(AgentError::Io(format!(
            "read {}: file too large",
            path.display()
        )));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| AgentError::Io(format!("read {}: {error}", path.display())))?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str::<RewindRecord>(line) {
            Ok(record) => {
                if record.schema != REWIND_SCHEMA {
                    return Err(AgentError::SessionState(format!(
                        "{}:{}: rewind schema {} != {}",
                        path.display(),
                        index + 1,
                        record.schema,
                        REWIND_SCHEMA
                    )));
                }
                records.push(record);
            }
            Err(error) => {
                // 崩溃可能留下半行；只容忍最后一行。
                if index + 1 == lines.len() {
                    break;
                }
                return Err(AgentError::SessionState(format!(
                    "{}:{}: {error}",
                    path.display(),
                    index + 1
                )));
            }
        }
    }
    Ok(records)
}

#[derive(Default)]
struct OpenRewind {
    turn: usize,
    planned: Vec<String>,
    applied: Vec<String>,
    backups: Vec<RewindBackup>,
    backend: String,
    backup: String,
    at: u64,
}

fn rewind_report(records: Vec<RewindRecord>) -> Result<RewindJournalReport, AgentError> {
    let mut report = RewindJournalReport::default();
    let mut open: BTreeMap<u64, OpenRewind> = BTreeMap::new();
    for record in records {
        report.total += 1;
        match record.phase {
            RewindPhase::Begin => {
                open.insert(
                    record.op_id,
                    OpenRewind {
                        turn: record.turn,
                        planned: record.paths,
                        applied: Vec::new(),
                        backups: record.backups,
                        backend: record.backend,
                        backup: record.backup,
                        at: record.at,
                    },
                );
            }
            RewindPhase::Applying => {
                if let Some(entry) = open.get_mut(&record.op_id)
                    && !record.path.is_empty()
                    && !entry.applied.contains(&record.path)
                {
                    entry.applied.push(record.path);
                }
            }
            RewindPhase::Finish => {
                open.remove(&record.op_id);
                report.completed += 1;
            }
            RewindPhase::Rollback => {
                let entry = open.remove(&record.op_id).unwrap_or_default();
                report.rolled_back += 1;
                if !record.failures.is_empty() {
                    report.in_doubt.push(RewindDoubt {
                        op_id: record.op_id,
                        turn: entry.turn,
                        planned: entry.planned,
                        applied: entry.applied,
                        rollback_failures: record.failures,
                        backups: entry.backups,
                        backend: entry.backend,
                        backup: entry.backup,
                        at: entry.at,
                    });
                }
            }
            RewindPhase::Recover => {
                let entry = open.remove(&record.op_id);
                if record.failures.is_empty() {
                    report.recovered += 1;
                } else if let Some(entry) = entry {
                    report.in_doubt.push(RewindDoubt {
                        op_id: record.op_id,
                        turn: entry.turn,
                        planned: entry.planned,
                        applied: entry.applied,
                        rollback_failures: record.failures,
                        backups: entry.backups,
                        backend: entry.backend,
                        backup: entry.backup,
                        at: entry.at,
                    });
                }
            }
        }
    }
    for (op_id, entry) in open {
        report.in_doubt.push(RewindDoubt {
            op_id,
            turn: entry.turn,
            planned: entry.planned,
            applied: entry.applied,
            rollback_failures: Vec::new(),
            backups: entry.backups,
            backend: entry.backend,
            backup: entry.backup,
            at: entry.at,
        });
    }
    Ok(report)
}

fn restore_rewind_backups(
    journal: &RewindJournal,
    root: &Path,
    doubt: &RewindDoubt,
) -> Vec<String> {
    let mut failures = Vec::new();
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(error) => return vec![format!("workspace root: {error}")],
    };
    let dir = match Dir::open_ambient_dir(&root, cap_std::ambient_authority()) {
        Ok(dir) => dir,
        Err(error) => return vec![format!("open workspace: {error}")],
    };
    let backup_dir = journal.backup_dir(doubt.op_id);
    for raw in doubt.applied.iter().rev() {
        let Some(backup) = doubt.backups.iter().find(|entry| entry.path == *raw) else {
            failures.push(format!("{raw}: missing backup"));
            continue;
        };
        let Some(relative) = normalize_relative(Path::new(raw)) else {
            failures.push(format!("{raw}: path escapes workspace"));
            continue;
        };
        if backup.existed {
            let Some(name) = &backup.backup else {
                failures.push(format!("{raw}: missing backup file"));
                continue;
            };
            let bytes = match std::fs::read(backup_dir.join(name)) {
                Ok(bytes) => bytes,
                Err(error) => {
                    failures.push(format!("{raw}: read backup: {error}"));
                    continue;
                }
            };
            if let Err(error) = ensure_parents(&dir, &relative, raw, &mut Vec::new())
                .and_then(|()| write_guarded(&dir, &relative, &bytes, raw))
            {
                failures.push(format!("{raw}: {error}"));
            }
        } else {
            match dir.remove_file(&relative) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => failures.push(format!("{raw}: {error}")),
            }
        }
    }
    failures
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub struct CheckpointRecorder {
    store: CheckpointStore,
    checkpoint: Checkpoint,
}

impl CheckpointRecorder {
    /// 第一次触碰某个路径时记录 turn 开始前的内容。
    ///
    /// 快照必须按 workspace 内的真实路径读取：先逐段拒绝符号链接祖先，再用
    /// cap-std `FollowSymlinks::No` 打开。否则一个 `link -> ~/.ssh` 的父目录就能
    /// 把 workspace 外的文件内容读进 checkpoint（agent 可读）。
    pub fn snapshot(&mut self, root: &Path, raw: &str) -> Result<(), AgentError> {
        if self.checkpoint.files.contains_key(raw) {
            return Ok(());
        }
        let Some(relative) = normalize_relative(Path::new(raw)) else {
            return Ok(());
        };
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority()).map_err(|error| {
            AgentError::Io(format!("checkpoint root {}: {error}", root.display()))
        })?;
        check_ancestors(&dir, &relative, raw)?;
        let metadata = match dir.symlink_metadata(&relative) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.checkpoint.files.insert(raw.to_owned(), None);
                return Ok(());
            }
            Err(error) => {
                return Err(AgentError::Io(format!("checkpoint stat {raw}: {error}")));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(());
        }
        if metadata.len() > MAX_CHECKPOINT_FILE_BYTES {
            return Err(AgentError::Tool(format!(
                "checkpoint {raw}: file is {} bytes, over the {} byte limit",
                metadata.len(),
                MAX_CHECKPOINT_FILE_BYTES
            )));
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = dir
            .open_with(&relative, &options)
            .map_err(|error| AgentError::Io(format!("checkpoint read {raw}: {error}")))?;
        let mut bytes = Vec::new();
        file.take(MAX_CHECKPOINT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| AgentError::Io(format!("checkpoint read {raw}: {error}")))?;
        if bytes.len() as u64 > MAX_CHECKPOINT_FILE_BYTES {
            return Err(AgentError::Tool(format!(
                "checkpoint {raw}: file grew over the {MAX_CHECKPOINT_FILE_BYTES} byte limit"
            )));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| AgentError::Io(format!("checkpoint read {raw}: not UTF-8")))?;
        self.checkpoint.files.insert(raw.to_owned(), Some(content));
        Ok(())
    }

    pub fn record_command(&mut self, command: &str) {
        if !self
            .checkpoint
            .commands
            .iter()
            .any(|existing| existing == command)
        {
            self.checkpoint.commands.push(command.to_owned());
        }
    }

    pub fn finish(self) -> Result<Checkpoint, AgentError> {
        self.store.save(&self.checkpoint)?;
        Ok(self.checkpoint)
    }
}

/// 给写工具套上 checkpoint：turn 开始建记录，写入前快照，turn 结束落盘。
pub struct CheckpointingTools<T> {
    inner: T,
    root: PathBuf,
    store: CheckpointStore,
    recorder: Option<CheckpointRecorder>,
    retain: Option<usize>,
}

impl<T: ToolExecutor> CheckpointingTools<T> {
    pub fn new(store: CheckpointStore, root: impl Into<PathBuf>, inner: T) -> Self {
        Self {
            inner,
            root: root.into(),
            store,
            recorder: None,
            retain: None,
        }
    }

    pub fn with_retain(mut self, keep_last: usize) -> Self {
        self.retain = Some(keep_last.max(1));
        self
    }

    pub fn store(&self) -> &CheckpointStore {
        &self.store
    }
}

impl<T: ToolExecutor> ToolExecutor for CheckpointingTools<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if let Some(recorder) = self.recorder.as_mut() {
            if request.tool == "bash"
                && let Some(command) = request.command.as_deref()
            {
                recorder.record_command(command);
            }
            // shell 的 read_only 不可信：只要它可能写，就必须先拍快照。
            if !request.read_only || request.tool == "bash" {
                for path in &request.paths {
                    recorder.snapshot(&self.root, path)?;
                }
            }
        }
        self.inner.execute(request)
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.recorder = Some(self.store.begin(turn, objective));
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        if let Some(recorder) = self.recorder.take() {
            recorder.finish()?;
        }
        if let Some(retain) = self.retain {
            self.store.prune(retain)?;
        }
        self.inner.end_turn(complete)
    }
}

/// 回退会话与代码；turn 必须是已存在的 turn（0-based）。
pub fn rewind_session(
    session_path: &Path,
    checkpoint_dir: &Path,
    root: &Path,
    turn: usize,
    scope: RewindScope,
) -> Result<RewindReport, AgentError> {
    rewind_session_with_policy(session_path, checkpoint_dir, root, turn, scope, false)
}

/// `allow_unrewindable` 为 false 时，遇到 shell 命令会 fail-closed。
pub fn rewind_session_with_policy(
    session_path: &Path,
    checkpoint_dir: &Path,
    root: &Path,
    turn: usize,
    scope: RewindScope,
    allow_unrewindable: bool,
) -> Result<RewindReport, AgentError> {
    let mut log = SessionLog::open(session_path)?;
    let records = log.load()?;
    let turns = records
        .iter()
        .filter(|record| matches!(record.event, crate::SessionEvent::TurnStarted { .. }))
        .count();
    if turn >= turns {
        return Err(AgentError::SessionState(format!(
            "rewind turn {turn} out of range (session has {turns} turns)"
        )));
    }
    let log_commands = commands_from_session(&records, turn);
    let mut report = RewindReport {
        turn,
        ..RewindReport::default()
    };
    if scope.includes_code() {
        let store = CheckpointStore::open(checkpoint_dir)?;
        let mut commands = store.commands_since(turn)?;
        merge_commands(&mut commands, log_commands);
        if !allow_unrewindable && !commands.is_empty() {
            return Err(AgentError::Unrewindable(commands));
        }
        report = store.rewind_code(root, turn)?;
        report.turn = turn;
        report.unrewindable = commands;
    }
    if scope.includes_conversation() {
        log.truncate_before_turn(turn)?;
        report.conversation_truncated = true;
    }
    Ok(report)
}

pub(crate) fn commands_from_session(records: &[SessionRecord], turn: usize) -> Vec<String> {
    let mut commands = Vec::new();
    let mut index = 0usize;
    for record in records {
        match &record.event {
            SessionEvent::TurnStarted { .. } => index += 1,
            SessionEvent::ToolRequested { request, .. }
                if index > turn && request.tool == "bash" =>
            {
                if let Some(command) = request.command.as_deref() {
                    merge_commands(&mut commands, [command.to_owned()]);
                }
            }
            _ => {}
        }
    }
    commands
}

pub(crate) fn merge_commands(into: &mut Vec<String>, commands: impl IntoIterator<Item = String>) {
    for command in commands {
        if !into.contains(&command) {
            into.push(command);
        }
    }
}

struct RewindEntry {
    raw: String,
    content: Option<Vec<u8>>,
}

fn capture_current(dir: &Dir, relative: &Path, raw: &str) -> Result<Option<Vec<u8>>, AgentError> {
    check_ancestors(dir, relative, raw)?;
    let metadata = match dir.symlink_metadata(relative) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AgentError::Io(format!("rewind stat {raw}: {error}"))),
    };
    if metadata.file_type().is_symlink() {
        return Err(AgentError::Tool(format!("rewind refuses symlink: {raw}")));
    }
    if !metadata.is_file() {
        return Err(AgentError::Tool(format!(
            "rewind refuses non-regular file: {raw}"
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = dir
        .open_with(relative, &options)
        .map_err(|error| AgentError::Io(format!("rewind read {raw}: {error}")))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| AgentError::Io(format!("rewind read {raw}: {error}")))?;
    Ok(Some(bytes))
}

pub(crate) fn check_ancestors(dir: &Dir, relative: &Path, raw: &str) -> Result<(), AgentError> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component);
        match dir.symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::Tool(format!("refuses symlink in path: {raw}")));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(AgentError::Tool(format!(
                    "refuses non-directory parent: {raw}"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(AgentError::Io(format!("rewind stat parent {raw}: {error}")));
            }
        }
    }
    Ok(())
}

fn ensure_parents(
    dir: &Dir,
    relative: &Path,
    raw: &str,
    created: &mut Vec<PathBuf>,
) -> Result<(), AgentError> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component);
        match dir.symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::Tool(format!("refuses symlink in path: {raw}")));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(AgentError::Tool(format!(
                    "refuses non-directory parent: {raw}"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                dir.create_dir(&current)
                    .map_err(|error| AgentError::Io(format!("rewind mkdir {raw}: {error}")))?;
                created.push(current.clone());
            }
            Err(error) => {
                return Err(AgentError::Io(format!("rewind stat parent {raw}: {error}")));
            }
        }
    }
    Ok(())
}

fn write_guarded(dir: &Dir, relative: &Path, bytes: &[u8], raw: &str) -> Result<(), AgentError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(true)
        .follow(FollowSymlinks::No);
    let mut file = dir
        .open_with(relative, &options)
        .map_err(|error| AgentError::Io(format!("rewind write {raw}: {error}")))?;
    file.write_all(bytes)
        .map_err(|error| AgentError::Io(format!("rewind write {raw}: {error}")))
}

fn rollback(
    dir: &Dir,
    entries: &[(PathBuf, RewindEntry)],
    previous: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    attempted: &[PathBuf],
    created_dirs: &[PathBuf],
) -> Vec<String> {
    let mut failures = Vec::new();
    for relative in attempted.iter().rev() {
        let Some((_, entry)) = entries.iter().find(|(path, _)| path == relative) else {
            failures.push(format!("rollback missing entry: {}", relative.display()));
            continue;
        };
        match previous.get(relative) {
            Some(Some(bytes)) => {
                if let Err(error) = ensure_parents(dir, relative, &entry.raw, &mut Vec::new())
                    .and_then(|()| write_guarded(dir, relative, bytes, &entry.raw))
                {
                    failures.push(format!("{}: {error}", entry.raw));
                }
            }
            Some(None) => match dir.remove_file(relative) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => failures.push(format!("{}: {error}", entry.raw)),
            },
            None => failures.push(format!("rollback missing snapshot: {}", entry.raw)),
        }
    }
    for relative in created_dirs.iter().rev() {
        let _ = dir.remove_dir(relative);
    }
    failures
}

pub(crate) fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trit-ckpt-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// 造一条 turn 0 的 checkpoint：a.txt 在 turn 开始时是 `before`。
    fn store_with_checkpoint(dir: &Path, root: &Path, before: &str) -> CheckpointStore {
        let store = CheckpointStore::open(dir).unwrap();
        std::fs::write(root.join("a.txt"), before).unwrap();
        let mut recorder = store.begin(0, "t");
        recorder.snapshot(root, "a.txt").unwrap();
        recorder.finish().unwrap();
        store
    }

    #[test]
    fn rewind_code_journals_begin_apply_and_finish() {
        let root = temp_root("journal-ok");
        let store_dir = temp_root("journal-ok-store");
        let store = store_with_checkpoint(&store_dir, &root, "before");
        std::fs::write(root.join("a.txt"), "after").unwrap();

        let report = store.rewind_code(&root, 0).unwrap();
        assert_eq!(report.restored, ["a.txt"]);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "before"
        );

        let status = store.rewind_status().unwrap();
        assert_eq!(status.completed, 1);
        assert!(!status.has_in_doubt(), "{status:?}");
        let journal = std::fs::read_to_string(store_dir.join(REWIND_JOURNAL)).unwrap();
        let phases: Vec<RewindRecord> = journal
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            phases.iter().map(|record| record.phase).collect::<Vec<_>>(),
            [
                RewindPhase::Begin,
                RewindPhase::Applying,
                RewindPhase::Finish
            ]
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn unfinished_rewind_is_in_doubt_and_blocks_the_next_one() {
        let root = temp_root("journal-block");
        let store_dir = temp_root("journal-block-store");
        let store = store_with_checkpoint(&store_dir, &root, "before");
        std::fs::write(root.join("a.txt"), "after").unwrap();

        // 模拟崩溃：begin + applying 已落盘，但没有 finish/rollback。
        let mut journal = RewindJournal::open(&store_dir).unwrap();
        let op_id = journal
            .begin(0, &[("a.txt".to_owned(), Some(b"after".to_vec()))])
            .unwrap();
        journal.applying(op_id, 0, "a.txt").unwrap();

        let status = store.rewind_status().unwrap();
        assert!(status.has_in_doubt(), "{status:?}");
        assert_eq!(status.in_doubt[0].planned, ["a.txt"]);
        assert_eq!(status.in_doubt[0].touched_paths(), ["a.txt"]);

        let error = store.rewind_code(&root, 0).unwrap_err();
        match error {
            AgentError::SessionState(message) => {
                assert!(message.contains("unfinished"), "{message}")
            }
            other => panic!("expected SessionState, got {other:?}"),
        }
        // 被拦截的回退不能改动工作区。
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "after"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn crash_after_first_path_reports_only_touched_paths() {
        let root = temp_root("journal-matrix");
        let store_dir = temp_root("journal-matrix-store");
        let _ = store_with_checkpoint(&store_dir, &root, "before");

        let mut journal = RewindJournal::open(&store_dir).unwrap();
        // 计划两条，只 applied 第一条：崩溃点矩阵的第一格。
        let op_id = journal
            .begin(
                0,
                &[
                    ("a.txt".to_owned(), Some(b"before".to_vec())),
                    ("b.txt".to_owned(), None),
                ],
            )
            .unwrap();
        journal.applying(op_id, 0, "a.txt").unwrap();

        let status = journal.report().unwrap();
        assert_eq!(status.in_doubt.len(), 1);
        assert_eq!(status.in_doubt[0].planned.len(), 2);
        assert_eq!(status.in_doubt[0].touched_paths(), ["a.txt"]);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn recover_rewind_restores_backups_and_removes_new_files() {
        let root = temp_root("journal-recover");
        let store_dir = temp_root("journal-recover-store");
        std::fs::write(root.join("a.txt"), "half-a").unwrap();
        std::fs::write(root.join("b.txt"), "half-b").unwrap();

        let mut journal = RewindJournal::open(&store_dir).unwrap();
        // 崩溃前：a 原有 "before-a"，b 原本不存在；两个路径都已进入 applying。
        let op_id = journal
            .begin(
                0,
                &[
                    ("a.txt".to_owned(), Some(b"before-a".to_vec())),
                    ("b.txt".to_owned(), None),
                ],
            )
            .unwrap();
        journal.applying(op_id, 0, "a.txt").unwrap();
        journal.applying(op_id, 0, "b.txt").unwrap();

        let store = CheckpointStore::open(&store_dir).unwrap();
        let recovery = store.recover_rewind(&root).unwrap();
        assert_eq!(recovery.attempted, 1);
        assert_eq!(recovery.recovered, 1);
        assert!(!recovery.has_failures(), "{recovery:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "before-a"
        );
        assert!(!root.join("b.txt").exists());

        let status = store.rewind_status().unwrap();
        assert!(!status.has_in_doubt(), "{status:?}");
        assert_eq!(status.recovered, 1);
        assert!(
            !store_dir
                .join(REWIND_BACKUP_DIR)
                .join(op_id.to_string())
                .exists()
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn recover_failure_keeps_operation_in_doubt() {
        let root = temp_root("journal-recover-fail");
        let store_dir = temp_root("journal-recover-fail-store");
        std::fs::write(root.join("a.txt"), "half-a").unwrap();

        let mut journal = RewindJournal::open(&store_dir).unwrap();
        let op_id = journal
            .begin(0, &[("a.txt".to_owned(), Some(b"before-a".to_vec()))])
            .unwrap();
        journal.applying(op_id, 0, "a.txt").unwrap();
        // 备份文件被外部清掉：恢复必须失败并保持 in_doubt，而不是假装好了。
        std::fs::remove_dir_all(store_dir.join(REWIND_BACKUP_DIR)).unwrap();

        let store = CheckpointStore::open(&store_dir).unwrap();
        let recovery = store.recover_rewind(&root).unwrap();
        assert_eq!(recovery.recovered, 0);
        assert!(recovery.has_failures(), "{recovery:?}");
        let status = store.rewind_status().unwrap();
        assert!(status.has_in_doubt(), "{status:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "half-a"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn rollback_restores_previous_contents_and_removes_new_files() {
        let root = std::env::temp_dir().join(format!(
            "trit-ckpt-rollback-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "applied-a").unwrap();
        std::fs::write(root.join("b.txt"), "applied-b").unwrap();
        std::fs::write(root.join("c.txt"), "applied-c").unwrap();
        let dir = Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();

        let entries = vec![
            (
                PathBuf::from("a.txt"),
                RewindEntry {
                    raw: "a.txt".into(),
                    content: Some(b"target-a".to_vec()),
                },
            ),
            (
                PathBuf::from("b.txt"),
                RewindEntry {
                    raw: "b.txt".into(),
                    content: Some(b"target-b".to_vec()),
                },
            ),
            (
                PathBuf::from("c.txt"),
                RewindEntry {
                    raw: "c.txt".into(),
                    content: Some(b"target-c".to_vec()),
                },
            ),
        ];
        let previous = BTreeMap::from([
            (PathBuf::from("a.txt"), Some(b"before-a".to_vec())),
            (PathBuf::from("b.txt"), Some(b"before-b".to_vec())),
            (PathBuf::from("c.txt"), None),
        ]);
        let attempted = vec![
            PathBuf::from("a.txt"),
            PathBuf::from("b.txt"),
            PathBuf::from("c.txt"),
        ];

        let failures = rollback(&dir, &entries, &previous, &attempted, &[]);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "before-a"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "before-b"
        );
        assert!(!root.join("c.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
