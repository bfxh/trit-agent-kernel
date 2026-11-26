//! Git 影子仓库检查点。
//!
//! 每个 turn 开始时把工作区（非 .gitignore 路径）写成一个独立快照，
//! 快照只存在于 `--checkpoint-dir` 指定的裸仓库里，不碰用户仓库。
//! 回退用 `read-tree --reset -u` 恢复目标快照，并用当前索引对比得出报告。
//!
//! 边界：被 .gitignore 忽略的文件、嵌套仓库内容、工作区外的路径以及
//! shell 的非文件副作用都不在快照内；这些命令仍会被审计并默认 fail-closed。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::checkpoint::{
    RewindJournal, RewindJournalReport, RewindRecoveryFailure, RewindRecoveryReport, RewindReport,
    RewindScope, commands_from_session, merge_commands, normalize_relative,
};
use crate::{AgentError, SessionEvent, SessionLog, ToolExecutor, ToolRequest};

static SIDE_EFFECT_SEQ: AtomicU64 = AtomicU64::new(0);

const MARKER: &str = "trit-checkpoint.json";
const MARKER_SCHEMA: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct Marker {
    schema: u32,
}

struct TreeEntry {
    mode: String,
    kind: String,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCheckpointStore {
    dir: PathBuf,
    root: PathBuf,
}

impl GitCheckpointStore {
    pub fn open(dir: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let root = std::fs::canonicalize(root.into())
            .map_err(|error| AgentError::Io(format!("workspace root: {error}")))?;
        if !root.is_dir() {
            return Err(AgentError::Tool("workspace root is not a directory".into()));
        }
        let dir = dir.into();
        if std::fs::symlink_metadata(&dir)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            return Err(AgentError::Io(format!(
                "checkpoint dir is not a directory: {}",
                dir.display()
            )));
        }
        std::fs::create_dir_all(&dir).map_err(|error| {
            AgentError::Io(format!("checkpoint dir {}: {error}", dir.display()))
        })?;
        let dir = std::fs::canonicalize(&dir).map_err(|error| {
            AgentError::Io(format!("checkpoint dir {}: {error}", dir.display()))
        })?;
        if dir == root {
            return Err(AgentError::Tool(
                "checkpoint dir must not be the workspace root".into(),
            ));
        }
        if dir.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git")
        }) {
            return Err(AgentError::Tool(
                "checkpoint dir must not be inside a .git directory".into(),
            ));
        }
        let marker = dir.join(MARKER);
        if marker.exists() {
            let text = std::fs::read_to_string(&marker)
                .map_err(|error| AgentError::Io(format!("checkpoint marker: {error}")))?;
            let parsed: Marker = serde_json::from_str(&text)
                .map_err(|error| AgentError::SessionState(format!("checkpoint marker: {error}")))?;
            if parsed.schema != MARKER_SCHEMA {
                return Err(AgentError::SessionState(format!(
                    "checkpoint marker schema {} (expected {MARKER_SCHEMA})",
                    parsed.schema
                )));
            }
        } else {
            let mut entries = std::fs::read_dir(&dir).map_err(|error| {
                AgentError::Io(format!("checkpoint dir {}: {error}", dir.display()))
            })?;
            if entries.next().is_some() {
                return Err(AgentError::Tool(format!(
                    "checkpoint dir is not empty and is not a trit checkpoint: {}",
                    dir.display()
                )));
            }
        }
        let store = Self { dir, root };
        if !store.dir.join("HEAD").exists() {
            let output = Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(command_path(&store.dir))
                .output()
                .map_err(|error| AgentError::Io(format!("git init: {error}")))?;
            if !output.status.success() {
                return Err(AgentError::Io(format!(
                    "git init {}: {}",
                    store.dir.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        store.git(&["rev-parse", "--git-dir"])?;
        store.git(&["config", "user.name", "trit"])?;
        store.git(&["config", "user.email", "trit@localhost"])?;
        store.git(&["config", "core.autocrlf", "false"])?;
        store.git(&["config", "core.safecrlf", "false"])?;
        store.git(&["config", "commit.gpgsign", "false"])?;
        let hooks = command_path(&store.dir.join("hooks"));
        store.git(&[
            "config",
            "core.hooksPath",
            hooks
                .to_str()
                .ok_or_else(|| AgentError::Io("checkpoint dir is not utf-8".into()))?,
        ])?;
        store.write_exclude()?;
        if !marker.exists() {
            let text = serde_json::to_string(&Marker {
                schema: MARKER_SCHEMA,
            })
            .map_err(|error| AgentError::SessionState(format!("checkpoint marker: {error}")))?;
            write_atomic(&marker, text.as_bytes())?;
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把当前工作区提交为一个无父提交，并打上 turn ref。
    pub fn snapshot(&self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.git(&["add", "-A"]).map_err(|error| {
            AgentError::Tool(format!(
                "git checkpoint could not snapshot the workspace (nested repositories are unsupported): {error}"
            ))
        })?;
        let tree = self.git(&["write-tree"])?;
        let validation = self
            .tree_entries(&tree)
            .and_then(|entries| self.validate_tree(&entries));
        if let Err(error) = validation {
            let _ = self.git(&["read-tree", "--empty"]);
            return Err(error);
        }
        let message = format!("trit checkpoint turn {turn}: {objective}");
        let commit = self.git(&["commit-tree", &tree, "-m", &message])?;
        self.git(&["update-ref", &turn_ref(turn), &commit])?;
        Ok(())
    }

    /// 恢复 `turn` 开始时的工作区；目标不存在时 fail-closed。
    ///
    /// 先校验目标树与当前树，再暂存当前树作为回滚点；`read-tree` 失败时
    /// 用回滚点恢复，避免工作区停在半恢复状态。
    pub fn rewind(&self, turn: usize) -> Result<RewindReport, AgentError> {
        let mut journal = RewindJournal::open(&self.dir)?;
        let status = journal.report()?;
        if status.has_in_doubt() {
            return Err(AgentError::SessionState(format!(
                "rewind journal has {} unfinished operation(s); inspect rewind-status before retrying",
                status.in_doubt.len()
            )));
        }
        let turns = self.turns()?;
        if !turns.contains(&turn) {
            return Err(AgentError::SessionState(format!(
                "checkpoint for turn {turn} is missing"
            )));
        }
        let target = self.git(&["rev-parse", &turn_ref(turn)])?;
        let target_entries = self.tree_entries(&target)?;
        self.validate_tree(&target_entries)?;
        // 把当前工作区（包括 shell 新建但尚未提交的文件）放进索引，
        // 这样 read-tree 会删除目标快照中不存在的路径。
        self.git(&["add", "-A"])?;
        let current_tree = self.git(&["write-tree"])?;
        let current_entries = self.tree_entries(&current_tree)?;
        self.validate_tree(&current_entries)?;
        self.reject_ignored_collisions(&target_entries)?;
        self.reject_directory_collisions(&target_entries)?;
        let backup = self.git(&["commit-tree", &current_tree, "-m", "trit rewind backup"])?;
        let diff = self.git(&["diff", "--cached", "--name-status", "--no-renames", &target])?;
        let mut report = RewindReport {
            turn,
            ..RewindReport::default()
        };
        for line in diff.lines() {
            let mut parts = line.splitn(2, '\t');
            let status = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            match status.chars().next() {
                Some('A') => report.deleted.push(path.to_owned()),
                Some('D' | 'M' | 'T') => report.restored.push(path.to_owned()),
                _ => {}
            }
        }
        // 工作区即将被 read-tree 改写：整树 backup commit 先落 journal。
        let op_id = journal.begin_git(turn, &backup)?;
        journal.applying(op_id, turn, "")?;
        if let Err(original) = self.git(&["read-tree", "--reset", "-u", &target]) {
            let failures = match self.git(&["read-tree", "--reset", "-u", &backup]) {
                Ok(_) => Vec::new(),
                Err(rollback) => vec![format!("git rollback: {rollback}")],
            };
            journal.rollback(op_id, turn, &failures)?;
            if failures.is_empty() {
                return Err(original);
            }
            return Err(AgentError::RewindRollback {
                original: original.to_string(),
                failures,
            });
        }
        journal.finish(op_id, turn, &report.restored, &report.deleted)?;
        Ok(report)
    }

    /// 只读检查 git 后端回退 journal。
    pub fn rewind_status(&self) -> Result<RewindJournalReport, AgentError> {
        RewindJournal::open(&self.dir)?.report()
    }

    /// 用 `begin` 记录的整树 backup commit 恢复半写工作区。
    ///
    /// 只接受 git 后端的记录；文件后端应改用 `CheckpointStore::recover_rewind`。
    pub fn recover_rewind(&self) -> Result<RewindRecoveryReport, AgentError> {
        let mut journal = RewindJournal::open(&self.dir)?;
        let status = journal.report()?;
        if status.in_doubt.iter().any(|doubt| doubt.backend != "git") {
            return Err(AgentError::SessionState(
                "rewind journal contains files-backend operation(s); use the files checkpoint store"
                    .into(),
            ));
        }
        let mut report = RewindRecoveryReport::default();
        for doubt in status.in_doubt {
            report.attempted += 1;
            let failures = if doubt.backup.is_empty() {
                vec![format!("op {}: missing git backup commit", doubt.op_id)]
            } else {
                match self.git(&["read-tree", "--reset", "-u", &doubt.backup]) {
                    Ok(_) => Vec::new(),
                    Err(error) => vec![format!("git recover: {error}")],
                }
            };
            journal.recover(doubt.op_id, doubt.turn, "git", &doubt.backup, &failures)?;
            if failures.is_empty() {
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

    fn tree_entries(&self, treeish: &str) -> Result<Vec<TreeEntry>, AgentError> {
        let output = self.git(&["ls-tree", "-r", "-z", treeish])?;
        let mut entries = Vec::new();
        for record in output.split('\0') {
            if record.is_empty() {
                continue;
            }
            let (meta, path) = record.split_once('\t').ok_or_else(|| {
                AgentError::SessionState(format!("unexpected git tree entry: {record:?}"))
            })?;
            let mut fields = meta.split(' ');
            let mode = fields.next().unwrap_or_default();
            let kind = fields.next().unwrap_or_default();
            entries.push(TreeEntry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                path: path.to_owned(),
            });
        }
        Ok(entries)
    }

    fn validate_tree(&self, entries: &[TreeEntry]) -> Result<(), AgentError> {
        let symlinks = self.config_bool("core.symlinks")?.unwrap_or(cfg!(unix));
        #[cfg(windows)]
        let mut seen = BTreeSet::new();
        for entry in entries {
            let Some(relative) = normalize_relative(Path::new(&entry.path)) else {
                return Err(AgentError::Tool(format!(
                    "git checkpoint refuses unsafe path: {}",
                    entry.path
                )));
            };
            if relative != Path::new(&entry.path)
                || relative.components().any(|component| {
                    component
                        .as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(".git")
                })
            {
                return Err(AgentError::Tool(format!(
                    "git checkpoint refuses unsafe path: {}",
                    entry.path
                )));
            }
            match entry.mode.as_str() {
                "100644" | "100755" => {}
                "120000" if symlinks => {}
                "120000" => {
                    return Err(AgentError::Tool(format!(
                        "git checkpoint cannot restore symlinks with core.symlinks=false: {}",
                        entry.path
                    )));
                }
                "160000" => {
                    return Err(AgentError::Tool(format!(
                        "git checkpoint does not support nested repositories or submodules: {}",
                        entry.path
                    )));
                }
                mode => {
                    return Err(AgentError::Tool(format!(
                        "git checkpoint does not support tree entry mode {mode}: {}",
                        entry.path
                    )));
                }
            }
            if entry.kind != "blob" {
                return Err(AgentError::Tool(format!(
                    "git checkpoint does not support tree entry type {}: {}",
                    entry.kind, entry.path
                )));
            }
            #[cfg(windows)]
            {
                let key = relative.to_string_lossy().to_lowercase();
                if !seen.insert(key) {
                    return Err(AgentError::Tool(format!(
                        "git checkpoint refuses case-insensitive path collision: {}",
                        entry.path
                    )));
                }
            }
        }
        Ok(())
    }

    fn reject_ignored_collisions(&self, target: &[TreeEntry]) -> Result<(), AgentError> {
        let ignored = self.git(&[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])?;
        let target_paths: BTreeSet<&str> = target.iter().map(|entry| entry.path.as_str()).collect();
        let mut collisions: Vec<&str> = ignored
            .split('\0')
            .filter(|path| !path.is_empty() && target_paths.contains(path))
            .collect();
        collisions.sort_unstable();
        collisions.dedup();
        if !collisions.is_empty() {
            collisions.truncate(5);
            return Err(AgentError::Tool(format!(
                "git checkpoint refuses to restore paths that are currently ignored: {}",
                collisions.join(", ")
            )));
        }
        Ok(())
    }

    fn reject_directory_collisions(&self, target: &[TreeEntry]) -> Result<(), AgentError> {
        for entry in target {
            let full = self.root.join(Path::new(&entry.path));
            if std::fs::symlink_metadata(&full).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(AgentError::Tool(format!(
                    "git checkpoint refuses to replace a directory with a file: {}",
                    entry.path
                )));
            }
        }
        Ok(())
    }

    pub fn turns(&self) -> Result<Vec<usize>, AgentError> {
        let output = self.git(&["for-each-ref", "--format=%(refname)", "refs/trit/"])?;
        let mut turns = Vec::new();
        for reference in output.lines() {
            let Some(raw) = reference.strip_prefix("refs/trit/turn-") else {
                continue;
            };
            if let Ok(turn) = raw.parse::<usize>() {
                turns.push(turn);
            }
        }
        turns.sort_unstable();
        turns.dedup();
        Ok(turns)
    }

    pub fn record_commands(&self, turn: usize, commands: &[String]) -> Result<(), AgentError> {
        let path = self.commands_path(turn);
        if commands.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(AgentError::Io(format!(
                        "command audit {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        #[derive(Serialize)]
        struct Commands<'a> {
            turn: usize,
            commands: &'a [String],
        }
        let text = serde_json::to_string(&Commands { turn, commands })
            .map_err(|error| AgentError::SessionState(format!("command audit encode: {error}")))?;
        write_atomic(&self.commands_path(turn), text.as_bytes())
    }

    pub fn commands_since(&self, turn: usize) -> Result<Vec<String>, AgentError> {
        let turns = self.turns()?;
        if !turns.contains(&turn) {
            return Err(AgentError::SessionState(format!(
                "checkpoint for turn {turn} is missing"
            )));
        }
        let mut commands = Vec::new();
        for turn in turns.into_iter().filter(|candidate| *candidate >= turn) {
            let path = self.commands_path(turn);
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(AgentError::Io(format!(
                        "command audit {}: {error}",
                        path.display()
                    )));
                }
            };
            #[derive(Deserialize)]
            struct Commands {
                #[serde(default)]
                commands: Vec<String>,
            }
            let audit: Commands = serde_json::from_str(&text).map_err(|error| {
                AgentError::SessionState(format!("command audit {}: {error}", path.display()))
            })?;
            for command in audit.commands {
                if !commands.contains(&command) {
                    commands.push(command);
                }
            }
        }
        Ok(commands)
    }

    /// 只保留最近 `keep_last` 个 turn 的快照与命令审计。
    pub fn prune(&self, keep_last: usize) -> Result<usize, AgentError> {
        let turns = self.turns()?;
        let keep_last = keep_last.max(1);
        if turns.len() <= keep_last {
            return Ok(0);
        }
        let remove = turns.len() - keep_last;
        for turn in turns.iter().take(remove) {
            self.git(&["update-ref", "-d", &turn_ref(*turn)])?;
            let path = self.commands_path(*turn);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(AgentError::Io(format!("prune {}: {error}", path.display())));
                }
            }
        }
        self.git(&["gc", "--prune=now", "--quiet"])?;
        Ok(remove)
    }

    fn write_exclude(&self) -> Result<(), AgentError> {
        let Ok(relative) = self.dir.strip_prefix(&self.root) else {
            return Ok(());
        };
        let pattern = format!("/{}\n", relative.to_string_lossy().replace('\\', "/"));
        let info = self.dir.join("info");
        std::fs::create_dir_all(&info)
            .map_err(|error| AgentError::Io(format!("checkpoint info: {error}")))?;
        let path = info.join("exclude");
        let existing = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(AgentError::Io(format!("checkpoint exclude: {error}")));
            }
        };
        if existing.lines().any(|line| line == pattern.trim_end()) {
            return Ok(());
        }
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&pattern);
        write_atomic(&path, text.as_bytes())
    }

    fn commands_path(&self, turn: usize) -> PathBuf {
        self.dir.join(format!("commands-{turn:08}.json"))
    }

    fn config_bool(&self, key: &str) -> Result<Option<bool>, AgentError> {
        let output = self.git_output(&["config", "--bool", key])?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim() == "true",
            ));
        }
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        Err(AgentError::Io(format!(
            "git config {key}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    fn git_output(&self, args: &[&str]) -> Result<std::process::Output, AgentError> {
        Command::new("git")
            .arg("--git-dir")
            .arg(command_path(&self.dir))
            .arg("--work-tree")
            .arg(command_path(&self.root))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .args(args)
            .output()
            .map_err(|error| {
                AgentError::Io(format!(
                    "git {}: {error}",
                    args.first().copied().unwrap_or("")
                ))
            })
    }

    fn git(&self, args: &[&str]) -> Result<String, AgentError> {
        let output = self.git_output(args)?;
        if !output.status.success() {
            return Err(AgentError::Io(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    }
}

pub struct GitCheckpointingTools<T> {
    inner: T,
    store: GitCheckpointStore,
    turn: Option<usize>,
    commands: Vec<String>,
    retain: Option<usize>,
}

impl<T: ToolExecutor> GitCheckpointingTools<T> {
    pub fn new(store: GitCheckpointStore, inner: T) -> Self {
        Self {
            inner,
            store,
            turn: None,
            commands: Vec::new(),
            retain: None,
        }
    }

    pub fn with_retain(mut self, keep_last: usize) -> Self {
        self.retain = Some(keep_last.max(1));
        self
    }

    pub fn store(&self) -> &GitCheckpointStore {
        &self.store
    }
}

impl<T: ToolExecutor> ToolExecutor for GitCheckpointingTools<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<crate::ToolOutcome, AgentError> {
        if request.tool == "bash"
            && let Some(command) = request.command.as_deref()
            && !self.commands.iter().any(|existing| existing == command)
        {
            self.commands.push(command.to_owned());
        }
        self.inner.execute(request)
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.turn = Some(turn);
        self.commands.clear();
        self.store.snapshot(turn, objective)?;
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        if let Some(turn) = self.turn.take() {
            self.store.record_commands(turn, &self.commands)?;
        }
        if let Some(retain) = self.retain {
            self.store.prune(retain)?;
        }
        self.inner.end_turn(complete)
    }
}

/// 回退 git 影子仓库与会话；默认对 shell 命令 fail-closed。
pub fn rewind_session_git(
    session_path: &Path,
    checkpoint_dir: &Path,
    root: &Path,
    turn: usize,
    scope: RewindScope,
) -> Result<RewindReport, AgentError> {
    rewind_session_git_with_policy(session_path, checkpoint_dir, root, turn, scope, false)
}

pub fn rewind_session_git_with_policy(
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
        .filter(|record| matches!(record.event, SessionEvent::TurnStarted { .. }))
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
        let store = GitCheckpointStore::open(checkpoint_dir, root)?;
        let mut commands = store.commands_since(turn)?;
        merge_commands(&mut commands, log_commands);
        if !allow_unrewindable && !commands.is_empty() {
            return Err(AgentError::Unrewindable(commands));
        }
        report = store.rewind(turn)?;
        report.turn = turn;
        report.unrewindable = commands;
    }
    if scope.includes_conversation() {
        log.truncate_before_turn(turn)?;
        report.conversation_truncated = true;
    }
    Ok(report)
}

fn turn_ref(turn: usize) -> String {
    format!("refs/trit/turn-{turn:08}")
}

#[cfg(windows)]
fn command_path(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

#[cfg(not(windows))]
fn command_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    let seq = SIDE_EFFECT_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| AgentError::Io(format!("write {}: {error}", temporary.display())))?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&temporary, path)
                .map_err(|error| AgentError::Io(format!("rename {}: {error}", path.display())))
        }
    }
}
