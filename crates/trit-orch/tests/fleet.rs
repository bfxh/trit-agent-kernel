//! Fleet 执行层的端到端不变量：依赖顺序、并发上限、失败传播、取消归类与稳定引用。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use trit_core::value::Ternary;
use trit_orch::{
    DelegationStatus, Dependency, FleetConfig, FleetOutput, Plan, Task, TaskSpec, execute,
};
use trit_schedule::runtime::CancelToken;

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

#[test]
fn fleet_runs_dependency_chain_in_order() {
    let plan = Plan::new(
        7,
        vec![
            spec("a", vec![]),
            spec("b", vec![required("a")]),
            spec("c", vec![required("b")]),
        ],
    )
    .unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&order);
    let report = execute(
        &plan,
        FleetConfig::new(2),
        CancelToken::new(),
        move |spec, _cancel| {
            seen.lock().unwrap().push(spec.task.id.clone());
            Ok(FleetOutput {
                content: format!("done {}", spec.task.id).into_bytes(),
                tokens: 3,
            })
        },
    )
    .unwrap();

    assert_eq!(*order.lock().unwrap(), vec!["a", "b", "c"]);
    assert_eq!(report.completed, vec!["a", "b", "c"]);
    assert!(report.failed.is_none());
    for id in ["a", "b", "c"] {
        let result = report.result(id).unwrap();
        assert!(result.reusable(), "{id} not reusable");
        assert_eq!(result.epoch, 0);
        assert!(
            result.reference().starts_with(&format!("{id}@0#")),
            "{id}: {}",
            result.reference()
        );
        assert_eq!(result.tokens, 3);
    }
    assert_eq!(report.references().len(), 3);
}

#[test]
fn fleet_respects_worker_limit() {
    let plan = Plan::new(
        1,
        vec![
            spec("a", vec![]),
            spec("b", vec![]),
            spec("c", vec![]),
            spec("d", vec![]),
        ],
    )
    .unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (live_job, peak_job) = (Arc::clone(&live), Arc::clone(&peak));
    let report = execute(
        &plan,
        FleetConfig::new(2),
        CancelToken::new(),
        move |_spec, _cancel| {
            let now = live_job.fetch_add(1, Ordering::SeqCst) + 1;
            peak_job.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));
            live_job.fetch_sub(1, Ordering::SeqCst);
            Ok(FleetOutput {
                content: b"x".to_vec(),
                tokens: 1,
            })
        },
    )
    .unwrap();

    assert_eq!(report.completed.len(), 4);
    let peak = peak.load(Ordering::SeqCst);
    assert!(peak <= 2, "peak concurrency {peak} exceeded worker limit 2");
    assert_eq!(report.metrics.started, 4);
}

#[test]
fn fleet_propagates_failure_and_defers_downstream() {
    let plan = Plan::new(
        1,
        vec![
            spec("a", vec![]),
            spec("b", vec![required("a")]),
            spec("c", vec![required("b")]),
        ],
    )
    .unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&ran);
    let report = execute(
        &plan,
        FleetConfig::new(1),
        CancelToken::new(),
        move |spec, _cancel| {
            seen.lock().unwrap().push(spec.task.id.clone());
            if spec.task.id == "b" {
                return Err("boom".into());
            }
            Ok(FleetOutput {
                content: b"ok".to_vec(),
                tokens: 1,
            })
        },
    )
    .unwrap();

    assert!(!ran.lock().unwrap().contains(&"c".to_string()));
    assert_eq!(report.failed.as_ref().unwrap().task, "b");
    assert!(matches!(
        report.result("b").unwrap().status,
        DelegationStatus::Failed { .. }
    ));
    assert!(!report.reusable("b"));
    // 未显式取消：下游记 Deferred，而不是 Aborted。
    assert!(matches!(
        report.result("c").unwrap().status,
        DelegationStatus::Deferred
    ));
    assert!(!report.reusable("c"));
    assert!(report.reusable("a"));
}

#[test]
fn fleet_marks_not_started_as_aborted_on_cancel() {
    let plan = Plan::new(1, vec![spec("a", vec![]), spec("b", vec![required("a")])]).unwrap();
    let report = execute(
        &plan,
        FleetConfig::new(1),
        CancelToken::new(),
        move |_spec, cancel| {
            cancel.cancel();
            Ok(FleetOutput {
                content: b"ok".to_vec(),
                tokens: 1,
            })
        },
    )
    .unwrap();

    assert_eq!(report.completed, vec!["a"]);
    assert!(matches!(
        report.result("b").unwrap().status,
        DelegationStatus::Aborted
    ));
    assert!(!report.reusable("b"));
}

#[test]
fn fleet_defers_unknown_dependencies() {
    let optional_unknown = Dependency {
        task: "a".into(),
        required: Ternary::Unknown,
    };
    let plan = Plan::new(
        1,
        vec![spec("a", vec![]), spec("b", vec![optional_unknown])],
    )
    .unwrap();
    let report = execute(
        &plan,
        FleetConfig::new(1),
        CancelToken::new(),
        |_spec, _cancel| {
            Ok(FleetOutput {
                content: b"ok".to_vec(),
                tokens: 1,
            })
        },
    )
    .unwrap();

    assert_eq!(report.completed, vec!["a"]);
    assert_eq!(report.deferred, vec!["b"]);
    assert!(matches!(
        report.result("b").unwrap().status,
        DelegationStatus::Deferred
    ));
    assert!(!report.reusable("b"));
}

#[test]
fn fleet_reference_is_stable_across_runs_and_tracks_epoch() {
    let plan = Plan::new(1, vec![spec("a", vec![])]).unwrap();
    let run = |plan: &Plan| {
        execute(
            plan,
            FleetConfig::new(1),
            CancelToken::new(),
            |_spec, _cancel| {
                Ok(FleetOutput {
                    content: b"same".to_vec(),
                    tokens: 0,
                })
            },
        )
        .unwrap()
        .result("a")
        .unwrap()
        .reference()
    };
    let first = run(&plan);
    let second = run(&plan);
    assert_eq!(first, second, "same content must yield the same reference");

    let mut bumped = spec("a", vec![]);
    bumped.epoch = 2;
    let plan = Plan::new(2, vec![bumped]).unwrap();
    let third = run(&plan);
    assert_ne!(first, third, "epoch change must change the reference");
    assert!(third.starts_with("a@2#"));
}

#[test]
fn fleet_rejects_zero_workers() {
    let plan = Plan::new(1, vec![spec("a", vec![])]).unwrap();
    let error = execute(
        &plan,
        FleetConfig::new(0),
        CancelToken::new(),
        |_spec, _cancel| {
            Ok(FleetOutput {
                content: Vec::new(),
                tokens: 0,
            })
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), "no_workers");
}
