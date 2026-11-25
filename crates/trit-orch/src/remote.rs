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
//! - **取消**：等待中的调用方按 [`CancelToken`] 退出，返回 `Ok(None)`；
//! - **不自动重试**：远程可能已经执行过，重试需要幂等键；本层不做隐式重试。

use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

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
}

/// 节点状态快照（用于报告与测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSnapshot {
    pub id: String,
    pub healthy: bool,
    pub in_flight: usize,
    pub attempts: u64,
    pub failures: u64,
}

struct Slot {
    node: RemoteNode,
    in_flight: usize,
    attempts: u64,
    failures: u64,
    consecutive_failures: u32,
    healthy: bool,
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
            })
            .collect();
        snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        snapshots
    }

    /// 仍然健康的节点数。
    pub fn healthy_nodes(&self) -> usize {
        self.lock().iter().filter(|slot| slot.healthy).count()
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
        }
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
