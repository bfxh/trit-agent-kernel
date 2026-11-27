//! `trit-host session-fleet` 的进程级回归：依赖分层、workers 上限、循环报告。

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
        "trit-session-fleet-cli-{tag}-{}-{nanos}",
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

fn run(root: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec![
        "session-fleet",
        "--root",
        root.to_str().unwrap(),
        "--compact",
    ];
    args.extend_from_slice(extra);
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(&args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn parent_child_links_become_dependency_layers() {
    let dir = temp_dir("layers");
    open_session(&dir.join("parent.jsonl"), "parent");
    let child = dir.join("child.jsonl");
    open_session(&child, "child");
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(meta_path, r#"{"id":"child","parent":"parent"}"#).unwrap();

    let report = run(&dir, &["--workers", "2"]);
    assert_eq!(report["workers"], 2);
    let layers = report["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0][0], "parent");
    assert_eq!(layers[1][0], "child");
    let child_task = report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "child")
        .unwrap();
    assert_eq!(child_task["deps"][0], "parent");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn independent_sessions_share_the_first_layer() {
    let dir = temp_dir("independent");
    open_session(&dir.join("a.jsonl"), "a");
    open_session(&dir.join("b.jsonl"), "b");
    let report = run(&dir, &["--workers", "1"]);
    assert_eq!(report["workers"], 1);
    let layers = report["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].as_array().unwrap().len(), 2);
    let _ = fs::remove_dir_all(dir);
}
