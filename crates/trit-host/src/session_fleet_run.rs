//! 执行多会话调度计划：复用 `trit_orch::fleet::execute` 的依赖/并发/失败传播。
//!
//! 与 `run_fleet` 的 Mutex 串行版不同，这里的 runner 是 `Fn + Send + Sync`：
//! 每个任务在 worker 线程里独立创建 provider/model，真正的并发由 fleet runtime
//! 的 `workers` 决定。写权限仍按 `WorkspaceConfig` 默认只读，不开旁路。
//!
//! 失败语义沿用 fleet：第一个失败后不再启动新任务，未启动的进 `cancelled`/`deferred`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use trit_orch::{
    CancelToken, Dependency, FleetConfig, FleetOutput, Plan, Task, TaskGuidance, TaskSpec, Ternary,
    execute,
};

use crate::openai::{OpenAiConfig, OpenAiProvider};
use crate::provider::{ProviderModel, default_tools};
use crate::session_fleet::SessionFleetReport;
use crate::tools::WorkspaceConfig;
use crate::{AgentError, ContextPolicy, HeadlessConfig, run_headless, turn_spec_from_task};

pub const SESSION_FLEET_RUN_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRunOptions {
    pub workers: usize,
    pub max_steps: usize,
    /// 失败后重跑未完成任务（deferred + 失败任务）的次数上限。
    #[serde(default)]
    pub retries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRunOutcome {
    pub schema: u32,
    /// 实际执行轮数（首次 + 重试）。
    #[serde(default)]
    pub attempts: usize,
    pub completed: Vec<String>,
    #[serde(default)]
    pub failed: Option<String>,
    #[serde(default)]
    pub deferred: Vec<String>,
    #[serde(default)]
    pub cancelled: Vec<String>,
    #[serde(default)]
    pub references: BTreeMap<String, String>,
}

/// 把调度报告映射成 `trit_orch::Plan`，依赖一律 `True`（必需）。
pub fn build_plan(report: &SessionFleetReport) -> Result<Plan, AgentError> {
    let tasks = report
        .tasks
        .iter()
        .map(|task| TaskSpec {
            task: Task {
                id: task.id.clone(),
                duration: 1,
                priority: 0,
                deps: task
                    .deps
                    .iter()
                    .map(|dep| Dependency {
                        task: dep.clone(),
                        required: Ternary::True,
                    })
                    .collect(),
            },
            guidance: TaskGuidance {
                objective: task.task.clone(),
                ..TaskGuidance::default()
            },
            ..TaskSpec::default()
        })
        .collect();
    Plan::new(1, tasks).map_err(|error| AgentError::SessionState(format!("fleet plan: {error}")))
}

/// 执行调度计划；每个任务一个独立 session，provider 配置由调用方从环境构造。
pub fn execute_fleet(
    report: &SessionFleetReport,
    workspace: &Path,
    session_dir: &Path,
    config: OpenAiConfig,
    options: FleetRunOptions,
) -> Result<FleetRunOutcome, AgentError> {
    let cancel = CancelToken::new();
    // Ctrl-C 只做协作式取消；安装失败（例如已安装）不阻止执行。
    let _ = crate::cancel_guard::install(cancel.clone());

    let mut outcome = execute_once(
        report,
        workspace,
        session_dir,
        config.clone(),
        &options,
        cancel.clone(),
    )?;
    let mut attempts = 1usize;
    while outcome.failed.is_some() && attempts <= options.retries {
        let mut ids = std::collections::BTreeSet::new();
        ids.extend(outcome.deferred.iter().cloned());
        if let Some(failed) = &outcome.failed {
            ids.insert(failed.clone());
        }
        if ids.is_empty() {
            break;
        }
        let subset = retry_subset(report, &ids);
        if subset.tasks.is_empty() {
            break;
        }
        let next = execute_once(
            &subset,
            workspace,
            session_dir,
            config.clone(),
            &options,
            cancel.clone(),
        )?;
        attempts += 1;
        outcome = merge_outcomes(outcome, next, attempts);
    }
    outcome.attempts = attempts;
    Ok(outcome)
}

fn execute_once(
    report: &SessionFleetReport,
    workspace: &Path,
    session_dir: &Path,
    config: OpenAiConfig,
    options: &FleetRunOptions,
    cancel: CancelToken,
) -> Result<FleetRunOutcome, AgentError> {
    let plan = build_plan(report)?;
    let config = std::sync::Arc::new(config);
    let fallback_workspace = workspace.to_path_buf();
    let workspaces: BTreeMap<String, PathBuf> = report
        .tasks
        .iter()
        .filter_map(|task| {
            task.workspace
                .as_ref()
                .map(|workspace| (task.id.clone(), PathBuf::from(workspace)))
        })
        .collect();
    let parents: BTreeMap<String, String> = report
        .tasks
        .iter()
        .filter_map(|task| {
            task.parent
                .as_ref()
                .map(|parent| (task.id.clone(), parent.clone()))
        })
        .collect();
    let session_dir = session_dir.to_path_buf();
    let max_steps = options.max_steps;
    let runner = move |task: &TaskSpec, cancel: &CancelToken| -> Result<FleetOutput, String> {
        if cancel.is_cancelled() {
            return Err("cancelled before start".to_owned());
        }
        let provider = OpenAiProvider::new((*config).clone());
        let model = ProviderModel::with_tools(provider, default_tools());
        let workspace = workspaces
            .get(&task.task.id)
            .cloned()
            .unwrap_or_else(|| fallback_workspace.clone());
        let session = session_dir.join(format!("{}.jsonl", task.task.id));
        if let Some(parent) = parents.get(&task.task.id) {
            let mut meta_path = session.as_os_str().to_os_string();
            meta_path.push(".meta");
            let meta = serde_json::json!({"id": task.task.id, "parent": parent});
            if let Ok(bytes) = serde_json::to_vec(&meta) {
                let _ = std::fs::write(meta_path, bytes);
            }
        }
        let headless = HeadlessConfig {
            workspace: WorkspaceConfig::new(&workspace),
            max_steps,
            session: Some(session),
            context: ContextPolicy::default(),
            write_guard: None,
        };
        let outcome = run_headless(turn_spec_from_task(task), model, headless)
            .map_err(|error| error.to_string())?;
        let tokens = outcome.usage.total().map_err(|error| error.to_string())?;
        Ok(FleetOutput {
            content: outcome.render_markdown().into_bytes(),
            tokens,
        })
    };
    let report = execute(
        &plan,
        FleetConfig::new(options.workers.max(1)),
        cancel,
        runner,
    )
    .map_err(|error| AgentError::SessionState(format!("fleet: {error}")))?;
    let references = report
        .references()
        .into_iter()
        .map(|(task, reference)| (task.to_owned(), reference))
        .collect();
    Ok(FleetRunOutcome {
        schema: SESSION_FLEET_RUN_SCHEMA,
        attempts: 1,
        completed: report.completed,
        failed: report.failed.map(|failure| failure.task),
        deferred: report.deferred,
        cancelled: report.cancelled,
        references,
    })
}

/// 只保留 `ids` 中的任务；指向集合外的依赖被移除，再重新分层。
fn retry_subset(
    report: &SessionFleetReport,
    ids: &std::collections::BTreeSet<String>,
) -> SessionFleetReport {
    let tasks: Vec<crate::session_fleet::FleetTask> = report
        .tasks
        .iter()
        .filter(|task| ids.contains(&task.id))
        .map(|task| {
            let mut task = task.clone();
            task.deps.retain(|dep| ids.contains(dep));
            task
        })
        .collect();
    let (layers, cyclic) = crate::session_fleet::topological_layers(&tasks);
    SessionFleetReport {
        schema: report.schema,
        workers: report.workers,
        tasks,
        layers,
        skipped: Vec::new(),
        cyclic,
    }
}

/// 合并重试结果：completed 累加，failed/deferred/cancelled 以最后一轮为准。
fn merge_outcomes(
    mut target: FleetRunOutcome,
    next: FleetRunOutcome,
    attempts: usize,
) -> FleetRunOutcome {
    target.attempts = attempts;
    target.completed.extend(next.completed);
    target.completed.sort();
    target.completed.dedup();
    target.failed = next.failed;
    target.deferred = next.deferred;
    target.cancelled = next.cancelled;
    target.references.extend(next.references);
    target
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_fleet::{FleetTask, SESSION_FLEET_SCHEMA};

    #[test]
    fn plan_carries_dependencies() {
        let report = SessionFleetReport {
            schema: SESSION_FLEET_SCHEMA,
            workers: 2,
            tasks: vec![
                FleetTask {
                    id: "parent".into(),
                    task: "do parent".into(),
                    deps: Vec::new(),
                    workspace: None,
                    parent: None,
                },
                FleetTask {
                    id: "child".into(),
                    task: "do child".into(),
                    deps: vec!["parent".into()],
                    workspace: None,
                    parent: Some("parent".into()),
                },
            ],
            layers: vec![vec!["parent".into()], vec!["child".into()]],
            skipped: Vec::new(),
            cyclic: Vec::new(),
        };
        let plan = build_plan(&report).unwrap();
        assert_eq!(plan.tasks().len(), 2);
        let child = plan.task("child").unwrap();
        assert_eq!(child.task.deps.len(), 1);
        assert_eq!(child.task.deps[0].task, "parent");
        assert_eq!(child.task.deps[0].required, Ternary::True);
        assert_eq!(child.guidance.objective, "do child");
    }

    #[test]
    fn cyclic_plan_is_rejected_by_trit_orch() {
        let report = SessionFleetReport {
            schema: SESSION_FLEET_SCHEMA,
            workers: 1,
            tasks: vec![
                FleetTask {
                    id: "a".into(),
                    task: "a".into(),
                    deps: vec!["b".into()],
                    workspace: None,
                    parent: Some("b".into()),
                },
                FleetTask {
                    id: "b".into(),
                    task: "b".into(),
                    deps: vec!["a".into()],
                    workspace: None,
                    parent: Some("a".into()),
                },
            ],
            layers: Vec::new(),
            skipped: Vec::new(),
            cyclic: vec!["a".into(), "b".into()],
        };
        assert!(build_plan(&report).is_err());
    }

    #[test]
    fn retry_subset_filters_external_deps_and_rebuilds_layers() {
        let report = SessionFleetReport {
            schema: SESSION_FLEET_SCHEMA,
            workers: 2,
            tasks: vec![
                FleetTask {
                    id: "a".into(),
                    task: "a".into(),
                    deps: Vec::new(),
                    workspace: None,
                    parent: None,
                },
                FleetTask {
                    id: "b".into(),
                    task: "b".into(),
                    deps: vec!["a".into()],
                    workspace: None,
                    parent: Some("a".into()),
                },
                FleetTask {
                    id: "c".into(),
                    task: "c".into(),
                    deps: vec!["b".into()],
                    workspace: None,
                    parent: Some("b".into()),
                },
            ],
            layers: vec![vec!["a".into()], vec!["b".into()], vec!["c".into()]],
            skipped: Vec::new(),
            cyclic: Vec::new(),
        };
        let ids: std::collections::BTreeSet<String> =
            ["b", "c"].into_iter().map(str::to_owned).collect();
        let subset = retry_subset(&report, &ids);
        assert_eq!(subset.tasks.len(), 2);
        let b = subset.tasks.iter().find(|task| task.id == "b").unwrap();
        assert!(b.deps.is_empty(), "external dep must be dropped");
        assert_eq!(
            subset.layers,
            vec![vec!["b".to_owned()], vec!["c".to_owned()]]
        );
    }
}
