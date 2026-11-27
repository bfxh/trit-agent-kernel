//! `trit-host session-run` 的进程级回归：只读派发单、打印命令、不执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::{SessionEvent, SessionLog};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-session-run-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn resume_session(path: &Path) {
    let mut log = SessionLog::open(path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "unfinished".into(),
        objective: "resume source".into(),
        max_steps: 6,
    })
    .unwrap();
}

#[test]
fn dry_run_prints_run_command_without_executing() {
    let dir = temp_dir("dry");
    let session = dir.join("open.resume.jsonl");
    resume_session(&session);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-run",
            "--session",
            session.to_str().unwrap(),
            "--workspace",
            dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["task"], "unfinished");
    assert_eq!(report["max_steps"], 6);
    assert_eq!(report["applied"], false);
    let target = report["target_session"].as_str().unwrap();
    assert!(
        !Path::new(target).exists(),
        "dry-run must not create the run session"
    );
    let command = report["command"].as_array().unwrap();
    assert_eq!(command[0], "run");
    assert!(command.iter().any(|value| value == "unfinished"));
    assert!(command.iter().any(|value| value == "--session"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn missing_session_fails_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires --session"), "{stderr}");
}

#[test]
fn unknown_arguments_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-run", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown session-run argument"), "{stderr}");
}
