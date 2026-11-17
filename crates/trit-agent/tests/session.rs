use std::path::PathBuf;

use trit_agent::tools::WorkspaceConfig;
use trit_agent::{
    AgentError, CacheObservation, ContextReceipt, HeadlessConfig, Proposal, SESSION_SCHEMA,
    ScriptedModel, ScriptedTools, Session, SessionEvent, SessionLog, ToolOutcome, ToolRequest,
    recovery_context_for_session, run_headless,
};
use trit_orch::{EvidenceVerdict, TokenUsage};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-agent-session-{tag}-{}-{:?}",
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
        "trit-agent-session-{tag}-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn turn() -> TurnSpec {
    TurnSpec {
        task: "headless".into(),
        objective: "write a file".into(),
        constraints: vec![],
        stop_when: vec![],
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

fn proposal() -> Proposal {
    Proposal {
        tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", "hello")],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "c1".into(),
                kind: CriterionKind::Diff {
                    paths: vec!["a.txt".into()],
                },
            }],
        }),
        usage: HostUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cache_hit_tokens: 8,
            cache_miss_tokens: 2,
            estimated: false,
        },
        ..Proposal::default()
    }
}

fn write_outcome() -> ToolOutcome {
    ToolOutcome {
        call_id: "w1".into(),
        tool: "write_file".into(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec!["a.txt".into()],
        in_doubt: false,
        output: "wrote 5 bytes".into(),
    }
}

fn config(root: &std::path::Path, session: Option<PathBuf>) -> HeadlessConfig {
    let mut workspace = WorkspaceConfig::new(root);
    workspace.allow_write = true;
    HeadlessConfig {
        workspace,
        max_steps: 2,
        session,
        context: trit_agent::ContextPolicy::default(),
    }
}

#[test]
fn session_records_turn_and_continues_sequence() {
    let root = temp_workspace("records");
    let path = temp_file("records");
    let outcome = run_headless(
        turn(),
        ScriptedModel::new(vec![proposal()]),
        config(&root, Some(path.clone())),
    )
    .unwrap();
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Proved);

    let records = SessionLog::open(&path).unwrap().load().unwrap();
    assert!(records.len() >= 8, "turn should record the whole loop");
    assert!(
        matches!(records[0].event, SessionEvent::TurnStarted { .. }),
        "ORCH_SESSION_START"
    );
    assert!(matches!(records[1].event, SessionEvent::Proposal { .. }));
    assert!(matches!(
        records[2].event,
        SessionEvent::ToolRequested { .. }
    ));
    assert!(matches!(records[3].event, SessionEvent::PlanPatched { .. }));
    assert!(matches!(
        records[4].event,
        SessionEvent::ToolCompleted { .. }
    ));
    assert!(matches!(
        records[5].event,
        SessionEvent::CompletionClaimed { .. }
    ));
    assert!(matches!(records[6].event, SessionEvent::Judgment { .. }));
    assert!(matches!(
        records.last().unwrap().event,
        SessionEvent::TurnFinished { complete: true, .. }
    ));
    assert!(records.iter().all(|record| record.schema == SESSION_SCHEMA));
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64 + 1, "ORCH_SESSION_SEQ");
    }

    let first_len = records.len();
    run_headless(
        turn(),
        ScriptedModel::new(vec![proposal()]),
        config(&root, Some(path.clone())),
    )
    .unwrap();
    let records = SessionLog::open(&path).unwrap().load().unwrap();
    assert_eq!(records.len(), first_len * 2, "ORCH_SESSION_CONTINUE");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64 + 1);
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn corrupt_session_reports_the_line() {
    let path = temp_file("corrupt");
    std::fs::write(
        &path,
        "{\"schema\":1,\"seq\":1,\"event\":{\"type\":\"turn_finished\",\"steps\":0,\"complete\":false}}\nnot-json\n",
    )
    .unwrap();
    let error = SessionLog::open(&path).unwrap_err();
    assert!(
        matches!(&error, AgentError::SessionParse { line: 2, .. }),
        "ORCH_SESSION_PARSE: {error:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn session_schema_mismatch_fails_closed() {
    let path = temp_file("schema");
    std::fs::write(
        &path,
        "{\"schema\":99,\"seq\":1,\"event\":{\"type\":\"turn_finished\",\"steps\":0,\"complete\":false}}\n",
    )
    .unwrap();
    let error = SessionLog::open(&path).unwrap_err();
    assert!(
        matches!(
            &error,
            AgentError::SessionSchema {
                found: 99,
                expected: 1,
                ..
            }
        ),
        "ORCH_SESSION_SCHEMA: {error:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn session_recovers_history_and_usage() {
    let path = temp_file("recover");
    let mut session = Session::open(
        ScriptedModel::new(vec![proposal(), proposal()]),
        ScriptedTools::new(vec![write_outcome(), write_outcome()]),
        &path,
    )
    .unwrap();
    assert!(session.run_turn(&turn()).unwrap().judgment.is_complete());
    assert!(session.run_turn(&turn()).unwrap().judgment.is_complete());

    let state = session.state();
    assert_eq!(state.turns.len(), 2);
    assert!(
        state
            .turns
            .iter()
            .all(|turn| turn.finished && turn.complete)
    );
    assert_eq!(
        state.usage,
        TokenUsage {
            fresh: 4,
            cached: 16,
            output: 4,
            estimated: false,
        },
        "ORCH_SESSION_USAGE"
    );
    drop(session);

    let mut resumed = Session::open(
        ScriptedModel::new(vec![proposal()]),
        ScriptedTools::new(vec![write_outcome()]),
        &path,
    )
    .unwrap();
    assert_eq!(
        resumed.state().turns.len(),
        2,
        "ORCH_SESSION_RECOVER: previous turns must be loaded"
    );
    resumed.run_turn(&turn()).unwrap();
    assert_eq!(resumed.state().turns.len(), 3);
    let records = resumed.log().load().unwrap();
    assert!(records.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unfinished_turn_is_recovered_as_incomplete() {
    let path = temp_file("unfinished");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "t".into(),
        objective: "o".into(),
        max_steps: 1,
    })
    .unwrap();
    drop(log);

    let session = Session::open(
        ScriptedModel::new(vec![]),
        ScriptedTools::new(vec![]),
        &path,
    )
    .unwrap();
    assert_eq!(session.state().turns.len(), 1);
    assert!(!session.state().turns[0].finished);
    assert!(!session.state().turns[0].complete);
    assert_eq!(session.state().next_turn(), 1);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reopening_a_session_rebuilds_the_cache_ledger() {
    let root = temp_workspace("ledger");
    let path = temp_file("ledger");
    let mut proposal = proposal();
    proposal.context_receipt = Some(ContextReceipt {
        inclusion: None,
        cache: Some(CacheObservation {
            provider: "provider".into(),
            model: "model-x".into(),
            prefix_fingerprint: "fnv1a64:replay".into(),
            prompt_tokens: 100,
            cache_hit_tokens: 80,
            cache_miss_tokens: 20,
            estimated: false,
            observed_at_ms: None,
        }),
    });
    let mut session = Session::open(
        ScriptedModel::new(vec![proposal]),
        ScriptedTools::new(vec![write_outcome()]),
        &path,
    )
    .unwrap();
    session.run_turn(&turn()).unwrap();
    drop(session);

    let reopened = Session::open(
        ScriptedModel::new(Vec::new()),
        ScriptedTools::new(Vec::new()),
        &path,
    )
    .unwrap();
    let ledger = reopened.cache_ledger();
    let ledger = ledger.lock().unwrap();
    let stat = ledger
        .stat_for("provider", "model-x", "fnv1a64:replay")
        .expect("replayed ledger should contain the prefix");
    assert_eq!(stat.cache_hit_tokens, 80);
    assert_eq!(stat.cache_miss_tokens, 20);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn legacy_compaction_event_defaults_char_counts() {
    let record: trit_agent::SessionRecord = serde_json::from_str(
        r#"{"schema":1,"seq":1,"event":{"type":"context_compacted","step":0,"dropped":1,"truncated":0,"digest":"d"}}"#,
    )
    .unwrap();
    match record.event {
        SessionEvent::ContextCompacted {
            dropped_chars,
            truncated_chars,
            digest,
            ..
        } => {
            assert_eq!(dropped_chars, 0);
            assert_eq!(truncated_chars, 0);
            assert_eq!(digest.as_deref(), Some("d"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn recovery_context_rebuilds_from_session_log() {
    let root = temp_workspace("recovery-context");
    let path = temp_file("recovery-context");
    let mut session = Session::open(
        ScriptedModel::new(vec![proposal()]),
        ScriptedTools::new(vec![write_outcome()]),
        &path,
    )
    .unwrap();
    session.run_turn(&turn()).unwrap();

    let recovery = recovery_context_for_session(&path).unwrap();
    assert_eq!(recovery.session_id.as_deref(), path.to_str());
    let state = recovery.state.expect("state layer");
    assert_eq!(state.state, trit_agent::AgentState::Finished);
    assert_eq!(state.turns, 1);
    assert_eq!(state.validation.verdict, trit_orch::Ternary::True);
    assert!(recovery.cache.is_some());
    assert!(recovery.hooks.is_empty());

    let live = session.recovery_context().unwrap();
    assert_eq!(live.state.unwrap().turns, 1);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn session_exposes_state_layer_and_validation() {
    let root = temp_workspace("state-layer");
    let path = temp_file("state-layer");
    let mut session = Session::open(
        ScriptedModel::new(vec![proposal()]),
        ScriptedTools::new(vec![write_outcome()]),
        &path,
    )
    .unwrap();
    session.run_turn(&turn()).unwrap();
    let state = session.state_layer().unwrap();
    assert_eq!(state.state, trit_agent::AgentState::Finished);
    assert_eq!(state.turns, 1);
    assert_eq!(state.completed_turns, 1);
    assert!(state.in_progress.is_none());
    assert_eq!(state.validation.verdict, trit_orch::Ternary::True);
    assert_eq!(
        session.validate_state().unwrap().verdict,
        trit_orch::Ternary::True
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
}
