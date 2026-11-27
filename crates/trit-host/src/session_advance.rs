//! 一条命令推进多会话：scan → plan → auto dispatch → fleet run → callback → verify。
//!
//! 默认 dry-run，只输出将派发/将执行的任务；`--apply` 才写派发单并调用 fleet。
//! 执行会话使用 `<source>.run` 任务 id，写到独立 `<source>.run.jsonl`，不覆盖原会话或派发单。

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use trit_orch::RemoteNode;

use crate::AgentError;
use crate::openai::OpenAiConfig;
use crate::session_callback::{CallbackOptions, CallbackReport, callback_sessions};
use crate::session_dispatch::{AutoDispatchOptions, DispatchReport, auto_dispatch};
use crate::session_fleet::{
    FleetTask, SESSION_FLEET_SCHEMA, SessionFleetReport, apply_budget, topological_layers,
};
use crate::session_fleet_remote::{RemoteFleetOptions, RemoteFleetOutcome, run_remote_fleet};
use crate::session_fleet_run::{FleetRunOptions, FleetRunOutcome, execute_fleet};
use crate::session_plan::{SessionPlanReport, plan_sessions};
use crate::session_scan::{SessionScanReport, scan_sessions, scan_sessions_cached};
use crate::session_verify::{CallbackVerifyReport, verify_callbacks};

pub const SESSION_ADVANCE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdvanceOptions {
    pub max_items: usize,
    pub workers: usize,
    pub max_steps: usize,
    pub retries: usize,
    pub budget_tokens: u64,
    /// 美元预算；0 表示不限。跨过 80% 时报告告警。
    #[serde(default)]
    pub budget_usd: f64,
    pub dry_run: bool,
    /// 跨机执行时的计划作用域。
    #[serde(default)]
    pub scope: Option<String>,
}

impl Default for AdvanceOptions {
    fn default() -> Self {
        Self {
            max_items: 4,
            workers: 4,
            max_steps: 20,
            retries: 0,
            budget_tokens: 0,
            budget_usd: 0.0,
            dry_run: true,
            scope: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdvanceReport {
    pub schema: u32,
    pub dry_run: bool,
    pub scanned: usize,
    pub planned: usize,
    pub dispatch: DispatchReport,
    pub fleet: SessionFleetReport,
    #[serde(default)]
    pub run: Option<FleetRunOutcome>,
    /// 提供 `--nodes` 时的跨机执行摘要；此时不做本地回填。
    #[serde(default)]
    pub remote: Option<RemoteFleetOutcome>,
    #[serde(default)]
    pub callbacks: Option<CallbackReport>,
    #[serde(default)]
    pub verification: Option<CallbackVerifyReport>,
    /// 本次推进的成本汇总（人民币）；没有任何可定价用量时为 None。
    #[serde(default)]
    pub cost: Option<CostSummary>,
}

/// 本地 + 跨机成本汇总；金额单位固定为人民币。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSummary {
    pub local_cny: f64,
    pub remote_cny: f64,
    pub total_cny: f64,
    pub currency: String,
}

/// 编排一次推进；`config` 只在 `dry_run = false` 时需要。
pub fn advance(
    scan: &SessionScanReport,
    plan: &SessionPlanReport,
    workspace: &Path,
    session_dir: &Path,
    cache: Option<&Path>,
    config: Option<OpenAiConfig>,
    nodes: Option<Vec<RemoteNode>>,
    options: &AdvanceOptions,
) -> Result<AdvanceReport, AgentError> {
    let dispatch = auto_dispatch(
        scan,
        plan,
        &AutoDispatchOptions {
            max_items: options.max_items,
            dry_run: options.dry_run,
        },
    )?;
    let fleet = fleet_from_dispatch(scan, &dispatch, options.workers, options.budget_tokens)
        .map_err(AgentError::SessionState)?;
    let mut report = AdvanceReport {
        schema: SESSION_ADVANCE_SCHEMA,
        dry_run: options.dry_run,
        scanned: scan.scanned,
        planned: plan.items.len(),
        dispatch,
        fleet,
        run: None,
        remote: None,
        callbacks: None,
        verification: None,
        cost: None,
    };
    if options.dry_run {
        return Ok(report);
    }

    if let Some(nodes) = nodes {
        let remote = run_remote_fleet(
            &report.fleet,
            nodes,
            &RemoteFleetOptions {
                workers: options.workers,
                retry_limit: options.retries as u32,
                health_probe: false,
                dry_run: false,
                scope: options.scope.clone(),
            },
        )?;
        report.remote = Some(remote);
        summarize_cost(&mut report);
        return Ok(report);
    }

    let config = config.ok_or_else(|| {
        AgentError::Provider("session-advance --apply requires provider config".into())
    })?;
    let run = execute_fleet(
        &report.fleet,
        workspace,
        session_dir,
        config,
        FleetRunOptions {
            workers: options.workers,
            max_steps: options.max_steps,
            retries: options.retries,
            budget_usd: options.budget_usd,
        },
    )?;
    let rescanned = match cache {
        Some(path) => scan_sessions_cached(session_dir, path)?,
        None => scan_sessions(session_dir)?,
    };
    let callbacks = callback_sessions(&rescanned, &CallbackOptions { dry_run: false })?;
    let verification = verify_callbacks(&rescanned)?;
    report.run = Some(run);
    report.callbacks = Some(callbacks);
    report.verification = Some(verification);
    summarize_cost(&mut report);
    Ok(report)
}

/// 汇总本地 run 与跨机 remote 的成本；两者都无定价时不生成汇总。
fn summarize_cost(report: &mut AdvanceReport) {
    let local_cny = report
        .run
        .as_ref()
        .and_then(|run| run.cost.as_ref())
        .map(|cost| cost.cny)
        .unwrap_or(0.0);
    let remote_cny = report
        .remote
        .as_ref()
        .and_then(|remote| remote.cost_cny)
        .unwrap_or(0.0);
    if local_cny > 0.0 || remote_cny > 0.0 {
        report.cost = Some(CostSummary {
            local_cny,
            remote_cny,
            total_cny: local_cny + remote_cny,
            currency: "CNY".into(),
        });
    }
}

/// 只把已派发（或 dry-run 将派发）的 resume 项映射成 fleet 任务。
fn fleet_from_dispatch(
    scan: &SessionScanReport,
    dispatch: &DispatchReport,
    workers: usize,
    budget_tokens: u64,
) -> Result<SessionFleetReport, String> {
    let selected: BTreeSet<String> = dispatch
        .items
        .iter()
        .map(|item| item.source_session.clone())
        .collect();
    let mut seen = BTreeSet::new();
    let mut tasks = Vec::new();
    for item in &dispatch.items {
        let id = task_id(&item.source_session);
        if !seen.insert(safe_id(&id)) {
            return Err(format!("duplicate fleet task id after sanitization: {id}"));
        }
        let entry = scan
            .sessions
            .iter()
            .find(|entry| entry.session_id == item.source_session);
        let parent = scan
            .links
            .iter()
            .find(|link| link.to == item.source_session && selected.contains(&link.from))
            .map(|link| link.from.clone());
        let deps = scan
            .links
            .iter()
            .filter(|link| link.to == item.source_session && selected.contains(&link.from))
            .map(|link| task_id(&link.from))
            .collect();
        tasks.push(FleetTask {
            id,
            task: item.task.clone(),
            deps,
            workspace: entry.and_then(|entry| entry.project.clone()),
            parent,
            max_tokens: 0,
        });
    }
    let (layers, cyclic) = topological_layers(&tasks);
    let mut report = SessionFleetReport {
        schema: SESSION_FLEET_SCHEMA,
        workers: workers.max(1),
        tasks,
        layers,
        skipped: Vec::new(),
        cyclic,
    };
    apply_budget(&mut report, budget_tokens)?;
    Ok(report)
}

/// 从扫描报告重新生成计划；CLI 用它把 scan/plan 两步收敛。
pub fn plan_from_scan(scan: &SessionScanReport) -> SessionPlanReport {
    plan_sessions(scan)
}

fn task_id(source: &str) -> String {
    format!("{source}.run")
}

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_migration::SessionRole;
    use crate::session_scan::{
        SessionEntry, SessionFormat, SessionLink, SessionScanReport, SessionTask,
    };
    use std::fs;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "trit-session-advance-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn entry(root: &Path, id: &str) -> SessionEntry {
        SessionEntry {
            path: root.join(format!("{id}.jsonl")).display().to_string(),
            session_id: id.into(),
            project: Some(root.display().to_string()),
            role: SessionRole::Primary,
            format: SessionFormat::Trit,
            meta: None,
            bytes: 1,
            modified_ms: 0,
            records: 1,
            corrupt_lines: 0,
            tasks: vec![SessionTask {
                index: 1,
                task: format!("task-{id}"),
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

    fn report(root: &Path) -> SessionScanReport {
        SessionScanReport {
            schema: 1,
            root: root.display().to_string(),
            roots: vec![root.display().to_string()],
            scanned: 2,
            sessions: vec![entry(root, "parent"), entry(root, "child")],
            links: vec![SessionLink {
                kind: crate::session_scan::LinkKind::ParentChild,
                from: "parent".into(),
                to: "child".into(),
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn dry_run_plans_dispatch_and_fleet_without_writing() {
        let root = temp("dry");
        let scan = report(&root);
        let plan = plan_from_scan(&scan);
        let report = advance(
            &scan,
            &plan,
            &root,
            &root,
            None,
            None,
            None,
            &AdvanceOptions {
                max_items: 2,
                ..AdvanceOptions::default()
            },
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.dispatch.items.len(), 2);
        assert_eq!(report.fleet.tasks.len(), 2);
        assert_eq!(
            report.fleet.layers,
            vec![vec!["parent.run".to_owned()], vec!["child.run".to_owned()]]
        );
        assert!(report.run.is_none());
        assert!(!root.join("parent.run.jsonl").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn max_items_limits_the_fleet() {
        let root = temp("limit");
        let scan = report(&root);
        let plan = plan_from_scan(&scan);
        let report = advance(
            &scan,
            &plan,
            &root,
            &root,
            None,
            None,
            None,
            &AdvanceOptions {
                max_items: 1,
                ..AdvanceOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.dispatch.items.len(), 1);
        assert_eq!(report.fleet.tasks.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }
}
