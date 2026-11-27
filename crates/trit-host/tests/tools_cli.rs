//! `trit-host tools` 的进程级回归：不连 MCP 时只列内置工具。

use std::process::Command;

#[test]
fn tools_lists_builtin_tools_without_mcp() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["tools", "--compact"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = report["builtin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"browser"));
    assert!(report.get("mcp").is_none());
}

#[test]
fn tools_rejects_unknown_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["tools", "--nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown tools argument"), "{stderr}");
}
