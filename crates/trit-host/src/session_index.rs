//! 会话 JSONL 的 SQLite 投影索引：可随时丢弃、按日志重建。
//!
//! JSONL 仍是唯一权威；SQLite 只是查询加速层。任何投影都可能过期或损坏，
//! 因此接口只承诺两件事：`rebuild` 能从日志完整重建，`verify` 能判断现有投影
//! 是否与日志一致。投影文件放在调用方指定路径（约定 `<session>.index.db`）。
//!
//! 重建是原子的：先删旧库（含 `-wal`/`-shm`），再在新库里建表并整批写入；
//! 中途失败不会留下半份可读投影。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::AgentError;
use crate::session::{SessionEvent, SessionLog};

/// 投影 schema；改表结构时递增，旧库由调用方删除重建。
pub const INDEX_SCHEMA: u32 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
CREATE TABLE records (
    seq INTEGER PRIMARY KEY,
    turn INTEGER NOT NULL,
    kind TEXT NOT NULL,
    tool TEXT,
    passed INTEGER,
    gate_status TEXT
);
CREATE TABLE turns (
    turn INTEGER PRIMARY KEY,
    task TEXT NOT NULL,
    started_seq INTEGER NOT NULL,
    finished_seq INTEGER,
    steps INTEGER,
    complete INTEGER
);
";

/// 投影的聚合摘要；`verify` 用它做第一层一致性判断。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexSummary {
    pub schema: u32,
    pub records: usize,
    pub turns: usize,
    pub tool_calls: usize,
    pub gate_chains: usize,
    pub last_seq: u64,
}

/// 重建结果。
#[derive(Debug, Clone, Serialize)]
pub struct RebuildReport {
    pub path: String,
    pub replaced: bool,
    pub summary: IndexSummary,
}

/// 校验结果：`expected` 是按日志重新构建的摘要，`actual` 是现有投影的摘要。
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub matches: bool,
    pub expected: IndexSummary,
    pub actual: Option<IndexSummary>,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SessionIndex {
    path: PathBuf,
}

impl SessionIndex {
    /// 绑定一个投影路径；不创建文件，也不假设它已经存在。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 删掉旧投影并按 `session` 日志重建。
    ///
    /// 先写暂存库，成功后才替换目标文件；失败不会留下半份可读投影。
    pub fn rebuild(session: &Path, index: &Path) -> Result<RebuildReport, AgentError> {
        let replaced = index.exists();
        let staged = staged_path(index);
        remove_projection(&staged)?;
        let summary = match build(session, &staged) {
            Ok(summary) => summary,
            Err(error) => {
                let _ = remove_projection(&staged);
                return Err(error);
            }
        };
        remove_projection(index)?;
        if let Err(error) = std::fs::rename(&staged, index) {
            let _ = remove_projection(&staged);
            return Err(AgentError::SessionIo {
                path: index.display().to_string(),
                message: format!("session index: install: {error}"),
            });
        }
        Ok(RebuildReport {
            path: index.display().to_string(),
            replaced,
            summary,
        })
    }

    /// 读取现有投影的摘要；投影不存在时报错，而不是悄悄建一个空库。
    pub fn summary(&self) -> Result<IndexSummary, AgentError> {
        if !self.path.is_file() {
            return Err(self.error("projection is missing; rebuild it first"));
        }
        let conn = self.connection()?;
        summarize(&conn, &self.path)
    }

    /// 按日志重建到临时库，再与现有投影逐行比较；不会改动现有投影。
    pub fn verify(session: &Path, index: &Path) -> Result<VerifyReport, AgentError> {
        let scratch = staged_path(index);
        remove_projection(&scratch)?;
        let expected = build(session, &scratch)?;
        let result = (|| -> Result<VerifyReport, AgentError> {
            if !index.is_file() {
                return Ok(VerifyReport {
                    matches: false,
                    expected: expected.clone(),
                    actual: None,
                    detail: "projection is missing".into(),
                });
            }
            let existing = SessionIndex::open(index);
            let actual = existing.summary()?;
            if actual != expected {
                return Ok(VerifyReport {
                    matches: false,
                    expected,
                    actual: Some(actual),
                    detail: "summary differs".into(),
                });
            }
            let expected_rows = dump(
                &Connection::open(&scratch).map_err(|error| AgentError::SessionIo {
                    path: scratch.display().to_string(),
                    message: format!("session index: {error}"),
                })?,
                &scratch,
            )?;
            let actual_rows = existing.dump()?;
            let matches = expected_rows == actual_rows;
            Ok(VerifyReport {
                matches,
                expected,
                actual: Some(actual),
                detail: if matches {
                    "projection matches the session log".into()
                } else {
                    format!(
                        "rows differ (expected {}, actual {})",
                        expected_rows.len(),
                        actual_rows.len()
                    )
                },
            })
        })();
        let _ = remove_projection(&scratch);
        result
    }

    /// 现有投影的全量行转储，用于逐行一致性比较。
    pub fn dump(&self) -> Result<Vec<String>, AgentError> {
        if !self.path.is_file() {
            return Err(self.error("projection is missing; rebuild it first"));
        }
        let conn = self.connection()?;
        dump(&conn, &self.path)
    }

    fn connection(&self) -> Result<Connection, AgentError> {
        Connection::open(&self.path).map_err(|error| self.error(format!("open: {error}")))
    }

    fn error(&self, message: impl std::fmt::Display) -> AgentError {
        AgentError::SessionIo {
            path: self.path.display().to_string(),
            message: format!("session index: {message}"),
        }
    }
}

fn build(session: &Path, index: &Path) -> Result<IndexSummary, AgentError> {
    let records = SessionLog::open(session)?.load()?;
    let mut conn = Connection::open(index).map_err(|error| AgentError::SessionIo {
        path: index.display().to_string(),
        message: format!("session index: open: {error}"),
    })?;
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|error| index_error(index, error))?;
    let transaction = conn
        .transaction()
        .map_err(|error| index_error(index, error))?;
    let mut turn = 0_u64;
    let mut last_seq = 0_u64;
    for record in &records {
        last_seq = record.seq;
        let (kind, tool, passed, gate_status) = describe(&record.event);
        if let SessionEvent::TurnStarted { task, .. } = &record.event {
            turn += 1;
            transaction
                .execute(
                    "INSERT INTO turns (turn, task, started_seq) VALUES (?1, ?2, ?3)",
                    params![turn as i64, task, record.seq as i64],
                )
                .map_err(|error| index_error(index, error))?;
        } else if let SessionEvent::TurnFinished {
            steps, complete, ..
        } = &record.event
        {
            transaction
                .execute(
                    "UPDATE turns SET finished_seq = ?2, steps = ?3, complete = ?4 WHERE turn = ?1",
                    params![
                        turn as i64,
                        record.seq as i64,
                        *steps as i64,
                        if *complete { 1 } else { 0 }
                    ],
                )
                .map_err(|error| index_error(index, error))?;
        }
        transaction
            .execute(
                "INSERT INTO records (seq, turn, kind, tool, passed, gate_status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.seq as i64,
                    turn as i64,
                    kind,
                    tool,
                    passed.map(|passed| if passed { 1 } else { 0 }),
                    gate_status,
                ],
            )
            .map_err(|error| index_error(index, error))?;
    }
    let meta = [
        ("schema", INDEX_SCHEMA.to_string()),
        ("source", session.display().to_string()),
        ("last_seq", last_seq.to_string()),
    ];
    for (key, value) in meta {
        transaction
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(|error| index_error(index, error))?;
    }
    transaction
        .commit()
        .map_err(|error| index_error(index, error))?;
    summarize(&conn, index)
}

/// 事件到投影列的映射；`kind` 与 JSONL 里的 `type` 保持一致。
fn describe(event: &SessionEvent) -> (&'static str, Option<String>, Option<bool>, Option<String>) {
    match event {
        SessionEvent::TurnStarted { .. } => ("turn_started", None, None, None),
        SessionEvent::Proposal { .. } => ("proposal", None, None, None),
        SessionEvent::ToolRequested { request, .. } => {
            ("tool_requested", Some(request.tool.clone()), None, None)
        }
        SessionEvent::PlanPatched { .. } => ("plan_patched", None, None, None),
        SessionEvent::ToolCompleted { outcome, .. } => (
            "tool_completed",
            Some(outcome.tool.clone()),
            Some(outcome.passed),
            None,
        ),
        SessionEvent::PrefixChanged { .. } => ("prefix_changed", None, None, None),
        SessionEvent::ContextCompacted { .. } => ("context_compacted", None, None, None),
        SessionEvent::ContextFreshnessRecovered { .. } => {
            ("context_freshness_recovered", None, None, None)
        }
        SessionEvent::CompletionClaimed { .. } => ("completion_claimed", None, None, None),
        SessionEvent::Judgment { .. } => ("judgment", None, None, None),
        SessionEvent::TurnFinished { .. } => ("turn_finished", None, None, None),
        SessionEvent::GateChain { status, .. } => ("gate_chain", None, None, Some(status.clone())),
    }
}

fn summarize(conn: &Connection, path: &Path) -> Result<IndexSummary, AgentError> {
    let schema: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |row| {
            row.get(0)
        })
        .map_err(|error| index_error(path, error))?;
    let records = count(conn, path, "records")?;
    let turns = count(conn, path, "turns")?;
    let tool_calls = conn
        .query_row(
            "SELECT count(*) FROM records WHERE tool IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| index_error(path, error))? as usize;
    let gate_chains = conn
        .query_row(
            "SELECT count(*) FROM records WHERE kind = 'gate_chain'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| index_error(path, error))? as usize;
    let last_seq = conn
        .query_row("SELECT coalesce(max(seq), 0) FROM records", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| index_error(path, error))? as u64;
    Ok(IndexSummary {
        schema: schema.parse().unwrap_or(0),
        records,
        turns,
        tool_calls,
        gate_chains,
        last_seq,
    })
}

fn count(conn: &Connection, path: &Path, table: &str) -> Result<usize, AgentError> {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|count| count as usize)
    .map_err(|error| index_error(path, error))
}

fn dump(conn: &Connection, path: &Path) -> Result<Vec<String>, AgentError> {
    let mut rows = Vec::new();
    {
        let mut statement = conn
            .prepare("SELECT seq, turn, kind, tool, passed, gate_status FROM records ORDER BY seq")
            .map_err(|error| index_error(path, error))?;
        let mut query = statement
            .query([])
            .map_err(|error| index_error(path, error))?;
        while let Some(row) = query.next().map_err(|error| index_error(path, error))? {
            rows.push(format!(
                "r|{}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)
                    .map_err(|error| index_error(path, error))?,
                row.get::<_, i64>(1)
                    .map_err(|error| index_error(path, error))?,
                row.get::<_, String>(2)
                    .map_err(|error| index_error(path, error))?,
                row.get::<_, Option<String>>(3)
                    .map_err(|error| index_error(path, error))?
                    .unwrap_or_default(),
                row.get::<_, Option<i64>>(4)
                    .map_err(|error| index_error(path, error))?
                    .map_or(String::new(), |value| value.to_string()),
                row.get::<_, Option<String>>(5)
                    .map_err(|error| index_error(path, error))?
                    .unwrap_or_default(),
            ));
        }
    }
    let mut statement = conn
        .prepare(
            "SELECT turn, task, started_seq, finished_seq, steps, complete FROM turns ORDER BY turn",
        )
        .map_err(|error| index_error(path, error))?;
    let mut query = statement
        .query([])
        .map_err(|error| index_error(path, error))?;
    while let Some(row) = query.next().map_err(|error| index_error(path, error))? {
        rows.push(format!(
            "t|{}|{}|{}|{}|{}|{}",
            row.get::<_, i64>(0)
                .map_err(|error| index_error(path, error))?,
            row.get::<_, String>(1)
                .map_err(|error| index_error(path, error))?,
            row.get::<_, i64>(2)
                .map_err(|error| index_error(path, error))?,
            row.get::<_, Option<i64>>(3)
                .map_err(|error| index_error(path, error))?
                .map_or(String::new(), |value| value.to_string()),
            row.get::<_, Option<i64>>(4)
                .map_err(|error| index_error(path, error))?
                .map_or(String::new(), |value| value.to_string()),
            row.get::<_, Option<i64>>(5)
                .map_err(|error| index_error(path, error))?
                .map_or(String::new(), |value| value.to_string()),
        ));
    }
    Ok(rows)
}

fn staged_path(index: &Path) -> PathBuf {
    let name = index.file_name().map_or_else(
        || "index.db".into(),
        |name| name.to_string_lossy().to_string(),
    );
    index.with_file_name(format!("{name}.rebuild.{}", std::process::id()))
}

fn remove_projection(path: &Path) -> Result<(), AgentError> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let target = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AgentError::SessionIo {
                    path: target.display().to_string(),
                    message: format!("session index: remove: {error}"),
                });
            }
        }
    }
    Ok(())
}

fn index_error(path: &Path, error: rusqlite::Error) -> AgentError {
    AgentError::SessionIo {
        path: path.display().to_string(),
        message: format!("session index: {error}"),
    }
}
