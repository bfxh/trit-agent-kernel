//! `trit-host session-advance` 的 dry-run 回归：一条命令输出派发与 fleet 计划。

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
        "trit-session-advance-cli-{tag}-{}-{nanos}",
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
fn dry_run_prints_dispatch_and_fleet_without_writing() {
    let dir = temp_dir("dry");
    open_session(&dir.join("parent.jsonl"), "parent");
    let child = dir.join("child.jsonl");
    open_session(&child, "child");
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(meta_path, r#"{"id":"child","parent":"parent"}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-advance",
            "--root",
            dir.to_str().unwrap(),
            "--max-items",
            "2",
            "--workers",
            "2",
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
    assert_eq!(report["dispatch"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(report["fleet"]["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(report["fleet"]["layers"][0][0], "parent.run");
    assert_eq!(report["fleet"]["layers"][1][0], "child.run");
    assert!(report["run"].is_null());
    assert!(!dir.join("parent.run.jsonl").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unknown_arguments_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-advance", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown session-advance argument"),
        "{stderr}"
    );
}

#[test]
fn nodes_are_accepted_in_dry_run_without_network() {
    let dir = temp_dir("nodes");
    open_session(&dir.join("session.jsonl"), "remote");
    let nodes = dir.join("nodes.json");
    fs::write(
        &nodes,
        r#"[{"id":"node-1","max_in_flight":1,"endpoint":{"url":"http://127.0.0.1:9","auth_token":null,"timeout_secs":1}}]"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-advance",
            "--root",
            dir.to_str().unwrap(),
            "--nodes",
            nodes.to_str().unwrap(),
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
    assert_eq!(report["fleet"]["tasks"].as_array().unwrap().len(), 1);
    assert!(report["remote"].is_null());
    let _ = fs::remove_dir_all(dir);
}
