use trit_agent::PrefixSignal;
use trit_agent::{
    AgentHost, ContextPolicy, ProviderModel, ProviderResponse, ScriptedProvider, ScriptedTools,
    ToolOutcome, ToolRequest, default_tools,
};
use trit_orch::{EvidenceVerdict, ObligationKind};
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
    assert_eq!(requests[0].tools, default_tools());
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
