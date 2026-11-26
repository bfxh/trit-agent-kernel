//! JSONL 布局兼容矩阵的进程级回归：老文件可读、未来 schema fail-closed、
//! 追加不重写既有字节、文档与代码矩阵同步。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::SessionEvent;
use trit_host::journal::MutationJournal;
use trit_host::jsonl_layout::JSONL_LAYOUTS;
use trit_host::session::SessionLog;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-jsonl-compat-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/jsonl/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn copy_fixture(name: &str, target: &Path) {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(fixture(name), target).unwrap();
}

#[test]
fn legacy_session_log_reads_and_appends_without_rewriting() {
    let dir = temp_dir("session-legacy");
    let path = dir.join("session.jsonl");
    copy_fixture("session-v1-legacy.jsonl", &path);
    let before = fs::read(&path).unwrap();

    let mut log = SessionLog::open(&path).unwrap();
    let records = log.load().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[2].seq, 3);

    // 老记录缺少后加的压缩字段：全部按默认值读入。
    match &records[2].event {
        SessionEvent::ContextCompacted {
            retained,
            dropped_chars,
            truncated_chars,
            epoch_before,
            epoch_after,
            freshness,
            ..
        } => {
            assert_eq!(*retained, None);
            assert_eq!(*dropped_chars, 0);
            assert_eq!(*truncated_chars, 0);
            assert_eq!(*epoch_before, None);
            assert_eq!(*epoch_after, None);
            assert_eq!(*freshness, None);
        }
        other => panic!("unexpected legacy event: {other:?}"),
    }

    let seq = log
        .append(SessionEvent::PlanPatched {
            revision: 2,
            task: "legacy".into(),
            operation: "finish-step".into(),
        })
        .unwrap();
    assert_eq!(seq, 4);
    let after = fs::read(&path).unwrap();
    // 追加只加新行，既有字节逐字节保留。
    assert!(after.starts_with(&before), "append rewrote existing lines");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn future_session_schema_is_rejected() {
    let dir = temp_dir("session-future");
    let path = dir.join("session.jsonl");
    copy_fixture("session-v2-future.jsonl", &path);
    let error = SessionLog::open(&path).unwrap_err().to_string();
    assert!(error.contains("session schema 2"), "{error}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn legacy_write_journal_reads_with_defaults() {
    let dir = temp_dir("journal-legacy");
    let root = dir.join("workspace");
    copy_fixture(
        "write-journal-v1-legacy.jsonl",
        &root.join(".agent/write-journal.jsonl"),
    );

    let journal = MutationJournal::open(&root).unwrap();
    let report = journal.report().unwrap();
    assert_eq!(report.total, 3);
    assert_eq!(report.completed, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.in_doubt.len(), 1);

    // 第四刀之前的老 begin：只有 lease_epoch，没有锁路径与备份。
    let doubt = &report.in_doubt[0];
    assert_eq!(doubt.op_id, 2);
    assert_eq!(doubt.lease_epoch, Some(5));
    assert!(doubt.lease_locks.is_empty());
    assert!(doubt.backups.is_empty());
    assert!(!doubt.passed);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn future_write_journal_schema_is_rejected() {
    let dir = temp_dir("journal-future");
    let root = dir.join("workspace");
    copy_fixture(
        "write-journal-v2-future.jsonl",
        &root.join(".agent/write-journal.jsonl"),
    );
    let error = MutationJournal::open(&root).unwrap_err().to_string();
    assert!(error.contains("schema 2"), "{error}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn legacy_rewind_journal_reads_through_cli() {
    let dir = temp_dir("rewind-legacy");
    copy_fixture(
        "rewind-journal-v1-legacy.jsonl",
        &dir.join("rewind-journal.jsonl"),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["rewind-status", "--checkpoint-dir", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["total"], 2);
    assert_eq!(report["completed"], 1);
    assert_eq!(report["in_doubt"].as_array().unwrap().len(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn future_rewind_schema_fails_closed_through_cli() {
    let dir = temp_dir("rewind-future");
    copy_fixture(
        "rewind-journal-v2-future.jsonl",
        &dir.join("rewind-journal.jsonl"),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["rewind-status", "--checkpoint-dir", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("schema 2"), "{stderr}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn documented_matrix_matches_the_code() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/JSONL-COMPAT.md");
    let doc = fs::read_to_string(&path).unwrap();
    for layout in JSONL_LAYOUTS {
        let row = format!("| `{}` | {} |", layout.file, layout.schema);
        assert!(doc.contains(&row), "missing matrix row: {row}");
    }
    assert!(doc.contains("fail-closed"));
}
