//! 前后端评分：规则可解释、阈值稳定、决策参与默认关闭。

use trit_host::settings::Settings;
use trit_host::{
    AgentHost, Proposal, ScriptedModel, ScriptedTools, Surface, ToolOutcome, ToolRequest,
    score_turn,
};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn turn() -> TurnSpec {
    TurnSpec {
        task: "score".into(),
        objective: "do it".into(),
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

fn claim() -> ClaimRequest {
    ClaimRequest {
        criteria: vec![CriterionRequest {
            id: "c1".into(),
            kind: CriterionKind::Verification {
                command: "cargo test".into(),
            },
        }],
    }
}

fn outcome(call_id: &str, command: &str, passed: bool, in_doubt: bool) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.into(),
        tool: "bash".into(),
        command: Some(command.into()),
        passed,
        assertions: if passed { 2 } else { 0 },
        written_paths: Vec::new(),
        in_doubt,
        output: String::new(),
    }
}

fn verification_proposal() -> Proposal {
    Proposal {
        tool_calls: vec![ToolRequest::verification("c1", "cargo test")],
        completion: Some(claim()),
        usage: HostUsage::default(),
        ..Proposal::default()
    }
}

#[test]
fn completed_turn_scores_full_marks() {
    let mut host = AgentHost::new(
        ScriptedModel::new(vec![verification_proposal()]),
        ScriptedTools::new(vec![outcome("c1", "cargo test", true, false)]),
    );
    let outcome = host.run_turn(&turn()).unwrap();
    let score = score_turn(&outcome, Surface::Backend);
    assert_eq!(score.score, 100, "{:?}", score.reasons);
    assert!(!score.blocked);
    assert!(score.reasons.is_empty());
}

#[test]
fn in_doubt_evidence_blocks_backend() {
    let mut host = AgentHost::new(
        ScriptedModel::new(vec![verification_proposal()]),
        ScriptedTools::new(vec![outcome("c1", "cargo test", false, true)]),
    );
    let outcome = host.run_turn(&turn()).unwrap();
    let score = score_turn(&outcome, Surface::Backend);
    assert!(
        score.blocked,
        "存疑证据必须低于阈值，实际 {}（{:?}）",
        score.score, score.reasons
    );
    assert!(
        score
            .reasons
            .iter()
            .any(|reason| reason.contains("in-doubt")),
        "{:?}",
        score.reasons
    );
}

#[test]
fn frontend_surface_penalizes_missing_artifact() {
    let proposal = Proposal {
        tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", "x")],
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
    };
    let tools = ScriptedTools::new(vec![ToolOutcome {
        call_id: "w1".into(),
        tool: "write_file".into(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec!["a.txt".into()],
        in_doubt: false,
        output: String::new(),
    }]);
    let mut host = AgentHost::new(ScriptedModel::new(vec![proposal]), tools);
    let outcome = host.run_turn(&turn()).unwrap();
    let score = score_turn(&outcome, Surface::Frontend);
    assert_eq!(score.score, 90, "{:?}", score.reasons);
    assert!(
        score
            .reasons
            .iter()
            .any(|reason| reason.contains("no frontend artifact")),
        "{:?}",
        score.reasons
    );
}

#[test]
fn decision_participation_is_off_by_default() {
    let settings = Settings::default();
    assert!(
        !settings.scoring.participate_in_decision,
        "评分默认不参与决策，只在显式开启后阻断"
    );
}
