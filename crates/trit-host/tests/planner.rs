//! planner 接线：submit_plan 拦截、目录过滤、PlanHandoff 解析与一轮集成。

use std::path::PathBuf;

use trit_host::{
    AgentError, PLAN_TOOL, Proposal, ScriptedModel, ScriptedTools, Session, ToolExecutor,
    ToolRequest, plan_from_capture, planner_catalog_specs, planner_tools,
};
use trit_reasonix::TurnSpec;

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("trit-host-planner-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

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

fn submit(content: &str) -> ToolRequest {
    ToolRequest {
        call_id: "p1".into(),
        tool: PLAN_TOOL.into(),
        read_only: true,
        content: Some(content.into()),
        ..ToolRequest::default()
    }
}

#[test]
fn submit_plan_is_captured_and_not_forwarded() {
    let (mut tools, captured) = planner_tools(ScriptedTools::new(vec![]));
    let outcome = tools.execute(&submit("{}")).unwrap();
    assert!(outcome.passed);
    assert_eq!(captured.lock().unwrap().as_deref(), Some("{}"));

    // 内层没有被消耗：随后的只读请求会命中空队列。
    let error = tools
        .execute(&ToolRequest::read("r1", "a.txt"))
        .unwrap_err();
    assert!(matches!(error, AgentError::ToolExhausted), "{error:?}");
}

#[test]
fn plan_from_capture_parses_and_validates() {
    let (mut tools, captured) = planner_tools(ScriptedTools::new(vec![]));
    tools.execute(&submit(&handoff_json())).unwrap();
    let handoff = plan_from_capture(&captured, &["goal"], 64).unwrap();
    assert!(handoff.reference().starts_with("plan@1#"));

    // 未捕获。
    let (_, empty) = planner_tools(ScriptedTools::new(vec![]));
    assert!(matches!(
        plan_from_capture(&empty, &["goal"], 64).unwrap_err(),
        AgentError::PlanProtocol { .. }
    ));

    // 非法载荷：内层 kind 被带出。
    let (mut tools, captured) = planner_tools(ScriptedTools::new(vec![]));
    tools.execute(&submit("not-json")).unwrap();
    let error = plan_from_capture(&captured, &["goal"], 64).unwrap_err();
    match error {
        AgentError::PlanProtocol { message } => {
            assert!(message.contains("plan_handoff_schema"), "{message}")
        }
        other => panic!("expected PlanProtocol, got {other:?}"),
    }
}

#[test]
fn planner_turn_captures_plan_from_scripted_model() {
    let dir = temp_dir("turn");
    let path = dir.join("planner.jsonl");
    let proposal = Proposal {
        tool_calls: vec![submit(&handoff_json())],
        ..Proposal::default()
    };
    let (tools, captured) = planner_tools(ScriptedTools::new(vec![]));
    let mut session = Session::open(ScriptedModel::new(vec![proposal]), tools, path).unwrap();
    let turn = TurnSpec {
        task: "plan".into(),
        objective: "plan the work".into(),
        constraints: vec![],
        stop_when: vec![],
        min_assertions: 0,
        max_tokens: 1000,
    };
    // planner 在 submit_plan 之后不再产出提议：轮次可能以 ModelExhausted 结束。
    let _ = session.run_turn(&turn);

    let handoff = plan_from_capture(&captured, &["goal"], 64).unwrap();
    assert_eq!(handoff.revision, 1);
    assert_eq!(handoff.tasks.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn planner_catalog_includes_submit_plan_without_write_tools() {
    let specs = planner_catalog_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&PLAN_TOOL), "{names:?}");
    assert!(names.contains(&"read_file"), "{names:?}");
    assert!(!names.contains(&"write_file"), "{names:?}");
    assert!(!names.contains(&"bash"), "{names:?}");
    assert!(
        specs.iter().all(|spec| spec.read_only),
        "planner catalog must stay read-only"
    );
}
