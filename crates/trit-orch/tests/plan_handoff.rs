//! 计划交接的不变量：构造即校验、规范顺序、稳定引用、序列化往返。

use trit_core::value::Ternary;
use trit_orch::{
    AccessMode, CapsuleEntry, ContextCapsule, Dependency, PlanHandoff, SubagentProfile, Task,
    TaskSpec,
};

fn spec(id: &str, deps: Vec<Dependency>) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps,
        },
        ..TaskSpec::default()
    }
}

fn required(task: &str) -> Dependency {
    Dependency {
        task: task.into(),
        required: Ternary::True,
    }
}

fn profile() -> SubagentProfile {
    SubagentProfile {
        name: "executor".into(),
        access: AccessMode::ReadWrite,
        write_roots: vec!["src".into()],
        tools: vec!["write_file".into(), "read_file".into()],
        max_tokens: 1000,
    }
}

fn capsule() -> ContextCapsule {
    ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "goal".into(),
            value: "ship B7.6".into(),
        }],
        &["goal"],
        64,
    )
    .unwrap()
}

#[test]
fn handoff_accepts_valid_plan_and_exposes_stable_reference() {
    let handoff = PlanHandoff::new(
        3,
        vec![spec("b", vec![required("a")]), spec("a", vec![])],
        capsule(),
        profile(),
    )
    .unwrap();
    assert_eq!(handoff.revision, 3);
    // 构造时已按 ID 排序，存规范形式。
    assert_eq!(handoff.tasks[0].task.id, "a");
    assert_eq!(handoff.tasks[1].task.id, "b");
    let digest = handoff.digest();
    assert_eq!(digest.len(), 16);
    assert_eq!(handoff.reference(), format!("plan@3#{digest}"));
}

#[test]
fn handoff_digest_is_order_independent() {
    let forward = PlanHandoff::new(
        1,
        vec![spec("a", vec![]), spec("b", vec![required("a")])],
        capsule(),
        profile(),
    )
    .unwrap();
    let reverse = PlanHandoff::new(
        1,
        vec![spec("b", vec![required("a")]), spec("a", vec![])],
        capsule(),
        profile(),
    )
    .unwrap();
    assert_eq!(forward.digest(), reverse.digest());
}

#[test]
fn handoff_rejects_invalid_plan() {
    let error = PlanHandoff::new(
        1,
        vec![spec("a", vec![required("missing")])],
        capsule(),
        profile(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), "unknown_dependency");
}

#[test]
fn handoff_rejects_invalid_profile() {
    let mut read_only = profile();
    read_only.access = AccessMode::ReadOnly; // 仍带 write_roots
    let error = PlanHandoff::new(1, vec![spec("a", vec![])], capsule(), read_only).unwrap_err();
    assert_eq!(error.kind(), "delegate_read_only_writes");
}

#[test]
fn handoff_round_trips_through_json() {
    let handoff = PlanHandoff::new(2, vec![spec("a", vec![])], capsule(), profile()).unwrap();
    let json = serde_json::to_string(&handoff).unwrap();
    let back: PlanHandoff = serde_json::from_str(&json).unwrap();
    assert_eq!(back, handoff);
    assert_eq!(back.digest(), handoff.digest());

    let plan = back.into_plan().unwrap();
    assert_eq!(plan.revision(), 2);
    assert_eq!(plan.tasks().len(), 1);
}

#[test]
fn from_json_revalidates_and_matches_constructor_digest() {
    let handoff = PlanHandoff::new(1, vec![spec("a", vec![])], capsule(), profile()).unwrap();
    let json = serde_json::to_string(&handoff).unwrap();
    let parsed = PlanHandoff::from_json(&json, &["goal"], 64).unwrap();
    assert_eq!(parsed, handoff);
    assert_eq!(parsed.digest(), handoff.digest());
}

#[test]
fn from_json_rejects_bad_schema_with_machine_readable_kind() {
    assert_eq!(
        PlanHandoff::from_json("not-json", &["goal"], 64)
            .unwrap_err()
            .kind(),
        "plan_handoff_schema"
    );

    // 缺 profile 字段。
    let missing = serde_json::json!({
        "revision": 1,
        "tasks": [{
            "task": {"id": "a", "duration": 1},
            "requirements": {"cpu": 0, "memory": 0},
            "max_tokens": 1,
        }],
    })
    .to_string();
    assert_eq!(
        PlanHandoff::from_json(&missing, &["goal"], 64)
            .unwrap_err()
            .kind(),
        "plan_handoff_schema"
    );
}

#[test]
fn from_json_cannot_bypass_semantic_checks() {
    // 手写 JSON 绕过构造器：未知依赖仍被拒绝。
    let bad_dep = serde_json::json!({
        "revision": 1,
        "tasks": [{
            "task": {"id": "a", "duration": 1, "deps": [{"task": "missing", "required": "True"}]},
            "requirements": {"cpu": 0, "memory": 0},
            "max_tokens": 1,
        }],
        "profile": {
            "name": "executor",
            "access": "read_write",
            "write_roots": ["src"],
            "tools": ["write_file"],
            "max_tokens": 100,
        },
    })
    .to_string();
    assert_eq!(
        PlanHandoff::from_json(&bad_dep, &["goal"], 64)
            .unwrap_err()
            .kind(),
        "unknown_dependency"
    );

    // 胶囊键不在白名单。
    let mut value: serde_json::Value = serde_json::from_str(&bad_dep).unwrap();
    value["tasks"][0]["task"]["deps"] = serde_json::json!([]);
    value["capsule"] = serde_json::json!({"entries": [{"key": "secret", "value": "x"}]});
    assert_eq!(
        PlanHandoff::from_json(&value.to_string(), &["goal"], 64)
            .unwrap_err()
            .kind(),
        "delegate_capsule_key"
    );

    // 只读 profile 带写根。
    let mut read_only = value.clone();
    read_only["capsule"] = serde_json::json!({"entries": []});
    read_only["profile"]["access"] = serde_json::json!("read_only");
    assert_eq!(
        PlanHandoff::from_json(&read_only.to_string(), &["goal"], 64)
            .unwrap_err()
            .kind(),
        "delegate_read_only_writes"
    );
}
