//! 会话扫描：只读目录，产出会话清单、任务与链接。
//!
//! 这是多会话调度的第一步：不启动模型、不写文件，只解析会话 JSONL 与同名 `.meta`，
//! 供上层建立会话链接、任务分配与恢复入口。扫描器容忍坏行与未知格式：
//! 坏行计入 `corrupt_lines`，未知格式仍保留路径/meta，不阻断整次扫描。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::session::{SessionEvent, SessionRecord};
use crate::session_migration::SessionRole;

pub const SESSION_SCAN_SCHEMA: u32 = 1;

/// 单文件读取上限；超过时只报 warning，不把整个文件读进内存。
const MAX_SESSION_BYTES: u64 = 32 * 1024 * 1024;
/// 扫描缓存上限；超过时忽略缓存并回退全量扫描。
const MAX_CACHE_BYTES: u64 = 8 * 1024 * 1024;
/// 会话目录递归深度；`projects/<name>/sessions` 通常两层。
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScanReport {
    pub schema: u32,
    pub root: String,
    pub scanned: usize,
    pub sessions: Vec<SessionEntry>,
    pub links: Vec<SessionLink>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub path: String,
    pub session_id: String,
    #[serde(default)]
    pub project: Option<String>,
    pub role: SessionRole,
    pub format: SessionFormat,
    #[serde(default)]
    pub meta: Option<SessionMeta>,
    pub bytes: u64,
    pub modified_ms: u64,
    pub records: usize,
    pub corrupt_lines: usize,
    pub tasks: Vec<SessionTask>,
    /// 最后一个 turn 未收到 `TurnFinished`。
    pub open_turn: bool,
    pub in_doubt: usize,
    #[serde(default)]
    pub last_gate: Option<SessionGate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTask {
    pub index: usize,
    pub task: String,
    pub objective: String,
    pub steps: usize,
    pub complete: bool,
    pub finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub role: Option<SessionRole>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub turns: Option<u64>,
    #[serde(default)]
    pub preview: Option<String>,
    pub finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGate {
    pub status: String,
    pub exit: i32,
    pub passed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFormat {
    /// 至少有一条可解析的 `TurnStarted`。
    Trit,
    /// Reasonix 的 `role/content` 消息格式；只读适配，不当内核 turn 续跑。
    Reasonix,
    /// 其他 JSONL，只保留路径/meta。
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// 同目录的 `planner.jsonl` 与 `executor.jsonl`。
    PlannerExecutor,
    /// `.meta` 声明的父会话。
    ParentChild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLink {
    pub kind: LinkKind,
    pub from: String,
    pub to: String,
}

/// 扫描缓存：按 size + mtime 复用未变化会话的 `SessionEntry`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScanCache {
    schema: u32,
    files: BTreeMap<String, CachedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEntry {
    size: u64,
    modified_ms: u64,
    entry: SessionEntry,
}

/// 扫描 `root` 下的会话文件；错误只降级为 warnings，不中断整次扫描。
pub fn scan_sessions(root: &Path) -> Result<SessionScanReport, AgentError> {
    scan_sessions_inner(root, None)
}

/// 带缓存扫描：未变化（size + mtime）的会话直接复用上次结果，只重扫变化文件。
pub fn scan_sessions_cached(
    root: &Path,
    cache_path: &Path,
) -> Result<SessionScanReport, AgentError> {
    scan_sessions_inner(root, Some(cache_path))
}

fn scan_sessions_inner(
    root: &Path,
    cache_path: Option<&Path>,
) -> Result<SessionScanReport, AgentError> {
    let mut files = Vec::new();
    collect_session_files(root, 0, &mut files)?;
    files.sort();

    let cache = cache_path
        .and_then(|path| {
            let metadata = fs::metadata(path).ok()?;
            if metadata.len() > MAX_CACHE_BYTES {
                return None;
            }
            fs::read_to_string(path).ok()
        })
        .and_then(|text| serde_json::from_str::<ScanCache>(&text).ok())
        .unwrap_or_default();
    let mut next = ScanCache {
        schema: SESSION_SCAN_SCHEMA,
        files: BTreeMap::new(),
    };
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    let mut dirty = false;
    for path in &files {
        let key = path.display().to_string();
        let metadata = fs::metadata(path).ok();
        let size = metadata.as_ref().map_or(0, |metadata| metadata.len());
        let modified_ms = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis() as u64);
        if let Some(cached) = cache.files.get(&key)
            && cached.size == size
            && cached.modified_ms == modified_ms
        {
            next.files.insert(key, cached.clone());
            sessions.push(cached.entry.clone());
            continue;
        }
        match scan_session(path) {
            Ok(entry) => {
                dirty = true;
                next.files.insert(
                    key,
                    CachedEntry {
                        size,
                        modified_ms,
                        entry: entry.clone(),
                    },
                );
                sessions.push(entry);
            }
            Err(error) => {
                dirty = true;
                warnings.push(format!("{}: {error}", path.display()));
            }
        }
    }
    if cache.files.len() != next.files.len() {
        dirty = true;
    }
    if let Some(path) = cache_path
        && dirty
        && let Ok(text) = serde_json::to_string(&next)
    {
        let _ = fs::write(path, text);
    }
    let links = build_links(&sessions);
    Ok(SessionScanReport {
        schema: SESSION_SCAN_SCHEMA,
        root: root.display().to_string(),
        scanned: files.len(),
        sessions,
        links,
        warnings,
    })
}

fn collect_session_files(
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<(), AgentError> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = fs::read_dir(dir)
        .map_err(|error| AgentError::Io(format!("scan {}: {error}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if matches!(name, "target" | "node_modules" | ".git")
                || name.ends_with(".outputs")
                || name.ends_with(".ckpt")
                || name.ends_with(".inbox")
            {
                continue;
            }
            collect_session_files(&path, depth + 1, out)?;
        } else if is_session_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_session_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name.ends_with(".jsonl") && !name.ends_with(".wire.jsonl") && !name.ends_with(".events.jsonl")
}

fn scan_session(path: &Path) -> Result<SessionEntry, AgentError> {
    let metadata = fs::metadata(path)
        .map_err(|error| AgentError::Io(format!("stat {}: {error}", path.display())))?;
    if metadata.len() > MAX_SESSION_BYTES {
        return Err(AgentError::SessionState(format!(
            "session too large: {} bytes",
            metadata.len()
        )));
    }
    let bytes = fs::read(path)
        .map_err(|error| AgentError::Io(format!("read {}: {error}", path.display())))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut records = Vec::new();
    let mut corrupt_lines = 0usize;
    let mut json_lines = 0usize;
    let mut reasonix_lines = 0usize;
    let mut reasonix_tasks = Vec::new();
    let mut current: Option<SessionTask> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        json_lines += 1;
        if let Ok(record) = serde_json::from_str::<SessionRecord>(line) {
            records.push(record);
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            corrupt_lines += 1;
            continue;
        };
        let Some(role) = value.get("role").and_then(serde_json::Value::as_str) else {
            continue;
        };
        reasonix_lines += 1;
        match role {
            "user" => {
                if let Some(task) = current.take() {
                    reasonix_tasks.push(task);
                }
                current = Some(SessionTask {
                    index: reasonix_tasks.len() + 1,
                    task: reasonix_task_text(&value),
                    objective: String::new(),
                    steps: 0,
                    complete: false,
                    finished: false,
                });
            }
            "assistant" | "tool" => {
                if let Some(task) = current.as_mut() {
                    task.steps += 1;
                    if role == "assistant" {
                        task.complete = true;
                        task.finished = true;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(task) = current.take() {
        reasonix_tasks.push(task);
    }
    let format = if records
        .iter()
        .any(|record| matches!(record.event, SessionEvent::TurnStarted { .. }))
    {
        SessionFormat::Trit
    } else if reasonix_lines > 0 {
        SessionFormat::Reasonix
    } else {
        SessionFormat::Unknown
    };
    let record_count = match format {
        SessionFormat::Trit => records.len(),
        SessionFormat::Reasonix => reasonix_lines,
        SessionFormat::Unknown => json_lines,
    };
    let meta = read_meta(path);
    let session_id = meta
        .as_ref()
        .and_then(|meta| meta.id.clone())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("session")
                .to_owned()
        });
    let role = meta
        .as_ref()
        .and_then(|meta| meta.role)
        .unwrap_or(SessionRole::Primary);
    let project = meta.as_ref().and_then(|meta| meta.project.clone());

    let (tasks, open_turn, in_doubt, last_gate) = match format {
        SessionFormat::Trit => {
            let mut tasks = Vec::new();
            let mut in_doubt = 0usize;
            let mut open_turn = false;
            let mut last_gate = None;
            for record in &records {
                match &record.event {
                    SessionEvent::TurnStarted {
                        task, objective, ..
                    } => {
                        tasks.push(SessionTask {
                            index: tasks.len() + 1,
                            task: task.clone(),
                            objective: objective.clone(),
                            steps: 0,
                            complete: false,
                            finished: false,
                        });
                        open_turn = true;
                    }
                    SessionEvent::TurnFinished {
                        steps, complete, ..
                    } => {
                        if let Some(task) = tasks.last_mut() {
                            task.steps = *steps;
                            task.complete = *complete;
                            task.finished = true;
                        }
                        open_turn = false;
                    }
                    SessionEvent::ToolCompleted { outcome, .. } if outcome.in_doubt => {
                        in_doubt += 1
                    }
                    SessionEvent::GateChain { status, exit, .. } => {
                        last_gate = Some(SessionGate {
                            status: status.clone(),
                            exit: *exit,
                            passed: *exit == 0 && matches!(status.as_str(), "passed" | "aligned"),
                        });
                    }
                    _ => {}
                }
            }
            (tasks, open_turn, in_doubt, last_gate)
        }
        SessionFormat::Reasonix => {
            let open_turn = reasonix_tasks.last().is_some_and(|task| !task.finished);
            (reasonix_tasks, open_turn, 0, None)
        }
        SessionFormat::Unknown => (Vec::new(), false, 0, None),
    };

    Ok(SessionEntry {
        path: path.display().to_string(),
        session_id,
        project,
        role,
        format,
        meta,
        bytes: metadata.len(),
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis() as u64),
        records: record_count,
        corrupt_lines,
        tasks,
        open_turn,
        in_doubt,
        last_gate,
    })
}

/// 从一条 `role/content` 消息提取任务摘要；只取前 200 个字符。
fn reasonix_task_text(value: &serde_json::Value) -> String {
    let text = value
        .get("raw_content")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("content").and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(200).collect()
}

/// 读取 `<session>.jsonl.meta`；字段缺失或非法时返回 None，不阻断会话扫描。
fn read_meta(path: &Path) -> Option<SessionMeta> {
    let mut meta_path = path.as_os_str().to_os_string();
    meta_path.push(".meta");
    let text = fs::read_to_string(meta_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let role = field("role").and_then(|role| match role.to_ascii_lowercase().as_str() {
        "subagent" | "sub" => Some(SessionRole::Subagent),
        "primary" | "main" => Some(SessionRole::Primary),
        _ => None,
    });
    Some(SessionMeta {
        id: field("id").or_else(|| field("session_id")),
        parent: field("parent").or_else(|| field("parent_id")),
        project: field("project").or_else(|| field("project_path")),
        role,
        model: field("model"),
        turns: value.get("turns").and_then(serde_json::Value::as_u64),
        preview: field("preview"),
        finished: value
            .get("finished_at")
            .map(|value| !value.is_null())
            .unwrap_or(false),
    })
}

fn build_links(sessions: &[SessionEntry]) -> Vec<SessionLink> {
    let mut links = Vec::new();

    // 同目录的 planner/executor 是同一轮 planner→executor 的两半。
    let mut planners: BTreeMap<PathBuf, &SessionEntry> = BTreeMap::new();
    let mut executors: BTreeMap<PathBuf, &SessionEntry> = BTreeMap::new();
    for entry in sessions {
        let path = Path::new(&entry.path);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        match path.file_name().and_then(|name| name.to_str()) {
            Some("planner.jsonl") => {
                planners.insert(dir, entry);
            }
            Some("executor.jsonl") => {
                executors.insert(dir, entry);
            }
            _ => {}
        }
    }
    for (dir, planner) in &planners {
        if let Some(executor) = executors.get(dir) {
            links.push(SessionLink {
                kind: LinkKind::PlannerExecutor,
                from: planner.session_id.clone(),
                to: executor.session_id.clone(),
            });
        }
    }

    // meta 声明的父会话；父会话可能尚未被扫描到，链接仍保留供上层解析。
    for entry in sessions {
        if let Some(parent) = entry.meta.as_ref().and_then(|meta| meta.parent.clone()) {
            links.push(SessionLink {
                kind: LinkKind::ParentChild,
                from: parent,
                to: entry.session_id.clone(),
            });
        }
    }

    links.sort_by(|left, right| {
        (left.kind as u8, &left.from, &left.to).cmp(&(right.kind as u8, &right.from, &right.to))
    });
    links.dedup();
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "trit-session-scan-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_session(path: &Path, events: &[SessionEvent]) {
        let mut log = crate::session::SessionLog::open(path).unwrap();
        for event in events {
            log.append(event.clone()).unwrap();
        }
    }

    #[test]
    fn scans_tasks_gates_and_links() {
        let root = temp("links");
        let planner = root.join("planner.jsonl");
        write_session(
            &planner,
            &[
                SessionEvent::TurnStarted {
                    task: "plan".into(),
                    objective: "split".into(),
                    max_steps: 2,
                },
                SessionEvent::TurnFinished {
                    steps: 1,
                    complete: true,
                    metrics: None,
                },
            ],
        );
        write_session(
            &root.join("executor.jsonl"),
            &[SessionEvent::TurnStarted {
                task: "exec".into(),
                objective: "run".into(),
                max_steps: 2,
            }],
        );

        let report = scan_sessions(&root).unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.sessions.len(), 2);
        assert!(
            report
                .links
                .iter()
                .any(|link| link.kind == LinkKind::PlannerExecutor)
        );
        let executor = report
            .sessions
            .iter()
            .find(|entry| entry.session_id == "executor")
            .unwrap();
        assert!(executor.open_turn);
        assert_eq!(executor.tasks.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn meta_parent_and_corrupt_lines_are_reported() {
        let root = temp("meta");
        let child = root.join("child.jsonl");
        write_session(
            &child,
            &[SessionEvent::TurnStarted {
                task: "child".into(),
                objective: "work".into(),
                max_steps: 1,
            }],
        );
        let mut meta_path = child.as_os_str().to_os_string();
        meta_path.push(".meta");
        fs::write(
            &meta_path,
            r#"{"id":"child-1","parent":"parent-1","role":"subagent","model":"m","finished_at":"now"}"#,
        )
        .unwrap();
        // 追加一行坏 JSON，扫描器应计数而不是整体失败。
        let mut text = fs::read_to_string(&child).unwrap();
        text.push_str("not json\n");
        fs::write(&child, text).unwrap();

        let report = scan_sessions(&root).unwrap();
        assert_eq!(report.sessions.len(), 1);
        let entry = &report.sessions[0];
        assert_eq!(entry.session_id, "child-1");
        assert_eq!(entry.role, SessionRole::Subagent);
        assert_eq!(entry.corrupt_lines, 1);
        assert!(entry.meta.as_ref().unwrap().finished);
        assert!(report.links.iter().any(|link| {
            link.kind == LinkKind::ParentChild && link.from == "parent-1" && link.to == "child-1"
        }));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_jsonl_is_kept_as_unknown_format() {
        let root = temp("unknown");
        fs::write(root.join("chat.jsonl"), "{\"foo\":\"bar\"}\n").unwrap();
        let report = scan_sessions(&root).unwrap();
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].format, SessionFormat::Unknown);
        assert_eq!(report.sessions[0].corrupt_lines, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reasonix_role_content_is_adapted_to_tasks() {
        let root = temp("reasonix");
        fs::write(
            root.join("chat.jsonl"),
            "{\"role\":\"user\",\"content\":\"do the thing\"}\n\
             {\"role\":\"assistant\",\"content\":\"calling tool\",\"tool_calls\":[]}\n\
             {\"role\":\"tool\",\"content\":\"ok\"}\n\
             {\"role\":\"user\",\"content\":\"second task\"}\n",
        )
        .unwrap();
        let report = scan_sessions(&root).unwrap();
        let entry = &report.sessions[0];
        assert_eq!(entry.format, SessionFormat::Reasonix);
        assert_eq!(entry.records, 4);
        assert_eq!(entry.corrupt_lines, 0);
        assert_eq!(entry.tasks.len(), 2);
        assert_eq!(entry.tasks[0].task, "do the thing");
        assert!(entry.tasks[0].finished);
        assert_eq!(entry.tasks[1].task, "second task");
        assert!(entry.open_turn, "last user message has no assistant reply");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn auxiliary_directories_are_skipped() {
        let root = temp("skipdirs");
        fs::create_dir_all(root.join("a.jsonl.outputs")).unwrap();
        fs::write(root.join("a.jsonl.outputs/big.jsonl"), "{}").unwrap();
        fs::create_dir_all(root.join("x.ckpt")).unwrap();
        fs::write(root.join("x.ckpt/state.jsonl"), "{}").unwrap();
        fs::write(root.join("real.jsonl"), "{}").unwrap();
        let report = scan_sessions(&root).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.sessions[0].session_id, "real");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cached_scan_reuses_and_updates_changed_files() {
        let root = temp("cache");
        let session = root.join("a.jsonl");
        let mut log = crate::session::SessionLog::open(&session).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "t".into(),
            objective: "o".into(),
            max_steps: 1,
        })
        .unwrap();
        drop(log);
        let cache = root.join("scan-cache.json");

        let first = scan_sessions_cached(&root, &cache).unwrap();
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].records, 1);
        assert!(cache.exists());

        let second = scan_sessions_cached(&root, &cache).unwrap();
        assert_eq!(second.sessions, first.sessions);

        let mut log = crate::session::SessionLog::open(&session).unwrap();
        log.append(SessionEvent::TurnFinished {
            steps: 1,
            complete: true,
            metrics: None,
        })
        .unwrap();
        drop(log);
        let third = scan_sessions_cached(&root, &cache).unwrap();
        assert_eq!(third.sessions[0].records, 2);
        let _ = fs::remove_dir_all(&root);
    }
}
