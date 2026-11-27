//! 会话任务分配：在扫描报告之上给出确定性的优先级与动作。
//!
//! 不启动模型、不写文件。规则只看可复算的字段：
//! `in_doubt` > 门链阻断 > 未完成 turn > 坏行 > 已完成。
//! 未知格式不参与调度，避免把前端会话误当内核会话执行。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::session_scan::{SessionEntry, SessionFormat, SessionScanReport};

pub const SESSION_PLAN_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    /// 有结果不明/门链阻断/坏行：先检查，不直接续跑。
    Inspect,
    /// 有未完成 turn：继续原任务。
    Resume,
    /// 已完成，无需动作。
    Idle,
    /// 未知格式或空会话：不调度。
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlanItem {
    pub session_id: String,
    pub path: String,
    pub action: SessionAction,
    /// 0 最高；数字越小越先处理。
    pub priority: u8,
    pub reason: String,
    #[serde(default)]
    pub task: Option<String>,
    /// 关联会话 id（planner/executor、parent/child）。
    #[serde(default)]
    pub links: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlanReport {
    pub schema: u32,
    pub root: String,
    pub items: Vec<SessionPlanItem>,
    /// 链接里指向未扫描会话的 id；留给上层补扫或标记孤儿。
    #[serde(default)]
    pub unassigned: Vec<String>,
}

/// 在扫描报告上生成任务分配；输出顺序即处理顺序。
pub fn plan_sessions(report: &SessionScanReport) -> SessionPlanReport {
    let mut links: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut unassigned = Vec::new();
    let known: std::collections::BTreeSet<&str> = report
        .sessions
        .iter()
        .map(|entry| entry.session_id.as_str())
        .collect();
    for link in &report.links {
        links
            .entry(link.from.as_str())
            .or_default()
            .push(link.to.clone());
        links
            .entry(link.to.as_str())
            .or_default()
            .push(link.from.clone());
        for side in [&link.from, &link.to] {
            if !known.contains(side.as_str()) {
                unassigned.push(side.clone());
            }
        }
    }
    unassigned.sort();
    unassigned.dedup();

    let mut items: Vec<SessionPlanItem> = report
        .sessions
        .iter()
        .map(|entry| plan_entry(entry, &links))
        .collect();
    items.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });

    SessionPlanReport {
        schema: SESSION_PLAN_SCHEMA,
        root: report.root.clone(),
        items,
        unassigned,
    }
}

fn plan_entry(entry: &SessionEntry, links: &BTreeMap<&str, Vec<String>>) -> SessionPlanItem {
    let (action, priority, reason, task) = classify(entry);
    let mut related = links
        .get(entry.session_id.as_str())
        .cloned()
        .unwrap_or_default();
    related.sort();
    related.dedup();
    SessionPlanItem {
        session_id: entry.session_id.clone(),
        path: entry.path.clone(),
        action,
        priority,
        reason,
        task,
        links: related,
    }
}

fn classify(entry: &SessionEntry) -> (SessionAction, u8, String, Option<String>) {
    let unfinished = entry.tasks.iter().rev().find(|task| !task.finished);
    let task = unfinished.map(|task| task.task.clone());

    if entry.format == SessionFormat::Unknown {
        return (
            SessionAction::Skip,
            90,
            "unknown format; not scheduled".into(),
            None,
        );
    }
    if entry.format == SessionFormat::Reasonix {
        return (
            SessionAction::Inspect,
            50,
            "external session (read-only adapter)".into(),
            task,
        );
    }
    if entry.in_doubt > 0 {
        return (
            SessionAction::Inspect,
            0,
            format!("in_doubt={}", entry.in_doubt),
            task,
        );
    }
    if let Some(gate) = &entry.last_gate
        && !gate.passed
    {
        return (
            SessionAction::Inspect,
            1,
            format!("gate blocked (status={}, exit={})", gate.status, gate.exit),
            task,
        );
    }
    if entry.open_turn {
        return (SessionAction::Resume, 2, "open turn".into(), task);
    }
    if let Some(unfinished) = unfinished {
        return (
            SessionAction::Resume,
            3,
            format!("unfinished turn {}", unfinished.index),
            Some(unfinished.task.clone()),
        );
    }
    if entry.corrupt_lines > 0 {
        return (
            SessionAction::Inspect,
            4,
            format!("corrupt_lines={}", entry.corrupt_lines),
            None,
        );
    }
    if !entry.tasks.is_empty() {
        return (SessionAction::Idle, 80, "all turns finished".into(), None);
    }
    (SessionAction::Skip, 90, "no turns".into(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_migration::SessionRole;
    use crate::session_scan::{
        LinkKind, SessionEntry, SessionGate, SessionLink, SessionMeta, SessionTask,
    };

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            path: format!("{id}.jsonl"),
            session_id: id.into(),
            project: None,
            role: SessionRole::Primary,
            format: SessionFormat::Trit,
            meta: None,
            bytes: 10,
            modified_ms: 0,
            records: 1,
            corrupt_lines: 0,
            tasks: Vec::new(),
            open_turn: false,
            in_doubt: 0,
            last_gate: None,
        }
    }

    fn task(index: usize, finished: bool) -> SessionTask {
        SessionTask {
            index,
            task: format!("task-{index}"),
            objective: String::new(),
            steps: 1,
            complete: finished,
            finished,
        }
    }

    #[test]
    fn priority_orders_in_doubt_gate_then_resume() {
        let mut in_doubt = entry("a-in-doubt");
        in_doubt.in_doubt = 2;

        let mut blocked = entry("b-gate");
        blocked.last_gate = Some(SessionGate {
            status: "failed".into(),
            exit: 1,
            passed: false,
        });

        let mut open = entry("c-open");
        open.open_turn = true;
        open.tasks = vec![task(1, false)];

        let mut idle = entry("d-idle");
        idle.tasks = vec![task(1, true)];
        let report = SessionScanReport {
            schema: 1,
            root: ".".into(),
            roots: vec![".".to_owned()],
            scanned: 4,
            sessions: vec![idle.clone(), open, blocked, in_doubt],
            links: Vec::new(),
            warnings: Vec::new(),
        };
        let plan = plan_sessions(&report);
        let ids: Vec<&str> = plan
            .items
            .iter()
            .map(|item| item.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a-in-doubt", "b-gate", "c-open", "d-idle"]);
        assert_eq!(plan.items[0].action, SessionAction::Inspect);
        assert_eq!(plan.items[1].action, SessionAction::Inspect);
        assert_eq!(plan.items[2].action, SessionAction::Resume);
        assert_eq!(plan.items[2].task.as_deref(), Some("task-1"));
        assert_eq!(plan.items[3].action, SessionAction::Idle);
    }

    #[test]
    fn unknown_format_is_skipped_and_links_are_attached() {
        let mut child = entry("child");
        child.format = SessionFormat::Unknown;
        child.meta = Some(SessionMeta {
            id: Some("child".into()),
            parent: Some("parent".into()),
            project: None,
            role: Some(SessionRole::Subagent),
            model: None,
            turns: None,
            preview: None,
            finished: false,
        });
        let parent = entry("parent");
        let report = SessionScanReport {
            schema: 1,
            root: ".".into(),
            roots: vec![".".to_owned()],
            scanned: 2,
            sessions: vec![child, parent],
            links: vec![SessionLink {
                kind: LinkKind::ParentChild,
                from: "parent".into(),
                to: "child".into(),
            }],
            warnings: Vec::new(),
        };
        let plan = plan_sessions(&report);
        let child = plan
            .items
            .iter()
            .find(|item| item.session_id == "child")
            .unwrap();
        assert_eq!(child.action, SessionAction::Skip);
        assert_eq!(child.links, vec!["parent".to_owned()]);
        let parent = plan
            .items
            .iter()
            .find(|item| item.session_id == "parent")
            .unwrap();
        assert_eq!(parent.links, vec!["child".to_owned()]);
        assert!(plan.unassigned.is_empty());
    }

    #[test]
    fn missing_parent_is_reported_as_unassigned() {
        let report = SessionScanReport {
            schema: 1,
            root: ".".into(),
            roots: vec![".".to_owned()],
            scanned: 1,
            sessions: vec![entry("child")],
            links: vec![SessionLink {
                kind: LinkKind::ParentChild,
                from: "missing-parent".into(),
                to: "child".into(),
            }],
            warnings: Vec::new(),
        };
        let plan = plan_sessions(&report);
        assert_eq!(plan.unassigned, vec!["missing-parent".to_owned()]);
    }

    #[test]
    fn reasonix_sessions_are_inspected_not_resumed() {
        let mut chat = entry("chat");
        chat.format = SessionFormat::Reasonix;
        chat.tasks = vec![task(1, false)];
        let report = SessionScanReport {
            schema: 1,
            root: ".".into(),
            roots: vec![".".to_owned()],
            scanned: 1,
            sessions: vec![chat],
            links: Vec::new(),
            warnings: Vec::new(),
        };
        let plan = plan_sessions(&report);
        assert_eq!(plan.items[0].action, SessionAction::Inspect);
        assert_eq!(plan.items[0].priority, 50);
        assert_eq!(plan.items[0].task.as_deref(), Some("task-1"));
        assert!(plan.items[0].reason.contains("read-only"));
    }
}
