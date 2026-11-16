use std::collections::BTreeMap;

use trit_orch::{
    Limits, Observation, Plan, Resources, SelectionPressure, Snapshot, Task, TaskSpec, Ternary,
};

fn spec(id: &str, priority: i32, max_tokens: u64) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            priority,
            duration: 1,
            deps: Vec::new(),
        },
        requirements: Resources::new(1, 1),
        max_tokens,
        epoch: 0,
        ..TaskSpec::default()
    }
}

fn snapshot(tasks: &[TaskSpec]) -> Snapshot {
    Snapshot {
        revision: 7,
        observations: tasks
            .iter()
            .map(|task| {
                (
                    task.task.id.clone(),
                    Observation {
                        authorization: Ternary::True,
                        freshness: Ternary::True,
                        ..Observation::default()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

#[test]
fn low_cache_hit_prefers_cheaper_tasks_within_the_same_priority() {
    let tasks = vec![spec("z-expensive", 5, 900), spec("a-cheap", 5, 50)];
    let snapshot = snapshot(&tasks);
    let plan = Plan::new(7, tasks).unwrap();
    let limits = Limits {
        workers: 1,
        capacity: Resources::new(10, 10),
        tokens: 10_000,
    };
    let normal = plan.select(&snapshot, &limits).unwrap();
    assert_eq!(normal.selected, ["z-expensive"]);
    let pressed = plan
        .select_with_pressure(&snapshot, &limits, SelectionPressure::LowCacheHit)
        .unwrap();
    assert_eq!(pressed.selected, ["a-cheap"]);
}
