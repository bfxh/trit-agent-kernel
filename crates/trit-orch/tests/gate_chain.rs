//! `gate-chain` 的库级与进程级回归：三门串联、stop-first、只读、退出码。
//!
//! 每个用例用独立临时根：`.agent/repo.json`、`packet.json`、`alignment.json`
//! 都在根内构造，避免依赖仓库自身的 `.agent/` 状态。

#[allow(dead_code)]
mod cli_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use trit_orch::alignment::AlignmentOptions;
use trit_orch::gate_chain::{self, AlignmentStep, DeliveryStep, GateChainOptions};

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-gate-chain-{tag}-{}-{nanos}",
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

fn identity(root: &Path, repo: &str, owns_agent_runtime: bool) {
    write(
        root,
        ".agent/repo.json",
        &format!(
            r#"{{"schema":"repo-identity/1","repo":"{repo}","role":"agent-kernel",
"owns_agent_runtime":{owns_agent_runtime},"agent_runtime_repo":"{repo}","notes":"fixture"}}"#
        ),
    );
}

fn packet(root: &Path, state_hash: &str) -> PathBuf {
    let path = root.join("packet.json");
    let body = json!({
        "id": "h-001",
        "from": "agent-a",
        "to": "agent-b",
        "goal": "run the gate chain",
        "base_commit": "6f39dee",
        "state_hash": state_hash,
        "decisions": ["read-only chain"],
        "open_questions": [],
        "claims": [],
        "evidence": [],
        "next_action": "run gate-chain",
        "idempotency_key": "h-001-key",
        "expires_at": 10_000_000_000u64,
    });
    fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    path
}

fn alignment(root: &Path) {
    write(root, "src/a.py", "print(1)\n");
    write(root, "tests/a.py", "def test_a(): pass\n");
    write(
        root,
        "alignment.json",
        &json!({
            "schema": "alignment/1",
            "goal": {"id": "G1", "text": "ship the chain"},
            "requirements": [{
                "id": "R1",
                "text": "the chain runs read-only",
                "priority": "must",
                "parent": "G1",
                "artifacts": ["A1"],
                "evidence": ["E1"],
                "changes": ["src/a.py"]
            }],
            "artifacts": [{"id": "A1", "path": "src/a.py"}],
            "evidence": [{"id": "E1", "path": "tests/a.py", "locator": "text:test_a"}]
        })
        .to_string(),
    );
}

fn options(root: &Path) -> GateChainOptions {
    packet(root, "H");
    alignment(root);
    GateChainOptions {
        root: root.to_path_buf(),
        expected_repo: "bfxh/trit-agent-kernel".to_owned(),
        task_kind: "agent".to_owned(),
        delivery: Some(DeliveryStep {
            packet: root.join("packet.json"),
            receiver_state_hash: "H".to_owned(),
            at: 1_000,
        }),
        alignment: Some(AlignmentStep {
            manifest: PathBuf::from("alignment.json"),
            options: AlignmentOptions::default(),
        }),
    }
}

#[test]
fn chain_passes_when_all_three_gates_pass() {
    let root = root("green");
    identity(&root, "bfxh/trit-agent-kernel", true);
    let report = gate_chain::run(&options(&root));

    assert_eq!(
        report.status,
        "passed",
        "{}",
        serde_json::to_string(&report).unwrap()
    );
    assert_eq!(report.exit, 0);
    assert_eq!(report.failed_at, None);
    assert_eq!(report.counts.passed, 3);
    assert_eq!(report.counts.blocked, 0);
    assert_eq!(report.counts.unknown, 0);
    assert_eq!(report.counts.skipped, 0);
    let ids: Vec<&str> = report.steps.iter().map(|step| step.id.as_str()).collect();
    assert_eq!(ids, ["repo", "delivery", "alignment"]);
    assert!(report.steps.iter().all(|step| step.state == "passed"));
    // 门链只读：delivery 只做 check，不补 sent/delivered 账本。
    assert!(!root.join(".agent/delivery").exists());
}

#[test]
fn chain_stops_at_repo_before_touching_later_gates() {
    let root = root("cross-repo");
    identity(&root, "bfxh/trit-agent-kernel", true);
    let mut options = options(&root);
    options.expected_repo = "bfxh/trit".to_owned();
    let report = gate_chain::run(&options);

    assert_eq!(report.status, "blocked");
    assert_eq!(report.exit, 1);
    assert_eq!(report.failed_at.as_deref(), Some("repo"));
    assert_eq!(report.steps.len(), 3);
    assert_eq!(report.steps[0].state, "blocked");
    assert!(report.steps[0].reason.contains("repo mismatch"));
    assert_eq!(report.steps[1].state, "skipped");
    assert_eq!(report.steps[2].state, "skipped");
    assert_eq!(report.counts.skipped, 2);
    assert!(!root.join(".agent/delivery").exists());
}

#[test]
fn chain_stops_at_delivery_when_receiver_hash_differs() {
    let root = root("hash-mismatch");
    identity(&root, "bfxh/trit-agent-kernel", true);
    let mut options = options(&root);
    options.delivery.as_mut().unwrap().receiver_state_hash = "X".to_owned();
    let report = gate_chain::run(&options);

    // 哈希不一致是 Unknown：先对账，不是结构不合法。
    assert_eq!(report.status, "unknown");
    assert_eq!(report.exit, 3);
    assert_eq!(report.failed_at.as_deref(), Some("delivery"));
    assert_eq!(report.steps[1].state, "unknown");
    assert_eq!(report.steps[2].state, "skipped");
    assert!(!root.join(".agent/delivery").exists());
}

#[test]
fn chain_fails_when_alignment_is_unaligned() {
    let root = root("unaligned");
    identity(&root, "bfxh/trit-agent-kernel", true);
    let mut options = options(&root);
    let alignment = options.alignment.as_mut().unwrap();
    alignment.options.changed = Some(vec!["other.py".to_owned()]);
    alignment.options.changed_known = true;
    let report = gate_chain::run(&options);

    assert_eq!(report.status, "blocked");
    assert_eq!(report.exit, 1);
    assert_eq!(report.failed_at.as_deref(), Some("alignment"));
    assert_eq!(report.steps[2].state, "blocked");
    assert!(report.steps[2].reason.contains("change"));
}

#[test]
fn chain_reports_unknown_when_alignment_manifest_missing() {
    let root = root("no-manifest");
    identity(&root, "bfxh/trit-agent-kernel", true);
    let mut options = options(&root);
    options.alignment.as_mut().unwrap().manifest = PathBuf::from(".agent/alignment.json");
    let report = gate_chain::run(&options);

    assert_eq!(report.status, "unknown");
    assert_eq!(report.exit, 3);
    assert_eq!(report.failed_at.as_deref(), Some("alignment"));
    assert!(report.steps[2].reason.contains("manifest-missing"));
}

#[test]
fn missing_delivery_packet_is_fail_closed() {
    let root = root("no-packet");
    identity(&root, "bfxh/trit-agent-kernel", true);
    let mut options = options(&root);
    options.delivery = None;
    let report = gate_chain::run(&options);

    assert_eq!(report.status, "unknown");
    assert_eq!(report.exit, 3);
    assert_eq!(report.failed_at.as_deref(), Some("delivery"));
    assert!(report.steps[1].reason.contains("not configured"));
    assert_eq!(report.steps[2].state, "skipped");
}

#[test]
fn cli_passes_and_exits_zero() {
    let root = root("cli-green");
    identity(&root, "bfxh/trit-agent-kernel", true);
    options(&root);
    let (code, value) = cli_support::run(
        "gate-chain",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit-agent-kernel",
            "task_kind": "agent",
            "packet": "packet.json",
            "receiver_state_hash": "H",
            "at": 1000,
            "manifest": "alignment.json"
        }),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["schema"], "gate-chain/1");
    assert_eq!(value["status"], "passed");
    assert_eq!(value["counts"]["passed"], 3);
    assert!(!root.join(".agent/delivery").exists());
}

#[test]
fn cli_blocked_exits_one_and_unknown_exits_three() {
    let root = root("cli-codes");
    identity(&root, "bfxh/trit-agent-kernel", true);
    options(&root);

    // 串仓：明确拒绝 -> 1。
    let (code, value) = cli_support::run(
        "gate-chain",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit",
            "task_kind": "agent",
            "packet": "packet.json",
            "receiver_state_hash": "H",
            "at": 1000,
            "manifest": "alignment.json"
        }),
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["status"], "blocked");
    assert_eq!(value["failed_at"], "repo");

    // 清单缺失：无法判定 -> 3，不是“差不多通过”。
    let (code, value) = cli_support::run(
        "gate-chain",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit-agent-kernel",
            "task_kind": "agent",
            "packet": "packet.json",
            "receiver_state_hash": "H",
            "at": 1000,
            "manifest": ".agent/alignment.json"
        }),
    );
    assert_eq!(code, 3, "{value}");
    assert_eq!(value["status"], "unknown");
    assert_eq!(value["failed_at"], "alignment");
}

#[test]
fn cli_stops_at_delivery_without_running_alignment() {
    let root = root("cli-delivery");
    identity(&root, "bfxh/trit-agent-kernel", true);
    options(&root);
    let (code, value) = cli_support::run(
        "gate-chain",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit-agent-kernel",
            "task_kind": "agent",
            "packet": "packet.json",
            "receiver_state_hash": "X",
            "at": 1000,
            "manifest": "alignment.json",
            "changed": ["other.py"],
            "changed_known": true
        }),
    );
    assert_eq!(code, 3, "{value}");
    assert_eq!(value["failed_at"], "delivery");
    assert_eq!(value["steps"][2]["state"], "skipped");
}
