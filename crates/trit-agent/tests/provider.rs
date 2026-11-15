use trit_agent::PrefixSignal;
use trit_agent::{
    AgentError, AgentHost, CacheObservation, ContextPlan, ContextPlanner, ContextPolicy,
    ContextReceipt, InclusionReceipt, ProviderModel, ProviderResponse, ScriptedProvider,
    ScriptedTools, SessionEvent, SessionLog, ToolOutcome, ToolRequest, TurnContext,
};
use trit_orch::{EvidenceVerdict, ObligationKind, TaskBrief};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn turn() -> TurnSpec {
    TurnSpec {
        task: "turn-1".into(),
        objective: "fix bug".into(),
        constraints: vec!["do not touch schema".into()],
        stop_when: vec![],
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

#[test]
fn provider_model_receives_brief_tools_and_receipts() {
    let provider = ScriptedProvider::new(vec![
        ProviderResponse {
            tool_calls: vec![ToolRequest::verification("call-1", "cargo test")],
            completion: None,
            usage: HostUsage {
                prompt_tokens: 10,
                completion_tokens: 1,
                cache_hit_tokens: 8,
                cache_miss_tokens: 2,
                estimated: false,
            },
            prefix: None,
            context: None,
        },
        ProviderResponse {
            tool_calls: vec![],
            completion: Some(ClaimRequest {
                criteria: vec![CriterionRequest {
                    id: "c1".into(),
                    kind: CriterionKind::Verification {
                        command: "cargo test".into(),
                    },
                }],
            }),
            usage: HostUsage {
                prompt_tokens: 5,
                completion_tokens: 1,
                cache_hit_tokens: 0,
                cache_miss_tokens: 5,
                estimated: true,
            },
            prefix: None,
            context: None,
        },
    ]);
    let tools = ScriptedTools::new(vec![ToolOutcome {
        call_id: "call-1".into(),
        tool: "bash".into(),
        command: Some("cargo test".into()),
        passed: true,
        assertions: 1,
        written_paths: vec![],
        in_doubt: false,
        output: String::new(),
    }]);
    let mut host = AgentHost::new(ProviderModel::new(provider), tools);
    let outcome = host.run_turn(&turn()).unwrap();

    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Proved);
    assert!(outcome.usage.estimated);

    let requests = host.model().provider().requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].brief.guidance.objective, "fix bug");
    assert_eq!(
        requests[0].brief.guidance.constraints,
        ["do not touch schema"]
    );
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "read_file")
    );
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "context_read")
    );
    assert!(requests[0].receipts.is_empty());
    assert_eq!(requests[0].step, 0);

    assert_eq!(requests[1].step, 1);
    assert_eq!(requests[1].receipts.len(), 1);
    assert_eq!(requests[1].usage.fresh, 2);
    assert_eq!(requests[1].usage.cached, 8);
    assert_eq!(requests[1].usage.output, 1);
}

#[test]
fn prefix_change_emits_rebasis_obligation() {
    let provider = ScriptedProvider::new(vec![ProviderResponse {
        tool_calls: vec![],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "m1".into(),
                kind: CriterionKind::Manual,
            }],
        }),
        usage: HostUsage::default(),
        prefix: Some(PrefixSignal {
            changed: true,
            reasons: vec!["compaction".into()],
        }),
        context: None,
    }]);
    let mut host = AgentHost::new(ProviderModel::new(provider), ScriptedTools::new(vec![]));
    let outcome = host.run_turn(&turn()).unwrap();
    assert_eq!(outcome.rebasis.len(), 1, "ORCH_AGENT_REBASIS");
    assert_eq!(outcome.rebasis[0].kind, ObligationKind::Rebasis);
    assert_eq!(outcome.rebasis[0].subject, "prefix");
}

fn verification_outcome(call_id: &str, command: &str) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.into(),
        tool: "bash".into(),
        command: Some(command.into()),
        passed: true,
        assertions: 1,
        written_paths: vec![],
        in_doubt: false,
        output: String::new(),
    }
}

#[test]
fn compacts_old_receipts_before_the_next_provider_call() {
    let provider = ScriptedProvider::new(vec![
        ProviderResponse {
            tool_calls: vec![
                ToolRequest::verification("call-1", "test1"),
                ToolRequest::verification("call-2", "test2"),
            ],
            completion: None,
            usage: HostUsage::default(),
            prefix: None,
            context: None,
        },
        ProviderResponse {
            tool_calls: vec![ToolRequest::verification("call-3", "test3")],
            completion: Some(ClaimRequest {
                criteria: vec![CriterionRequest {
                    id: "c3".into(),
                    kind: CriterionKind::Verification {
                        command: "test3".into(),
                    },
                }],
            }),
            usage: HostUsage::default(),
            prefix: None,
            context: None,
        },
    ]);
    let tools = ScriptedTools::new(vec![
        verification_outcome("call-1", "test1"),
        verification_outcome("call-2", "test2"),
        verification_outcome("call-3", "test3"),
    ]);
    let mut host =
        AgentHost::new(ProviderModel::new(provider), tools).with_context(ContextPolicy {
            max_receipts: 1,
            max_chars: 100_000,
            max_output_chars: 1_000,
        });
    let outcome = host.run_turn(&turn()).unwrap();
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Proved);
    assert_eq!(outcome.receipts.len(), 3, "audit keeps all receipts");

    let requests = host.model().provider().requests();
    assert_eq!(requests[1].receipts.len(), 1, "ORCH_AGENT_CONTEXT_COMPACT");
    assert_eq!(requests[1].receipts[0].call_id, "call-2");
    assert!(
        requests[1]
            .context_digest
            .as_deref()
            .unwrap()
            .contains("compacted 1")
    );
}

struct FixedPlanner(ContextPlan);

impl ContextPlanner for FixedPlanner {
    fn plan(
        &mut self,
        _brief: &TaskBrief,
        _context: &TurnContext,
    ) -> Result<Option<ContextPlan>, AgentError> {
        Ok(Some(self.0.clone()))
    }
}

#[test]
fn provider_model_passes_context_plan_to_provider() {
    let provider = ScriptedProvider::new(vec![ProviderResponse {
        tool_calls: vec![],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "m1".into(),
                kind: CriterionKind::Manual,
            }],
        }),
        usage: HostUsage::default(),
        prefix: None,
        context: None,
    }]);
    let plan = ContextPlan {
        project: "p1".into(),
        ..ContextPlan::default()
    };
    let model = ProviderModel::new(provider).with_context_planner(Box::new(FixedPlanner(plan)));
    let mut host = AgentHost::new(model, ScriptedTools::new(Vec::new()));
    host.run_turn(&turn()).unwrap();
    let requests = host.model().provider().requests();
    assert_eq!(
        requests[0]
            .context
            .as_ref()
            .map(|plan| plan.project.as_str()),
        Some("p1")
    );
}

#[test]
fn context_receipt_is_logged_to_session() {
    let receipt = ContextReceipt {
        inclusion: Some(InclusionReceipt {
            project: "p1".into(),
            session: "s1".into(),
            prefix_fingerprint: "fnv1a64:abc".into(),
            handles: vec!["C1".into()],
            refs: vec!["doc".into()],
            estimated_tokens: 10,
            hit_potential: trit_orch::Ternary::True,
        }),
        cache: Some(CacheObservation {
            model: "model-x".into(),
            prefix_fingerprint: "fnv1a64:abc".into(),
            prompt_tokens: 100,
            cache_hit_tokens: 80,
            cache_miss_tokens: 20,
            estimated: false,
        }),
    };
    let provider = ScriptedProvider::new(vec![ProviderResponse {
        tool_calls: vec![],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "m1".into(),
                kind: CriterionKind::Manual,
            }],
        }),
        usage: HostUsage::default(),
        prefix: None,
        context: Some(receipt),
    }]);
    let path = std::env::temp_dir().join(format!(
        "trit-context-receipt-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut log = SessionLog::open(&path).unwrap();
    let mut host = AgentHost::new(ProviderModel::new(provider), ScriptedTools::new(Vec::new()));
    host.run_turn_with_session(&turn(), &mut log).unwrap();
    let events = log.load().unwrap();
    let observed = events.iter().find_map(|record| match &record.event {
        SessionEvent::Proposal {
            context: Some(receipt),
            ..
        } => Some(receipt),
        _ => None,
    });
    let observed = observed.expect("context receipt should be logged");
    assert_eq!(
        observed.cache.as_ref().map(|cache| cache.cache_hit_tokens),
        Some(80)
    );
    let _ = std::fs::remove_file(&path);
}
