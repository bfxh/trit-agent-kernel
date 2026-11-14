//! 文件检查点与回退。
//!
//! 每个 turn 一份 checkpoint，只记录写工具触碰到的文件在 turn 开始前的内容；
//! `rewind_code` 恢复最早快照（None 表示当时不存在，恢复时删除）。
//! shell 命令无法由文件快照回退，会在 checkpoint 中记录，并在回退时报告。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};

use crate::{
    AgentError, SessionEvent, SessionLog, SessionRecord, ToolExecutor, ToolOutcome, ToolRequest,
};

pub const CHECKPOINT_SCHEMA: u32 = 1;
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

        let mut created_dirs = Vec::new();
        let mut attempted = Vec::new();
        let applied = (|| -> Result<(), AgentError> {
            for (relative, entry) in &entries {
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
            if failures.is_empty() {
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

pub struct CheckpointRecorder {
    store: CheckpointStore,
    checkpoint: Checkpoint,
}

impl CheckpointRecorder {
    /// 第一次触碰某个路径时记录 turn 开始前的内容。
    pub fn snapshot(&mut self, root: &Path, raw: &str) -> Result<(), AgentError> {
        if self.checkpoint.files.contains_key(raw) {
            return Ok(());
        }
        let Some(relative) = normalize_relative(Path::new(raw)) else {
            return Ok(());
        };
        let full = root.join(&relative);
        let metadata = match std::fs::symlink_metadata(&full) {
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
        let content = std::fs::read_to_string(&full)
            .map_err(|error| AgentError::Io(format!("checkpoint read {raw}: {error}")))?;
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
            if !request.read_only {
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

fn check_ancestors(dir: &Dir, relative: &Path, raw: &str) -> Result<(), AgentError> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component);
        match dir.symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::Tool(format!(
                    "rewind refuses symlink in path: {raw}"
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(AgentError::Tool(format!(
                    "rewind refuses non-directory parent: {raw}"
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
                return Err(AgentError::Tool(format!(
                    "rewind refuses symlink in path: {raw}"
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(AgentError::Tool(format!(
                    "rewind refuses non-directory parent: {raw}"
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
