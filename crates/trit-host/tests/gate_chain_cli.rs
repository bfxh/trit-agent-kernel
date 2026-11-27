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
"artifacts":["A1"],"evidence":["E1"],"changes":["src/a.rs"]}],
"artifacts":[{"id":"A1","path":"src/a.rs"}],
"evidence":[{"id":"E1","path":"tests/a.rs","locator":"text:test_a"}]}"#,
    );
    write(root, "src/a.rs", "print(1)\n");
    write(root, "tests/a.rs", "def test_a(): pass\n");
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
    write(&root, "src/b.rs", "print(2)\n");

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
    write(&root, "src/b.rs", "print(2)\n");

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
            "src/a.rs",
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
    write(&root, "src/b.rs", "print(2)\n");

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
    write(&root, "src/b.rs", "print(2)\n");

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
            "src/b.rs",
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

#[test]
fn host_alignment_flags_alone_enable_the_chain() {
    let root = root("flag-only");
    fixture(&root);
    // 只给 alignment 开关、不给 --gate-chain：也必须进链，而不是静默忽略参数。
    let (code, text) = host(&root, &["--alignment-no-git-changed"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain rejected at delivery"), "{text}");
    let _ = fs::remove_dir_all(root);
}

fn script(name: &str, path: &str) -> PathBuf {
    let file = std::env::temp_dir().join(format!(
        "trit-host-gate-chain-{name}-{}.json",
        std::process::id()
    ));
    let body = format!(
        r#"[
  {{"tool_calls":[{{"call_id":"w1","tool":"write_file","read_only":false,"paths":["{path}"],"content":"x"}}]}},
  {{"completion":{{"criteria":[{{"id":"c1","kind":{{"kind":"diff","paths":["{path}"]}}}}]}}}}
]"#
    );
    fs::write(&file, body).unwrap();
    file
}

#[test]
fn host_normalizes_alignment_changed_paths() {
    let root = root("normalize");
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
            "--alignment-changed",
            ".\\src\\a.rs",
        ],
    );
    // `.\src\a.rs` 折成 `src/a.rs`，命中清单的 changes，门链放行到 script 校验。
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("gate chain rejected"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_rejects_alignment_changed_escape() {
    let root = root("changed-escape");
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
            "--alignment-changed",
            "../outside.rs",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.contains("--alignment-changed escapes the workspace"),
        "{text}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_post_passes_when_changes_align() {
    let root = root("post-pass");
    fixture(&root);
    git_repo(&root);
    let script_path = script("post-pass", "src/a.rs");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("gate chain post: aligned"), "{text}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_post_rejects_unlinked_change() {
    let root = root("post-fail");
    fixture(&root);
    git_repo(&root);
    let script_path = script("post-fail", "src/b.rs");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post",
        ],
    );
    // run 自己成功（exit 0 的裁决），post 用完整变更集复核后拒绝。
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain post rejected"), "{text}");
    assert!(text.contains("unlinked-change"), "{text}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_post_requires_git_workspace() {
    let root = root("post-no-git");
    fixture(&root);
    let script_path = script("post-no-git", "src/a.rs");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("not a git repository"), "{text}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_writes_gate_chain_reports() {
    let root = root("reports");
    fixture(&root);
    git_repo(&root);
    let script_path = script("reports", "src/a.rs");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-report",
            ".agent/gate-chain/pre.json",
            "--gate-chain-post-report",
            ".agent/gate-chain/post.json",
            "--gate-chain-post",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    let pre: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".agent/gate-chain/pre.json")).unwrap())
            .unwrap();
    assert_eq!(pre["schema"], "gate-chain/1");
    assert_eq!(pre["status"], "passed");
    let post: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".agent/gate-chain/post.json")).unwrap())
            .unwrap();
    assert_eq!(post["status"], "aligned");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_post_report_is_written_on_failure() {
    let root = root("post-report-fail");
    fixture(&root);
    git_repo(&root);
    let script_path = script("post-report-fail", "src/b.rs");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post-report",
            ".agent/gate-chain/post.json",
            "--gate-chain-post",
        ],
    );
    assert_eq!(code, Some(2), "{text}");
    let post: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".agent/gate-chain/post.json")).unwrap())
            .unwrap();
    assert_eq!(post["status"], "unaligned");
    assert!(!post["failures"].as_array().unwrap().is_empty());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_logs_gate_chain_to_session() {
    let root = root("session-log");
    fixture(&root);
    git_repo(&root);
    let script_path = script("session-log", "src/a.rs");
    let session = root.join("session.jsonl");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post",
        ],
    );
    // session 自己也在 workspace 内：post 必须把它从 changed 里排除。
    assert_eq!(code, Some(0), "{text}");

    let records = trit_host::SessionLog::open(&session)
        .unwrap()
        .load()
        .unwrap();
    let mut phases = Vec::new();
    let mut statuses = Vec::new();
    for record in &records {
        if let trit_host::SessionEvent::GateChain { phase, status, .. } = &record.event {
            phases.push(match phase {
                trit_host::GateChainPhase::Pre => "pre",
                trit_host::GateChainPhase::Post => "post",
            });
            statuses.push(status.as_str());
        }
    }
    assert_eq!(phases, ["pre", "post"], "{records:?}");
    assert_eq!(statuses, ["passed", "aligned"], "{records:?}");

    // 门链事件不改变 turn 状态机：recover 仍然只看到一轮。
    let state = trit_host::recover(&records).unwrap();
    assert_eq!(state.turns.len(), 1, "{state:?}");
    assert_eq!(state.gate_chains.len(), 2, "{state:?}");
    assert_eq!(state.gate_chains[0].phase, trit_host::GateChainPhase::Pre);
    assert_eq!(state.gate_chains[0].status, "passed");
    assert_eq!(state.gate_chains[1].phase, trit_host::GateChainPhase::Post);
    assert_eq!(state.gate_chains[1].status, "aligned");

    // 恢复包也带同一份门链历史，宿主重连后不需要重读事件。
    let recovery = trit_host::recovery_context_for_session(&session).unwrap();
    assert_eq!(recovery.gate_chains.len(), 2);
    assert_eq!(recovery.gate_chains[0].status, "passed");
    assert_eq!(recovery.gate_chains[1].status, "aligned");
    assert!(!recovery.gate_chain_blocked());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_logs_post_failure_to_session() {
    let root = root("session-post-fail");
    fixture(&root);
    git_repo(&root);
    let script_path = script("session-post-fail", "src/b.rs");
    let session = root.join("session.jsonl");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post",
        ],
    );
    assert_eq!(code, Some(2), "{text}");

    let records = trit_host::SessionLog::open(&session)
        .unwrap()
        .load()
        .unwrap();
    let post = records
        .iter()
        .find_map(|record| match &record.event {
            trit_host::SessionEvent::GateChain {
                phase: trit_host::GateChainPhase::Post,
                status,
                failures,
                ..
            } => Some((status.clone(), *failures)),
            _ => None,
        })
        .expect("post gate_chain event");
    assert_eq!(post.0, "unaligned");
    assert!(post.1 > 0, "{post:?}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_report_path_rejects_workspace_escape() {
    let root = root("report-escape");
    fixture(&root);
    let name = format!("gate-chain-host-escape-{}.json", std::process::id());
    let escaped = root.parent().unwrap().join(&name);
    let _ = fs::remove_file(&escaped);
    let (code, text) = host(&root, &["--gate-chain-report", &format!("../{name}")]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("escapes the workspace"), "{text}");
    assert!(
        !escaped.exists(),
        "report escaped the workspace: {escaped:?}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_report_flag_alone_enables_the_chain() {
    let root = root("report-flag-only");
    fixture(&root);
    let (code, text) = host(
        &root,
        &["--gate-chain-report", ".agent/gate-chain/pre.json"],
    );
    // 只给报告路径也会进链；delivery 未配置，按 Unknown 停链。
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("gate chain rejected at delivery"), "{text}");
    assert!(root.join(".agent/gate-chain/pre.json").exists(), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_post_report_flag_alone_enables_post_check() {
    let root = root("post-report-flag");
    fixture(&root);
    git_repo(&root);
    let script_path = script("post-report-flag", "src/a.rs");
    let (code, text) = host(
        &root,
        &[
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--gate-chain",
            "--handoff-packet",
            "packet.json",
            "--handoff-state-hash",
            "H",
            "--alignment-manifest",
            "alignment.json",
            "--gate-chain-post-report",
            ".agent/gate-chain/post.json",
        ],
    );
    assert_eq!(code, Some(0), "{text}");
    assert!(root.join(".agent/gate-chain/post.json").exists(), "{text}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(script_path);
}

#[test]
fn host_disables_git_fsmonitor() {
    let root = root("fsmonitor");
    fixture(&root);
    git_repo(&root);
    #[cfg(windows)]
    let hook = "cmd /c echo pwned > fmon.txt";
    #[cfg(not(windows))]
    let hook = "sh -c 'echo pwned > fmon.txt'";
    git(&root, &["config", "core.fsmonitor", hook]);
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
    // 门链读取 git status 时不能执行仓库自带命令。
    assert_eq!(code, Some(2), "{text}");
    assert!(
        !root.join("fmon.txt").exists(),
        "fsmonitor executed: {text}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_ignores_repo_configured_worktree() {
    let root = root("worktree");
    fixture(&root);
    git_repo(&root);
    let outside = root.parent().unwrap().join(format!(
        "{}-outside",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("outside.txt"), "x").unwrap();
    git(
        &root,
        &["config", "core.worktree", outside.to_str().unwrap()],
    );
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
    // 仓库配置的 worktree 不能把扫描范围带出 workspace。
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("unlinked-change"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(root);
}

// APFS 在 open 时就拒绝非 UTF-8 文件名（EILSEQ），无法构造该场景；Linux 可以。
#[cfg(target_os = "linux")]
#[test]
fn host_gives_up_on_non_utf8_git_paths() {
    use std::os::unix::ffi::OsStrExt;

    let root = root("non-utf8");
    fixture(&root);
    git_repo(&root);
    let name = std::ffi::OsStr::from_bytes(b"non-utf8-\xFF.txt");
    fs::write(root.join(name), "x").unwrap();
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
    // 非 UTF-8 路径让自动 changed 放弃取值；门链只做结构检查，不误报。
    assert_eq!(code, Some(2), "{text}");
    assert!(!text.contains("unlinked-change"), "{text}");
    assert!(text.contains("--script is required"), "{text}");
    let _ = fs::remove_dir_all(root);
}
