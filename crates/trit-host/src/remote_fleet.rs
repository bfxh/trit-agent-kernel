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
//!   成功才调用 [`NodeTable::readmit`] 放行。`health_probe_concurrency` 控制一次
//!   扫描内的并行上限（默认串行），单个节点成功即放行、不等其余探测返回；事件
//!   仍按节点 id 升序写入 [`RemotePlanReport::probes`]，最多保留 1024 条；周期
//!   监控随本次派发结束而退出。
//!   跨运行的常驻恢复由 [`probe_remote_health_file`] + `trit-host remote-health`
//!   承担：它独立读写节点健康文件，不依赖某个演练进程一直活着；CLI 每轮重读
//!   节点表，并在 `<health>.lock` 上持单实例 OS 锁。
//! - 长演练可用 [`spawn_remote_health_flusher`] 把节点表快照周期写回健康文件；
//!   这样进程中途崩溃时，最近的隔离/放行结果仍留在磁盘上，而不是只在计划
//!   正常结束才落盘。写回与其它写者走同一套 `.write.lock` 合并协议。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use trit_orch::{
    CancelToken, DelegationResult, DelegationStatus, FleetConfig, FleetOutput, FleetReport,
    NodeOutcome, NodeSnapshot, NodeTable, Plan, RemoteNode, SubagentRequest, TaskSpec, execute,
};

use crate::AgentError;
use crate::lease::SingleInstanceLock;
use crate::remote_subagent::{
    probe_remote_health, run_remote_subagent_reply_cancellable_scoped, validate_idempotency_scope,
};

/// 周期探测的轮询步长；停止标志置位后最多等一个步长。
const HEALTH_MONITOR_STEP: Duration = Duration::from_millis(25);

/// 单次演练最多保留的探测事件；周期探测不能把报告变成无界日志。
const MAX_PROBE_EVENTS: usize = 1024;

/// 健康文件写锁的最长等待；写者只做一次读-合并-替换，正常远低于该上限。
const HEALTH_WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// 健康文件写锁的轮询步长。
const HEALTH_WRITE_LOCK_POLL: Duration = Duration::from_millis(10);

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
    /// 一次 preflight / 周期扫描内最多并行探测多少个已隔离节点；默认 1（串行）。
    pub health_probe_concurrency: usize,
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
            health_probe_concurrency: 1,
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

    /// 一次健康扫描最多并行探测多少个节点；0 与超上限都夹到合法区间。
    pub fn with_health_probe_concurrency(mut self, concurrency: usize) -> Self {
        self.health_probe_concurrency = concurrency.clamp(1, MAX_HEALTH_PROBE_CONCURRENCY);
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
    /// 独立健康守护进程的一次扫描。
    Monitor,
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

/// 一次独立健康扫描的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteHealthReport {
    /// 本轮实际发出 `GET /health` 的节点数（只探已隔离节点）。
    pub checked: usize,
    /// 本轮成功放行的节点数。
    pub readmitted: usize,
    /// 探测失败、仍保持隔离的节点数。
    pub still_isolated: usize,
    /// 探测事件；最多保留 1024 条。
    pub events: Vec<RemoteProbeEvent>,
    /// 写回健康文件后的完整节点快照（按 id 升序）。
    pub nodes: Vec<NodeSnapshot>,
    /// 健康文件里存在、但不在本次节点表中的节点 ID；保留原快照但不再探测。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
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
        probe_isolated_nodes(
            &table,
            &probes,
            RemoteProbePhase::Preflight,
            config.health_probe_concurrency,
        );
    }
    let monitor = match config.health_probe_interval {
        Some(interval) => Some(spawn_health_monitor(
            Arc::clone(&table),
            interval,
            Arc::clone(&probes),
            config.health_probe_concurrency,
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

/// 单次健康扫描最多并行探测多少个节点；避免一个超大节点表瞬间打满连接/线程。
pub const MAX_HEALTH_PROBE_CONCURRENCY: usize = 64;

/// 在目标集合上执行有界并行探测，最多 `concurrency` 个线程。
///
/// 这是 `remote-health` 与演练内监控共用的调度器：只负责按索引取任务、限制
/// 并发和回收线程，结果由 `probe` 自行保存。调用方应先按节点 id 排序
/// `targets`，并在 `probe` 内自行保证结果顺序确定；`concurrency` 会被夹到
/// `1..=MAX_HEALTH_PROBE_CONCURRENCY`，且不超过目标数。
fn probe_targets_in_parallel<T: Sync>(
    targets: &[T],
    concurrency: usize,
    probe: impl Fn(usize, &T) + Sync,
) {
    let workers = concurrency
        .clamp(1, MAX_HEALTH_PROBE_CONCURRENCY)
        .min(targets.len());
    if workers == 0 {
        return;
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(target) = targets.get(index) else {
                        break;
                    };
                    probe(index, target);
                }
            });
        }
    });
}

/// 对节点健康文件做一次独立扫描：只探测已隔离节点，成功即写回 `healthy`。
///
/// 与演练内的周期监控不同，本函数不依赖 `remote-plan` 进程：调用方可以按
/// `trit-host remote-health` 常驻循环，或用 cron / 任务计划程序单次调用。
/// 健康文件是 `Vec<NodeSnapshot>`；文件不存在时按“全部健康”初始化并写出。
/// 表里不存在的旧节点保留原快照、不再探测，并在 [`RemoteHealthReport::ignored`]
/// 中报告，避免监控器缩减节点表时抹掉其他节点的健康状态。
///
/// 本函数串行探测；需要限制并发时用
/// [`probe_remote_health_file_with_concurrency`]。
pub fn probe_remote_health_file(
    nodes: &[RemoteNode],
    health_path: impl AsRef<Path>,
) -> Result<RemoteHealthReport, AgentError> {
    probe_remote_health_file_with_concurrency(nodes, health_path, 1)
}

/// 同 [`probe_remote_health_file`]，但最多 `concurrency` 个探测并行。
///
/// 只并行 `GET /health` 与结果回填；健康文件仍然只写一次，事件与快照按节点
/// id 升序输出，因此同样的输入得到同样的报告。`concurrency` 必须在
/// `1..=MAX_HEALTH_PROBE_CONCURRENCY`。
pub fn probe_remote_health_file_with_concurrency(
    nodes: &[RemoteNode],
    health_path: impl AsRef<Path>,
    concurrency: usize,
) -> Result<RemoteHealthReport, AgentError> {
    if concurrency == 0 {
        return Err(AgentError::Io(
            "remote health probe concurrency must be positive".into(),
        ));
    }
    if concurrency > MAX_HEALTH_PROBE_CONCURRENCY {
        return Err(AgentError::Io(format!(
            "remote health probe concurrency {concurrency} exceeds {MAX_HEALTH_PROBE_CONCURRENCY}"
        )));
    }
    let path = health_path.as_ref();
    NodeTable::new(nodes.to_vec())
        .map_err(|error| AgentError::Io(format!("remote health nodes: {error}")))?;

    let mut stored = BTreeMap::new();
    for snapshot in load_remote_health_file(path)? {
        if stored.insert(snapshot.id.clone(), snapshot).is_some() {
            return Err(AgentError::Io(format!(
                "health file {} has duplicate node ids",
                path.display()
            )));
        }
    }

    let mut snapshots = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut snapshot = stored.remove(&node.id).unwrap_or_else(|| NodeSnapshot {
            id: node.id.clone(),
            healthy: true,
            in_flight: 0,
            attempts: 0,
            failures: 0,
        });
        // 健康文件跨运行保存，不应携带某一时刻的在途计数。
        snapshot.in_flight = 0;
        snapshots.push(snapshot);
    }

    let mut targets: Vec<(usize, &RemoteNode)> = nodes
        .iter()
        .enumerate()
        .filter(|(index, _)| !snapshots[*index].healthy)
        .collect();
    targets.sort_by(|left, right| left.1.id.cmp(&right.1.id));
    let outcomes = Mutex::new(vec![None; targets.len()]);
    probe_targets_in_parallel(&targets, concurrency, |index, (_, node)| {
        outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())[index] =
            Some(probe_remote_health(&node.endpoint));
    });
    let outcomes = outcomes
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut events = Vec::new();
    let mut checked = 0usize;
    let mut readmitted = 0usize;
    let mut still_isolated = 0usize;
    for ((snapshot_index, node), outcome) in targets.into_iter().zip(outcomes) {
        let Some(outcome) = outcome else {
            return Err(AgentError::Io(format!(
                "health probe for node {} produced no result",
                node.id
            )));
        };
        checked += 1;
        match outcome {
            Ok(()) => {
                let snapshot = &mut snapshots[snapshot_index];
                snapshot.healthy = true;
                readmitted += 1;
                if events.len() < MAX_PROBE_EVENTS {
                    events.push(RemoteProbeEvent {
                        node: node.id.clone(),
                        phase: RemoteProbePhase::Monitor,
                        ok: true,
                        readmitted: true,
                        error: None,
                    });
                }
            }
            Err(error) => {
                still_isolated += 1;
                if events.len() < MAX_PROBE_EVENTS {
                    events.push(RemoteProbeEvent {
                        node: node.id.clone(),
                        phase: RemoteProbePhase::Monitor,
                        ok: false,
                        readmitted: false,
                        error: Some(error),
                    });
                }
            }
        }
    }

    let ignored: Vec<String> = stored.keys().cloned().collect();
    snapshots.extend(stored.into_values());
    snapshots.sort_by(|left, right| left.id.cmp(&right.id));
    let report = RemoteHealthReport {
        checked,
        readmitted,
        still_isolated,
        events,
        nodes: snapshots,
        ignored,
    };
    save_remote_health_file(path, &report.nodes)?;
    Ok(report)
}

/// 周期把节点表快照写回健康文件的后台线程。
///
/// 只负责“最近一次成功写回”，不改变调度：写回失败会记录在
/// [`RemoteHealthFlusher::last_error`]，下一轮成功则清除；不终止演练。停止时
/// 先置停止标志再 join，保证返回后不会再有后台写者与调用方的最终写回并发。
pub struct RemoteHealthFlusher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl RemoteHealthFlusher {
    /// 停止并等待后台线程退出。
    pub fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// 最近一轮写回的错误；成功后会被清除。
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for RemoteHealthFlusher {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// 启动健康文件周期写回：立即写一次，之后每 `interval` 写一次节点表快照。
///
/// 每次写回都先把快照里的 `in_flight` 清零（健康文件跨运行恢复时不应携带
/// 某一时刻的在途计数），再调用 [`save_remote_health_file`]，因此会与
/// `remote-health` / 另一个演练的写者按 `.write.lock` 合并，不会抹掉节点表外
/// 的旧条目，也不会留下半截文件。`interval` 必须大于 0。
pub fn spawn_remote_health_flusher(
    table: Arc<NodeTable>,
    path: impl Into<PathBuf>,
    interval: Duration,
) -> Result<RemoteHealthFlusher, AgentError> {
    if interval.is_zero() {
        return Err(AgentError::Io(
            "remote health flush interval must be positive".into(),
        ));
    }
    let path = path.into();
    if path.as_os_str().is_empty() {
        return Err(AgentError::Io("remote health flush path is empty".into()));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let last_error = Arc::new(Mutex::new(None::<String>));
    let handle = std::thread::Builder::new()
        .name("trit-health-flush".into())
        .spawn({
            let stop = Arc::clone(&stop);
            let last_error = Arc::clone(&last_error);
            move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut snapshots = table.snapshot();
                    for snapshot in &mut snapshots {
                        snapshot.in_flight = 0;
                    }
                    let outcome = save_remote_health_file(&path, &snapshots)
                        .map_err(|error| error.to_string());
                    let mut slot = last_error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    *slot = outcome.err();
                    drop(slot);

                    let mut remaining = interval;
                    while !stop.load(Ordering::SeqCst) && !remaining.is_zero() {
                        let wait = remaining.min(HEALTH_MONITOR_STEP);
                        std::thread::sleep(wait);
                        remaining = remaining.saturating_sub(wait);
                    }
                }
            }
        })
        .map_err(|error| AgentError::Io(format!("spawn health flusher: {error}")))?;
    Ok(RemoteHealthFlusher {
        stop,
        handle: Some(handle),
        last_error,
    })
}

fn load_remote_health_file(path: &Path) -> Result<Vec<NodeSnapshot>, AgentError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            AgentError::Io(format!("parse health file {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(AgentError::Io(format!(
            "read health file {}: {error}",
            path.display()
        ))),
    }
}

/// 写回节点健康文件：在 `<path>.write.lock` 上取短临界区，先读取磁盘上的旧
/// 快照，再合并本次传入的节点；本次未覆盖的旧节点（包括节点表外的监控对象）
/// 按原快照保留，最终按 id 升序写临时文件并替换。
///
/// 传空切片不会清空已有条目；要重置健康文件请删除它。同一节点的并发写仍按
/// “最后写入者覆盖”，写锁只保证没有半截文件或丢失未覆盖的节点。
pub fn save_remote_health_file(path: &Path, nodes: &[NodeSnapshot]) -> Result<(), AgentError> {
    let _lock = acquire_health_write_lock(path)?;
    let mut merged = BTreeMap::new();
    for snapshot in load_remote_health_file(path)? {
        if merged.insert(snapshot.id.clone(), snapshot).is_some() {
            return Err(AgentError::Io(format!(
                "health file {} has duplicate node ids",
                path.display()
            )));
        }
    }
    for snapshot in nodes {
        merged.insert(snapshot.id.clone(), snapshot.clone());
    }
    let snapshots: Vec<NodeSnapshot> = merged.into_values().collect();
    write_health_file(path, &snapshots)
}

/// 健康文件写锁路径：与 `remote-health` 的实例锁分开，写者只短时持有。
fn health_write_lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".write.lock");
    PathBuf::from(name)
}

fn acquire_health_write_lock(path: &Path) -> Result<SingleInstanceLock, AgentError> {
    let lock_path = health_write_lock_path(path);
    let deadline = Instant::now() + HEALTH_WRITE_LOCK_TIMEOUT;
    loop {
        match SingleInstanceLock::acquire(&lock_path) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(AgentError::Io(format!(
                        "timed out waiting for health write lock {}",
                        lock_path.display()
                    )));
                }
                std::thread::sleep(HEALTH_WRITE_LOCK_POLL);
            }
            Err(error) => {
                return Err(AgentError::Io(format!(
                    "lock health write {}: {error}",
                    lock_path.display()
                )));
            }
        }
    }
}

/// 直接把给定快照写成健康文件；调用方负责持有写锁与合并旧条目。
fn write_health_file(path: &Path, nodes: &[NodeSnapshot]) -> Result<(), AgentError> {
    let text = serde_json::to_string_pretty(nodes)
        .map_err(|error| AgentError::Io(format!("encode health file: {error}")))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentError::Io(format!("create health dir: {error}")))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("health.json");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)
        .map_err(|error| AgentError::Io(format!("write health file: {error}")))?;
    // 直接 rename 覆盖目标；不要再先 remove，否则读者可能看到文件短暂缺失。
    std::fs::rename(&temporary, path)
        .map_err(|error| AgentError::Io(format!("rename health file: {error}")))
}

/// 探测当前所有已隔离节点；成功才放行，失败保持隔离并记录原因。
///
/// 一次扫描内最多 `concurrency` 个探测并行；每个节点探测成功后立即 readmit，
/// 不必等最慢的探测返回。事件先按本轮节点 id 升序整理，再追加到 `probes`，
/// 保证报告顺序确定。节点数少于并发上限时只起相应数量的线程。
fn probe_isolated_nodes(
    table: &NodeTable,
    probes: &Mutex<Vec<RemoteProbeEvent>>,
    phase: RemoteProbePhase,
    concurrency: usize,
) {
    let mut targets: Vec<RemoteNode> = table
        .snapshot()
        .into_iter()
        .filter(|snapshot| !snapshot.healthy)
        .filter_map(|snapshot| table.node(&snapshot.id))
        .collect();
    targets.sort_by(|left, right| left.id.cmp(&right.id));
    if targets.is_empty() {
        return;
    }

    let events = Mutex::new(Vec::with_capacity(targets.len()));
    probe_targets_in_parallel(&targets, concurrency, |_, node| {
        let event = match probe_remote_health(&node.endpoint) {
            Ok(()) => RemoteProbeEvent {
                node: node.id.clone(),
                phase,
                ok: true,
                readmitted: table.readmit(&node.id),
                error: None,
            },
            Err(error) => RemoteProbeEvent {
                node: node.id.clone(),
                phase,
                ok: false,
                readmitted: false,
                error: Some(error),
            },
        };
        events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
    });
    let mut events = events
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events.sort_by(|left, right| left.node.cmp(&right.node));

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
    concurrency: usize,
) -> Result<HealthMonitor, AgentError> {
    let stop = Arc::new(AtomicBool::new(false));
    let handle = std::thread::Builder::new()
        .name("trit-health-monitor".into())
        .spawn({
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(Ordering::SeqCst) {
                    probe_isolated_nodes(&table, &probes, RemoteProbePhase::Periodic, concurrency);
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
