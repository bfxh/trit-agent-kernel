//! `trit-host session-dispatch` 的进程级回归：dry-run 不写、apply 生成新会话、
//! 非 resume 项不派发。

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
        "trit-session-dispatch-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn open_session(path: &Path) {
    let mut log = SessionLog::open(path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "unfinished".into(),
        objective: "original".into(),
        max_steps: 2,
    })
    .unwrap();
}

fn run(root: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec![
        "session-dispatch",
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
fn dry_run_then_apply_creates_a_resumable_session() {
    let dir = temp_dir("apply");
    open_session(&dir.join("open.jsonl"));

    let dry = run(&dir, &[]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["items"].as_array().unwrap().len(), 1);
    assert_eq!(dry["items"][0]["applied"], false);
    assert_eq!(dry["items"][0]["action"], "create_session");
    let target = dry["items"][0]["target_path"].as_str().unwrap().to_owned();
    assert!(!Path::new(&target).exists(), "dry-run must not write");

    let applied = run(&dir, &["--apply"]);
    assert_eq!(applied["dry_run"], false);
    assert_eq!(applied["items"][0]["applied"], true);
    assert!(Path::new(&target).exists(), "apply must write the target");

    let scan = trit_host::scan_sessions(&dir).unwrap();
    assert!(
        scan.sessions
            .iter()
            .any(|entry| entry.session_id == "open.resume"),
        "{:?}",
        scan.sessions
            .iter()
            .map(|entry| entry.session_id.clone())
            .collect::<Vec<_>>()
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn read_only_sessions_are_not_dispatched() {
    let dir = temp_dir("readonly");
    fs::write(
        dir.join("chat.jsonl"),
        "{\"role\":\"user\",\"content\":\"hello\"}\n{\"role\":\"assistant\",\"content\":\"hi\"}\n",
    )
    .unwrap();
    let report = run(&dir, &["--apply"]);
    assert!(report["items"].as_array().unwrap().is_empty());
    assert_eq!(report["skipped"].as_array().unwrap().len(), 1);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unknown_arguments_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-dispatch", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown session-dispatch argument"),
        "{stderr}"
    );
}
