//! `trit-host session-status` 的进程级回归：只读、可判退出码。
//!
//! `0` 最新门链未阻塞，`1` 最新门链阻塞，`2` 参数错误。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use trit_host::{GateChainPhase, SessionEvent, SessionLog};

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-session-status-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn event(phase: GateChainPhase, status: &str, exit: i32) -> SessionEvent {
    SessionEvent::GateChain {
        phase,
        status: status.into(),
        exit,
        failed_at: None,
        failures: 0,
        passed: 0,
        blocked: 0,
        unknown: 0,
        skipped: 0,
        report: None,
    }
}

fn run(session: &Path) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-status", "--session", session.to_str().unwrap()])
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn session_status_reports_latest_verdict_and_exit_code() {
    let root = root("verdict");
    let path = root.join("session.jsonl");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(event(GateChainPhase::Pre, "passed", 0)).unwrap();

    let (code, value) = run(&path);
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["gate_chains"]["blocked"], false);
    assert_eq!(value["gate_chains"]["history"], 1);
    assert_eq!(value["gate_chains"]["latest"]["status"], "passed");

    log.append(event(GateChainPhase::Post, "unaligned", 1))
        .unwrap();
    let (code, value) = run(&path);
    assert_eq!(code, Some(1), "{value}");
    assert_eq!(value["gate_chains"]["blocked"], true);
    assert_eq!(value["gate_chains"]["history"], 2);
    assert_eq!(value["gate_chains"]["latest"]["phase"], "post");
    assert_eq!(value["gate_chains"]["latest"]["status"], "unaligned");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn session_status_handles_missing_session_and_bad_args() {
    let root = root("missing");
    let missing = root.join("nope.jsonl");
    let (code, value) = run(&missing);
    // 不存在的 session 视为空日志：没有门链记录，不阻塞。
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["gate_chains"]["history"], 0);
    assert_eq!(value["gate_chains"]["latest"], Value::Null);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-status"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--session is required"),
        "{output:?}"
    );
    let _ = fs::remove_dir_all(root);
}
