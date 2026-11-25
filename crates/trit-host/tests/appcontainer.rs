#![cfg(windows)]

//! Windows AppContainer 内置沙箱的进程级回归：workspace 内可写、外部不可写。

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
        "trit-appcontainer-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn run(root: &Path, command: &str) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "appcontainer-run",
            "--workspace",
            root.to_str().unwrap(),
            "--",
            "cmd",
            "/C",
            command,
        ])
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
fn appcontainer_writes_inside_workspace() {
    let root = root("inside");
    let (code, text) = run(&root, "echo hi > out.txt");
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        fs::read_to_string(root.join("out.txt")).unwrap().trim(),
        "hi"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn appcontainer_cannot_write_outside_workspace() {
    let root = root("outside");
    let escape = format!(
        "C:\\Windows\\Temp\\trit-ac-escape-{}.txt",
        std::process::id()
    );
    let _ = fs::remove_file(&escape);
    let (_, text) = run(&root, &format!("echo x > {escape}"));
    assert!(
        !Path::new(&escape).exists(),
        "appcontainer wrote outside workspace: {text}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_run_uses_builtin_sandbox_for_shell() {
    let root = root("run");
    let script = std::env::temp_dir().join(format!(
        "trit-appcontainer-script-{}.json",
        std::process::id()
    ));
    fs::write(
        &script,
        r#"[{"tool_calls":[{"call_id":"s1","tool":"bash","read_only":false,"command":"echo hi > out.txt"}]}]"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "run",
            "--task",
            "t",
            "--workspace",
            root.to_str().unwrap(),
            "--allow-shell",
            "--sandbox",
            "builtin",
            "--script",
            script.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join("out.txt").exists(),
        "builtin sandbox did not run shell: {text}"
    );
    assert_eq!(
        fs::read_to_string(root.join("out.txt")).unwrap().trim(),
        "hi"
    );
    let _ = fs::remove_file(script);
    let _ = fs::remove_dir_all(root);
}
