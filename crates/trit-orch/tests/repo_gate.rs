//! `repo-gate` 的进程级回归：串仓必须退出 1，不能只靠调用方自觉。

mod cli_support;

use std::path::Path;

use serde_json::json;

#[test]
fn repo_gate_accepts_agent_kernel_and_rejects_ternary_main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (code, value) = cli_support::run(
        "repo-gate",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit-agent-kernel",
            "task_kind": "agent",
        }),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["gate"], "True");
    assert_eq!(value["identity"]["repo"], "bfxh/trit-agent-kernel");

    let (code, value) = cli_support::run(
        "repo-gate",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit",
            "task_kind": "agent",
        }),
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["kind"], "repo_scope_rejected", "{value}");
    assert!(value["error"].as_str().unwrap().contains("repo mismatch"));
}

#[test]
fn repo_gate_rejects_missing_identity() {
    let root = std::env::temp_dir().join(format!("trit-repo-gate-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let (code, value) = cli_support::run(
        "repo-gate",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit-agent-kernel",
            "task_kind": "agent",
        }),
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["kind"], "repo_scope_rejected", "{value}");
    let _ = std::fs::remove_dir_all(root);
}
