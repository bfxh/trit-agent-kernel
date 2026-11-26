//! `trit-host rewind-status`：checkpoint 回退 journal 的进程级回归。

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
        "trit-rewind-status-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn rewind_status(dir: &Path) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["rewind-status", "--checkpoint-dir", dir.to_str().unwrap()])
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

fn rewind_status_recover(dir: &Path, workspace: &Path) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "rewind-status",
            "--checkpoint-dir",
            dir.to_str().unwrap(),
            "--recover",
            "--workspace",
            workspace.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn unfinished_rewind_reports_in_doubt_and_exits_one() {
    let dir = root("doubt");
    let journal = dir.join("rewind-journal.jsonl");
    fs::write(
        &journal,
        "{\"schema\":1,\"seq\":1,\"op_id\":1,\"phase\":\"begin\",\"turn\":0,\"paths\":[\"a.txt\"],\"path\":\"\",\"restored\":[],\"deleted\":[],\"failures\":[],\"at\":1}\n\
         {\"schema\":1,\"seq\":2,\"op_id\":1,\"phase\":\"applying\",\"turn\":0,\"paths\":[],\"path\":\"a.txt\",\"restored\":[],\"deleted\":[],\"failures\":[],\"at\":2}\n",
    )
    .unwrap();

    let (code, report) = rewind_status(&dir);
    assert_eq!(code, Some(1), "{report}");
    assert_eq!(report["in_doubt"].as_array().unwrap().len(), 1);
    assert_eq!(report["in_doubt"][0]["planned"][0], "a.txt");
    assert_eq!(report["in_doubt"][0]["applied"][0], "a.txt");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn empty_journal_reports_clean_and_exits_zero() {
    let dir = root("empty");
    let (code, report) = rewind_status(&dir);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["total"], 0);
    assert_eq!(report["in_doubt"].as_array().unwrap().len(), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_restores_workspace_and_exits_zero() {
    let dir = root("recover");
    let workspace = root("recover-workspace");
    fs::write(workspace.join("a.txt"), "half-a").unwrap();
    fs::write(workspace.join("b.txt"), "half-b").unwrap();
    let backups = dir.join("rewind-backups").join("1");
    fs::create_dir_all(&backups).unwrap();
    fs::write(backups.join("0"), "before-a").unwrap();
    fs::write(
        dir.join("rewind-journal.jsonl"),
        "{\"schema\":1,\"seq\":1,\"op_id\":1,\"phase\":\"begin\",\"turn\":0,\"paths\":[\"a.txt\",\"b.txt\"],\"backups\":[{\"path\":\"a.txt\",\"existed\":true,\"backup\":\"0\"},{\"path\":\"b.txt\",\"existed\":false,\"backup\":null}],\"path\":\"\",\"restored\":[],\"deleted\":[],\"failures\":[],\"at\":1}\n\
         {\"schema\":1,\"seq\":2,\"op_id\":1,\"phase\":\"applying\",\"turn\":0,\"paths\":[],\"path\":\"a.txt\",\"restored\":[],\"deleted\":[],\"failures\":[],\"at\":2}\n\
         {\"schema\":1,\"seq\":3,\"op_id\":1,\"phase\":\"applying\",\"turn\":0,\"paths\":[],\"path\":\"b.txt\",\"restored\":[],\"deleted\":[],\"failures\":[],\"at\":3}\n",
    )
    .unwrap();

    let (code, report) = rewind_status_recover(&dir, &workspace);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["recovery"]["attempted"], 1);
    assert_eq!(report["recovery"]["recovered"], 1);
    assert_eq!(report["recovery"]["failed"].as_array().unwrap().len(), 0);
    assert_eq!(report["status"]["in_doubt"].as_array().unwrap().len(), 0);
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "before-a"
    );
    assert!(!workspace.join("b.txt").exists());
    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn recover_without_workspace_is_a_usage_error() {
    let dir = root("recover-nousage");
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "rewind-status",
            "--checkpoint-dir",
            dir.to_str().unwrap(),
            "--recover",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let _ = fs::remove_dir_all(dir);
}
