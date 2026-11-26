//! planner 计划协议：`submit_plan` 唯一性、内容非空、全量重校验。

use trit_host::{AgentError, PLAN_TOOL, Proposal, ToolRequest, plan_handoff_from_proposal};

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

fn submit(call_id: &str, content: Option<&str>) -> ToolRequest {
    ToolRequest {
        call_id: call_id.into(),
        tool: PLAN_TOOL.into(),
        read_only: true,
        content: content.map(str::to_owned),
        ..ToolRequest::default()
    }
}

fn proposal(calls: Vec<ToolRequest>) -> Proposal {
    Proposal {
        tool_calls: calls,
        ..Proposal::default()
    }
}

fn protocol_error(result: Result<trit_orch::PlanHandoff, AgentError>) -> String {
    match result {
        Ok(_) => panic!("expected plan protocol error, got Ok"),
        Err(AgentError::PlanProtocol { message }) => message,
        Err(other) => panic!("expected PlanProtocol, got {other:?}"),
    }
}

#[test]
fn plan_handoff_is_extracted_from_submit_plan() {
    let handoff = plan_handoff_from_proposal(
        &proposal(vec![submit("p1", Some(&handoff_json()))]),
        &["goal"],
        64,
    )
    .unwrap();
    assert_eq!(handoff.revision, 1);
    assert!(handoff.reference().starts_with("plan@1#"));
}

#[test]
fn missing_submit_plan_is_rejected() {
    let message = protocol_error(plan_handoff_from_proposal(&proposal(vec![]), &["goal"], 64));
    assert!(message.contains("did not call"), "{message}");
}

#[test]
fn duplicate_submit_plan_is_rejected() {
    let message = protocol_error(plan_handoff_from_proposal(
        &proposal(vec![
            submit("p1", Some(&handoff_json())),
            submit("p2", Some(&handoff_json())),
        ]),
        &["goal"],
        64,
    ));
    assert!(message.contains("more than once"), "{message}");
}

#[test]
fn empty_content_is_rejected() {
    for content in [None, Some(""), Some("   ")] {
        let message = protocol_error(plan_handoff_from_proposal(
            &proposal(vec![submit("p1", content)]),
            &["goal"],
            64,
        ));
        assert!(message.contains("content is empty"), "{message}");
    }
}

#[test]
fn invalid_payload_is_rejected_with_inner_kind() {
    let message = protocol_error(plan_handoff_from_proposal(
        &proposal(vec![submit("p1", Some("not-json"))]),
        &["goal"],
        64,
    ));
    assert!(message.contains("plan_handoff_schema"), "{message}");
}

#[test]
fn capsule_allow_list_is_enforced_through_the_protocol() {
    let mut value: serde_json::Value = serde_json::from_str(&handoff_json()).unwrap();
    value["capsule"] = serde_json::json!({"entries": [{"key": "secret", "value": "x"}]});
    let message = protocol_error(plan_handoff_from_proposal(
        &proposal(vec![submit("p1", Some(&value.to_string()))]),
        &["goal"],
        64,
    ));
    assert!(message.contains("delegate_capsule_key"), "{message}");
}
