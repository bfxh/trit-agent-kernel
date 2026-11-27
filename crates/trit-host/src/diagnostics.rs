//! 静态能力诊断：一次只读扫描，输出内核版本、工具目录与工作区持久化状态。
//!
//! 默认刻意不启动任何 MCP、gateway 或子进程，也不创建目录/账本：所有探测都先看
//! 文件是否存在，再决定是否读取。只有显式传 `--live-mcp` 才拉起一个 stdio MCP
//! server 做启动探测，并且只采集脱敏后的 stderr，不握手、不读密钥。
//!
//! 输出是稳定 JSON（字段新增只加不删），供宿主、CI 与排障脚本消费。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::AgentError;
use crate::checkpoint::REWIND_SCHEMA;
use crate::journal::{JOURNAL_PATH, JOURNAL_SCHEMA, MutationJournal};
use crate::session::{SESSION_SCHEMA, recover};
use crate::session_index::{INDEX_SCHEMA, IndexSummary, SessionIndex};

/// 内核各 JSONL/SQLite 契约的当前版本。
#[derive(Debug, Clone, Serialize)]
pub struct KernelVersions {
    pub session: u32,
    pub write_journal: u32,
    pub rewind: u32,
    pub session_index: u32,
}

/// 内置工具的静态能力声明；不依赖任何执行器实例。
#[derive(Debug, Clone, Serialize)]
pub struct ToolCapability {
    pub name: &'static str,
    pub read_only: bool,
    pub writes: bool,
    /// 需要宿主提供 OS 沙箱（否则必须显式允许无沙箱）。
    pub requires_sandbox: bool,
    /// 覆盖该工具的集成测试目标（相对 crate 根）；设置栏据此显示“已有测试”。
    pub tests: &'static [&'static str],
    /// 对应基准入口；没有基准时为 None。
    pub bench: Option<&'static str>,
}

/// 无头模式的工具目录。新增工具时同步这里与 `tools.rs` 的分发。
pub const TOOL_CAPABILITIES: &[ToolCapability] = &[
    ToolCapability {
        name: "bash",
        read_only: false,
        writes: true,
        requires_sandbox: true,
        tests: &[
            "tests/native_tools.rs",
            "tests/tools.rs",
            "tests/appcontainer.rs",
        ],
        bench: None,
    },
    ToolCapability {
        name: "glob",
        read_only: true,
        writes: false,
        requires_sandbox: false,
        tests: &["tests/native_tools.rs"],
        bench: None,
    },
    ToolCapability {
        name: "move_path",
        read_only: false,
        writes: true,
        requires_sandbox: false,
        tests: &["tests/tools.rs"],
        bench: None,
    },
    ToolCapability {
        name: "read_file",
        read_only: true,
        writes: false,
        requires_sandbox: false,
        tests: &["tests/tools.rs"],
        bench: None,
    },
    ToolCapability {
        name: "search_code",
        read_only: true,
        writes: false,
        requires_sandbox: false,
        tests: &["tests/native_tools.rs"],
        bench: Some("search_compare"),
    },
    ToolCapability {
        name: "trit_search",
        read_only: true,
        writes: false,
        requires_sandbox: false,
        tests: &["tests/trit_search.rs", "tests/native_tools.rs"],
        bench: Some("search_compare"),
    },
    ToolCapability {
        name: "write_file",
        read_only: false,
        writes: true,
        requires_sandbox: false,
        tests: &["tests/tools.rs"],
        bench: None,
    },
];

/// write journal 的对账摘要；没有账本时 `present=false`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct JournalDiagnostics {
    pub present: bool,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub rolled_back: usize,
    pub in_doubt: usize,
}

/// 会话日志摘要；只在传入 `--session` 时出现。
#[derive(Debug, Clone, Serialize)]
pub struct SessionDiagnostics {
    pub path: String,
    pub turns: usize,
    pub gate_chains: usize,
    pub latest_gate_blocked: Option<bool>,
    pub last_seq: u64,
    /// 最后一条 `TurnFinished` 的 token 总量；没有完成轮次时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_total: Option<u64>,
    /// 最后一条 `TurnFinished` 的吞吐（token/s）；旧日志没有指标时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f64>,
}

/// SQLite 投影摘要；只在传入 `--db` 时出现。
#[derive(Debug, Clone, Serialize)]
pub struct ProjectionDiagnostics {
    pub path: String,
    pub present: bool,
    pub summary: Option<IndexSummary>,
}

/// `trit_search` 的确定性自检：正确性对齐朴素 oracle、并发与串行一致，并给出微基准。
///
/// 全部是固定语料上的纯计算，不读工作区、不创建文件、不启动线程池以外的资源。
#[derive(Debug, Clone, Serialize)]
pub struct SearchDiagnostics {
    pub corpus_bytes: usize,
    pub lines: usize,
    pub patterns: usize,
    pub verdicts_checked: usize,
    pub mismatches: usize,
    pub serial_parallel_equal: bool,
    pub elapsed_us: u64,
    pub mib_per_s: f64,
}

/// `--live` 探测目标：一个 stdio MCP server 的启动命令。
///
/// 只做启动观测：不握手、不读 stdout、不读环境里的凭据。
#[derive(Debug, Clone)]
pub struct LiveMcpProbe {
    pub program: String,
    pub args: Vec<String>,
    /// 启动观测窗口；窗口结束时仍在运行就主动结束，不等待它自己退出。
    pub wait: Duration,
    /// stderr 最多保留多少行；其余丢弃并标记 `stderr_truncated`。
    pub max_stderr_lines: usize,
}

impl LiveMcpProbe {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            wait: Duration::from_millis(1_500),
            max_stderr_lines: 40,
        }
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn with_wait_millis(mut self, wait: u64) -> Self {
        self.wait = Duration::from_millis(wait);
        self
    }

    pub fn with_max_stderr_lines(mut self, max: usize) -> Self {
        self.max_stderr_lines = max.max(1);
        self
    }
}

/// 一次 stdio MCP 启动探测的结果；`stderr_lines` 已脱敏。
#[derive(Debug, Clone, Serialize)]
pub struct LiveMcpDiagnostics {
    pub program: String,
    pub launched: bool,
    /// 观测窗口结束前就自行退出（常见于缺依赖、参数错误）。
    pub exited_early: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stderr_lines: Vec<String>,
    pub stderr_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 拉起一个 stdio MCP server 做启动探测，只采集脱敏后的 stderr。
///
/// 无论子进程是否自行退出，探测结束都会主动结束它：诊断命令不允许挂住。
/// stdin 保持打开，因为多数 MCP server 把 stdin EOF 当成退出信号。
pub fn live_mcp_diagnostics(probe: &LiveMcpProbe, workspace: &Path) -> LiveMcpDiagnostics {
    let mut result = LiveMcpDiagnostics {
        program: probe.program.clone(),
        launched: false,
        exited_early: false,
        exit_code: None,
        stderr_lines: Vec::new(),
        stderr_truncated: false,
        error: None,
    };
    let mut command = Command::new(&probe.program);
    command
        .args(&probe.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.error = Some(error.to_string());
            return result;
        }
    };
    result.launched = true;
    let (sender, receiver) = mpsc::channel();
    // 读线程按行转发 stderr；它随管道 EOF 结束，这里只 detach 不 join，
    // 避免孙进程持有写端时把诊断命令卡死。
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    }
    std::thread::sleep(probe.wait);
    match child.try_wait() {
        Ok(Some(status)) => {
            result.exited_early = true;
            result.exit_code = status.code();
        }
        Ok(None) => {}
        Err(error) => result.error = Some(error.to_string()),
    }
    let _ = child.kill();
    let _ = child.wait();

    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => result.stderr_lines.push(redact_line(&line, workspace)),
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    while let Ok(line) = receiver.try_recv() {
        result.stderr_lines.push(redact_line(&line, workspace));
    }
    if result.stderr_lines.len() > probe.max_stderr_lines {
        result.stderr_lines.truncate(probe.max_stderr_lines);
        result.stderr_truncated = true;
    }
    result
}

/// 把一行 stderr 里的工作区路径与凭据换成占位符。
pub fn redact_line(line: &str, workspace: &Path) -> String {
    let mut text = line.to_owned();
    let display = workspace.display().to_string();
    if !display.is_empty() {
        text = text.replace(&display, "<workspace>");
    }
    if let Ok(canonical) = std::fs::canonicalize(workspace) {
        let canonical = canonical.display().to_string();
        if !canonical.is_empty() && canonical != display {
            text = text.replace(&canonical, "<workspace>");
        }
    }
    redact_secrets(&text)
}

fn redact_secrets(text: &str) -> String {
    use std::sync::OnceLock;

    static BEARER: OnceLock<regex::Regex> = OnceLock::new();
    static ASSIGN: OnceLock<regex::Regex> = OnceLock::new();
    static KEY: OnceLock<regex::Regex> = OnceLock::new();
    let bearer = BEARER.get_or_init(|| {
        regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9._\-]{8,}").expect("bearer pattern")
    });
    let assign = ASSIGN.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)\b(api[_-]?key|access[_-]?token|auth[_-]?token|token|secret|password)(\s*[:=]\s*)([^\s"']{8,})"#,
        )
        .expect("assignment pattern")
    });
    let key =
        KEY.get_or_init(|| regex::Regex::new(r"\bsk-[A-Za-z0-9._\-]{8,}").expect("key pattern"));
    let text = bearer.replace_all(text, "Bearer <redacted>");
    let text = assign.replace_all(&text, "$1$2<redacted>");
    key.replace_all(&text, "sk-<redacted>").into_owned()
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsReport {
    pub kernel: KernelVersions,
    pub tools: &'static [ToolCapability],
    pub search: SearchDiagnostics,
    pub workspace: String,
    pub journal: JournalDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<ProjectionDiagnostics>,
    /// 仅传 `--live-mcp` 时出现；只含启动探测与脱敏 stderr。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveMcpDiagnostics>,
    /// 当前进程仍在运行的后台任务（CLI 短进程通常为空；长期服务可见）。
    pub background_tasks: Vec<crate::background::BackgroundTask>,
    /// 本进程累计启动过的后台任务数。
    pub background_started: u64,
    pub warnings: Vec<String>,
}

/// 诊断输入；所有路径都必须先存在才会被读取。
#[derive(Debug, Clone)]
pub struct DiagnosticsOptions<'a> {
    pub workspace: &'a Path,
    pub session: Option<&'a Path>,
    pub projection: Option<&'a Path>,
    /// 显式传入才会拉起 stdio MCP server 做启动探测；默认不拉进程。
    pub live: Option<&'a LiveMcpProbe>,
}

pub fn diagnose(options: &DiagnosticsOptions<'_>) -> Result<DiagnosticsReport, AgentError> {
    let mut warnings = Vec::new();
    let search = search_diagnostics();
    if search.mismatches > 0 {
        warnings.push(format!(
            "trit_search oracle mismatch on {} of {} verdict(s)",
            search.mismatches, search.verdicts_checked
        ));
    }
    if !search.serial_parallel_equal {
        warnings.push("trit_search parallel judgement diverged from serial".to_string());
    }
    let journal = journal_diagnostics(options.workspace)?;
    if journal.in_doubt > 0 {
        warnings.push(format!(
            "write journal has {} in-doubt operation(s); run journal-status --rollback",
            journal.in_doubt
        ));
    }

    let session = match options.session {
        Some(path) => {
            let records = crate::session::SessionLog::open(path)?.load()?;
            let state = recover(&records)?;
            let latest_gate_blocked = state.gate_chains.last().map(|record| !record.passed());
            let metrics = records.iter().rev().find_map(|record| match &record.event {
                crate::session::SessionEvent::TurnFinished {
                    metrics: Some(metrics),
                    ..
                } => Some(metrics.clone()),
                _ => None,
            });
            Some(SessionDiagnostics {
                path: path.display().to_string(),
                turns: state.turns.len(),
                gate_chains: state.gate_chains.len(),
                latest_gate_blocked,
                last_seq: records.last().map_or(0, |record| record.seq),
                tokens_total: metrics.as_ref().map(|metrics| metrics.tokens_total),
                tokens_per_second: metrics
                    .as_ref()
                    .and_then(|metrics| metrics.tokens_per_second),
            })
        }
        None => None,
    };

    let projection = match options.projection {
        Some(path) => {
            let index = SessionIndex::open(path);
            let summary = if path.is_file() {
                Some(index.summary()?)
            } else {
                warnings.push(format!(
                    "projection {} is missing; run session-index --rebuild",
                    path.display()
                ));
                None
            };
            if let Some(summary) = &summary
                && summary.schema != INDEX_SCHEMA
            {
                warnings.push(format!(
                    "projection schema {} != {INDEX_SCHEMA}; rebuild it",
                    summary.schema
                ));
            }
            Some(ProjectionDiagnostics {
                path: path.display().to_string(),
                present: path.is_file(),
                summary,
            })
        }
        None => None,
    };

    let live = options.live.map(|probe| {
        let probe_report = live_mcp_diagnostics(probe, options.workspace);
        if !probe_report.launched
            && let Some(error) = probe_report.error.as_ref()
        {
            warnings.push(format!(
                "live MCP {} failed to launch: {error}",
                probe_report.program
            ));
        }
        if probe_report.exited_early {
            warnings.push(format!(
                "live MCP {} exited before the probe window (code {:?})",
                probe_report.program, probe_report.exit_code
            ));
        }
        probe_report
    });

    Ok(DiagnosticsReport {
        kernel: KernelVersions {
            session: SESSION_SCHEMA,
            write_journal: JOURNAL_SCHEMA,
            rewind: REWIND_SCHEMA,
            session_index: INDEX_SCHEMA,
        },
        tools: TOOL_CAPABILITIES,
        search,
        workspace: options.workspace.display().to_string(),
        journal,
        session,
        projection,
        live,
        background_tasks: crate::background::snapshot(),
        background_started: crate::background::started_total(),
        warnings,
    })
}

/// 固定语料 + 固定模式的自检；与单元/集成测试同源，供 CI 与排障脚本读取。
///
/// 正确性以 [`trit_core::search::naive_find`] 为独立 oracle；并发一致性直接比较
/// `judge_lines` 的串行与多线程结果。耗时为整段自检的墙钟时间，用于观察回归。
fn search_diagnostics() -> SearchDiagnostics {
    use std::time::Instant;

    use trit_core::search::naive_find;
    use trit_core::{MatchVerdict, Ternary, TritSearcher};

    let lines: Vec<String> = (0..1_500)
        .map(|i| match i % 5 {
            0 => format!("pub fn open(path: &Path) -> io::Result<File> {{ // {i}"),
            1 => format!("    let handle = File::open(path)?; // needle {i}"),
            2 => format!("    let abc_{i} = compute({i});"),
            _ => format!("    let value_{i} = {i} * 2;"),
        })
        .collect();
    let corpus_bytes: usize = lines.iter().map(String::len).sum();
    let slices: Vec<&[u8]> = lines.iter().map(|line| line.as_bytes()).collect();
    let patterns = ["fn open", "fn ?pen", "???", "a?c", "ERROR", "k?"];
    let jobs = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);

    let started = Instant::now();
    let mut verdicts_checked = 0usize;
    let mut mismatches = 0usize;
    let mut serial_parallel_equal = true;
    for pattern in patterns {
        let Ok(searcher) = TritSearcher::new(pattern, false) else {
            mismatches += 1;
            continue;
        };
        for line in &lines {
            let fast = searcher.line_verdict(line.as_bytes());
            let expected = match naive_find(pattern, false, line.as_bytes()) {
                Some((_, MatchVerdict::Match)) => Ternary::True,
                Some((_, MatchVerdict::Unknown)) => Ternary::Unknown,
                _ => Ternary::False,
            };
            verdicts_checked += 1;
            if fast != expected {
                mismatches += 1;
            }
        }
        if searcher.judge_lines(&slices, 1) != searcher.judge_lines(&slices, jobs) {
            serial_parallel_equal = false;
        }
    }
    let elapsed_us = started.elapsed().as_micros().max(1) as u64;
    let mib_per_s = corpus_bytes as f64 / (1024.0 * 1024.0) / (elapsed_us as f64 / 1_000_000.0);
    SearchDiagnostics {
        corpus_bytes,
        lines: lines.len(),
        patterns: patterns.len(),
        verdicts_checked,
        mismatches,
        serial_parallel_equal,
        elapsed_us,
        mib_per_s,
    }
}

fn journal_diagnostics(workspace: &Path) -> Result<JournalDiagnostics, AgentError> {
    let path = workspace.join(JOURNAL_PATH);
    if !path.is_file() {
        return Ok(JournalDiagnostics::default());
    }
    let journal = MutationJournal::open(workspace)
        .map_err(|error| AgentError::SessionState(format!("write journal: {error}")))?;
    let report = journal
        .report()
        .map_err(|error| AgentError::SessionState(format!("write journal: {error}")))?;
    Ok(JournalDiagnostics {
        present: true,
        total: report.total,
        completed: report.completed,
        failed: report.failed,
        rolled_back: report.rolled_back,
        in_doubt: report.in_doubt.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_catalog_is_unique_and_ordered() {
        let names: Vec<&str> = TOOL_CAPABILITIES.iter().map(|tool| tool.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "tool catalog must stay name-sorted");
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names, unique, "tool catalog has duplicates");
        assert!(
            TOOL_CAPABILITIES
                .iter()
                .any(|tool| tool.requires_sandbox && tool.name == "bash")
        );
    }

    #[test]
    fn live_probe_builder_defaults_are_bounded() {
        let probe = LiveMcpProbe::new("mcp-server")
            .with_arg("--stdio")
            .with_wait_millis(0)
            .with_max_stderr_lines(0);
        assert_eq!(probe.program, "mcp-server");
        assert_eq!(probe.args, vec!["--stdio".to_owned()]);
        assert_eq!(probe.wait, Duration::from_millis(0));
        assert_eq!(probe.max_stderr_lines, 1, "stderr 上限至少保留一行");
    }

    #[test]
    fn redact_line_hides_workspace_and_credentials() {
        let workspace = std::env::temp_dir().join(format!(
            "trit-redact-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let line = format!(
            "failed in {}/src/main.rs: token=abcdef123456 Bearer sk-live-abcdef123456",
            workspace.display()
        );
        let redacted = redact_line(&line, &workspace);
        assert!(
            !redacted.contains(&workspace.display().to_string()),
            "workspace path must be hidden: {redacted}"
        );
        assert!(redacted.contains("<workspace>"), "{redacted}");
        assert!(
            !redacted.contains("abcdef123456"),
            "credentials must be hidden: {redacted}"
        );
        assert!(redacted.contains("<redacted>"), "{redacted}");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn redact_line_keeps_ordinary_diagnostics() {
        let workspace = std::env::temp_dir();
        let line = "server listening on stdio, tools=3";
        assert_eq!(redact_line(line, &workspace), line);
    }
}
