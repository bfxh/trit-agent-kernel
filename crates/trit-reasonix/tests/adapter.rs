use trit_orch::{
    EvidenceVerdict, Limits, Observation, Plan, Resources, Snapshot, TaskBrief, Ternary,
    TokenUsage, task_brief,
};
use trit_reasonix::{
    AdapterError, CompletionClaim, CriterionClaim, CriterionKind, HostToolCall, HostUsage,
    TurnSpec, claim_rails, judge_completion,
};

fn limits() -> Limits {
    Limits {
        workers: 4,
        capacity: Resources::new(16, 16),
        tokens: 10_000,
    }
}

fn brief(min_assertions: u32) -> TaskBrief {
    let spec = TurnSpec {
        task: "turn-1".into(),
        objective: "fix bug".into(),
        constraints: vec!["do not touch schema".into()],
        stop_when: vec!["permission unknown".into()],
        min_assertions,
        max_tokens: 1_000,
    }
    .task_spec();
    let tasks = vec![spec];
    let plan = Plan::new(1, tasks.clone()).unwrap();
    let snapshot = Snapshot {
        revision: 1,
        observations: tasks
            .iter()
            .map(|spec| {
                (
                    spec.task.id.clone(),
                    Observation {
                        authorization: Ternary::True,
                        freshness: Ternary::True,
                        ..Observation::default()
                    },
                )
            })
            .collect(),
    };
    let selection = plan.select(&snapshot, &limits()).unwrap();
    task_brief(&plan, &selection, "turn-1").unwrap()
}

fn criterion(id: &str, backed: bool, passed: bool) -> CriterionClaim {
    CriterionClaim {
        id: id.into(),
        kind: CriterionKind::Verification {
            command: "cargo test".into(),
        },
        backed,
        passed,
    }
}

#[test]
fn usage_maps_cache_hit_and_estimated() {
    let usage = HostUsage {
        prompt_tokens: 1_000,
        completion_tokens: 50,
        cache_hit_tokens: 700,
        cache_miss_tokens: 300,
        estimated: false,
    };
    assert_eq!(
        usage.into_token_usage(),
        TokenUsage {
            fresh: 300,
            cached: 700,
            output: 50,
            estimated: false,
        },
        "ORCH_ADAPTER_USAGE"
    );

    let prompt_only = HostUsage {
        prompt_tokens: 123,
        completion_tokens: 7,
        ..HostUsage::default()
    }
    .into_token_usage();
    assert_eq!(prompt_only.fresh, 123);
    assert_eq!(prompt_only.cached, 0);

    let estimated = HostUsage {
        estimated: true,
        ..usage
    }
    .into_token_usage();
    assert!(estimated.estimated, "未知用量必须保留 estimated 标记");
}

#[test]
fn task_spec_carries_guidance_and_evidence_floor() {
    let spec = TurnSpec {
        task: "turn-1".into(),
        objective: "fix bug".into(),
        constraints: vec!["do not touch schema".into()],
        stop_when: vec!["permission unknown".into()],
        min_assertions: 0,
        max_tokens: 500,
    }
    .task_spec();
    assert_eq!(spec.guidance.objective, "fix bug");
    assert_eq!(spec.guidance.constraints, ["do not touch schema"]);
    assert_eq!(spec.guidance.stop_when, ["permission unknown"]);
    assert_eq!(spec.produces.min_assertions, 1, "至少要求一条断言");
}

#[test]
fn backed_passed_criterion_proves() {
    let claim = CompletionClaim {
        task: "turn-1".into(),
        criteria: vec![criterion("c1", true, true)],
    };
    let judgment = judge_completion(&brief(1), &claim).unwrap();
    assert_eq!(judgment.verdict, EvidenceVerdict::Proved);
    assert_eq!(judgment.ternary, Ternary::True);
    assert!(judgment.is_complete());
    assert!(judgment.adherence.is_compliant());
}

#[test]
fn backed_failed_criterion_refutes() {
    let claim = CompletionClaim {
        task: "turn-1".into(),
        criteria: vec![criterion("c1", true, false)],
    };
    let judgment = judge_completion(&brief(1), &claim).unwrap();
    assert_eq!(judgment.verdict, EvidenceVerdict::Refuted);
    assert_eq!(judgment.ternary, Ternary::False);
    assert!(judgment.is_refuted());
    assert!(!judgment.is_complete());
}

#[test]
fn unbacked_and_manual_criteria_never_prove() {
    let claim = CompletionClaim {
        task: "turn-1".into(),
        criteria: vec![
            criterion("unbacked", false, true),
            CriterionClaim {
                id: "manual".into(),
                kind: CriterionKind::Manual,
                backed: true,
                passed: true,
            },
        ],
    };
    let judgment = judge_completion(&brief(2), &claim).unwrap();
    assert_eq!(judgment.verdict, EvidenceVerdict::Neither);
    assert_eq!(judgment.ternary, Ternary::Unknown);
    assert!(!judgment.is_complete());
    assert!(
        !judgment.adherence.is_compliant(),
        "Neither 完成必须触发遵循度偏离"
    );
    assert!(!judgment.adherence.escalations.is_empty());
}

#[test]
fn conflicting_backed_criteria_are_conflicted() {
    let claim = CompletionClaim {
        task: "turn-1".into(),
        criteria: vec![criterion("yes", true, true), criterion("no", true, false)],
    };
    let judgment = judge_completion(&brief(1), &claim).unwrap();
    assert_eq!(judgment.verdict, EvidenceVerdict::Conflicted);
    assert_eq!(judgment.ternary, Ternary::Unknown);
    assert!(judgment.is_conflicted());
    assert!(!judgment.is_complete());
    assert!(
        judgment.adherence.is_compliant(),
        "冲突不是断言不足；出口是裁决，不是补断言"
    );
}

#[test]
fn tool_call_maps_effects_and_claim_paths() {
    let write = HostToolCall {
        task: "turn-1".into(),
        call_id: "call-1".into(),
        tool: "edit_file".into(),
        read_only: false,
        paths: vec!["src/login.rs".into()],
        operation: None,
    };
    let effect = write.effect_spec().unwrap();
    assert_eq!(effect.operation, "reasonix:turn-1:edit_file:call-1");
    assert_eq!(write.claim_paths(), ["src/login.rs".to_owned()].as_slice());

    let read = HostToolCall {
        read_only: true,
        ..write
    };
    assert!(read.effect_spec().is_none());
}

#[test]
fn task_mismatch_fails_closed() {
    let claim = CompletionClaim {
        task: "other".into(),
        criteria: vec![criterion("c1", true, true)],
    };
    assert_eq!(
        judge_completion(&brief(1), &claim),
        Err(AdapterError::TaskMismatch {
            claim: "other".into(),
            brief: "turn-1".into()
        })
    );
}

#[test]
fn rails_keep_epoch_and_unbacked_state() {
    let claim = CompletionClaim {
        task: "turn-1".into(),
        criteria: vec![criterion("c1", false, true)],
    };
    let rails = claim_rails(&claim, 3);
    assert_eq!(rails.judgment(3), EvidenceVerdict::Neither);
    assert_eq!(rails.judgment(2), EvidenceVerdict::Neither);
    assert!(!rails.is_empty());
}
