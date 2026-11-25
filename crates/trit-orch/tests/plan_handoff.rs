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
