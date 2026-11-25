//! 宿主执行器接线（B7.6）：把 fleet 的子任务接到真实的 executor 执行。
//!
//! 这一层**不复制调度**：并发上限、依赖语义、失败传播仍由 `trit_orch::fleet` +
//! `trit-schedule` runtime 负责。这里只回答“子任务怎么真跑”：
//!
//! - [`FleetJob`]：宿主动作的唯一入口（模型调用、工具执行、子 session）；
//! - [`HeadlessFleetRunner`]：每个子任务一个独立 JSONL session，用 [`run_headless`]
//!   在给定 workspace 里跑一轮，结果序列化成稳定引用内容；
//! - 写权限仍由 `WorkspaceConfig`（`allow_write`）与可选 `WriteGuard` 约束，
//!   与单任务 headless 路径完全一致，不存在“fleet 偷偷放宽权限”的旁路。
//!
//! 并发语义：`run_fleet` 把 job 放进 `Mutex` 后交给 runtime 的 worker。runtime 的
//! worker 数决定“谁先开始”，实际执行是否安全并发由宿主决定；需要真并发时，宿主
//! 在 `FleetJob::run` 内部自行分片。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use trit_orch::{CancelToken, FleetConfig, FleetOutput, FleetReport, Plan, TaskSpec, execute};
use trit_reasonix::TurnSpec;

use crate::tools::WorkspaceConfig;
use crate::{AgentError, ContextPolicy, HeadlessConfig, Model, run_headless};

/// 宿主实现的子任务执行动作。
pub trait FleetJob: Send {
    /// 执行一个子任务；`Ok` 内容会成为稳定引用，`Err` 说明会进入 fleet 失败传播。
    fn run(&mut self, task: &TaskSpec, cancel: &CancelToken) -> Result<FleetOutput, String>;
}

/// 用宿主 job 执行 fleet。
///
/// job 在 `Mutex` 后串行化，避免宿主动作被多个 worker 同时调用而互相踩踏；
/// 真正的并行由宿主在 `FleetJob::run` 内部决定。
pub fn run_fleet<J>(
    plan: &Plan,
    config: FleetConfig,
    cancel: CancelToken,
    job: J,
) -> Result<FleetReport, AgentError>
where
    J: FleetJob + 'static,
{
    let job = Arc::new(Mutex::new(job));
    let runner = move |task: &TaskSpec, cancel: &CancelToken| match job.lock() {
        Ok(mut job) => job.run(task, cancel),
        Err(_) => Err("fleet job lock poisoned".to_string()),
    };
    execute(plan, config, cancel, runner).map_err(AgentError::from)
}

/// 每个子任务一个独立 executor session 的 headless runner。
///
/// session 文件是 `<dir>/<task-id>.jsonl`；workspace 与写权限沿用调用方给的
/// `WorkspaceConfig`，不额外放大。
pub struct HeadlessFleetRunner<F> {
    workspace: WorkspaceConfig,
    dir: PathBuf,
    max_steps: usize,
    model: F,
}

impl<F> HeadlessFleetRunner<F> {
    /// `model` 是每个任务现取一个 [`Model`]；不同子任务可以用不同模型。
    pub fn new(workspace: WorkspaceConfig, dir: impl Into<PathBuf>, model: F) -> Self {
        Self {
            workspace,
            dir: dir.into(),
            max_steps: 20,
            model,
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }
}

impl<M, F> FleetJob for HeadlessFleetRunner<F>
where
    M: Model,
    F: FnMut(&TaskSpec) -> M + Send,
{
    fn run(&mut self, task: &TaskSpec, _cancel: &CancelToken) -> Result<FleetOutput, String> {
        std::fs::create_dir_all(&self.dir).map_err(|error| error.to_string())?;
        let path = self.dir.join(format!("{}.jsonl", task.task.id));
        let config = HeadlessConfig {
            workspace: self.workspace.clone(),
            max_steps: self.max_steps,
            session: Some(path),
            context: ContextPolicy::default(),
            write_guard: None,
        };
        let outcome = run_headless(turn_spec_from_task(task), (self.model)(task), config)
            .map_err(|error| error.to_string())?;
        let tokens = outcome.usage.total().map_err(|error| error.to_string())?;
        let content = serde_json::to_vec(&outcome).map_err(|error| error.to_string())?;
        Ok(FleetOutput { content, tokens })
    }
}

/// 把任务定义映射成一轮执行：目标、约束、停止条件来自 `guidance`，
/// 断言下限来自 `produces`，预算来自 `max_tokens`。
pub fn turn_spec_from_task(task: &TaskSpec) -> TurnSpec {
    TurnSpec {
        task: task.task.id.clone(),
        objective: task.guidance.objective.clone(),
        constraints: task.guidance.constraints.clone(),
        stop_when: task.guidance.stop_when.clone(),
        min_assertions: task.produces.min_assertions,
        max_tokens: task.max_tokens,
    }
}

/// 该任务在 `dir` 下的 session 文件路径。
pub fn session_path(dir: impl AsRef<Path>, task: &str) -> PathBuf {
    dir.as_ref().join(format!("{task}.jsonl"))
}
