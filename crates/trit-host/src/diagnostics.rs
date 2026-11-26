//! 静态能力诊断：一次只读扫描，输出内核版本、工具目录与工作区持久化状态。
//!
//! 这里刻意不启动任何 MCP、gateway 或子进程，也不创建目录/账本：所有探测都先看
//! 文件是否存在，再决定是否读取。需要实跑 MCP 的 `--live` 模式留给后续切片。
//!
//! 输出是稳定 JSON（字段新增只加不删），供宿主、CI 与排障脚本消费。

use std::path::Path;

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
}

/// 无头模式的工具目录。新增工具时同步这里与 `tools.rs` 的分发。
pub const TOOL_CAPABILITIES: &[ToolCapability] = &[
    ToolCapability {
        name: "bash",
        read_only: false,
        writes: true,
        requires_sandbox: true,
    },
    ToolCapability {
        name: "glob",
        read_only: true,
        writes: false,
        requires_sandbox: false,
    },
    ToolCapability {
        name: "move_path",
        read_only: false,
        writes: true,
        requires_sandbox: false,
    },
    ToolCapability {
        name: "read_file",
        read_only: true,
        writes: false,
        requires_sandbox: false,
    },
    ToolCapability {
        name: "search_code",
        read_only: true,
        writes: false,
        requires_sandbox: false,
    },
    ToolCapability {
        name: "write_file",
        read_only: false,
        writes: true,
        requires_sandbox: false,
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
}

/// SQLite 投影摘要；只在传入 `--db` 时出现。
#[derive(Debug, Clone, Serialize)]
pub struct ProjectionDiagnostics {
    pub path: String,
    pub present: bool,
    pub summary: Option<IndexSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsReport {
    pub kernel: KernelVersions,
    pub tools: &'static [ToolCapability],
    pub workspace: String,
    pub journal: JournalDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<ProjectionDiagnostics>,
    pub warnings: Vec<String>,
}

/// 诊断输入；所有路径都必须先存在才会被读取。
#[derive(Debug, Clone)]
pub struct DiagnosticsOptions<'a> {
    pub workspace: &'a Path,
    pub session: Option<&'a Path>,
    pub projection: Option<&'a Path>,
}

pub fn diagnose(options: &DiagnosticsOptions<'_>) -> Result<DiagnosticsReport, AgentError> {
    let mut warnings = Vec::new();
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
            Some(SessionDiagnostics {
                path: path.display().to_string(),
                turns: state.turns.len(),
                gate_chains: state.gate_chains.len(),
                latest_gate_blocked,
                last_seq: records.last().map_or(0, |record| record.seq),
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

    Ok(DiagnosticsReport {
        kernel: KernelVersions {
            session: SESSION_SCHEMA,
            write_journal: JOURNAL_SCHEMA,
            rewind: REWIND_SCHEMA,
            session_index: INDEX_SCHEMA,
        },
        tools: TOOL_CAPABILITIES,
        workspace: options.workspace.display().to_string(),
        journal,
        session,
        projection,
        warnings,
    })
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
}
