//! `trit-host session-fleet-remote` 的 dry-run 回归：不连网，只列任务与节点。

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
        "trit-session-fleet-remote-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn open_session(path: &Path) {
    let mut log = SessionLog::open(path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "remote work".into(),
        objective: "objective".into(),
        max_steps: 2,
    })
    .unwrap();
}

#[test]
fn dry_run_lists_tasks_and_nodes_without_network() {
    let dir = temp_dir("dry");
    open_session(&dir.join("session.jsonl"));
    let nodes = dir.join("nodes.json");
    fs::write(
        &nodes,
        r#"[{"id":"node-1","max_in_flight":1,"endpoint":{"url":"http://127.0.0.1:9","auth_token":null,"timeout_secs":1}}]"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-fleet-remote",
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
    assert_eq!(report["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(report["nodes"][0], "node-1");
    assert!(report["completed"].as_array().unwrap().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn missing_nodes_fails_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-fleet-remote"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires --nodes"), "{stderr}");
}
