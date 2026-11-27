//! 进程内后台任务登记：让长期服务报告“有多少 worker 在跑”。
//!
//! 只做观测，不参与调度：`register` 返回守卫，线程结束时自动注销；
//! `snapshot` / `running` / `started_total` 供 diagnostics 与健康检查读取。

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 一个仍在运行的后台任务快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundTask {
    pub id: u64,
    pub name: String,
    pub started_unix_ms: u64,
    pub elapsed_ms: u64,
}

struct Registry {
    next_id: u64,
    started_total: u64,
    tasks: BTreeMap<u64, (String, Instant, u64)>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            next_id: 0,
            started_total: 0,
            tasks: BTreeMap::new(),
        })
    })
}

/// 任务守卫；`Drop` 时自动注销。
pub struct BackgroundTaskGuard {
    id: u64,
}

impl Drop for BackgroundTaskGuard {
    fn drop(&mut self) {
        if let Ok(mut registry) = registry().lock() {
            registry.tasks.remove(&self.id);
        }
    }
}

/// 登记一个正在运行的后台任务。
pub fn register(name: impl Into<String>) -> BackgroundTaskGuard {
    let mut registry = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.next_id += 1;
    let id = registry.next_id;
    registry.started_total += 1;
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    registry
        .tasks
        .insert(id, (name.into(), Instant::now(), started_unix_ms));
    BackgroundTaskGuard { id }
}

/// 当前仍在运行的后台任务（按 id 升序）。
pub fn snapshot() -> Vec<BackgroundTask> {
    let registry = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .tasks
        .iter()
        .map(|(id, (name, started, started_unix_ms))| BackgroundTask {
            id: *id,
            name: name.clone(),
            started_unix_ms: *started_unix_ms,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
        .collect()
}

/// 当前运行中的后台任务数。
pub fn running() -> usize {
    registry()
        .lock()
        .map(|registry| registry.tasks.len())
        .unwrap_or(0)
}

/// 本进程累计启动过的后台任务数。
pub fn started_total() -> u64 {
    registry()
        .lock()
        .map(|registry| registry.started_total)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_registers_and_unregisters() {
        let before = running();
        let guard = register("test-task");
        assert!(snapshot().iter().any(|task| task.name == "test-task"));
        assert_eq!(running(), before + 1);
        drop(guard);
        assert!(!snapshot().iter().any(|task| task.name == "test-task"));
    }
}
