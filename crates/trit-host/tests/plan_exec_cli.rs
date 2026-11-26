//! `trit-host plan-exec` 的进程级回归：计划交接、write_roots 执行侧强制与缓存隔离。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-plan-exec-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// planner 脚本：一次 `submit_plan`，payload 是完整的 PlanHandoff JSON。
fn planner_script(name: &str, write_roots: &[&str]) -> PathBuf {
    let handoff = serde_json::json!({
        "revision": 1,
        "tasks": [{
            "task": {"id": "a", "duration": 1},
            "requirements": {"cpu": 0, "memory": 0},
            "max_tokens": 100,
        }],
        "capsule": {"entries": [{"key": "goal", "value": "ship"}]},
        "profile": {
            "name": "executor",
            "access": "read_write",
            "write_roots": write_roots,
            "tools": ["write_file"],
            "max_tokens": 100,
        },
    })
    .to_string();
    let body = serde_json::json!([{
        "tool_calls": [{
            "call_id": "p1",
            "tool": "submit_plan",
            "read_only": true,
            "content": handoff,
        }],
    }]);
    let file = std::env::temp_dir().join(format!(
        "trit-plan-exec-planner-{name}-{}.json",
        std::process::id()
    ));
    fs::write(&file, serde_json::to_string(&body).unwrap()).unwrap();
    file
}

/// 空 planner 脚本：不提交计划。
fn empty_planner_script(name: &str) -> PathBuf {
    let file = std::env::temp_dir().join(format!(
        "trit-plan-exec-planner-empty-{name}-{}.json",
        std::process::id()
    ));
    fs::write(&file, "[]").unwrap();
    file
}

/// executor 脚本：写一个文件，再用 diff 声明它。
fn executor_script(name: &str, path: &str) -> PathBuf {
    let body = serde_json::json!([
        {
            "tool_calls": [{
                "call_id": "w1",
                "tool": "write_file",
                "read_only": false,
                "paths": [path],
                "content": "hello",
            }],
        },
        {
            "completion": {
                "criteria": [{"id": "c1", "kind": {"kind": "diff", "paths": [path]}}],
            },
        },
    ]);
    let file = std::env::temp_dir().join(format!(
        "trit-plan-exec-executor-{name}-{}.json",
        std::process::id()
    ));
    fs::write(&file, serde_json::to_string(&body).unwrap()).unwrap();
    file
}

fn plan_exec(
    root: &Path,
    planner: &Path,
    executor: &Path,
    extra: &[&str],
) -> (Option<i32>, String) {
    let mut args = vec![
        "plan-exec",
        "--task",
        "ship it",
        "--workspace",
        root.to_str().unwrap(),
        "--planner-script",
        planner.to_str().unwrap(),
        "--script",
        executor.to_str().unwrap(),
        "--max-plan-steps",
        "2",
        "--max-exec-steps",
        "3",
    ];
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

#[test]
fn plan_exec_writes_only_inside_profile_roots() {
    let root = root("inside");
    let planner = planner_script("inside", &["src"]);
    let executor = executor_script("inside", "src/a.txt");

    let (code, text) = plan_exec(
        &root,
        &planner,
        &executor,
        &["--allow-write", "--write-journal"],
    );
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("plan: plan@1#"), "缺少计划引用: {text}");
    assert_eq!(fs::read_to_string(root.join("src/a.txt")).unwrap(), "hello");
    let journal = root.join(".agent/write-journal.jsonl");
    assert!(journal.is_file(), "plan-exec 没有接上 mutation journal");
    assert_eq!(fs::read_to_string(journal).unwrap().lines().count(), 2);

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(planner);
    let _ = fs::remove_file(executor);
}

#[test]
fn plan_exec_rejects_writes_outside_profile_roots() {
    let root = root("outside");
    let planner = planner_script("outside", &["src"]);
    let executor = executor_script("outside", "docs/x.txt");

    // 即便 CLI 给了 --allow-write，计划只授权 src，写 docs 必须被拒并导致不完成。
    let (code, text) = plan_exec(&root, &planner, &executor, &["--allow-write"]);
    assert_eq!(code, Some(1), "越界写不应判完成: {text}");
    assert!(!root.join("docs/x.txt").exists(), "越界文件仍然落盘");

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(planner);
    let _ = fs::remove_file(executor);
}

#[test]
fn plan_exec_fails_before_execution_without_a_plan() {
    let root = root("noplan");
    let planner = empty_planner_script("noplan");
    let executor = executor_script("noplan", "src/a.txt");

    let (code, text) = plan_exec(&root, &planner, &executor, &["--allow-write"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("submit_plan"), "{text}");
    assert!(!root.join("src/a.txt").exists(), "执行阶段不应启动");

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(planner);
    let _ = fs::remove_file(executor);
}

#[test]
fn plan_exec_json_reports_plan_and_completion() {
    let root = root("json");
    let planner = planner_script("json", &["src"]);
    let executor = executor_script("json", "src/a.txt");

    let (code, text) = plan_exec(
        &root,
        &planner,
        &executor,
        &["--allow-write", "--output-format", "json"],
    );
    assert_eq!(code, Some(0), "{text}");
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert!(
        value["plan_ref"].as_str().unwrap().starts_with("plan@1#"),
        "计划引用缺失: {value}"
    );
    assert!(value["complete"].as_bool().unwrap(), "{value}");
    assert_eq!(value["plan"]["profile"]["write_roots"][0], "src");

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(planner);
    let _ = fs::remove_file(executor);
}
