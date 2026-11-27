//! 跨机 fleet 派发：把 `session-fleet` 的任务图交给节点表执行。
//!
//! 复用 `trit-orch` 的 `Plan` + `NodeTable` + `run_remote_plan_with_table`：
//! 节点容量、失败隔离、换节点重试、租约/幂等键都由现有层负责。这里只做
//! “会话任务 → SubagentRequest”的映射，并保持只读 profile 的默认权限。
//!
//! 默认 dry-run；`--apply` 才真正向远程端点发请求。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, NodeTable, RemoteNode, SubagentProfile,
    SubagentRequest, TaskSpec, TokenUsage,
};

use crate::AgentError;
use crate::cost::PriceTable;
use crate::remote_fleet::{RemotePlanConfig, RemotePlanReport, run_remote_plan_with_table};
use crate::session_fleet::SessionFleetReport;
use crate::session_fleet_run::build_plan;

pub const SESSION_FLEET_REMOTE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFleetOptions {
    pub workers: usize,
    pub retry_limit: u32,
    pub health_probe: bool,
    pub dry_run: bool,
    /// 计划作用域；不同计划用不同值，避免同名任务在共享账本中碰撞。
    #[serde(default)]
    pub scope: Option<String>,
}

impl Default for RemoteFleetOptions {
    fn default() -> Self {
        Self {
            workers: 4,
            retry_limit: 0,
            health_probe: false,
            dry_run: true,
            scope: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteFleetOutcome {
    pub schema: u32,
    pub dry_run: bool,
    pub tasks: Vec<String>,
    pub nodes: Vec<String>,
    #[serde(default)]
    pub budget_tokens: u64,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub failed: Option<String>,
    #[serde(default)]
    pub deferred: Vec<String>,
    #[serde(default)]
    pub assignments: BTreeMap<String, String>,
    /// 远端自报的 token 用量（按每次尝试累加；旧节点不返回时为 0）。
    #[serde(default)]
    pub usage: TokenUsage,
    /// 按远端模型单价折算的预估费用（美元）；没有任何可定价的尝试时为 None。
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// 按当前汇率折算的预估费用（人民币）；默认展示口径。
    #[serde(default)]
    pub cost_cny: Option<f64>,
    /// 成本模型告警（未知模型、定价文件非法等）。
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 执行跨机派发；`dry_run` 时只回任务/节点清单。
pub fn run_remote_fleet(
    report: &SessionFleetReport,
    nodes: Vec<RemoteNode>,
    options: &RemoteFleetOptions,
) -> Result<RemoteFleetOutcome, AgentError> {
    let tasks: Vec<String> = report.tasks.iter().map(|task| task.id.clone()).collect();
    let node_ids: Vec<String> = nodes.iter().map(|node| node.id.clone()).collect();
    let budget_tokens: u64 = report.tasks.iter().map(|task| task.max_tokens).sum();
    if options.dry_run {
        return Ok(RemoteFleetOutcome {
            schema: SESSION_FLEET_REMOTE_SCHEMA,
            dry_run: true,
            tasks,
            nodes: node_ids,
            budget_tokens,
            completed: Vec::new(),
            failed: None,
            deferred: Vec::new(),
            assignments: BTreeMap::new(),
            usage: TokenUsage::default(),
            cost_usd: None,
            cost_cny: None,
            warnings: Vec::new(),
        });
    }

    let plan = build_plan(report)?;
    let table = Arc::new(
        NodeTable::new(nodes)
            .map_err(|error| AgentError::SessionState(format!("remote node table: {error}")))?,
    );
    let config = RemotePlanConfig {
        workers: options.workers.max(1),
        failure_threshold: 1,
        retry_limit: options.retry_limit,
        scope: options.scope.clone(),
        health_probe_on_start: options.health_probe,
        health_probe_interval: None,
        health_probe_concurrency: 1,
        health_max_age: None,
        retry_backoff: Duration::ZERO,
        retry_backoff_max: Duration::from_secs(30),
    };
    let report = run_remote_plan_with_table(&plan, table, config, CancelToken::new(), |task| {
        build_request(task)
    })
    .map_err(|error| AgentError::SessionState(format!("remote fleet: {error}")))?;
    Ok(outcome_from_report(report))
}

fn build_request(task: &TaskSpec) -> SubagentRequest {
    let capsule = ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "objective".into(),
            value: task.guidance.objective.clone(),
        }],
        &["objective"],
        64 * 1024,
    )
    .unwrap_or_else(|_| {
        ContextCapsule::inherit(Vec::new(), &[], 0).expect("empty capsule is always valid")
    });
    let profile = SubagentProfile {
        name: format!("fleet-{}", task.task.id),
        access: AccessMode::ReadOnly,
        write_roots: Vec::new(),
        tools: vec!["read_file".into()],
        max_tokens: task.max_tokens,
    };
    SubagentRequest::new(task, capsule, profile)
}

fn outcome_from_report(report: RemotePlanReport) -> RemoteFleetOutcome {
    let mut usage = TokenUsage::default();
    let mut warnings = Vec::new();
    let mut cost_usd = 0.0f64;
    let mut priced = false;
    let table = match PriceTable::from_env() {
        Ok(table) => Some(table),
        Err(error) => {
            warnings.push(format!("cost model: {error}"));
            None
        }
    };
    for attempts in report.attempts.values() {
        for attempt in attempts {
            let Some(remote) = attempt.usage else {
                continue;
            };
            usage.fresh += remote.fresh;
            usage.cached += remote.cached;
            usage.output += remote.output;
            usage.estimated |= remote.estimated;
            let (Some(table), Some(model)) = (table.as_ref(), attempt.model.as_deref()) else {
                continue;
            };
            let local = TokenUsage {
                fresh: remote.fresh,
                cached: remote.cached,
                output: remote.output,
                estimated: remote.estimated,
            };
            match table.estimate(model, &local) {
                Some(estimate) => {
                    cost_usd += estimate.usd;
                    priced = true;
                }
                None => warnings.push(format!("cost model: no price for model {model}")),
            }
        }
    }
    warnings.sort();
    warnings.dedup();
    RemoteFleetOutcome {
        schema: SESSION_FLEET_REMOTE_SCHEMA,
        dry_run: false,
        tasks: report.results.keys().cloned().collect(),
        nodes: report.nodes.iter().map(|node| node.id.clone()).collect(),
        budget_tokens: report.results.values().map(|result| result.tokens).sum(),
        completed: report.report.completed,
        failed: report.report.failed.map(|failure| failure.task),
        deferred: report.report.deferred,
        assignments: report.assignments,
        usage,
        cost_usd: priced.then_some(cost_usd),
        cost_cny: priced.then_some(cost_usd * crate::cost::usd_cny_rate()),
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_fleet::{FleetTask, SESSION_FLEET_SCHEMA};

    fn report() -> SessionFleetReport {
        SessionFleetReport {
            schema: SESSION_FLEET_SCHEMA,
            workers: 2,
            tasks: vec![FleetTask {
                id: "task-1".into(),
                task: "do work".into(),
                deps: Vec::new(),
                workspace: None,
                parent: None,
                max_tokens: 100,
            }],
            layers: vec![vec!["task-1".into()]],
            skipped: Vec::new(),
            cyclic: Vec::new(),
        }
    }

    #[test]
    fn dry_run_lists_tasks_and_nodes_without_network() {
        let nodes = vec![RemoteNode::new(
            "node-1",
            trit_orch::RemoteEndpoint {
                url: "http://127.0.0.1:9".into(),
                auth_token: None,
                timeout_secs: 1,
            },
        )];
        let outcome = run_remote_fleet(&report(), nodes, &RemoteFleetOptions::default()).unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.tasks, vec!["task-1".to_owned()]);
        assert_eq!(outcome.nodes, vec!["node-1".to_owned()]);
        assert!(outcome.completed.is_empty());
    }

    #[test]
    fn request_uses_read_only_profile_and_objective() {
        let plan = build_plan(&report()).unwrap();
        let task = plan.task("task-1").unwrap();
        let request = build_request(task);
        assert_eq!(request.task, "task-1");
        assert_eq!(request.profile.access, AccessMode::ReadOnly);
        assert!(request.profile.write_roots.is_empty());
        assert_eq!(request.profile.max_tokens, 100);
    }
}
