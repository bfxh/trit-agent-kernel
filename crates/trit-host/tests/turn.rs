use trit_host::{
    AgentError, AgentHost, Proposal, ScriptedModel, ScriptedTools, ToolOutcome, ToolRequest,
    effect_confirmed,
};
use trit_orch::{Effect, EvidenceVerdict, Ternary, TokenUsage};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn turn(min_assertions: u32) -> TurnSpec {
    TurnSpec {
        task: "turn-1".into(),
        objective: "fix bug".into(),
        constraints: vec![],
        stop_when: vec![],
        min_assertions,
        max_tokens: 1_000,
    }
}

fn verification(id: &str, command: &str) -> CriterionRequest {
    CriterionRequest {
        id: id.into(),
        kind: CriterionKind::Verification {
            command: command.into(),
        },
    }
}

fn diff(id: &str, paths: &[&str]) -> CriterionRequest {
    CriterionRequest {
        id: id.into(),
        kind: CriterionKind::Diff {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        },
    }
}

fn verification_tool(call_id: &str, command: &str) -> ToolRequest {
    ToolRequest::verification(call_id, command)
}

fn write_tool(call_id: &str, path: &str) -> ToolRequest {
    ToolRequest::write(call_id, "edit_file", path, "content")
}

fn verification_outcome(call_id: &str, command: &str, passed: bool) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.into(),
        tool: "bash".into(),
        command: Some(command.into()),
        passed,
        assertions: 1,
        written_paths: vec![],
        in_doubt: false,
        output: String::new(),
    }
}

#[test]
fn backed_verification_proves_and_usage_maps() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![verification_tool("call-1", "cargo test")],
        completion: Some(ClaimRequest {
            criteria: vec![verification("c1", "cargo test")],
        }),
        usage: HostUsage {
            prompt_tokens: 100,
            completion_tokens: 10,
            cache_hit_tokens: 80,
            cache_miss_tokens: 20,
            estimated: false,
        },
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![verification_outcome("call-1", "cargo test", true)]);
    let mut host = AgentHost::new(model, tools);

    let outcome = host.run_turn(&turn(1)).unwrap();
    assert_eq!(outcome.steps, 1);
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Proved);
    assert_eq!(outcome.judgment.ternary, Ternary::True);
    assert!(outcome.judgment.is_complete());
    assert_eq!(outcome.receipts.len(), 1);
    assert_eq!(
        outcome.usage,
        TokenUsage {
            fresh: 20,
            cached: 80,
            output: 10,
            estimated: false,
        },
        "ORCH_AGENT_USAGE"
    );
    assert_eq!(outcome.metrics.steps, 1);
    assert_eq!(outcome.metrics.max_steps, 8);
    assert_eq!(outcome.metrics.model_calls, 1);
    assert_eq!(outcome.metrics.tool_calls, 1);
    assert_eq!(outcome.metrics.verified_tools, 1);
    assert_eq!(outcome.metrics.failed_tools, 0);
    assert_eq!(outcome.metrics.in_doubt_tools, 0);
    assert_eq!(outcome.metrics.step_budget_used_bp, 1_250);
    assert_eq!(outcome.metrics.tool_success_bp, Some(10_000));
    assert_eq!(outcome.metrics.cache_hit_bp, Some(8_000));
    assert_eq!(outcome.metrics.tokens_total, 110);
    assert_eq!(outcome.metrics.tokens_per_step, 110);
    assert!(outcome.metrics.completed);

    let markdown = outcome.render_markdown();
    assert!(markdown.starts_with("# 任务：turn-1"), "{markdown}");
    assert!(markdown.contains("12.50%"), "{markdown}");
    assert!(markdown.contains("| 工具成功 | 100.00% |"), "{markdown}");
    assert!(markdown.contains("## 工具回执"), "{markdown}");
    assert!(markdown.contains("call-1"), "{markdown}");
}

#[test]
fn write_tool_goes_through_the_effect_ledger() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![write_tool("call-1", "src/a.rs")],
        completion: Some(ClaimRequest {
            criteria: vec![diff("c1", &["src/a.rs"])],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![ToolOutcome {
        call_id: "call-1".into(),
        tool: "edit_file".into(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec!["src/a.rs".into()],
        in_doubt: false,
        output: String::new(),
    }]);
    let mut host = AgentHost::new(model, tools);

    let outcome = host.run_turn(&turn(1)).unwrap();
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Proved);
    assert!(effect_confirmed(
        &outcome.effects,
        "reasonix:turn-1:edit_file:call-1"
    ));
    assert_eq!(outcome.brief.effects.len(), 1, "ORCH_AGENT_PLAN_PATCH");
    assert_eq!(
        outcome.brief.effects[0].operation_for("turn-1"),
        "reasonix:turn-1:edit_file:call-1"
    );
    assert_eq!(outcome.brief.epoch, 1, "effect patch bumps epoch");
}

#[test]
fn failed_verification_refutes() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![verification_tool("call-1", "cargo test")],
        completion: Some(ClaimRequest {
            criteria: vec![verification("c1", "cargo test")],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![verification_outcome("call-1", "cargo test", false)]);
    let mut host = AgentHost::new(model, tools);

    let outcome = host.run_turn(&turn(1)).unwrap();
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Refuted);
    assert_eq!(outcome.judgment.ternary, Ternary::False);
    assert!(!outcome.judgment.is_complete());
}

#[test]
fn unbacked_completion_is_unknown_and_escalates() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![],
        completion: Some(ClaimRequest {
            criteria: vec![verification("c1", "cargo test")],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let mut host = AgentHost::new(model, ScriptedTools::new(vec![]));

    let outcome = host.run_turn(&turn(1)).unwrap();
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Neither);
    assert_eq!(outcome.judgment.ternary, Ternary::Unknown);
    assert!(!outcome.judgment.is_complete());
    assert!(!outcome.judgment.adherence.is_compliant());
    assert!(!outcome.judgment.adherence.escalations.is_empty());
}

#[test]
fn in_doubt_write_does_not_back_a_diff_and_stays_unknown() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![write_tool("call-1", "src/a.rs")],
        completion: Some(ClaimRequest {
            criteria: vec![diff("c1", &["src/a.rs"])],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![ToolOutcome {
        call_id: "call-1".into(),
        tool: "edit_file".into(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec!["src/a.rs".into()],
        in_doubt: true,
        output: String::new(),
    }]);
    let mut host = AgentHost::new(model, tools);

    let outcome = host.run_turn(&turn(1)).unwrap();
    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Neither);
    assert_eq!(
        outcome.effects.state("reasonix:turn-1:edit_file:call-1"),
        Some(Effect::Unknown),
        "ORCH_AGENT_IN_DOUBT"
    );
}

#[test]
fn estimated_usage_propagates() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![verification_tool("call-1", "cargo test")],
        completion: Some(ClaimRequest {
            criteria: vec![verification("c1", "cargo test")],
        }),
        usage: HostUsage {
            prompt_tokens: 50,
            completion_tokens: 5,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            estimated: true,
        },
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![verification_outcome("call-1", "cargo test", true)]);
    let mut host = AgentHost::new(model, tools);

    let outcome = host.run_turn(&turn(1)).unwrap();
    assert!(outcome.usage.estimated, "ORCH_AGENT_ESTIMATED");
    assert_eq!(outcome.usage.fresh, 50);
}

#[test]
fn step_limit_without_completion_fails_closed() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![verification_tool("call-1", "cargo test")],
        completion: None,
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![verification_outcome("call-1", "cargo test", true)]);
    let mut host = AgentHost::new(model, tools).with_max_steps(1);

    assert_eq!(
        host.run_turn(&turn(1)),
        Err(AgentError::StepLimit { limit: 1 })
    );
}

#[test]
fn tool_result_mismatch_and_empty_proposals_fail_closed() {
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![verification_tool("call-1", "cargo test")],
        completion: None,
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let tools = ScriptedTools::new(vec![verification_outcome("other", "cargo test", true)]);
    let mut host = AgentHost::new(model, tools);
    assert_eq!(
        host.run_turn(&turn(1)),
        Err(AgentError::ToolMismatch {
            expected: "call-1".into(),
            actual: "other".into()
        })
    );

    let model = ScriptedModel::new(vec![Proposal::default()]);
    let mut host = AgentHost::new(model, ScriptedTools::new(vec![]));
    assert_eq!(host.run_turn(&turn(1)), Err(AgentError::EmptyProposal));
}
