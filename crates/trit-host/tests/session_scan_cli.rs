//! `trit-host session-scan` 的进程级回归：多会话清单、planner/executor 与父子链接、
//! 坏行/未完成 turn/门链健康信息。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::{GateChainPhase, SessionEvent, SessionLog};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-session-scan-cli-{tag}-{}-{nanos}",
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

#[test]
fn session_scan_cli_reports_tasks_links_and_health() {
    let dir = temp_dir("report");
    write_session(
        &dir.join("planner.jsonl"),
        &[
            SessionEvent::TurnStarted {
                task: "plan".into(),
                objective: "split".into(),
                max_steps: 2,
            },
            SessionEvent::TurnFinished {
                steps: 1,
                complete: true,
                metrics: None,
            },
        ],
    );
    write_session(
        &dir.join("executor.jsonl"),
        &[
            SessionEvent::TurnStarted {
                task: "exec".into(),
                objective: "run".into(),
                max_steps: 4,
            },
            SessionEvent::GateChain {
                phase: GateChainPhase::Post,
                status: "failed".into(),
                exit: 1,
                failed_at: Some("test".into()),
                failures: 1,
                passed: 0,
                blocked: 0,
                unknown: 0,
                skipped: 0,
                report: None,
            },
        ],
    );

    let child = dir.join("child.jsonl");
    write_session(
        &child,
        &[SessionEvent::TurnStarted {
            task: "child".into(),
            objective: "work".into(),
            max_steps: 1,
        }],
    );
    let mut meta_path = child.as_os_str().to_os_string();
    meta_path.push(".meta");
    fs::write(
        meta_path,
        r#"{"id":"child-1","parent":"planner","role":"subagent","model":"m","finished_at":null}"#,
    )
    .unwrap();
    let mut text = fs::read_to_string(&child).unwrap();
    text.push_str("not json\n");
    fs::write(&child, text).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-scan", "--root", dir.to_str().unwrap(), "--compact"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scanned"], 3);
    let sessions = report["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3);

    let links = report["links"].as_array().unwrap();
    assert!(
        links.iter().any(|link| link["kind"] == "planner_executor"),
        "{links:?}"
    );
    assert!(
        links.iter().any(|link| {
            link["kind"] == "parent_child" && link["from"] == "planner" && link["to"] == "child-1"
        }),
        "{links:?}"
    );

    let child_entry = sessions
        .iter()
        .find(|entry| entry["session_id"] == "child-1")
        .unwrap();
    assert_eq!(child_entry["role"], "subagent");
    assert_eq!(child_entry["corrupt_lines"], 1);
    assert_eq!(child_entry["open_turn"], true);
    assert_eq!(child_entry["tasks"][0]["task"], "child");

    let executor = sessions
        .iter()
        .find(|entry| entry["session_id"] == "executor")
        .unwrap();
    assert_eq!(executor["last_gate"]["passed"], false);
    assert_eq!(executor["open_turn"], true);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn session_scan_cli_rejects_unknown_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-scan", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown session-scan argument"), "{stderr}");
}
