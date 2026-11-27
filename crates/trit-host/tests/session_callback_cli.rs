//! `trit-host session-callback` 的进程级回归：dry-run、apply、幂等。

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
        "trit-session-callback-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn done_session(path: &Path, task: &str) {
    let mut log = SessionLog::open(path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: task.into(),
        objective: "objective".into(),
        max_steps: 2,
    })
    .unwrap();
    log.append(SessionEvent::TurnFinished {
        steps: 1,
        complete: true,
        metrics: None,
    })
    .unwrap();
}

fn run(root: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec![
        "session-callback",
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
fn dry_run_then_apply_then_idempotent() {
    let dir = temp_dir("callback");
    done_session(&dir.join("parent.jsonl"), "parent");
    let child = dir.join("child.jsonl");
    done_session(&child, "child work");
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(meta_path, r#"{"id":"child","parent":"parent"}"#).unwrap();

    let dry = run(&dir, &[]);
    assert_eq!(dry["items"].as_array().unwrap().len(), 1);
    assert_eq!(dry["items"][0]["status"], "completed");
    assert_eq!(dry["items"][0]["written"], false);
    assert!(!dir.join("delegations.jsonl").exists());

    let applied = run(&dir, &["--apply"]);
    assert_eq!(applied["items"][0]["written"], true);
    let target = dir.join("delegations.jsonl");
    assert!(target.exists());
    assert_eq!(fs::read_to_string(&target).unwrap().lines().count(), 1);

    let again = run(&dir, &["--apply"]);
    assert_eq!(again["items"][0]["written"], false);
    assert!(again["skipped"].as_array().unwrap().iter().any(|line| {
        line.as_str()
            .unwrap_or_default()
            .contains("already recorded")
    }));
    assert_eq!(fs::read_to_string(&target).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn child_without_parent_link_is_skipped() {
    let dir = temp_dir("orphan");
    done_session(&dir.join("child.jsonl"), "child");
    let report = run(&dir, &["--apply"]);
    assert!(report["items"].as_array().unwrap().is_empty());
    assert!(!dir.join("delegations.jsonl").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unknown_arguments_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-callback", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown session-callback argument"),
        "{stderr}"
    );
}
