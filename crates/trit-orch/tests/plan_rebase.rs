//! Plan 批量 rebase：一次调用推进所有任务 epoch 与 revision，原计划保持不变。

use trit_orch::{Plan, Task, TaskSpec};

fn spec(id: &str, epoch: u64) -> TaskSpec {
    let mut spec = TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        ..TaskSpec::default()
    };
    spec.epoch = epoch;
    spec
}

#[test]
fn rebase_epoch_advances_every_task_and_revision() {
    let plan = Plan::new(1, vec![spec("a", 0), spec("b", 3)]).unwrap();
    let rebased = plan.rebase_epoch().unwrap();
    assert_eq!(rebased.revision(), 2);
    assert_eq!(rebased.task("a").unwrap().epoch, 1);
    assert_eq!(rebased.task("b").unwrap().epoch, 4);
    // 返回新计划，原计划不变。
    assert_eq!(plan.revision(), 1);
    assert_eq!(plan.task("a").unwrap().epoch, 0);
}
