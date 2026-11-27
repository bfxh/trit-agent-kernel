//! `trit-host session-plan` 的进程级回归：优先级、动作与链接分配。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::{SessionEvent, SessionLog, ToolOutcome};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-session-plan-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_session(path: &Path, events: &[SessionEvent]) {
    let mut log = SessionLog::open(path).unwrap();
    for event in events {
        log.append(event.clone()).unwrap();
    }
}

fn turn(task: &str) -> SessionEvent {
    SessionEvent::TurnStarted {
        task: task.into(),
        objective: "objective".into(),
        max_steps: 2,
    }
}

fn finished() -> SessionEvent {
    SessionEvent::TurnFinished {
        steps: 1,
        complete: true,
        metrics: None,
    }
}

#[test]
fn session_plan_cli_orders_inspect_before_resume_and_idle() {
    let dir = temp_dir("plan");
    write_session(
        &dir.join("blocked.jsonl"),
        &[
            turn("blocked"),
            SessionEvent::ToolCompleted {
                step: 1,
                outcome: ToolOutcome {
                    call_id: "c1".into(),
                    tool: "bash".into(),
                    in_doubt: true,
                    ..ToolOutcome::default()
                },
            },
        ],
    );
    write_session(&dir.join("open.jsonl"), &[turn("open")]);
    write_session(&dir.join("done.jsonl"), &[turn("done"), finished()]);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-plan", "--root", dir.to_str().unwrap(), "--compact"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let items = plan["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["session_id"], "blocked");
    assert_eq!(items[0]["action"], "inspect");
    assert_eq!(items[0]["priority"], 0);
    assert_eq!(items[1]["session_id"], "open");
    assert_eq!(items[1]["action"], "resume");
    assert_eq!(items[1]["task"], "open");
    assert_eq!(items[2]["session_id"], "done");
    assert_eq!(items[2]["action"], "idle");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn session_plan_cli_rejects_unknown_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-plan", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown session-plan argument"), "{stderr}");
}
