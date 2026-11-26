//! `session-index` 与 `SessionIndex` 的进程级回归：丢弃投影后重建必须逐行一致，
//! 现有投影被篡改或缺失时 `--verify` 必须 fail-closed。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::SessionEvent;
use trit_host::SessionIndex;
use trit_host::session::SessionLog;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-session-index-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn build_session(path: &Path) {
    let mut log = SessionLog::open(path).unwrap();
    for turn in 1..=2 {
        log.append(SessionEvent::TurnStarted {
            task: format!("task-{turn}"),
            objective: "index it".into(),
            max_steps: 3,
        })
        .unwrap();
        log.append(SessionEvent::PlanPatched {
            revision: turn as u64,
            task: format!("task-{turn}"),
            operation: "append-step".into(),
        })
        .unwrap();
        log.append(SessionEvent::TurnFinished {
            steps: 1,
            complete: true,
            metrics: None,
        })
        .unwrap();
    }
}

fn session_index(session: &Path, db: &Path, extra: &str) -> (Option<i32>, serde_json::Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-index",
            "--session",
            session.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            extra,
        ])
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn rebuild_is_repeatable_after_deleting_the_database() {
    let dir = temp_dir("repeat");
    let session = dir.join("session.jsonl");
    let db = dir.join("session.index.db");
    build_session(&session);

    let first = SessionIndex::rebuild(&session, &db).unwrap();
    assert!(!first.replaced);
    assert_eq!(first.summary.records, 6);
    assert_eq!(first.summary.turns, 2);
    assert_eq!(first.summary.tool_calls, 0);
    assert_eq!(first.summary.gate_chains, 0);
    assert_eq!(first.summary.last_seq, 6);
    let rows = SessionIndex::open(&db).dump().unwrap();

    // 覆盖已有投影重建：摘要与逐行转储都必须与第一次完全一致。
    let second = SessionIndex::rebuild(&session, &db).unwrap();
    assert!(second.replaced);
    assert_eq!(second.summary, first.summary);
    assert_eq!(SessionIndex::open(&db).dump().unwrap(), rows);

    // 删库后重建走同一条冷启动路径，结果仍然一致。
    fs::remove_file(&db).unwrap();
    let third = SessionIndex::rebuild(&session, &db).unwrap();
    assert!(!third.replaced);
    assert_eq!(third.summary, first.summary);
    assert_eq!(SessionIndex::open(&db).dump().unwrap(), rows);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn verify_detects_tampering_and_missing_projection() {
    let dir = temp_dir("verify");
    let session = dir.join("session.jsonl");
    let db = dir.join("session.index.db");
    build_session(&session);

    SessionIndex::rebuild(&session, &db).unwrap();
    let clean = SessionIndex::verify(&session, &db).unwrap();
    assert!(clean.matches, "{clean:?}");
    assert_eq!(clean.expected, clean.actual.unwrap());

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO records (seq, turn, kind) VALUES (99, 9, 'tampered')",
        [],
    )
    .unwrap();
    drop(conn);
    let tampered = SessionIndex::verify(&session, &db).unwrap();
    assert!(!tampered.matches, "{tampered:?}");

    fs::remove_file(&db).unwrap();
    let missing = SessionIndex::verify(&session, &db).unwrap();
    assert!(!missing.matches);
    assert!(missing.actual.is_none());
    assert!(missing.detail.contains("missing"), "{missing:?}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_rebuild_and_verify_cover_the_projection_lifecycle() {
    let dir = temp_dir("cli");
    let session = dir.join("session.jsonl");
    let db = dir.join("session.index.db");
    build_session(&session);

    let (code, report) = session_index(&session, &db, "--rebuild");
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["replaced"], false);
    assert_eq!(report["summary"]["records"], 6);
    assert_eq!(report["summary"]["turns"], 2);

    let (code, report) = session_index(&session, &db, "--verify");
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["matches"], true);

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO records (seq, turn, kind) VALUES (99, 9, 'tampered')",
        [],
    )
    .unwrap();
    drop(conn);
    let (code, report) = session_index(&session, &db, "--verify");
    assert_eq!(code, Some(1), "{report}");
    assert_eq!(report["matches"], false);
    let _ = fs::remove_dir_all(dir);
}
