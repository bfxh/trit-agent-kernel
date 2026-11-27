//! 多会话调度计划：把 `session-plan` 的 resume 项变成带依赖的分层任务图。
//!
//! 这一层只做计划，不启动模型/工具：
//! - 依赖来自 `parent_child` 与 `planner_executor` 链接，只有两端都在任务集合里才生效；
//! - `layers` 是拓扑层，同层可并发，实际并发上限由 `workers` 和后续 fleet runtime 决定；
//! - 有环的任务不进层，单独列在 `cyclic`，交由上层处理而不是死循环。
//!
//! 真正的失败传播/取消由 `trit_orch::fleet` + `trit-schedule` runtime 执行时负责；
//! 本报告只保证“谁先谁后、同类最多几个”是可复算的。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::session_plan::{SessionAction, SessionPlanReport};
use crate::session_scan::{LinkKind, SessionFormat, SessionScanReport};

pub const SESSION_FLEET_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetTask {
    pub id: String,
    pub task: String,
    #[serde(default)]
    pub deps: Vec<String>,
    /// 该任务的写根；来自会话 meta 的 project/project_path，缺省时由执行层回退。
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFleetReport {
    pub schema: u32,
    pub workers: usize,
    pub tasks: Vec<FleetTask>,
    /// 拓扑层：同一层的任务互不依赖，可并发。
    pub layers: Vec<Vec<String>>,
    /// 因格式/缺少任务被跳过。
    #[serde(default)]
    pub skipped: Vec<String>,
    /// 有环、无法排进 layers 的任务。
    #[serde(default)]
    pub cyclic: Vec<String>,
}

/// 生成调度计划；`workers == 0` 视为 1。
pub fn plan_fleet(
    scan: &SessionScanReport,
    plan: &SessionPlanReport,
    workers: usize,
) -> SessionFleetReport {
    let mut skipped = Vec::new();
    let mut task_ids = BTreeSet::new();
    let mut tasks = Vec::new();
    for item in &plan.items {
        if item.action != SessionAction::Resume {
            continue;
        }
        let Some(entry) = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == item.session_id)
        else {
            skipped.push(format!("{}: source missing", item.session_id));
            continue;
        };
        if entry.format != SessionFormat::Trit {
            skipped.push(format!("{}: non-trit format", entry.session_id));
            continue;
        }
        let Some(task) = item.task.clone().filter(|task| !task.is_empty()) else {
            skipped.push(format!("{}: no task", entry.session_id));
            continue;
        };
        task_ids.insert(entry.session_id.clone());
        tasks.push(FleetTask {
            id: entry.session_id.clone(),
            task,
            deps: Vec::new(),
            workspace: entry.project.clone(),
        });
    }

    // 链接方向就是执行方向：parent/planner 先，child/executor 后。
    for link in &scan.links {
        let (dependency, dependent) = match link.kind {
            LinkKind::ParentChild | LinkKind::PlannerExecutor => {
                (link.from.clone(), link.to.clone())
            }
        };
        if dependency == dependent
            || !task_ids.contains(&dependency)
            || !task_ids.contains(&dependent)
        {
            continue;
        }
        if let Some(task) = tasks.iter_mut().find(|task| task.id == dependent) {
            task.deps.push(dependency);
        }
    }
    for task in &mut tasks {
        task.deps.sort();
        task.deps.dedup();
    }

    let (layers, cyclic) = topological_layers(&tasks);
    SessionFleetReport {
        schema: SESSION_FLEET_SCHEMA,
        workers: workers.max(1),
        tasks,
        layers,
        skipped,
        cyclic,
    }
}

fn topological_layers(tasks: &[FleetTask]) -> (Vec<Vec<String>>, Vec<String>) {
    let mut remaining: BTreeSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
    let deps: BTreeMap<&str, &[String]> = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.deps.as_slice()))
        .collect();
    let mut layers = Vec::new();
    while !remaining.is_empty() {
        let layer: Vec<String> = remaining
            .iter()
            .filter(|id| {
                deps.get(id.as_str())
                    .is_some_and(|deps| deps.iter().all(|dep| !remaining.contains(dep)))
            })
            .cloned()
            .collect();
        if layer.is_empty() {
            break;
        }
        for id in &layer {
            remaining.remove(id);
        }
        layers.push(layer);
    }
    (layers, remaining.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_migration::SessionRole;
    use crate::session_plan::SessionPlanItem;
    use crate::session_scan::{
        SessionEntry, SessionFormat, SessionLink, SessionScanReport, SessionTask,
    };

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            path: format!("{id}.jsonl"),
            session_id: id.into(),
            project: None,
            role: SessionRole::Primary,
            format: SessionFormat::Trit,
            meta: None,
            bytes: 1,
            modified_ms: 0,
            records: 1,
            corrupt_lines: 0,
            tasks: vec![SessionTask {
                index: 1,
                task: "work".into(),
                objective: String::new(),
                steps: 0,
                complete: false,
                finished: false,
            }],
            open_turn: true,
            in_doubt: 0,
            last_gate: None,
        }
    }

    fn plan_item(id: &str) -> SessionPlanItem {
        SessionPlanItem {
            session_id: id.into(),
            path: format!("{id}.jsonl"),
            action: SessionAction::Resume,
            priority: 2,
            reason: "open turn".into(),
            task: Some(format!("task-{id}")),
            links: Vec::new(),
        }
    }

    fn report(ids: &[&str], links: Vec<SessionLink>) -> (SessionScanReport, SessionPlanReport) {
        let scan = SessionScanReport {
            schema: 1,
            root: ".".into(),
            scanned: ids.len(),
            sessions: ids.iter().map(|id| entry(id)).collect(),
            links,
            warnings: Vec::new(),
        };
        let plan = SessionPlanReport {
            schema: 1,
            root: ".".into(),
            items: ids.iter().map(|id| plan_item(id)).collect(),
            unassigned: Vec::new(),
        };
        (scan, plan)
    }

    #[test]
    fn dependencies_form_layers() {
        let (scan, plan) = report(
            &["parent", "child"],
            vec![SessionLink {
                kind: LinkKind::ParentChild,
                from: "parent".into(),
                to: "child".into(),
            }],
        );
        let fleet = plan_fleet(&scan, &plan, 4);
        assert_eq!(fleet.workers, 4);
        assert_eq!(
            fleet.layers,
            vec![vec!["parent".to_owned()], vec!["child".to_owned()]]
        );
        assert_eq!(fleet.tasks[1].deps, vec!["parent".to_owned()]);
    }

    #[test]
    fn independent_tasks_share_the_first_layer() {
        let (scan, plan) = report(&["a", "b"], Vec::new());
        let fleet = plan_fleet(&scan, &plan, 2);
        assert_eq!(fleet.layers, vec![vec!["a".to_owned(), "b".to_owned()]]);
    }

    #[test]
    fn cycles_are_reported_not_scheduled() {
        let (scan, plan) = report(
            &["a", "b"],
            vec![
                SessionLink {
                    kind: LinkKind::ParentChild,
                    from: "a".into(),
                    to: "b".into(),
                },
                SessionLink {
                    kind: LinkKind::ParentChild,
                    from: "b".into(),
                    to: "a".into(),
                },
            ],
        );
        let fleet = plan_fleet(&scan, &plan, 2);
        assert!(fleet.layers.is_empty());
        assert_eq!(fleet.cyclic, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn non_trit_sessions_are_skipped() {
        let mut chat = entry("chat");
        chat.format = SessionFormat::Reasonix;
        let scan = SessionScanReport {
            schema: 1,
            root: ".".into(),
            scanned: 1,
            sessions: vec![chat],
            links: Vec::new(),
            warnings: Vec::new(),
        };
        let plan = SessionPlanReport {
            schema: 1,
            root: ".".into(),
            items: vec![SessionPlanItem {
                session_id: "chat".into(),
                path: "chat.jsonl".into(),
                action: SessionAction::Resume,
                priority: 2,
                reason: "open turn".into(),
                task: Some("t".into()),
                links: Vec::new(),
            }],
            unassigned: Vec::new(),
        };
        let fleet = plan_fleet(&scan, &plan, 1);
        assert!(fleet.tasks.is_empty());
        assert_eq!(fleet.skipped.len(), 1);
        assert!(fleet.skipped[0].contains("non-trit"));
    }

    #[test]
    fn project_meta_becomes_task_workspace() {
        let mut session = entry("s");
        session.project = Some("/repo".into());
        let scan = SessionScanReport {
            schema: 1,
            root: ".".into(),
            scanned: 1,
            sessions: vec![session],
            links: Vec::new(),
            warnings: Vec::new(),
        };
        let plan = SessionPlanReport {
            schema: 1,
            root: ".".into(),
            items: vec![plan_item("s")],
            unassigned: Vec::new(),
        };
        let fleet = plan_fleet(&scan, &plan, 1);
        assert_eq!(fleet.tasks[0].workspace.as_deref(), Some("/repo"));
    }
}
