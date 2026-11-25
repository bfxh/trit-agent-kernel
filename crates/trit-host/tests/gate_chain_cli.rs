//! `trit-host run --gate-chain` 的进程级回归：三道门都在开工前判，拒绝即退出 2。
//!
//! 放行用例不追求跑完一次完整 agent：门链在 `--script` 校验之前执行，所以绿链
//! 会走到 “--script is required”，只要 stderr 里没有 `gate chain rejected` 就说明
//! 门链放行了。

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
        "trit-host-gate-chain-{tag}-{}-{nanos}",
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

fn fixture(root: &Path) {
    write(
        root,
        ".agent/repo.json",
        r#"{"schema":"repo-identity/1","repo":"bfxh/trit-agent-kernel","role":"agent-kernel",
"owns_agent_runtime":true,"agent_runtime_repo":"bfxh/trit-agent-kernel","notes":"fixture"}"#,
    );
    write(
        root,
        "alignment.json",
        r#"{"schema":"alignment/1","goal":{"id":"G1","text":"ship it"},
"requirements":[{"id":"R1","text":"a","priority":"must","parent":"G1",
"artifacts":["A1"],"evidence":["E1"],"changes":["src/a.py"]}],
"artifacts":[{"id":"A1","path":"src/a.py"}],
"evidence":[{"id":"E1","path":"tests/a.py","locator":"text:test_a"}]}"#,
    );
    write(root, "src/a.py", "print(1)\n");
    write(root, "tests/a.py", "def test_a(): pass\n");
    write(
        root,
        "packet.json",
        r#"{"id":"h-001","from":"agent-a","to":"agent-b","goal":"run",
"base_commit":"6f39dee","state_hash":"H","decisions":[],"open_questions":[],
"claims":[],"evidence":[],"next_action":"run","idempotency_key":"h-001-key",
"expires_at":10000000000}"#,
    );
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

#[test]
fn host_allows_when_gate_chain_passes() {
    let root = root("green");
    fixture(&root);
    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
        ],
    );
    // 门链已放行；后续失败在 --script 校验，不是门链拒绝。
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("gate chain rejected"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_rejects_at_repo_before_delivery_and_alignment() {
    let root = root("repo");
    fixture(&root);
    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--repo-expected",
            "bfxh/trit",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.contains("gate chain rejected at repo: repo mismatch"),
        "{text}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_rejects_at_delivery_when_receiver_hash_differs() {
    let root = root("delivery");
    fixture(&root);
    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "X",
            "--alignment-manifest",
            "alignment.json",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain rejected at delivery"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_rejects_at_alignment_before_running() {
    let root = root("alignment");
    fixture(&root);
    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            ".agent/alignment.json",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain rejected at alignment"), "{text}");
    assert!(text.contains("manifest-missing"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_requires_packet_when_state_hash_is_given() {
    let root = root("hash-flag");
    fixture(&root);
    let (code, text) = host(&root, &["--handoff-state-hash", "H"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.contains("--handoff-state-hash requires --handoff-packet"),
        "{text}"
    );
    let _ = fs::remove_dir_all(root);
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_repo(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "trit"]);
    git(root, &["config", "user.email", "trit@localhost"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fixture"]);
}

#[test]
fn host_uses_git_changes_and_rejects_unlinked_path() {
    let root = root("git-changed");
    fixture(&root);
    git_repo(&root);
    write(&root, "src/b.py", "print(2)\n");

    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain rejected at alignment"), "{text}");
    assert!(text.contains("unlinked-change"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_explicit_changed_overrides_git_lookup() {
    let root = root("explicit-changed");
    fixture(&root);
    git_repo(&root);
    write(&root, "src/b.py", "print(2)\n");

    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--alignment-changed",
            "src/a.py",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("gate chain rejected"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_can_disable_git_changed_lookup() {
    let root = root("no-git-changed");
    fixture(&root);
    git_repo(&root);
    write(&root, "src/b.py", "print(2)\n");

    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--alignment-no-git-changed",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("gate chain rejected"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_changed_set_enforces_declared_changes() {
    let root = root("changed-set");
    fixture(&root);
    git_repo(&root);
    write(&root, "src/b.py", "print(2)\n");

    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--alignment-changed",
            "src/b.py",
            "--alignment-changed-set",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain rejected at alignment"), "{text}");
    assert!(text.contains("change"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_attaches_delivery_when_requested() {
    let root = root("attach");
    fixture(&root);
    let (code, text) = host(
        &root,
        &[
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--handoff-attach",
            "--alignment-manifest",
            "alignment.json",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("gate chain rejected"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let journal = trit_orch::delivery::DeliveryJournal::load(&root, "h-001").unwrap();
    assert_eq!(journal.state_names(), vec!["sent", "delivered"]);
    let _ = fs::remove_dir_all(root);
}
