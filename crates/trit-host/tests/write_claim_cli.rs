//! `trit-host run --write-claim/--write-lease` 的进程级回归。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use trit_host::OperationLease;

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-write-guard-{tag}-{}-{nanos}",
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
        "trit-write-guard-{name}-{}.json",
        std::process::id()
    ));
    let body = format!(
        r#"[{{"tool_calls":[{{"call_id":"w1","tool":"write_file","read_only":false,"paths":["{path}"],"content":"y"}}]}},{{"completion":{{"criteria":[{{"id":"c1","kind":{{"kind":"diff","paths":["{path}"]}}}}]}}}}]"#
    );
    fs::write(&file, body).unwrap();
    file
}

fn script_move(name: &str, from: &str, to: &str) -> PathBuf {
    let file = std::env::temp_dir().join(format!(
        "trit-write-guard-move-{name}-{}.json",
        std::process::id()
    ));
    let body = format!(
        r#"[{{"tool_calls":[{{"call_id":"m1","tool":"move_path","read_only":false,"paths":["{from}","{to}"]}}]}},{{"completion":{{"criteria":[{{"id":"c1","kind":{{"kind":"diff","paths":["{to}"]}}}}]}}}}]"#
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

fn lease_file(root: &Path) -> Value {
    let text = fs::read_to_string(root.join(".agent/write-lease")).unwrap();
    serde_json::from_str(text.trim_start()).unwrap()
}

#[test]
fn write_lease_requires_claim() {
    let root = root("needs-claim");
    let (code, text) = host(&root, &["--write-lease", "host-a"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.contains("--write-lease requires --write-claim"),
        "{text}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn write_claim_blocks_undeclared_write() {
    let root = root("blocked");
    write(&root, "src/a.py", "x");
    let script_path = script("blocked", "src/b.py");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/a.py",
        ],
    );
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("write claim does not allow"), "{text}");
    assert!(!root.join("src/b.py").exists());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn write_lease_binds_and_increments_epoch() {
    let root = root("epoch");
    write(&root, "src/a.py", "x");
    let script_path = script("epoch", "src/a.py");

    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/a.py",
            "--write-lease",
            "host-a",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    let first = lease_file(&root);
    assert_eq!(first["owner"], "host-a");
    assert_eq!(first["epoch"], 1);

    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/a.py",
            "--write-lease",
            "host-b",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    let second = lease_file(&root);
    assert_eq!(second["owner"], "host-b");
    assert_eq!(second["epoch"], 2);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn write_lease_is_busy_when_held() {
    let root = root("busy");
    write(&root, "src/a.py", "x");
    let script_path = script("busy", "src/a.py");
    let holder = OperationLease::acquire(&root, "test-holder").unwrap();
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/a.py",
            "--write-lease",
            "host-b",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("write lease is busy"), "{text}");
    drop(holder);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn write_claim_accepts_glob_pattern() {
    let root = root("glob");
    write(&root, "src/a.py", "x");
    let script_path = script("glob", "src/a.py");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/*.py",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn write_claim_checks_both_move_endpoints() {
    let root = root("move");
    write(&root, "src/a.py", "x");
    let script_path = script_move("move", "src/a.py", "src/b.py");

    // 目标未被 claim 覆盖：整个 move 被 blocked，源文件保持原位。
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src/a.py",
        ],
    );
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("write claim does not allow"), "{text}");
    assert!(root.join("src/a.py").exists());
    assert!(!root.join("src/b.py").exists());

    // 两端都在 claim 内：移动成功。
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--write-claim",
            "src",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    assert!(!root.join("src/a.py").exists());
    assert!(root.join("src/b.py").exists());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}
