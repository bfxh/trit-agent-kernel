//! `trit-host run --repo-identity` 的进程级回归：仓不对时在开工前拒绝。

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn host_rejects_wrong_repo_before_running() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "run",
            "--task",
            "x",
            "--workspace",
            root.to_str().unwrap(),
            "--repo-identity",
            "--repo-expected",
            "bfxh/trit",
            "--task-kind",
            "agent",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("repo gate rejected"), "{stderr}");
    assert!(stderr.contains("repo mismatch"), "{stderr}");
}

#[test]
fn host_auto_gates_when_workspace_has_identity() {
    let root = std::env::temp_dir().join(format!(
        "trit-host-repo-auto-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".agent")).unwrap();
    fs::write(
        root.join(".agent/repo.json"),
        r#"{
          "schema": "repo-identity/1",
          "repo": "bfxh/trit",
          "role": "ternary-main",
          "owns_agent_runtime": false,
          "agent_runtime_repo": "bfxh/trit-agent-kernel"
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["run", "--task", "x", "--workspace", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("repo gate rejected"), "{stderr}");
    assert!(stderr.contains("does not own agent runtime"), "{stderr}");
    let _ = fs::remove_dir_all(root);
}
