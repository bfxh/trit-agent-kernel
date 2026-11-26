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

#[test]
fn appcontainer_temp_is_isolated() {
    let root = root("temp");
    let (code, text) = run(&root, "echo %TEMP% > temp.txt");
    assert_eq!(code, Some(0), "{text}");
    let temp = fs::read_to_string(root.join("temp.txt"))
        .unwrap()
        .to_lowercase();
    // AppContainer 会把 TEMP 指向自己的包目录，而不是用户 TEMP。
    assert!(
        temp.contains("packages") && temp.contains("trit-sandbox-"),
        "TEMP not isolated: {temp}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn appcontainer_prune_removes_recorded_profiles() {
    let root = root("prune");
    let (code, text) = run(&root, "echo hi");
    assert_eq!(code, Some(0), "{text}");
    let list = root.join(".agent/appcontainer-profiles");
    assert!(list.is_file(), "profile list missing");

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["appcontainer-prune", "--workspace", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!list.exists(), "profile list not removed");
    let _ = fs::remove_dir_all(root);
}

fn acl(root: &Path) -> String {
    let output = Command::new("icacls").arg(root).output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn appcontainer_prune_revokes_workspace_ace() {
    let root = root("revoke");
    let (code, text) = run(&root, "echo hi");
    assert_eq!(code, Some(0), "{text}");
    let before = acl(&root);
    assert!(
        before.contains("S-1-15-2-"),
        "no AppContainer ACE before prune: {before}"
    );
    let removed = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["appcontainer-prune", "--workspace", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(removed.status.success(), "{removed:?}");
    let after = acl(&root);
    assert!(
        !after.contains("S-1-15-2-"),
        "orphan ACE survived prune: {after}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn appcontainer_run_reclaims_expired_profile() {
    let root = root("ttl");
    let (code, text) = run(&root, "echo one");
    assert_eq!(code, Some(0), "{text}");
    let list = root.join(".agent/appcontainer-profiles");
    let recorded = fs::read_to_string(&list).unwrap();
    let name = recorded
        .lines()
        .next()
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .to_owned();
    // 把时间戳推到过期：下一次运行应先回收再重建。
    fs::write(&list, format!("{name}\t1\n")).unwrap();
    let (code, text) = run(&root, "echo two");
    assert_eq!(code, Some(0), "{text}");
    let refreshed = fs::read_to_string(&list).unwrap();
    let stamp = refreshed
        .lines()
        .next()
        .unwrap()
        .split('\t')
        .nth(1)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(stamp > 1, "profile timestamp not refreshed: {refreshed}");
    assert_eq!(refreshed.lines().count(), 1, "{refreshed}");
    let _ = fs::remove_dir_all(root);
}
