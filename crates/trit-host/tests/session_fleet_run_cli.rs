//! `trit-host session-fleet-run` 的 dry-run 回归：只打印计划，不启动模型。

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
        "trit-session-fleet-run-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn open_session(path: &Path, task: &str) {
    let mut log = SessionLog::open(path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: task.into(),
        objective: "objective".into(),
        max_steps: 2,
    })
    .unwrap();
}

#[test]
fn dry_run_prints_the_fleet_without_calling_a_model() {
    let dir = temp_dir("dry");
    open_session(&dir.join("parent.jsonl"), "parent");
    let child = dir.join("child.jsonl");
    open_session(&child, "child");
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(meta_path, r#"{"id":"child","parent":"parent"}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-fleet-run",
            "--root",
            dir.to_str().unwrap(),
            "--workers",
            "2",
            "--max-steps",
            "5",
            "--compact",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["workers"], 2);
    assert_eq!(report["max_steps"], 5);
    assert_eq!(report["layers"][0][0], "parent");
    assert_eq!(report["layers"][1][0], "child");
    assert!(report["cyclic"].as_array().unwrap().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unknown_arguments_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-fleet-run", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown session-fleet-run argument"),
        "{stderr}"
    );
}
