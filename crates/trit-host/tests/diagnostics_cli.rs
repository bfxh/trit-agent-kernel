//! `trit-host diagnostics` 的进程级回归：静态报告不创建目录、不遗漏 in_doubt，
//! 会话与投影摘要按传入路径出现。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::SessionEvent;
use trit_host::SessionIndex;
use trit_host::session::SessionLog;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-diagnostics-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn diagnostics(workspace: &Path, extra: &[&str]) -> (Option<i32>, serde_json::Value) {
    let mut args = vec!["diagnostics", "--workspace", workspace.to_str().unwrap()];
    args.extend_from_slice(extra);
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(&args)
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null),
    )
}

fn build_session(path: &Path) {
    let mut log = SessionLog::open(path).unwrap();
    for turn in 1..=2 {
        log.append(SessionEvent::TurnStarted {
            task: format!("task-{turn}"),
            objective: "diagnose".into(),
            max_steps: 2,
        })
        .unwrap();
        log.append(SessionEvent::TurnFinished {
            steps: 1,
            complete: true,
            metrics: None,
        })
        .unwrap();
        log.append(SessionEvent::PlanPatched {
            revision: turn as u64,
            task: format!("task-{turn}"),
            operation: "step".into(),
        })
        .unwrap();
    }
}

#[test]
fn fresh_workspace_reports_without_creating_any_directory() {
    let dir = temp_dir("fresh");
    let (code, report) = diagnostics(&dir, &[]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["journal"]["present"], false);
    assert_eq!(report["journal"]["in_doubt"], 0);
    assert_eq!(report["tools"].as_array().unwrap().len(), 7);
    let tools = report["tools"].as_array().unwrap();
    let trit = tools
        .iter()
        .find(|tool| tool["name"] == "trit_search")
        .expect("trit_search capability");
    assert!(
        trit["tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|test| test == "tests/trit_search.rs")
    );
    assert_eq!(trit["bench"], "search_compare");
    assert_eq!(report["search"]["mismatches"], 0);
    assert_eq!(report["search"]["serial_parallel_equal"], true);
    assert!(report["search"]["verdicts_checked"].as_u64().unwrap() > 0);
    assert_eq!(report["kernel"]["session"], 1);
    assert_eq!(report["kernel"]["write_journal"], 1);
    assert_eq!(report["kernel"]["rewind"], 1);
    assert_eq!(report["kernel"]["session_index"], 1);
    assert!(report["warnings"].as_array().unwrap().is_empty());
    assert!(
        !dir.join(".agent").exists(),
        "diagnostics must not create .agent/"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn journal_in_doubt_surfaces_as_a_warning() {
    let dir = temp_dir("doubt");
    let journal = dir.join(".agent/write-journal.jsonl");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(
        &journal,
        "{\"schema\":1,\"seq\":1,\"op_id\":1,\"phase\":\"begin\",\"tool\":\"write_file\",\"at\":1}\n",
    )
    .unwrap();

    let (code, report) = diagnostics(&dir, &[]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["journal"]["present"], true);
    assert_eq!(report["journal"]["in_doubt"], 1);
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap_or_default().contains("in-doubt")),
        "{warnings:?}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn session_and_projection_summaries_are_optional() {
    let dir = temp_dir("session");
    let session = dir.join("session.jsonl");
    let db = dir.join("session.index.db");
    build_session(&session);
    SessionIndex::rebuild(&session, &db).unwrap();

    let db_text = db.to_str().unwrap();
    let session_text = session.to_str().unwrap();
    let (code, report) = diagnostics(&dir, &["--session", session_text, "--db", db_text]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["session"]["turns"], 2);
    assert_eq!(report["session"]["last_seq"], 6);
    assert_eq!(report["projection"]["present"], true);
    assert_eq!(report["projection"]["summary"]["records"], 6);
    assert!(report["warnings"].as_array().unwrap().is_empty());

    fs::remove_file(&db).unwrap();
    let (code, report) = diagnostics(&dir, &["--session", session_text, "--db", db_text]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["projection"]["present"], false);
    assert!(report["projection"]["summary"].is_null());
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap_or_default().contains("missing")),
        "{warnings:?}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn default_report_has_no_live_section() {
    let dir = temp_dir("no-live");
    let (code, report) = diagnostics(&dir, &[]);
    assert_eq!(code, Some(0));
    assert!(
        report.get("live").is_none(),
        "不传 --live-mcp 时报告里不该出现 live 段"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn live_mcp_probe_collects_redacted_stderr() {
    let dir = temp_dir("live");
    let workspace = dir.to_str().unwrap();
    let line = format!("leaked {workspace} token=abcdef123456 Bearer sk-live-abcdef123456");
    #[cfg(windows)]
    let (program, script) = (
        "cmd",
        format!("echo {line} 1>&2 & ping -n 3 127.0.0.1 >nul"),
    );
    #[cfg(not(windows))]
    let (program, script) = ("sh", format!("echo {line} >&2; sleep 2"));

    let extra = vec![
        "--live-mcp",
        program,
        "--live-mcp-arg",
        if cfg!(windows) { "/C" } else { "-c" },
        "--live-mcp-arg",
        script.as_str(),
        "--live-wait-ms",
        "400",
    ];
    let (code, report) = diagnostics(&dir, &extra);
    assert_eq!(code, Some(0), "diagnostics 必须成功返回 JSON");
    let live = &report["live"];
    assert_eq!(live["launched"], true, "子进程应当被拉起");
    assert_eq!(live["program"], program);
    let lines = live["stderr_lines"]
        .as_array()
        .expect("stderr_lines 必须是数组");
    assert!(!lines.is_empty(), "启动阶段的 stderr 应当被采集");
    let text = lines
        .iter()
        .filter_map(|line| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains(workspace), "工作区路径必须脱敏: {text}");
    assert!(!text.contains("abcdef123456"), "凭据必须脱敏: {text}");
    assert!(!dir.join(".agent").exists(), "诊断不得创建 .agent 目录");
    let _ = fs::remove_dir_all(&dir);
}
