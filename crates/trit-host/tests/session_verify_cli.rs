//! `trit-host session-callback-verify` 的进程级回归：ok → child 增长后 stale。

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
        "trit-session-verify-cli-{tag}-{}-{nanos}",
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

fn run(root: &Path, command: &str, extra: &[&str]) -> serde_json::Value {
    let mut args = vec![command, "--root", root.to_str().unwrap(), "--compact"];
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
fn applied_record_is_ok_then_child_growth_is_stale() {
    let dir = temp_dir("verify");
    done_session(&dir.join("parent.jsonl"), "parent");
    let child = dir.join("child.jsonl");
    done_session(&child, "child work");
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(meta_path, r#"{"id":"child","parent":"parent"}"#).unwrap();

    let applied = run(&dir, "session-callback", &["--apply"]);
    assert_eq!(applied["items"][0]["written"], true);

    let ok = run(&dir, "session-callback-verify", &[]);
    assert_eq!(ok["checks"][0]["verdict"], "ok");

    let mut log = SessionLog::open(&child).unwrap();
    log.append(SessionEvent::PrefixChanged {
        step: 1,
        reasons: Vec::new(),
        rebasis: false,
    })
    .unwrap();

    let stale = run(&dir, "session-callback-verify", &[]);
    assert_eq!(stale["checks"][0]["verdict"], "stale");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn missing_delegation_is_reported() {
    let dir = temp_dir("missing");
    done_session(&dir.join("parent.jsonl"), "parent");
    let child = dir.join("child.jsonl");
    done_session(&child, "child");
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(meta_path, r#"{"id":"child","parent":"parent"}"#).unwrap();

    let report = run(&dir, "session-callback-verify", &[]);
    assert_eq!(report["checks"][0]["verdict"], "missing_delegation");
    let _ = fs::remove_dir_all(dir);
}
