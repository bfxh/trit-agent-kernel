use std::path::PathBuf;

use trit_agent::tools::{LocalTools, WorkspaceConfig};
use trit_agent::{
    AgentError, CheckpointStore, CheckpointingTools, Proposal, RewindScope, ScriptedModel,
    ScriptedTools, Session, ToolExecutor, ToolRequest, rewind_session, rewind_session_with_policy,
};
use trit_orch::EvidenceVerdict;
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-agent-ckpt-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn temp_file(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-agent-ckpt-{tag}-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn turn() -> TurnSpec {
    TurnSpec {
        task: "headless".into(),
        objective: "write a.txt".into(),
        constraints: vec![],
        stop_when: vec![],
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

fn proposal(content: &str) -> Proposal {
    Proposal {
        tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", content)],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "c1".into(),
                kind: CriterionKind::Diff {
                    paths: vec!["a.txt".into()],
                },
            }],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }
}

fn tools(root: &std::path::Path) -> LocalTools {
    let mut config = WorkspaceConfig::new(root);
    config.allow_write = true;
    LocalTools::new(config).unwrap()
}

#[test]
fn checkpointing_tools_snapshot_before_writes_and_rewind_code() {
    let root = temp_dir("tools");
    let store = CheckpointStore::open(temp_dir("store")).unwrap();
    let mut tools = CheckpointingTools::new(store.clone(), &root, tools(&root));

    tools.begin_turn(0, "first").unwrap();
    tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "v1"))
        .unwrap();
    tools.end_turn(true).unwrap();

    tools.begin_turn(1, "second").unwrap();
    tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "v2"))
        .unwrap();
    tools.end_turn(true).unwrap();

    let checkpoints = store.list().unwrap();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].files["a.txt"], None);
    assert_eq!(checkpoints[1].files["a.txt"].as_deref(), Some("v1"));

    let report = store.rewind_code(&root, 1).unwrap();
    assert_eq!(report.restored, ["a.txt"], "ORCH_CKPT_RESTORE");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");

    let report = store.rewind_code(&root, 0).unwrap();
    assert_eq!(report.deleted, ["a.txt"]);
    assert!(!root.join("a.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn session_rewind_restores_code_and_truncates_conversation() {
    let root = temp_dir("session");
    let session_path = temp_file("session");
    let checkpoint_dir = temp_dir("session-store");
    let store = CheckpointStore::open(&checkpoint_dir).unwrap();
    let wrapped = CheckpointingTools::new(store, &root, tools(&root));

    let mut session = Session::open(
        ScriptedModel::new(vec![proposal("v1"), proposal("v2")]),
        wrapped,
        &session_path,
    )
    .unwrap();
    assert!(session.run_turn(&turn()).unwrap().judgment.is_complete());
    assert!(session.run_turn(&turn()).unwrap().judgment.is_complete());
    assert_eq!(session.state().turns.len(), 2);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v2");

    let report =
        rewind_session(&session_path, &checkpoint_dir, &root, 1, RewindScope::Both).unwrap();
    assert_eq!(report.restored, ["a.txt"], "ORCH_CKPT_SESSION_RESTORE");
    assert!(report.conversation_truncated);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");

    let resumed = Session::open(
        ScriptedModel::new(vec![]),
        ScriptedTools::new(vec![]),
        &session_path,
    )
    .unwrap();
    assert_eq!(
        resumed.state().turns.len(),
        1,
        "ORCH_CKPT_CONVERSATION_TRUNCATE"
    );
    assert_eq!(
        resumed.state().turns[0].verdict,
        Some(EvidenceVerdict::Proved)
    );
    let _ = std::fs::remove_file(&session_path);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rewind_out_of_range_fails_closed() {
    let root = temp_dir("range");
    let session_path = temp_file("range");
    let checkpoint_dir = temp_dir("range-store");
    let wrapped = CheckpointingTools::new(
        CheckpointStore::open(&checkpoint_dir).unwrap(),
        &root,
        tools(&root),
    );
    let mut session = Session::open(
        ScriptedModel::new(vec![proposal("v1")]),
        wrapped,
        &session_path,
    )
    .unwrap();
    session.run_turn(&turn()).unwrap();
    assert!(rewind_session(&session_path, &checkpoint_dir, &root, 2, RewindScope::Both).is_err());
    let _ = std::fs::remove_file(&session_path);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bash_commands_make_rewind_fail_closed() {
    let root = temp_dir("bash");
    let session_path = temp_file("bash");
    let checkpoint_dir = temp_dir("bash-store");
    let store = CheckpointStore::open(&checkpoint_dir).unwrap();
    let mut config = WorkspaceConfig::new(&root);
    config.allow_write = true;
    config.allow_shell = true;
    config.allow_unsandboxed_shell = true;
    let local = LocalTools::new(config).unwrap();
    let wrapped = CheckpointingTools::new(store, &root, local);

    let mut session = Session::open(
        ScriptedModel::new(vec![Proposal {
            tool_calls: vec![
                ToolRequest::write("w1", "write_file", "a.txt", "v1"),
                ToolRequest::shell("b1", "echo hi"),
            ],
            completion: Some(ClaimRequest {
                criteria: vec![CriterionRequest {
                    id: "m1".into(),
                    kind: CriterionKind::Manual,
                }],
            }),
            usage: HostUsage::default(),
            ..Proposal::default()
        }]),
        wrapped,
        &session_path,
    )
    .unwrap();
    session.run_turn(&turn()).unwrap();

    assert_eq!(
        rewind_session(&session_path, &checkpoint_dir, &root, 0, RewindScope::Both),
        Err(AgentError::Unrewindable(vec!["echo hi".into()])),
        "ORCH_CKPT_BASH_FAIL_CLOSED"
    );
    assert!(
        root.join("a.txt").exists(),
        "fail-closed rewind must not restore code"
    );
    let resumed = Session::open(
        ScriptedModel::new(vec![]),
        ScriptedTools::new(vec![]),
        &session_path,
    )
    .unwrap();
    assert_eq!(
        resumed.state().turns.len(),
        1,
        "fail-closed rewind must not truncate conversation"
    );
    let report = rewind_session_with_policy(
        &session_path,
        &checkpoint_dir,
        &root,
        0,
        RewindScope::Both,
        true,
    )
    .unwrap();
    assert_eq!(report.unrewindable, ["echo hi"]);
    assert_eq!(report.deleted, ["a.txt"]);
    assert!(report.conversation_truncated);
    assert!(!root.join("a.txt").exists());
    let _ = std::fs::remove_file(&session_path);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn rewind_preflight_rejects_symlink_before_mutation() {
    let root = temp_dir("symlink");
    let outside = temp_dir("symlink-outside");
    let store = CheckpointStore::open(temp_dir("symlink-store")).unwrap();
    let mut tools = CheckpointingTools::new(store.clone(), &root, tools(&root));
    tools.begin_turn(0, "write").unwrap();
    tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "v1"))
        .unwrap();
    tools.end_turn(true).unwrap();

    std::fs::write(outside.join("keep.txt"), "keep").unwrap();
    std::fs::remove_file(root.join("a.txt")).unwrap();
    std::os::unix::fs::symlink(outside.join("keep.txt"), root.join("a.txt")).unwrap();

    let error = store.rewind_code(&root, 0).unwrap_err();
    assert!(
        matches!(error, AgentError::Tool(ref message) if message.contains("symlink")),
        "{error}"
    );
    assert!(
        std::fs::symlink_metadata(root.join("a.txt"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "preflight failure must not replace the symlink"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn rewind_rolls_back_applied_files_when_a_later_write_fails() {
    let root = temp_dir("rollback-apply");
    let store = CheckpointStore::open(temp_dir("rollback-apply-store")).unwrap();
    let mut tools = CheckpointingTools::new(store.clone(), &root, tools(&root));
    tools.begin_turn(0, "old").unwrap();
    tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "old-a"))
        .unwrap();
    tools
        .execute(&ToolRequest::write("w2", "write_file", "b.txt", "old-b"))
        .unwrap();
    tools.end_turn(true).unwrap();
    tools.begin_turn(1, "new").unwrap();
    tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "new-a"))
        .unwrap();
    tools
        .execute(&ToolRequest::write("w2", "write_file", "b.txt", "new-b"))
        .unwrap();
    tools.end_turn(true).unwrap();

    let b = root.join("b.txt");
    let original = std::fs::metadata(&b).unwrap().permissions();
    let mut read_only = original.clone();
    read_only.set_readonly(true);
    std::fs::set_permissions(&b, read_only).unwrap();
    let result = store.rewind_code(&root, 1);
    std::fs::set_permissions(&b, original).unwrap();

    if result.is_ok() {
        eprintln!("filesystem allows writing read-only files; skipping rollback assertion");
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "new-a",
        "already applied paths must roll back"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "new-b"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_retention_keeps_only_recent_turns() {
    let root = temp_dir("retain");
    let store = CheckpointStore::open(temp_dir("retain-store")).unwrap();
    let mut tools = CheckpointingTools::new(store.clone(), &root, tools(&root)).with_retain(2);
    for turn in 0..4 {
        tools.begin_turn(turn, "write").unwrap();
        tools
            .execute(&ToolRequest::write(
                "w1",
                "write_file",
                "a.txt",
                format!("v{turn}"),
            ))
            .unwrap();
        tools.end_turn(true).unwrap();
    }
    let checkpoints = store.list().unwrap();
    assert_eq!(checkpoints.len(), 2, "ORCH_CKPT_RETAIN");
    assert_eq!(checkpoints[0].turn, 2);
    assert_eq!(checkpoints[1].turn, 3);

    assert!(
        store.rewind_code(&root, 1).is_err(),
        "pruned checkpoint must fail closed"
    );
    let report = store.rewind_code(&root, 2).unwrap();
    assert_eq!(report.restored, ["a.txt"]);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");
    let _ = std::fs::remove_dir_all(&root);
}
