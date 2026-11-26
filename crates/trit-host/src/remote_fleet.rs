//! 跨机派发（B7.6 后续）：DAG 与并发仍由 [`trit_orch::execute`] 负责，这里只回答
//! “每个子任务送到哪台机器、失败怎么记账”。
//!
//! 分工：
//!
//! - 依赖语义 / worker 上限 / 失败后不再启动新任务 → `trit_orch::fleet`；
//! - 节点选择、每节点容量、故障隔离 → [`NodeTable`]（纯策略）；
//! - 真实传输 → [`run_remote_subagent_reply`]（ureq 单帧 POST）。
//!
//! 与本地 fleet 的差异：
//!
//! - `assignments` 记录每个任务实际落在哪个节点；
//! - `results` 保留远端自报的精确状态：远端 `Aborted` 不会在汇总里被改写成
//!   `Failed`（[`FleetReport::results`] 仍按 runtime 的 Ok/Err 视图记录）；
//! - 节点级失败（传输/超时/协议违约/远端中止）按 `failure_threshold` 隔离该节点，
//!   任务级失败（远端回了 `Failed`）不牵连节点健康度；远端带
//!   `X-Trit-Lease-Lost` 的 `Aborted` 表示任务已被其他账本持有者接手，记
//!   `NodeOutcome::Superseded`，节点仍健康，任务按重试预算等待新结果；带
//!   `X-Trit-Deadline-Exceeded` 的 `Aborted` 记 `NodeOutcome::OverBudget`，
//!   同样不隔离节点，只换节点重试；
//! - 节点级失败、租约被接管与执行超预算最多换节点重试 `retry_limit` 次（默认 0）。重试复用
//!   同一个幂等键 `X-Trit-Idempotency: task@epoch`，因此要求服务端去重；任务级
//!   失败与 `Deferred` 不重试。每次尝试（节点 + 结局）记进
//!   [`RemotePlanReport::attempts`]。
//! - 健康恢复由本层配置：`health_probe_on_start` 在派发前探测一次，
//!   `health_probe_interval` 在派发期间周期探测；只探已隔离节点，`GET /health`
//!   成功才调用 [`NodeTable::readmit`] 放行。事件写入 [`RemotePlanReport::probes`]，
//!   最多保留 1024 条；周期监控随本次派发结束而退出，不做常驻后台恢复。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use trit_orch::{
    CancelToken, DelegationResult, DelegationStatus, FleetConfig, FleetOutput, FleetReport,
    NodeOutcome, NodeSnapshot, NodeTable, Plan, RemoteNode, SubagentRequest, TaskSpec, execute,
};

use crate::AgentError;
use crate::remote_subagent::{
    probe_remote_health, run_remote_subagent_reply_cancellable_scoped, validate_idempotency_scope,
};

/// 周期探测的轮询步长；停止标志置位后最多等一个步长。
const HEALTH_MONITOR_STEP: Duration = Duration::from_millis(25);

/// 单次演练最多保留的探测事件；周期探测不能把报告变成无界日志。
const MAX_PROBE_EVENTS: usize = 1024;

/// 一次派发尝试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteAttempt {
    /// 本次尝试落到的节点。
    pub node: String,
    /// 本次尝试的结局；协议级错误记为 `Failed`（带错误文本）。
    pub status: DelegationStatus,
}

/// 跨机派发配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlanConfig {
    /// 并发 worker 上限（至少 1）。
    pub workers: usize,
    /// 节点连续失败多少次后隔离（默认 1）。
    pub failure_threshold: u32,
    /// 节点级失败或租约被接管后最多换节点重试几次（默认 0，即不重试）。
    ///
    /// 只有在服务端按 `X-Trit-Idempotency: [scope|]task@epoch` 去重时，重试才是安全的；
    /// 参考实现 `remote-echo` 会去重。任务级 `Failed` 永远不重试；租约被接管的
    /// 节点不记故障，重试会等待当前持有者完成并回放其结果。
    pub retry_limit: u32,
    /// 计划作用域：不同计划用不同值，避免同名任务在共享账本/workspace 中碰撞。
    /// `None` 保持旧的 `task@epoch` 键与 `<root>/<task>` 目录。
    pub scope: Option<String>,
    /// 派发前先对已隔离节点做一次 `GET /health`，成功才放行。
    pub health_probe_on_start: bool,
    /// 派发期间周期探测已隔离节点的间隔；`None` 不启动周期监控。
    /// 周期监控随本次派发结束而退出，不做常驻后台恢复。
    pub health_probe_interval: Option<Duration>,
}

impl RemotePlanConfig {
    pub fn new(workers: usize) -> Self {
        Self {
            workers,
            failure_threshold: 1,
            retry_limit: 0,
            scope: None,
            health_probe_on_start: false,
            health_probe_interval: None,
        }
    }

    pub fn with_failure_threshold(mut self, failure_threshold: u32) -> Self {
        self.failure_threshold = failure_threshold.max(1);
        self
    }

    pub fn with_retry_limit(mut self, retry_limit: u32) -> Self {
        self.retry_limit = retry_limit;
        self
    }

    /// 设置计划作用域；非法值会在派发开始时被拒绝。
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// 派发前对已隔离节点做一次健康探测。
    pub fn with_health_probe_on_start(mut self) -> Self {
        self.health_probe_on_start = true;
        self
    }

    /// 派发期间周期探测已隔离节点；`Duration::ZERO` 关闭周期监控。
    pub fn with_health_probe_interval(mut self, interval: Duration) -> Self {
        self.health_probe_interval = (!interval.is_zero()).then_some(interval);
        self
    }
}

/// 一次健康探测发生在哪个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteProbePhase {
    /// 派发开始前的一次性探测。
    Preflight,
    /// 派发期间的周期探测。
    Periodic,
}

/// 一次节点健康探测事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteProbeEvent {
    pub node: String,
    pub phase: RemoteProbePhase,
    /// `GET /health` 是否成功。
    pub ok: bool,
    /// 本次探测是否真的把节点从隔离状态放行；本来就健康时为 `false`。
    pub readmitted: bool,
    /// 探测失败的原因；成功时省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次跨机执行的报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlanReport {
    /// fleet 视图：依赖、并发、完成/失败/延迟。
    pub report: FleetReport,
    /// 每个任务的精确结局（远端自报状态优先，未派发的任务取 fleet 的归类）。
    pub results: BTreeMap<String, DelegationResult>,
    /// `task -> node id`：任务最终落到的节点（重试时是最后一次尝试的节点）。
    pub assignments: BTreeMap<String, String>,
    /// `task -> 每次尝试`：按发生顺序记录节点与结局。
    pub attempts: BTreeMap<String, Vec<RemoteAttempt>>,
    /// 节点状态快照（按 id 升序）。
    pub nodes: Vec<NodeSnapshot>,
    /// 健康探测事件（preflight 在前，periodic 随后）；最多保留 1024 条。
    pub probes: Vec<RemoteProbeEvent>,
}

impl RemotePlanReport {
    pub fn result(&self, task: &str) -> Option<&DelegationResult> {
        self.results.get(task)
    }

    /// 只有 `Completed` 才能作为下游输入。
    pub fn reusable(&self, task: &str) -> bool {
        self.results
            .get(task)
            .is_some_and(DelegationResult::reusable)
    }

    pub fn node_of(&self, task: &str) -> Option<&str> {
        self.assignments.get(task).map(String::as_str)
    }

    pub fn attempts_of(&self, task: &str) -> &[RemoteAttempt] {
        self.attempts.get(task).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// 按 `plan` 的三元依赖图，把每个子任务送到节点表里的一个远程端点。
///
/// `build_request` 负责把任务定义映射成 subagent 请求（capsule/profile 由调用方
/// 决定，本层不隐式放宽权限）。远端没有合法响应、传输失败或全部节点被隔离时，
/// 对应任务失败；fleet 随即停止启动新任务，未执行任务记为 `Deferred`。
pub fn run_remote_plan<F>(
    plan: &Plan,
    nodes: Vec<RemoteNode>,
    config: RemotePlanConfig,
    cancel: CancelToken,
    build_request: F,
) -> Result<RemotePlanReport, AgentError>
where
    F: Fn(&TaskSpec) -> SubagentRequest + Send + Sync + 'static,
{
    let table = Arc::new(NodeTable::new(nodes)?.with_failure_threshold(config.failure_threshold));
    run_remote_plan_with_table(plan, table, config, cancel, build_request)
}

/// 同 [`run_remote_plan`]，但使用调用方预建的节点表。
///
/// 典型用途：先 [`NodeTable::with_snapshots`] 恢复上一次运行的节点健康状态，再派发。
/// 调用方负责按 `config.failure_threshold` 配置该表；本函数不再重建。配置了
/// `health_probe_on_start` / `health_probe_interval` 时，只探测表中当前已隔离的
/// 节点，成功才放行；探测事件随报告返回。
pub fn run_remote_plan_with_table<F>(
    plan: &Plan,
    table: Arc<NodeTable>,
    config: RemotePlanConfig,
    cancel: CancelToken,
    build_request: F,
) -> Result<RemotePlanReport, AgentError>
where
    F: Fn(&TaskSpec) -> SubagentRequest + Send + Sync + 'static,
{
    if let Some(scope) = config.scope.as_deref() {
        validate_idempotency_scope(scope)?;
    }
    let scope = config.scope.clone();
    let retry_limit = config.retry_limit;
    let replies: Arc<Mutex<BTreeMap<String, DelegationResult>>> = Arc::default();
    let assignments: Arc<Mutex<BTreeMap<String, String>>> = Arc::default();
    let attempts: Arc<Mutex<BTreeMap<String, Vec<RemoteAttempt>>>> = Arc::default();

    let runner = {
        let table = Arc::clone(&table);
        let replies = Arc::clone(&replies);
        let assignments = Arc::clone(&assignments);
        let attempts = Arc::clone(&attempts);
        move |task: &TaskSpec, cancel: &CancelToken| -> Result<FleetOutput, String> {
            let request = build_request(task);
            let task_id = task.task.id.clone();
            let mut excluded: BTreeSet<String> = BTreeSet::new();
            let mut retries_used: u32 = 0;
            let mut last_error: Option<String> = None;

            loop {
                if cancel.is_cancelled() {
                    return Err("remote plan cancelled before dispatch".into());
                }
                let lease = match table.acquire_excluding(cancel, &excluded) {
                    Ok(Some(lease)) => lease,
                    Ok(None) => return Err("remote plan cancelled before dispatch".into()),
                    Err(error) => {
                        return Err(last_error.unwrap_or_else(|| error.to_string()));
                    }
                };
                if cancel.is_cancelled() {
                    lease.finish(NodeOutcome::Abandoned);
                    return Err("remote plan cancelled before dispatch".into());
                }
                let node_id = lease.node().id.clone();
                if let Ok(mut map) = assignments.lock() {
                    map.insert(task_id.clone(), node_id.clone());
                }

                match run_remote_subagent_reply_cancellable_scoped(
                    &lease.node().endpoint,
                    &request,
                    cancel,
                    scope.as_deref(),
                ) {
                    Ok(reply) => {
                        if let Ok(mut map) = replies.lock() {
                            map.insert(task_id.clone(), reply.result.clone());
                        }
                        let status = reply.result.status.clone();
                        record_attempt(&attempts, &task_id, &node_id, &status);
                        if reply.lease_lost {
                            // 同一任务已被其他持有者接管：节点健康，任务等待新持有者。
                            lease.finish(NodeOutcome::Superseded);
                            last_error = Some(format!(
                                "remote node {node_id} lost the ledger lease to another owner"
                            ));
                        } else if reply.deadline_exceeded {
                            // 节点明确回了超时帧：它还能服务，只是本次请求超出墙钟预算。
                            lease.finish(NodeOutcome::OverBudget);
                            last_error = Some(format!(
                                "remote node {node_id} exceeded its execution deadline"
                            ));
                        } else {
                            match &status {
                                DelegationStatus::Completed => {
                                    lease.finish(NodeOutcome::Succeeded);
                                    return Ok(FleetOutput {
                                        content: reply.content,
                                        tokens: reply.result.tokens,
                                    });
                                }
                                // 对端明确回了 Failed：任务失败，但节点仍健康，不重试。
                                DelegationStatus::Failed { message } => {
                                    lease.finish(NodeOutcome::TaskFailed);
                                    return Err(format!("remote node {node_id}: {message}"));
                                }
                                // 用户取消：不把节点判成故障，也不换节点重试。
                                DelegationStatus::Aborted if cancel.is_cancelled() => {
                                    lease.finish(NodeOutcome::Abandoned);
                                    return Err("remote plan cancelled".into());
                                }
                                // 没有合法响应：节点是否还能服务无法确认，可换节点重试。
                                DelegationStatus::Aborted => {
                                    lease.finish(NodeOutcome::NodeFailed);
                                    last_error = Some(format!(
                                        "remote node {node_id} aborted without a response"
                                    ));
                                }
                                // 远端不该回 Deferred；按协议违约处理，不重试。
                                DelegationStatus::Deferred => {
                                    lease.finish(NodeOutcome::NodeFailed);
                                    return Err(format!("remote node {node_id} returned Deferred"));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let status = DelegationStatus::Failed {
                            message: error.to_string(),
                        };
                        record_attempt(&attempts, &task_id, &node_id, &status);
                        if cancel.is_cancelled() {
                            lease.finish(NodeOutcome::Abandoned);
                            return Err("remote plan cancelled".into());
                        }
                        lease.finish(NodeOutcome::NodeFailed);
                        last_error = Some(format!("remote node {node_id}: {error}"));
                    }
                }

                // 能走到这里只有节点级失败（Aborted / 传输错误）、租约被接管或
                // 执行超预算：跳过本节点并重试；后两者在节点表里按健康记账。
                excluded.insert(node_id);
                if retries_used >= retry_limit {
                    return Err(last_error.unwrap_or_else(|| "remote node failed".into()));
                }
                retries_used += 1;
            }
        }
    };

    let probes = Arc::new(Mutex::new(Vec::new()));
    if config.health_probe_on_start {
        probe_isolated_nodes(&table, &probes, RemoteProbePhase::Preflight);
    }
    let monitor = match config.health_probe_interval {
        Some(interval) => Some(spawn_health_monitor(
            Arc::clone(&table),
            interval,
            Arc::clone(&probes),
        )?),
        None => None,
    };
    let result = execute(plan, FleetConfig::new(config.workers), cancel, runner);
    if let Some(monitor) = monitor {
        monitor.stop();
    }
    let report = result?;

    let remote = replies.lock().map(|map| map.clone()).unwrap_or_default();
    let mut results = report.results.clone();
    results.extend(remote);

    Ok(RemotePlanReport {
        results,
        assignments: assignments
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default(),
        attempts: attempts.lock().map(|map| map.clone()).unwrap_or_default(),
        nodes: table.snapshot(),
        probes: probes
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default(),
        report,
    })
}

/// 探测当前所有已隔离节点；成功才放行，失败保持隔离并记录原因。
fn probe_isolated_nodes(
    table: &NodeTable,
    probes: &Mutex<Vec<RemoteProbeEvent>>,
    phase: RemoteProbePhase,
) {
    let mut events = Vec::new();
    for snapshot in table
        .snapshot()
        .into_iter()
        .filter(|snapshot| !snapshot.healthy)
    {
        let Some(node) = table.node(&snapshot.id) else {
            continue;
        };
        match probe_remote_health(&node.endpoint) {
            Ok(()) => {
                let readmitted = table.readmit(&node.id);
                events.push(RemoteProbeEvent {
                    node: node.id,
                    phase,
                    ok: true,
                    readmitted,
                    error: None,
                });
            }
            Err(error) => events.push(RemoteProbeEvent {
                node: node.id,
                phase,
                ok: false,
                readmitted: false,
                error: Some(error),
            }),
        }
    }
    let mut guard = probes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for event in events {
        if guard.len() >= MAX_PROBE_EVENTS {
            guard.remove(0);
        }
        guard.push(event);
    }
}

/// 演练期间的周期健康监控：只处理已隔离节点，停止标志置位后尽快退出。
struct HealthMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HealthMonitor {
    fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HealthMonitor {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn spawn_health_monitor(
    table: Arc<NodeTable>,
    interval: Duration,
    probes: Arc<Mutex<Vec<RemoteProbeEvent>>>,
) -> Result<HealthMonitor, AgentError> {
    let stop = Arc::new(AtomicBool::new(false));
    let handle = std::thread::Builder::new()
        .name("trit-health-monitor".into())
        .spawn({
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(Ordering::SeqCst) {
                    probe_isolated_nodes(&table, &probes, RemoteProbePhase::Periodic);
                    let mut remaining = interval;
                    while !stop.load(Ordering::SeqCst) && !remaining.is_zero() {
                        let wait = remaining.min(HEALTH_MONITOR_STEP);
                        std::thread::sleep(wait);
                        remaining = remaining.saturating_sub(wait);
                    }
                }
            }
        })
        .map_err(|error| AgentError::Io(format!("spawn health monitor: {error}")))?;
    Ok(HealthMonitor {
        stop,
        handle: Some(handle),
    })
}

/// 追加一条尝试记录。
fn record_attempt(
    attempts: &Mutex<BTreeMap<String, Vec<RemoteAttempt>>>,
    task: &str,
    node: &str,
    status: &DelegationStatus,
) {
    if let Ok(mut map) = attempts.lock() {
        map.entry(task.to_string())
            .or_default()
            .push(RemoteAttempt {
                node: node.to_string(),
                status: status.clone(),
            });
    }
}
