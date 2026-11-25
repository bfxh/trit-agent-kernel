//! `trit-host run --repo-identity` 的进程级回归：仓不对时在开工前拒绝。

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
