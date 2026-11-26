//! planner → executor 两阶段：计划交接、独立执行与缓存账本隔离。

use trit_host::{
    PLAN_TOOL, PlanExecuteConfig, Proposal, ScriptedModel, ScriptedTools, ToolOutcome, ToolRequest,
    run_plan_then_execute,
};
use trit_reasonix::{CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn handoff_json() -> String {
    serde_json::json!({
        "revision": 1,
        "tasks": [{
            "task": {"id": "a", "duration": 1},
            "requirements": {"cpu": 0, "memory": 0},
            "max_tokens": 100,
        }],
        "capsule": {"entries": [{"key": "goal", "value": "ship"}]},
        "profile": {
            "name": "executor",
            "access": "read_write",
            "write_roots": ["src"],
            "tools": ["write_file"],
            "max_tokens": 100,
        },
    })
    .to_string()
}

fn turn() -> TurnSpec {
    TurnSpec {
        task: "t".into(),
        objective: "ship it".into(),
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 0,
        max_tokens: 1000,
    }
}

#[test]
fn plan_is_handed_to_a_fresh_executor_session() {
    let planner_model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![ToolRequest {
            call_id: "p1".into(),
            tool: PLAN_TOOL.into(),
            read_only: true,
            content: Some(handoff_json()),
            ..ToolRequest::default()
        }],
        usage: HostUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cache_hit_tokens: 100,
            cache_miss_tokens: 5,
            estimated: false,
        },
        ..Proposal::default()
    }]);
    let executor_model = ScriptedModel::new(vec![
        Proposal {
            tool_calls: vec![ToolRequest {
                call_id: "w1".into(),
                tool: "write_file".into(),
                read_only: false,
                paths: vec!["src/a.txt".into()],
                content: Some("hello".into()),
                ..ToolRequest::default()
            }],
            usage: HostUsage {
                prompt_tokens: 3,
                completion_tokens: 1,
                cache_hit_tokens: 7,
                cache_miss_tokens: 1,
                estimated: false,
            },
            ..Proposal::default()
        },
        Proposal {
            completion: Some(trit_reasonix::ClaimRequest {
                criteria: vec![CriterionRequest {
                    id: "c1".into(),
                    kind: CriterionKind::Diff {
                        paths: vec!["src/a.txt".into()],
                    },
                }],
            }),
            ..Proposal::default()
        },
    ]);
    let executor_tools = ScriptedTools::new(vec![ToolOutcome {
        call_id: "w1".into(),
        tool: "write_file".into(),
        passed: true,
        written_paths: vec!["src/a.txt".into()],
        ..ToolOutcome::default()
    }]);

    let outcome = run_plan_then_execute(
        planner_model,
        ScriptedTools::new(vec![]),
        executor_model,
        executor_tools,
        &turn(),
        &PlanExecuteConfig {
            capsule_allowed: vec!["goal".into()],
            capsule_budget: 64,
            ..PlanExecuteConfig::default()
        },
    )
    .unwrap();

    assert_eq!(outcome.plan.revision, 1);
    assert_eq!(outcome.plan.tasks.len(), 1);
    assert_eq!(outcome.plan.profile.name, "executor");

    let execution = &outcome.execution;
    assert!(execution.judgment.is_complete(), "{:?}", execution.judgment);
    assert_eq!(execution.receipts.len(), 1);
    assert!(
        execution.brief.guidance.objective.contains("PlanHandoff"),
        "executor 的目标里必须带上批准的计划: {}",
        execution.brief.guidance.objective
    );
    assert_eq!(
        execution.usage.cached, 7,
        "只统计执行侧自己的缓存命中；planner 的 100 不得串到 executor"
    );
    assert!(execution.usage.fresh > 0, "执行侧仍要记录自己的新增 token");
}

#[test]
fn planner_without_a_plan_fails_before_execution() {
    let planner_model = ScriptedModel::new(vec![]);
    let executor_model = ScriptedModel::new(vec![]);
    let error = run_plan_then_execute(
        planner_model,
        ScriptedTools::new(vec![]),
        executor_model,
        ScriptedTools::new(vec![]),
        &turn(),
        &PlanExecuteConfig::default(),
    )
    .err()
    .expect("planner produced no plan");

    match error {
        trit_host::AgentError::PlanProtocol { message } => {
            assert!(message.contains("submit_plan"), "{message}")
        }
        other => panic!("expected PlanProtocol, got {other:?}"),
    }
}
