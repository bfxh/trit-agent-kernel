//! 执行多会话调度计划：复用 `trit_orch::fleet::execute` 的依赖/并发/失败传播。
//!
//! 与 `run_fleet` 的 Mutex 串行版不同，这里的 runner 是 `Fn + Send + Sync`：
//! 每个任务在 worker 线程里独立创建 provider/model，真正的并发由 fleet runtime
//! 的 `workers` 决定。写权限仍按 `WorkspaceConfig` 默认只读，不开旁路。
//!
//! 失败语义沿用 fleet：第一个失败后不再启动新任务，未启动的进 `cancelled`/`deferred`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use trit_orch::{
    CancelToken, Dependency, FleetConfig, FleetOutput, Plan, Task, TaskGuidance, TaskSpec, Ternary,
    TokenUsage, execute,
};

use crate::cost::{self, BudgetAlert, CostEstimate, PriceTable};

use crate::openai::{OpenAiConfig, OpenAiProvider};
use crate::provider::{ProviderModel, default_tools};
use crate::session::{SessionEvent, SessionLog, SessionRecord};
use crate::session_fleet::SessionFleetReport;
use crate::tools::WorkspaceConfig;
use crate::{AgentError, ContextPolicy, HeadlessConfig, run_headless, turn_spec_from_task};

pub const SESSION_FLEET_RUN_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FleetRunOptions {
    pub workers: usize,
    pub max_steps: usize,
    /// 失败后重跑未完成任务（deferred + 失败任务）的次数上限。
    #[serde(default)]
    pub retries: usize,
    /// 美元预算；0 表示不限。达到 80% 时报告告警，不改变执行行为。
    #[serde(default)]
    pub budget_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 本次执行（含重试）累计的真实 token 用量。
    #[serde(default)]
    pub usage: TokenUsage,
    /// 按模型单价折算的预估费用；模型不在价目表时为 None。
    #[serde(default)]
    pub cost: Option<CostEstimate>,
    /// 达到预算阈值时的结构化告警。
    #[serde(default)]
    pub budget_alert: Option<BudgetAlert>,
    /// 人读告警文本（预算告警、未知模型等）。
    #[serde(default)]
    pub warnings: Vec<String>,
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
            max_tokens: task.max_tokens,
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
    if outcome.usage.total().unwrap_or(0) > 0 {
        match PriceTable::from_env() {
            Ok(table) => {
                if let Some(estimate) = table.estimate(&config.model, &outcome.usage) {
                    if let Some(alert) = cost::budget_alert(
                        estimate.usd,
                        options.budget_usd,
                        cost::DEFAULT_ALERT_THRESHOLD,
                    ) {
                        outcome.warnings.push(alert.warning());
                        outcome.budget_alert = Some(alert);
                    }
                    outcome.cost = Some(estimate);
                } else {
                    outcome.warnings.push(format!(
                        "cost model: no price for model {}; cost unknown",
                        config.model
                    ));
                }
            }
            Err(error) => return Err(AgentError::SessionState(format!("cost model: {error}"))),
        }
    }
    Ok(outcome)
}

/// 续跑：跳过上次已经完成（session 文件最后一条 `TurnFinished.complete=true`）的任务。
///
/// 只读检查文件，不创建或追加任何 session；读不到/解析失败一律视为未完成。
pub fn skip_completed_tasks(report: &SessionFleetReport, session_dir: &Path) -> SessionFleetReport {
    let tasks: Vec<crate::session_fleet::FleetTask> = report
        .tasks
        .iter()
        .filter(|task| !task_completed(session_dir, &task.id))
        .cloned()
        .collect();
    let (layers, cyclic) = crate::session_fleet::topological_layers(&tasks);
    SessionFleetReport {
        schema: report.schema,
        workers: report.workers,
        tasks,
        layers,
        skipped: report.skipped.clone(),
        cyclic,
    }
}

fn task_completed(dir: &Path, id: &str) -> bool {
    let path = safe_task_path(dir, id);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<SessionRecord>(line).ok())
        .and_then(|record| match record.event {
            SessionEvent::TurnFinished { complete, .. } => Some(complete),
            _ => None,
        })
        .unwrap_or(false)
}

/// 续跑前归档未完成的旧 session，避免新 turn 追加进同一文件。
///
/// 只做同目录 rename；失败直接返回错误，由调用方 fail closed。
fn archive_incomplete_session(dir: &Path, id: &str) -> Result<(), String> {
    let path = safe_task_path(dir, id);
    if !path.is_file() {
        return Ok(());
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_owned());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let archived = dir.join(format!("{stem}.prev-{nanos}.jsonl"));
    std::fs::rename(&path, &archived)
        .map_err(|error| format!("archive {}: {error}", path.display()))
}

/// 该任务的 session 能否从中间状态续跑。
///
/// 只有「未收尾 turn 的 task 与本次要跑的 task 一致」才算可恢复；日志损坏、
/// 断点属于别的 task（例如任务改过名）都返回 false，调用方退回归档重跑。
fn session_resumable(dir: &Path, id: &str) -> bool {
    let path = safe_task_path(dir, id);
    SessionLog::open(&path)
        .ok()
        .and_then(|log| log.load().ok())
        .and_then(|records| crate::session::resume_turn(&records).ok().flatten())
        .is_some_and(|resume| resume.task == id)
}

/// 任务 id 只允许 `[A-Za-z0-9._-]`，避免 meta 里的 `../` 把 session 写出目录。
fn safe_task_path(dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let name = if safe.is_empty() {
        "task"
    } else {
        safe.as_str()
    };
    dir.join(format!("{name}.jsonl"))
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(format!("refusing symlink target {}", path.display()));
    }
    Ok(())
}

fn execute_once(
    report: &SessionFleetReport,
    workspace: &Path,
    session_dir: &Path,
    config: OpenAiConfig,
    options: &FleetRunOptions,
    cancel: CancelToken,
) -> Result<FleetRunOutcome, AgentError> {
    reject_symlink(session_dir).map_err(AgentError::SessionState)?;
    let plan = build_plan(report)?;
    let provider = OpenAiProvider::new(config);
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
    let usage_acc = Arc::new(Mutex::new(TokenUsage::default()));
    let usage_for_runner = Arc::clone(&usage_acc);
    let runner = move |task: &TaskSpec, cancel: &CancelToken| -> Result<FleetOutput, String> {
        if cancel.is_cancelled() {
            return Err("cancelled before start".to_owned());
        }
        let provider = provider.clone();
        let model = ProviderModel::with_tools(provider, default_tools());
        let workspace = workspaces
            .get(&task.task.id)
            .cloned()
            .unwrap_or_else(|| fallback_workspace.clone());
        let session = safe_task_path(&session_dir, &task.task.id);
        reject_symlink(&session)?;
        // 能从中断 turn 的中间状态恢复就不归档：归档会丢掉已经拿到的证据。
        // 日志损坏或断点属于别的任务时退回「归档后重跑」，保持旧语义。
        let resumable = session_resumable(&session_dir, &task.task.id);
        if !resumable && let Err(error) = archive_incomplete_session(&session_dir, &task.task.id) {
            return Err(error);
        }
        if let Some(parent) = parents.get(&task.task.id) {
            let mut meta_path = session.as_os_str().to_os_string();
            meta_path.push(".meta");
            reject_symlink(Path::new(&meta_path))?;
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
            resume: resumable,
        };
        let outcome = run_headless(turn_spec_from_task(task), model, headless)
            .map_err(|error| error.to_string())?;
        let tokens = outcome.usage.total().map_err(|error| error.to_string())?;
        {
            let mut usage = usage_for_runner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            usage.fresh += outcome.usage.fresh;
            usage.cached += outcome.usage.cached;
            usage.output += outcome.usage.output;
            usage.estimated |= outcome.usage.estimated;
        }
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
    let usage = *usage_acc
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        usage,
        cost: None,
        budget_alert: None,
        warnings: Vec::new(),
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
    target.usage.fresh += next.usage.fresh;
    target.usage.cached += next.usage.cached;
    target.usage.output += next.usage.output;
    target.usage.estimated |= next.usage.estimated;
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
                    max_tokens: 0,
                },
                FleetTask {
                    id: "child".into(),
                    task: "do child".into(),
                    deps: vec!["parent".into()],
                    workspace: None,
                    parent: Some("parent".into()),
                    max_tokens: 0,
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
                    max_tokens: 0,
                },
                FleetTask {
                    id: "b".into(),
                    task: "b".into(),
                    deps: vec!["a".into()],
                    workspace: None,
                    parent: Some("a".into()),
                    max_tokens: 0,
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
                    max_tokens: 0,
                },
                FleetTask {
                    id: "b".into(),
                    task: "b".into(),
                    deps: vec!["a".into()],
                    workspace: None,
                    parent: Some("a".into()),
                    max_tokens: 0,
                },
                FleetTask {
                    id: "c".into(),
                    task: "c".into(),
                    deps: vec!["b".into()],
                    workspace: None,
                    parent: Some("b".into()),
                    max_tokens: 0,
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

    #[test]
    fn task_id_cannot_escape_session_dir() {
        let path = safe_task_path(Path::new("root"), "../../evil");
        assert_eq!(path.parent(), Some(Path::new("root")));
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains('\\'), "{name}");
        assert!(name.ends_with("evil.jsonl"), "{name}");
    }

    #[test]
    fn merge_accumulates_usage_across_retries() {
        let outcome = |fresh, cached, output| FleetRunOutcome {
            schema: SESSION_FLEET_RUN_SCHEMA,
            attempts: 1,
            completed: Vec::new(),
            failed: None,
            deferred: Vec::new(),
            cancelled: Vec::new(),
            references: BTreeMap::new(),
            usage: TokenUsage {
                fresh,
                cached,
                output,
                estimated: false,
            },
            cost: None,
            budget_alert: None,
            warnings: Vec::new(),
        };
        let merged = merge_outcomes(outcome(10, 3, 7), outcome(5, 1, 2), 2);
        assert_eq!(merged.usage.fresh, 15);
        assert_eq!(merged.usage.cached, 4);
        assert_eq!(merged.usage.output, 9);
        assert_eq!(merged.attempts, 2);
    }

    #[test]
    fn skip_completed_tasks_drops_finished_sessions() {
        let dir = std::env::temp_dir().join(format!(
            "trit-resume-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = crate::session::SessionLog::open(dir.join("done.run.jsonl")).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "done".into(),
            objective: "o".into(),
            max_steps: 1,
        })
        .unwrap();
        log.append(SessionEvent::TurnFinished {
            steps: 1,
            complete: true,
            metrics: None,
        })
        .unwrap();
        let mut log = crate::session::SessionLog::open(dir.join("pending.run.jsonl")).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "pending".into(),
            objective: "o".into(),
            max_steps: 1,
        })
        .unwrap();
        let report = SessionFleetReport {
            schema: SESSION_FLEET_SCHEMA,
            workers: 1,
            tasks: vec![
                FleetTask {
                    id: "done.run".into(),
                    task: "done".into(),
                    deps: Vec::new(),
                    workspace: None,
                    parent: None,
                    max_tokens: 0,
                },
                FleetTask {
                    id: "pending.run".into(),
                    task: "pending".into(),
                    deps: Vec::new(),
                    workspace: None,
                    parent: None,
                    max_tokens: 0,
                },
            ],
            layers: vec![vec!["done.run".into(), "pending.run".into()]],
            skipped: Vec::new(),
            cyclic: Vec::new(),
        };
        let filtered = skip_completed_tasks(&report, &dir);
        assert_eq!(filtered.tasks.len(), 1);
        assert_eq!(filtered.tasks[0].id, "pending.run");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn archive_incomplete_session_moves_old_file() {
        let dir = std::env::temp_dir().join(format!(
            "trit-archive-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("task.run.jsonl"), "{}\n").unwrap();
        archive_incomplete_session(&dir, "task.run").unwrap();
        assert!(!dir.join("task.run.jsonl").exists());
        let archived = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".prev-"));
        assert!(archived);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_resumable_requires_matching_task_and_open_turn() {
        let dir = std::env::temp_dir().join(format!(
            "trit-resumable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut log = SessionLog::open(dir.join("old.run.jsonl")).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "old.run".into(),
            objective: "o".into(),
            max_steps: 2,
        })
        .unwrap();
        log.append(SessionEvent::Proposal {
            step: 0,
            tool_calls: 1,
            completion: false,
            usage: trit_reasonix::HostUsage::default(),
            context: None,
        })
        .unwrap();
        log.append(SessionEvent::ToolRequested {
            step: 0,
            request: crate::ToolRequest::write("w1", "write_file", "a.txt", "x"),
        })
        .unwrap();
        drop(log);

        assert!(
            session_resumable(&dir, "old.run"),
            "同任务的中断 turn 应可恢复"
        );
        assert!(
            !session_resumable(&dir, "new.run"),
            "任务改名后断点属于别人，必须退回归档重跑"
        );
        assert!(
            !session_resumable(&dir, "missing.run"),
            "没有日志时不可恢复"
        );

        std::fs::write(dir.join("broken.run.jsonl"), "not-json\n").unwrap();
        assert!(
            !session_resumable(&dir, "broken.run"),
            "损坏日志必须退回归档重跑"
        );

        let mut log = SessionLog::open(dir.join("done.run.jsonl")).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "done.run".into(),
            objective: "o".into(),
            max_steps: 1,
        })
        .unwrap();
        log.append(SessionEvent::TurnFinished {
            steps: 1,
            complete: true,
            metrics: None,
        })
        .unwrap();
        drop(log);
        assert!(!session_resumable(&dir, "done.run"), "已收尾 turn 不可恢复");

        let _ = std::fs::remove_dir_all(dir);
    }
}
