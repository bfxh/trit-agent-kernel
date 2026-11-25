//! Fleet 执行层（B7.6）：把 `Plan` 的三元依赖图交给 `trit-schedule` 的 runtime 执行。
//!
//! 分工明确，不造第二套调度：
//! - 依赖语义（True 必需 / False 可选 / Unknown 延迟）、worker 并发上限、失败后不再
//!   启动新任务 —— 全部复用 `trit_schedule::runtime`；
//! - 本节只负责把每个子任务的产出映射成稳定引用 [`DelegationResult`]
//!   （`task@epoch#digest`），并区分“没轮到”与“被取消”。
//!
//! 未执行任务的归类：
//! - 显式取消（`CancelToken` 已置位）→ [`DelegationStatus::Aborted`]；
//! - 上游失败或 Unknown 依赖 → [`DelegationStatus::Deferred`]。
//!
//! 两类都**不可复用**（`reusable() == false`），因此不会被当成完成证据。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use trit_schedule::runtime::{
    CancelToken, RuntimeConfig, RuntimeMetrics, RuntimeTask, TaskFailure, run_with_config,
};

use crate::{DelegationResult, DelegationStatus, Error, Plan, Resources, TaskSpec, digest_hex};

/// 子任务执行器的产出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetOutput {
    /// 结果内容；digest 由它计算。
    pub content: Vec<u8>,
    /// 实际消耗的 token。
    pub tokens: u64,
}

/// Fleet 执行配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetConfig {
    /// 并发 worker 上限（至少 1）。
    pub workers: usize,
    /// 可选资源容量；`None` 表示不限资源，只限 worker。
    pub capacity: Option<Resources>,
}

impl FleetConfig {
    pub fn new(workers: usize) -> Self {
        Self {
            workers,
            capacity: None,
        }
    }

    pub fn with_capacity(mut self, capacity: Resources) -> Self {
        self.capacity = Some(capacity);
        self
    }
}

/// 一次 fleet 执行的结果视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetReport {
    /// 每个任务的稳定结果引用，按任务 ID 排序。
    pub results: BTreeMap<String, DelegationResult>,
    /// 成功完成的任务（按 ID 排序）。
    pub completed: Vec<String>,
    /// 第一个失败任务（与 runtime 一致，只记第一个）。
    pub failed: Option<TaskFailure>,
    /// 因上游失败或 Unknown 依赖而未执行的任务。
    pub deferred: Vec<String>,
    /// 被取消或未启动的任务。
    pub cancelled: Vec<String>,
    /// runtime 统计快照（并发/成本基线）。
    pub metrics: RuntimeMetrics,
}

impl FleetReport {
    /// 取某个任务的结果引用。
    pub fn result(&self, task: &str) -> Option<&DelegationResult> {
        self.results.get(task)
    }

    /// 该任务的结果是否可作为下游输入（只有 Completed 可以）。
    pub fn reusable(&self, task: &str) -> bool {
        self.results
            .get(task)
            .is_some_and(DelegationResult::reusable)
    }

    /// 稳定引用视图：`task -> task@epoch#digest`。
    pub fn references(&self) -> BTreeMap<&str, String> {
        self.results
            .iter()
            .map(|(task, result)| (task.as_str(), result.reference()))
            .collect()
    }
}

/// 按 `plan` 的三元依赖图执行子任务。
///
/// `runner` 是宿主动作（模型调用、工具调用等），按任务定义执行并返回内容；
/// 返回 `Err(message)` 表示该任务失败。失败后 runtime 不再启动新任务，fleet 把
/// 未执行的任务标记为 Aborted / Deferred，二者都不可复用。
pub fn execute<F>(
    plan: &Plan,
    config: FleetConfig,
    cancel: CancelToken,
    runner: F,
) -> Result<FleetReport, Error>
where
    F: Fn(&TaskSpec, &CancelToken) -> Result<FleetOutput, String> + Send + Sync + 'static,
{
    if config.workers == 0 {
        return Err(Error::NoWorkers);
    }
    let runner = Arc::new(runner);
    let results: Arc<Mutex<BTreeMap<String, DelegationResult>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    let mut tasks = Vec::with_capacity(plan.tasks().len());
    for spec in plan.tasks() {
        let id = spec.task.id.clone();
        let epoch = spec.epoch;
        let priority = spec.task.priority;
        let deps = spec.task.deps.clone();
        let requirements = spec.requirements;
        let job_spec = spec.clone();
        let job_runner = Arc::clone(&runner);
        let job_results = Arc::clone(&results);
        let task = RuntimeTask::new(id, deps, move |cancel| {
            let record = |status: DelegationStatus, digest: String, tokens: u64| {
                if let Ok(mut map) = job_results.lock() {
                    map.insert(
                        job_spec.task.id.clone(),
                        DelegationResult {
                            task: job_spec.task.id.clone(),
                            epoch,
                            digest,
                            status,
                            tokens,
                        },
                    );
                }
            };
            match job_runner(&job_spec, cancel) {
                Ok(output) => {
                    record(
                        DelegationStatus::Completed,
                        digest_hex(&output.content),
                        output.tokens,
                    );
                    Ok(())
                }
                Err(message) => {
                    record(
                        DelegationStatus::Failed {
                            message: message.clone(),
                        },
                        digest_hex(message.as_bytes()),
                        0,
                    );
                    Err(message)
                }
            }
        })
        .with_priority(priority)
        .with_requirements(requirements);
        tasks.push(task);
    }

    let mut runtime_config = RuntimeConfig::new(config.workers);
    if let Some(capacity) = config.capacity {
        runtime_config = runtime_config.with_capacity(capacity);
    }
    let report = run_with_config(tasks, runtime_config, cancel.clone()).map_err(|error| {
        Error::FleetRuntime {
            message: error.to_string(),
        }
    })?;

    // runtime 已结束，所有 job 闭包都已丢弃；Arc 应当可以唯一取出。
    let mut results = match Arc::try_unwrap(results) {
        Ok(mutex) => mutex.into_inner().unwrap_or_default(),
        Err(shared) => shared.lock().map(|map| map.clone()).unwrap_or_default(),
    };

    let epoch_of = |task: &str| plan.task(task).map(|spec| spec.epoch).unwrap_or(0);
    let not_started = if cancel.is_cancelled() {
        DelegationStatus::Aborted
    } else {
        DelegationStatus::Deferred
    };
    for id in &report.cancelled {
        results
            .entry(id.clone())
            .or_insert_with(|| DelegationResult {
                task: id.clone(),
                epoch: epoch_of(id),
                digest: digest_hex(b""),
                status: not_started.clone(),
                tokens: 0,
            });
    }
    for deferred in &report.deferred {
        results
            .entry(deferred.id.clone())
            .or_insert_with(|| DelegationResult {
                task: deferred.id.clone(),
                epoch: epoch_of(&deferred.id),
                digest: digest_hex(b""),
                status: DelegationStatus::Deferred,
                tokens: 0,
            });
    }
    if let Some(failure) = &report.failed {
        results
            .entry(failure.task.clone())
            .or_insert_with(|| DelegationResult {
                task: failure.task.clone(),
                epoch: epoch_of(&failure.task),
                digest: digest_hex(failure.message.as_bytes()),
                status: DelegationStatus::Failed {
                    message: failure.message.clone(),
                },
                tokens: 0,
            });
    }

    Ok(FleetReport {
        results,
        completed: report.completed,
        failed: report.failed,
        deferred: report.deferred.into_iter().map(|task| task.id).collect(),
        cancelled: report.cancelled,
        metrics: report.metrics,
    })
}
