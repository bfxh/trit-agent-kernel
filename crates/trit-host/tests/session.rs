use std::path::PathBuf;

use trit_host::tools::WorkspaceConfig;
use trit_host::{
    AgentError, CacheObservation, ContextReceipt, HeadlessConfig, Proposal, SESSION_SCHEMA,
    ScriptedModel, ScriptedTools, Session, SessionEvent, SessionLog, ToolOutcome, ToolRequest,
    recovery_context_for_session, run_headless,
};
use trit_orch::{EvidenceVerdict, TokenUsage};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-session-{tag}-{}-{:?}",
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
        "trit-host-session-{tag}-{}-{:?}.jsonl",
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
        context: trit_host::ContextPolicy::default(),
        write_guard: None,
        resume: false,
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
    let record: trit_host::SessionRecord = serde_json::from_str(
        r#"{"schema":1,"seq":1,"event":{"type":"context_compacted","step":0,"dropped":1,"truncated":0,"digest":"d"}}"#,
    )
    .unwrap();
    match record.event {
        SessionEvent::ContextCompacted {
            dropped_chars,
            truncated_chars,
            retained,
            epoch_before,
            epoch_after,
            freshness,
            digest,
            ..
        } => {
            assert_eq!(dropped_chars, 0);
            assert_eq!(truncated_chars, 0);
            assert_eq!(retained, None);
            assert_eq!(epoch_before, None);
            assert_eq!(epoch_after, None);
            assert_eq!(freshness, None);
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
    assert_eq!(state.state, trit_host::AgentState::Finished);
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
    assert_eq!(state.state, trit_host::AgentState::Finished);
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

#[test]
fn resume_turn_extracts_crashed_turn_state() {
    let path = temp_file("resume-state");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "headless".into(),
        objective: "write a file".into(),
        max_steps: 4,
    })
    .unwrap();
    log.append(SessionEvent::Proposal {
        step: 0,
        tool_calls: 1,
        completion: false,
        usage: HostUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cache_hit_tokens: 8,
            cache_miss_tokens: 2,
            estimated: false,
        },
        context: None,
    })
    .unwrap();
    log.append(SessionEvent::ToolRequested {
        step: 0,
        request: ToolRequest::write("w1", "write_file", "a.txt", "hello"),
    })
    .unwrap();
    log.append(SessionEvent::ToolCompleted {
        step: 0,
        outcome: write_outcome(),
    })
    .unwrap();
    drop(log);

    let records = SessionLog::open(&path).unwrap().load().unwrap();
    let resume = trit_host::session::resume_turn(&records)
        .unwrap()
        .expect("ORCH_SESSION_RESUME: crashed turn must be resumable");
    assert_eq!(resume.task, "headless");
    assert_eq!(resume.next_step, 1);
    assert_eq!(resume.step, 0);
    assert_eq!(resume.receipts().len(), 1);
    assert_eq!(resume.receipts()[0].call_id, "w1");
    assert!(resume.unsettled().is_empty());
    assert_eq!(
        resume.usage,
        TokenUsage {
            fresh: 2,
            cached: 8,
            output: 2,
            estimated: false,
        }
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn resume_turn_reruns_the_step_of_an_unsettled_request() {
    let path = temp_file("resume-unsettled");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "headless".into(),
        objective: "write a file".into(),
        max_steps: 4,
    })
    .unwrap();
    log.append(SessionEvent::Proposal {
        step: 1,
        tool_calls: 1,
        completion: false,
        usage: HostUsage::default(),
        context: None,
    })
    .unwrap();
    log.append(SessionEvent::ToolRequested {
        step: 1,
        request: ToolRequest::write("w2", "write_file", "b.txt", "x"),
    })
    .unwrap();
    drop(log);

    let records = SessionLog::open(&path).unwrap().load().unwrap();
    let resume = trit_host::session::resume_turn(&records).unwrap().unwrap();
    assert_eq!(resume.step, 1);
    assert_eq!(resume.next_step, 1, "unsettled request must rerun its step");
    assert!(resume.receipts().is_empty());
    assert_eq!(resume.unsettled().len(), 1);
    assert_eq!(resume.unsettled()[0].call_id, "w2");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn finished_turn_is_not_resumable() {
    let path = temp_file("resume-finished");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "t".into(),
        objective: "o".into(),
        max_steps: 1,
    })
    .unwrap();
    log.append(SessionEvent::TurnFinished {
        steps: 1,
        complete: true,
        metrics: None,
    })
    .unwrap();
    drop(log);
    let records = SessionLog::open(&path).unwrap().load().unwrap();
    assert!(trit_host::session::resume_turn(&records).unwrap().is_none());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn headless_resume_continues_without_a_new_turn_start() {
    let root = temp_workspace("resume-run");
    let path = temp_file("resume-run");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "headless".into(),
        objective: "write a file".into(),
        max_steps: 4,
    })
    .unwrap();
    log.append(SessionEvent::Proposal {
        step: 0,
        tool_calls: 1,
        completion: false,
        usage: HostUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cache_hit_tokens: 8,
            cache_miss_tokens: 2,
            estimated: false,
        },
        context: None,
    })
    .unwrap();
    log.append(SessionEvent::ToolRequested {
        step: 0,
        request: ToolRequest::write("w1", "write_file", "a.txt", "hello"),
    })
    .unwrap();
    log.append(SessionEvent::ToolCompleted {
        step: 0,
        outcome: write_outcome(),
    })
    .unwrap();
    drop(log);

    let mut resumed_config = config(&root, Some(path.clone()));
    resumed_config.resume = true;
    let outcome = run_headless(
        turn(),
        ScriptedModel::new(vec![Proposal {
            tool_calls: Vec::new(),
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
        }]),
        resumed_config,
    )
    .unwrap();
    assert_eq!(
        outcome.judgment.verdict,
        EvidenceVerdict::Proved,
        "ORCH_SESSION_RESUME_EVIDENCE: crashed-run receipts must still back the claim"
    );
    assert_eq!(outcome.steps, 2, "resume continues at step 1");
    assert_eq!(outcome.usage.fresh, 2, "resumed usage is carried over");

    let records = SessionLog::open(&path).unwrap().load().unwrap();
    let starts = records
        .iter()
        .filter(|record| matches!(record.event, SessionEvent::TurnStarted { .. }))
        .count();
    assert_eq!(
        starts, 1,
        "ORCH_SESSION_RESUME_TURN: no duplicate TurnStarted"
    );
    assert!(matches!(
        records.last().unwrap().event,
        SessionEvent::TurnFinished { complete: true, .. }
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
}

/// 记录实际被执行过的工具调用，用来证明恢复不会重放已完成的请求。
struct CountingTools {
    seen: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    outcomes: std::collections::VecDeque<ToolOutcome>,
}

impl trit_host::ToolExecutor for CountingTools {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        self.seen.borrow_mut().push(request.call_id.clone());
        self.outcomes.pop_front().ok_or(AgentError::ToolExhausted)
    }
}

fn written(call_id: &str, path: &str) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.into(),
        tool: "write_file".into(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec![path.into()],
        in_doubt: false,
        output: String::new(),
    }
}

fn completion(paths: &[&str]) -> Proposal {
    Proposal {
        tool_calls: Vec::new(),
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "c1".into(),
                kind: CriterionKind::Diff {
                    paths: paths.iter().map(|path| (*path).to_owned()).collect(),
                },
            }],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }
}

/// 穷举截断点：日志停在每个位置时，恢复都不得重放已经拿到回执的工具。
#[test]
fn resume_from_every_truncation_never_replays_settled_tools() {
    let events = [
        SessionEvent::TurnStarted {
            task: "headless".into(),
            objective: "write".into(),
            max_steps: 4,
        },
        SessionEvent::Proposal {
            step: 0,
            tool_calls: 1,
            completion: false,
            usage: HostUsage::default(),
            context: None,
        },
        SessionEvent::ToolRequested {
            step: 0,
            request: ToolRequest::write("w1", "write_file", "a.txt", "a"),
        },
        SessionEvent::ToolCompleted {
            step: 0,
            outcome: written("w1", "a.txt"),
        },
        SessionEvent::Proposal {
            step: 1,
            tool_calls: 1,
            completion: false,
            usage: HostUsage::default(),
            context: None,
        },
        SessionEvent::ToolRequested {
            step: 1,
            request: ToolRequest::write("w2", "write_file", "b.txt", "b"),
        },
        SessionEvent::ToolCompleted {
            step: 1,
            outcome: written("w2", "b.txt"),
        },
    ];
    for cut in 1..events.len() {
        let path = temp_file(&format!("trunc-{cut}"));
        let mut log = SessionLog::open(&path).unwrap();
        for event in events[..cut].iter().cloned() {
            log.append(event).unwrap();
        }
        drop(log);

        let records = SessionLog::open(&path).unwrap().load().unwrap();
        let Some(resume) = trit_host::session::resume_turn(&records).unwrap() else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let settled: Vec<String> = resume
            .receipts()
            .iter()
            .map(|outcome| outcome.call_id.clone())
            .collect();
        let mut unique = settled.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), settled.len(), "cut={cut}: 回执不得重复");
        assert_eq!(
            resume.requests.len(),
            settled.len() + resume.unsettled().len(),
            "cut={cut}: 每个已发请求要么有回执、要么未确定"
        );
        assert!(
            resume.next_step == resume.step || resume.next_step == resume.step + 1,
            "cut={cut}: next_step 必须落在断点或其后一步"
        );
        let steps: Vec<usize> = resume.history.iter().map(|record| record.step).collect();
        let mut sorted = steps.clone();
        sorted.sort_unstable();
        assert_eq!(steps, sorted, "cut={cut}: history 必须按 step 递增");
        assert!(
            steps.iter().all(|step| *step <= resume.step),
            "cut={cut}: history 不得包含断点之后的步骤"
        );

        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut host = trit_host::AgentHost::new(
            ScriptedModel::new(vec![completion(&["a.txt", "b.txt"])]),
            CountingTools {
                seen: std::rc::Rc::clone(&seen),
                outcomes: vec![written("w1", "a.txt"), written("w2", "b.txt")].into(),
            },
        );
        let mut log = SessionLog::open(&path).unwrap();
        let _ = host.run_turn_resume_with_session(&turn(), &mut log, resume);
        for call_id in seen.borrow().iter() {
            assert!(
                !settled.contains(call_id),
                "cut={cut}: 已完成的工具 {call_id} 被重放执行"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// 跨层对账：恢复补记的 `in_doubt` 写与 journal 里未闭合的 begin 指向同一路径时，
/// 完成声明必须是 `Conflicted`，不能被其余证据抬成 Complete。
#[test]
fn resumed_in_doubt_write_conflicts_with_open_journal_entry() {
    let root = temp_workspace("resume-journal");
    let path = temp_file("resume-journal");
    let mut log = SessionLog::open(&path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "headless".into(),
        objective: "write".into(),
        max_steps: 4,
    })
    .unwrap();
    log.append(SessionEvent::Proposal {
        step: 0,
        tool_calls: 1,
        completion: false,
        usage: HostUsage::default(),
        context: None,
    })
    .unwrap();
    log.append(SessionEvent::ToolRequested {
        step: 0,
        request: ToolRequest::write("w1", "write_file", "a.txt", "hello"),
    })
    .unwrap();
    drop(log);

    // 崩溃时写操作已经开始但没落回执：journal 只有 begin，report 会把它列为 in_doubt。
    let mut journal = trit_host::journal::MutationJournal::open(&root).unwrap();
    let _open = journal
        .record_begin(
            &ToolRequest::write("w1", "write_file", "a.txt", "hello"),
            None,
        )
        .unwrap();
    assert!(journal.report().unwrap().has_in_doubt());

    let mut resuming = config(&root, Some(path.clone()));
    resuming.resume = true;
    resuming.write_guard = Some(trit_host::permissions::WriteGuard {
        claim: trit_host::permissions::WriteClaim::new(&root, vec![PathBuf::from("a.txt")])
            .unwrap(),
        lease: None,
        journal: Some(journal),
    });
    let outcome = run_headless(
        turn(),
        ScriptedModel::new(vec![completion(&["a.txt"])]),
        resuming,
    )
    .unwrap();
    assert!(
        outcome.judgment.is_conflicted(),
        "ORCH_SESSION_RESUME_JOURNAL: 未闭合写必须让完成声明冲突，实际 {:?}",
        outcome.judgment.verdict
    );
    assert!(!outcome.judgment.is_complete());
    assert!(
        outcome.receipts.iter().any(|outcome| outcome.in_doubt),
        "恢复必须把发出但没回执的写补记为 in_doubt"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
}
