//! 跨机调度的**节点策略**：静态节点表、准入、最轻负载选择与故障隔离。
//!
//! 这一层是纯策略，**不做 I/O**：真正的请求由宿主（`trit_host::remote_fleet`）
//! 用 [`RemoteEndpoint`] + subagent 帧发出。这样调度规则可以在没有网络的
//! 情况下被确定性测试，也不会把 HTTP 依赖带进 `trit-orch`。
//!
//! 语义：
//!
//! - **发现（v1）**：节点由调用方静态注入（配置/清单），不做 mDNS/注册中心；
//! - **准入**：每个节点有独立容量 `max_in_flight`；选择“负载比最低”的节点
//!   （`in_flight / max_in_flight`，平手按 id 升序），满载时等待而不是超发；
//! - **故障隔离**：连续 `failure_threshold` 次节点级失败后该节点退出本轮调度，
//!   不再接收新任务；任务级失败（对端明确回了 `Failed`）不牵连节点；
//!   隔离不会自动解除，只有显式 [`NodeTable::readmit`]（通常由健康探测成功驱动）
//!   才重新放行；
//! - **取消**：等待中的调用方按 [`CancelToken`] 退出，返回 `Ok(None)`；
//! - **不自动重试**：远程可能已经执行过，重试需要幂等键；本层不做隐式重试。

use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CancelToken, Error, RemoteEndpoint};

/// 等待容量的轮询步长：保证取消能在等待中及时生效。
const WAIT_STEP: Duration = Duration::from_millis(25);

/// 一个可调度的远程节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNode {
    /// 节点 ID；同一张表内唯一，不能为空。
    pub id: String,
    /// 同时在途请求上限；至少 1。
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    /// 节点端点（校验规则见 [`RemoteEndpoint::validate`]）。
    pub endpoint: RemoteEndpoint,
}

fn default_max_in_flight() -> usize {
    1
}

impl RemoteNode {
    pub fn new(id: impl Into<String>, endpoint: RemoteEndpoint) -> Self {
        Self {
            id: id.into(),
            max_in_flight: 1,
            endpoint,
        }
    }

    pub fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight;
        self
    }
}

/// 节点失败的分类：只有真正“节点不可用”才影响健康度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeOutcome {
    /// 请求成功完成。
    Succeeded,
    /// 对端给出了合法响应，但任务本身失败；节点仍是健康的。
    TaskFailed,
    /// 传输失败、超时、协议违约或中止：无法确认节点还能继续服务。
    NodeFailed,
    /// 调用方取消：归还容量，但既不算尝试也不影响节点健康。
    Abandoned,
    /// 账本租约被其他持有者接管：本节点中止在途执行；节点仍健康，任务应等待
    /// 当前持有者的结果或换节点重试。
    Superseded,
    /// 服务端执行时限到点：本节点明确回了 `Aborted`，节点仍健康；本次尝试计入
    /// 失败数但不累计连续失败，可按重试预算换节点。
    OverBudget,
}

/// 节点状态快照（用于报告、跨运行健康文件与测试断言）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub id: String,
    pub healthy: bool,
    pub in_flight: usize,
    pub attempts: u64,
    pub failures: u64,
    /// 最近一次节点状态变更的 UNIX 毫秒时间戳；旧健康文件缺省为 0。
    ///
    /// 写回健康文件时用它比较新鲜度：磁盘上时间戳更大的条目优先保留，
    /// 避免慢写回用旧快照覆盖别处刚放行/刚隔离的状态。
    #[serde(default)]
    pub updated_at_ms: u64,
}

struct Slot {
    node: RemoteNode,
    in_flight: usize,
    attempts: u64,
    failures: u64,
    consecutive_failures: u32,
    healthy: bool,
    updated_at_ms: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 状态变更时间戳：正常取当前时间；若时钟回拨，至少比上一次大 1ms。
fn next_state_timestamp(previous: u64) -> u64 {
    now_millis().max(previous.saturating_add(1))
}

fn touch_state(slot: &mut Slot) {
    slot.updated_at_ms = next_state_timestamp(slot.updated_at_ms);
}

/// 静态节点表：准入 + 故障隔离。
pub struct NodeTable {
    inner: Mutex<Vec<Slot>>,
    cond: Condvar,
    failure_threshold: u32,
}

impl NodeTable {
    /// 建表并校验：节点非空、id 唯一非空白、容量 ≥ 1、端点通过准入校验。
    pub fn new(nodes: Vec<RemoteNode>) -> Result<Self, Error> {
        if nodes.is_empty() {
            return Err(Error::RemoteNodes {
                message: "remote node table is empty".into(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for node in &nodes {
            if node.id.trim().is_empty() {
                return Err(Error::RemoteNodes {
                    message: "remote node id is blank".into(),
                });
            }
            if !seen.insert(node.id.clone()) {
                return Err(Error::RemoteNodes {
                    message: format!("duplicate remote node id: {}", node.id),
                });
            }
            if node.max_in_flight == 0 {
                return Err(Error::RemoteNodes {
                    message: format!("remote node {} has zero capacity", node.id),
                });
            }
            node.endpoint
                .validate()
                .map_err(|error| Error::RemoteNodes {
                    message: format!("remote node {}: {error}", node.id),
                })?;
        }
        Ok(Self {
            inner: Mutex::new(
                nodes
                    .into_iter()
                    .map(|node| Slot {
                        node,
                        in_flight: 0,
                        attempts: 0,
                        failures: 0,
                        consecutive_failures: 0,
                        healthy: true,
                        updated_at_ms: now_millis(),
                    })
                    .collect(),
            ),
            cond: Condvar::new(),
            failure_threshold: 1,
        })
    }

    /// 连续失败多少次后隔离该节点（默认 1，即首次节点级失败就退出调度）。
    pub fn with_failure_threshold(mut self, failure_threshold: u32) -> Self {
        self.failure_threshold = failure_threshold.max(1);
        self
    }

    /// 用上一次运行的快照恢复健康度与计数。
    ///
    /// 只恢复同 id 的节点，未知 id 忽略；`in_flight` 不恢复（新表从 0 开始）。
    /// 隔离节点的连续失败数按当前阈值补齐，因此应在
    /// [`NodeTable::with_failure_threshold`] 之后调用。`updated_at_ms` 一并恢复，
    /// 让后续健康文件写回能判断快照是否已经落后于磁盘上的其他写者。
    pub fn with_snapshots(self, snapshots: &[NodeSnapshot]) -> Self {
        let threshold = self.failure_threshold;
        let mut slots = self.lock();
        for snapshot in snapshots {
            if let Some(slot) = slots.iter_mut().find(|slot| slot.node.id == snapshot.id) {
                slot.attempts = snapshot.attempts;
                slot.failures = snapshot.failures;
                slot.healthy = snapshot.healthy;
                slot.consecutive_failures = if snapshot.healthy { 0 } else { threshold };
                slot.updated_at_ms = snapshot.updated_at_ms;
            }
        }
        drop(slots);
        self
    }

    /// 申请一个节点。
    ///
    /// - `Ok(Some(lease))`：已占用一个槽位，用完必须 [`NodeLease::finish`]
    ///   （或直接 drop，按节点级失败处理）；
    /// - `Ok(None)`：取消令牌已置位，未占用任何节点；
    /// - `Err(remote_nodes)`：所有节点都被隔离/排除，本轮不再有可用容量。
    ///
    /// 满载时会阻塞等待，等待期间每 [`WAIT_STEP`] 复查取消令牌。
    pub fn acquire(&self, cancel: &CancelToken) -> Result<Option<NodeLease<'_>>, Error> {
        self.acquire_excluding(cancel, &BTreeSet::new())
    }

    /// 同 [`NodeTable::acquire`]，但跳过 `excluded` 里的节点。
    ///
    /// 重试语义：一次尝试在节点 A 上出现节点级失败后，用 `excluded = {A}`
    /// 再申请，保证重试落到**另一台**机器，而不是立刻回到刚失败的节点。
    pub fn acquire_excluding(
        &self,
        cancel: &CancelToken,
        excluded: &BTreeSet<String>,
    ) -> Result<Option<NodeLease<'_>>, Error> {
        let mut slots = self.lock();
        loop {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            if let Some(index) = pick(&slots, excluded) {
                slots[index].in_flight += 1;
                let node = slots[index].node.clone();
                return Ok(Some(NodeLease {
                    table: self,
                    index,
                    node,
                    released: false,
                }));
            }
            let healthy = slots.iter().any(|slot| slot.healthy);
            let usable = slots
                .iter()
                .any(|slot| slot.healthy && !excluded.contains(&slot.node.id));
            if !usable {
                return Err(Error::RemoteNodes {
                    message: if healthy {
                        "no remote nodes remain outside the excluded set".into()
                    } else {
                        "no healthy remote nodes remain".into()
                    },
                });
            }
            // 还有可用节点，只是暂时满载：等释放，同时保持取消可响应。
            let (guard, _) = self
                .cond
                .wait_timeout(slots, WAIT_STEP)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slots = guard;
        }
    }

    /// 节点快照，按 id 升序。
    pub fn snapshot(&self) -> Vec<NodeSnapshot> {
        let slots = self.lock();
        let mut snapshots: Vec<NodeSnapshot> = slots
            .iter()
            .map(|slot| NodeSnapshot {
                id: slot.node.id.clone(),
                healthy: slot.healthy,
                in_flight: slot.in_flight,
                attempts: slot.attempts,
                failures: slot.failures,
                updated_at_ms: slot.updated_at_ms,
            })
            .collect();
        snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        snapshots
    }

    /// 仍然健康的节点数。
    pub fn healthy_nodes(&self) -> usize {
        self.lock().iter().filter(|slot| slot.healthy).count()
    }

    /// 按 id 读取节点定义（含端点），供宿主做健康探测等只读检查。
    ///
    /// 返回克隆而不是引用，避免把内部锁暴露给 I/O 调用方。
    pub fn node(&self, id: &str) -> Option<RemoteNode> {
        self.lock()
            .iter()
            .find(|slot| slot.node.id == id)
            .map(|slot| slot.node.clone())
    }

    /// 显式健康探测成功后重新放行一个已隔离节点。
    ///
    /// 保留累计 `attempts/failures` 作为历史，只清零连续失败计数并恢复 `healthy`。
    /// 未知节点或本来就健康的节点返回 `false`，不做隐式放行。
    pub fn readmit(&self, id: &str) -> bool {
        let mut slots = self.lock();
        let changed = match slots.iter_mut().find(|slot| slot.node.id == id) {
            Some(slot) if !slot.healthy => {
                slot.healthy = true;
                slot.consecutive_failures = 0;
                touch_state(slot);
                true
            }
            _ => false,
        };
        drop(slots);
        if changed {
            self.cond.notify_all();
        }
        changed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Slot>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn release(&self, index: usize, outcome: NodeOutcome) {
        let mut slots = self.lock();
        let slot = &mut slots[index];
        slot.in_flight = slot.in_flight.saturating_sub(1);
        if outcome == NodeOutcome::Abandoned {
            self.cond.notify_all();
            return;
        }
        slot.attempts = slot.attempts.saturating_add(1);
        match outcome {
            NodeOutcome::Succeeded => slot.consecutive_failures = 0,
            NodeOutcome::TaskFailed => slot.failures = slot.failures.saturating_add(1),
            NodeOutcome::NodeFailed => {
                slot.failures = slot.failures.saturating_add(1);
                slot.consecutive_failures = slot.consecutive_failures.saturating_add(1);
                if slot.consecutive_failures >= self.failure_threshold {
                    slot.healthy = false;
                }
            }
            NodeOutcome::Superseded => slot.consecutive_failures = 0,
            NodeOutcome::OverBudget => {
                slot.failures = slot.failures.saturating_add(1);
                slot.consecutive_failures = 0;
            }
            NodeOutcome::Abandoned => {}
        }
        touch_state(slot);
        self.cond.notify_all();
    }
}

impl std::fmt::Debug for NodeTable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不在这里取锁：Debug 可能发生在持锁/panic 路径上。
        formatter
            .debug_struct("NodeTable")
            .field("failure_threshold", &self.failure_threshold)
            .finish_non_exhaustive()
    }
}

fn pick(slots: &[Slot], excluded: &BTreeSet<String>) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, slot) in slots.iter().enumerate() {
        if !slot.healthy
            || slot.in_flight >= slot.node.max_in_flight
            || excluded.contains(&slot.node.id)
        {
            continue;
        }
        best = Some(match best {
            None => index,
            Some(current) => {
                let candidate = slot;
                let incumbent = &slots[current];
                // 负载比比较用交叉相乘，避免浮点误差。
                let candidate_load =
                    candidate.in_flight as u128 * incumbent.node.max_in_flight as u128;
                let incumbent_load =
                    incumbent.in_flight as u128 * candidate.node.max_in_flight as u128;
                match candidate_load.cmp(&incumbent_load) {
                    std::cmp::Ordering::Less => index,
                    std::cmp::Ordering::Greater => current,
                    std::cmp::Ordering::Equal => {
                        if candidate.node.id < incumbent.node.id {
                            index
                        } else {
                            current
                        }
                    }
                }
            }
        });
    }
    best
}

/// 一次准入的占用凭据。
///
/// 必须显式 [`NodeLease::finish`] 归还原容量；直接 `drop` 视为节点级失败，
/// 防止 worker panic 后节点容量被永久泄漏。
pub struct NodeLease<'a> {
    table: &'a NodeTable,
    index: usize,
    node: RemoteNode,
    released: bool,
}

impl NodeLease<'_> {
    /// 被选中的节点（含端点）。
    pub fn node(&self) -> &RemoteNode {
        &self.node
    }

    /// 归还容量并记录本次结果。
    pub fn finish(mut self, outcome: NodeOutcome) {
        self.released = true;
        self.table.release(self.index, outcome);
    }
}

impl std::fmt::Debug for NodeLease<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NodeLease")
            .field("node", &self.node)
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for NodeLease<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.table.release(self.index, NodeOutcome::NodeFailed);
        }
    }
}
