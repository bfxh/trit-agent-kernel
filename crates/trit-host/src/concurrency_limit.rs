//! 进程级并发上限：provider 与远端主请求共享一个信号量。
//!
//! `TRIT_MAX_CONCURRENCY`（0=不限）在进程启动时配置；`acquire` 返回守卫，
//! Drop 时释放。健康探测、取消等短请求不占用该配额，避免互相阻塞。

use std::sync::{Condvar, Mutex, OnceLock};

struct Limit {
    max: usize,
    active: usize,
}

fn state() -> &'static (Mutex<Limit>, Condvar) {
    static STATE: OnceLock<(Mutex<Limit>, Condvar)> = OnceLock::new();
    STATE.get_or_init(|| (Mutex::new(Limit { max: 0, active: 0 }), Condvar::new()))
}

/// 设置全局上限；0 表示不限。
pub fn configure(max: usize) {
    let (lock, cond) = state();
    let mut limit = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    limit.max = max;
    cond.notify_all();
}

/// 从 `TRIT_MAX_CONCURRENCY` 读取上限；非法值忽略。
pub fn configure_from_env() {
    if let Ok(value) = std::env::var("TRIT_MAX_CONCURRENCY")
        && let Ok(max) = value.trim().parse::<usize>()
    {
        configure(max);
    }
}

/// 在请求期间持有；Drop 时释放一个名额。
pub struct LimitGuard;

impl Drop for LimitGuard {
    fn drop(&mut self) {
        let (lock, cond) = state();
        let mut limit = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        limit.active = limit.active.saturating_sub(1);
        cond.notify_one();
    }
}

/// 获取一个名额；达到上限时阻塞。
pub fn acquire() -> LimitGuard {
    let (lock, cond) = state();
    let mut limit = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    while limit.max > 0 && limit.active >= limit.max {
        limit = cond
            .wait(limit)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    limit.active += 1;
    LimitGuard
}

/// 当前持有名额的请求数。
pub fn active() -> usize {
    let (lock, _) = state();
    lock.lock().map(|limit| limit.active).unwrap_or(0)
}

/// 当前配置的上限；0 表示不限。
pub fn configured_max() -> usize {
    let (lock, _) = state();
    lock.lock().map(|limit| limit.max).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn limit_caps_concurrent_holders() {
        configure(2);
        let peak = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let peak = Arc::clone(&peak);
                scope.spawn(move || {
                    let _guard = acquire();
                    peak.fetch_max(active(), Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                });
            }
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "{}",
            peak.load(Ordering::SeqCst)
        );
        configure(0);
    }
}
