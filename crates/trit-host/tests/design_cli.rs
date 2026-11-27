//! `run` 的设计分：默认写入 JSON/Markdown；`--no-design-score` 关闭；
//! `--design-block-below` 显式选择让分数参与决策。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn workspace(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-host-design-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(path.join("src")).unwrap();
    path
}

/// 写一个"干净"的 Rust 产物 + 对应的完成声明。
fn script(tag: &str) -> PathBuf {
    let file = std::env::temp_dir().join(format!(
        "trit-host-design-{tag}-{}.json",
        std::process::id()
    ));
    let body = serde_json::json!([
        {"tool_calls": [{
            "call_id": "w1",
            "tool": "write_file",
            "read_only": false,
            "paths": ["src/a.rs"],
            "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        }]},
        {"completion": {"criteria": [{
            "id": "c1",
            "kind": {"kind": "diff", "paths": ["src/a.rs"]},
        }]}},
    ]);
    std::fs::write(&file, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    file
}

fn run(root: &Path, script: &Path, extra: &[&str]) -> (Option<i32>, String, String) {
    let mut args = vec![
        "run",
        "--task",
        "design",
        "--workspace",
        root.to_str().unwrap(),
        "--allow-write",
        "--script",
        script.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(&args)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn run_reports_design_score_in_json() {
    let root = workspace("json");
    let script = script("json");
    let (code, stdout, stderr) = run(&root, &script, &["--output-format", "json"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    let design = value.get("design").expect("design section");
    assert_eq!(design["files"], 1, "{design}");
    assert_eq!(design["overall"], 100, "{design}");
    assert!(
        design["axes"]
            .as_array()
            .is_some_and(|axes| axes.len() == 4),
        "{design}"
    );
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(script);
}

#[test]
fn no_design_score_omits_the_section() {
    let root = workspace("off");
    let script = script("off");
    let (code, stdout, stderr) = run(
        &root,
        &script,
        &["--output-format", "json", "--no-design-score"],
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    assert!(
        value.get("design").is_none(),
        "关闭后不应出现 design 段: {value}"
    );
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(script);
}

#[test]
fn design_block_below_fails_the_run() {
    let root = workspace("block");
    let script = script("block");
    // 干净产物是 100 分；阈值 101 一定低于 → 显式参与决策时必须失败。
    let (code, _stdout, stderr) = run(
        &root,
        &script,
        &["--output-format", "json", "--design-block-below", "101"],
    );
    assert_ne!(code, Some(0), "低于阈值必须失败");
    assert!(stderr.contains("design score"), "{stderr}");
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(script);
}
