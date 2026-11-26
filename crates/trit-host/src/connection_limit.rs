//! 进程内连接并发上限：超额连接回 503，不排队、不创建线程。
//!
//! 远程执行端点与幂等账本共用此限流器。

use std::sync::{Arc, Mutex};

/// 参考服务端的默认并发连接数。
pub(crate) const DEFAULT_MAX_CONNECTIONS: usize = 64;

pub(crate) struct ConnectionSlots {
    limit: usize,
    active: Mutex<usize>,
}

pub(crate) struct ConnectionSlot {
    slots: Arc<ConnectionSlots>,
}

impl ConnectionSlots {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(1),
            active: Mutex::new(0),
        })
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ConnectionSlot> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *active >= self.limit {
            return None;
        }
        *active += 1;
        Some(ConnectionSlot {
            slots: Arc::clone(self),
        })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut active = self
            .slots
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
    }
}
