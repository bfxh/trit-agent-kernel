//! `trit-host run --write-journal` 与 `journal-status` 的进程级回归。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-journal-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn script(name: &str, path: &str) -> PathBuf {
    let file = std::env::temp_dir().join(format!(
        "trit-journal-script-{name}-{}.json",
        std::process::id()
    ));
    let body = format!(
        r#"[{{"tool_calls":[{{"call_id":"w1","tool":"write_file","read_only":false,"paths":["{path}"],"content":"y"}}]}},{{"completion":{{"criteria":[{{"id":"c1","kind":{{"kind":"diff","paths":["{path}"]}}}}]}}}}]"#
    );
    fs::write(&file, body).unwrap();
    file
}

fn host(root: &Path, extra: &[&str]) -> (Option<i32>, String) {
    let mut args = vec!["run", "--task", "x", "--workspace", root.to_str().unwrap()];
    args.extend_from_slice(extra);
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(&args)
        .output()
        .unwrap();
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn journal_status(root: &Path) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["journal-status", "--workspace", root.to_str().unwrap()])
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

fn run_with_journal(root: &Path, script_path: &Path) -> (Option<i32>, String) {
    host(
        root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/a.py",
            "--write-journal",
        ],
    )
}

#[test]
fn run_writes_journal_and_status_reports_completed() {
    let root = root("pair");
    write(&root, "src/a.py", "x");
    let script_path = script("pair", "src/a.py");
    let (code, text) = run_with_journal(&root, &script_path);
    assert_eq!(code, Some(0), "{text}");

    let path = root.join(".agent/write-journal.jsonl");
    assert!(path.is_file(), "journal file missing");
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);

    let (code, report) = journal_status(&root);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["completed"], 1);
    assert_eq!(report["in_doubt"].as_array().unwrap().len(), 0);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn journal_status_exits_one_on_unclosed_begin() {
    let root = root("doubt");
    let path = root.join(".agent/write-journal.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "{\"schema\":1,\"seq\":1,\"op_id\":1,\"phase\":\"begin\",\"tool\":\"write_file\",\"call_id\":\"crash\",\"paths\":[\"src/b.py\"],\"at\":1}\n",
    )
    .unwrap();

    let (code, report) = journal_status(&root);
    assert_eq!(code, Some(1), "{report}");
    assert_eq!(report["in_doubt"].as_array().unwrap().len(), 1);
    assert_eq!(report["in_doubt"][0]["call_id"], "crash");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn run_warns_when_journal_has_in_doubt() {
    let root = root("warn");
    write(&root, "src/a.py", "x");
    let path = root.join(".agent/write-journal.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "{\"schema\":1,\"seq\":1,\"op_id\":1,\"phase\":\"begin\",\"tool\":\"write_file\",\"call_id\":\"crash\",\"paths\":[\"src/b.py\"],\"at\":1}\n",
    )
    .unwrap();

    let script_path = script("warn", "src/a.py");
    let (_, text) = run_with_journal(&root, &script_path);
    assert!(text.contains("left in doubt"), "{text}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn journal_status_without_journal_creates_nothing() {
    let root = root("empty");
    let (code, report) = journal_status(&root);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["total"], 0);
    assert!(
        !root.join(".agent").exists(),
        "read-only status created .agent"
    );
    let _ = fs::remove_dir_all(root);
}
