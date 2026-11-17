//! 模型连接健康状态：纯状态机，不发请求、不读时钟。
//!
//! 自动探测仍由宿主决定；这里只记录成功 / 失败序列，避免把一次 429 或
//! 一次鉴权失败永久当成“可用”。未知状态不等于健康。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Unknown,
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionFailureKind {
    Transport,
    RateLimit,
    Auth,
    Quota,
    Server,
    Protocol,
    Unknown,
}

impl ConnectionFailureKind {
    /// 鉴权 / 配额 / 协议错误通常不能靠重试恢复；其余可重试。
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transport | Self::RateLimit | Self::Server | Self::Unknown
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionFailure {
    pub kind: ConnectionFailureKind,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionHealthPolicy {
    /// 连续可重试失败达到该次数后进入 Unavailable。
    pub unavailable_after: u32,
}

impl Default for ConnectionHealthPolicy {
    fn default() -> Self {
        Self {
            unavailable_after: 3,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionHealth {
    pub state: ConnectionState,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_failure: Option<ConnectionFailure>,
}

impl ConnectionHealth {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&mut self) {
        self.state = ConnectionState::Healthy;
        self.consecutive_failures = 0;
        self.consecutive_successes = self.consecutive_successes.saturating_add(1);
        self.last_failure = None;
    }

    pub fn record_failure(&mut self, kind: ConnectionFailureKind, policy: ConnectionHealthPolicy) {
        self.consecutive_successes = 0;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let retryable = kind.is_retryable();
        self.last_failure = Some(ConnectionFailure { kind, retryable });
        self.state = if !retryable || self.consecutive_failures >= policy.unavailable_after.max(1) {
            ConnectionState::Unavailable
        } else {
            ConnectionState::Degraded
        };
    }

    /// 是否还值得发起请求；Unknown 仍然允许探测。
    pub fn is_usable(&self) -> bool {
        !matches!(self.state, ConnectionState::Unavailable)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_failures_degrade_then_unavailable() {
        let mut health = ConnectionHealth::new();
        let policy = ConnectionHealthPolicy::default();
        health.record_failure(ConnectionFailureKind::Transport, policy);
        assert_eq!(health.state, ConnectionState::Degraded);
        assert!(health.is_usable());
        health.record_failure(ConnectionFailureKind::RateLimit, policy);
        assert_eq!(health.state, ConnectionState::Degraded);
        health.record_failure(ConnectionFailureKind::Server, policy);
        assert_eq!(health.state, ConnectionState::Unavailable);
        assert!(!health.is_usable());
        assert_eq!(health.consecutive_failures(), 3);
    }

    #[test]
    fn auth_or_quota_failure_is_immediately_unavailable() {
        let mut health = ConnectionHealth::new();
        health.record_failure(
            ConnectionFailureKind::Auth,
            ConnectionHealthPolicy::default(),
        );
        assert_eq!(health.state, ConnectionState::Unavailable);
        assert!(!health.is_usable());
        assert_eq!(
            health.last_failure.unwrap().kind,
            ConnectionFailureKind::Auth
        );
    }

    #[test]
    fn success_clears_failure_streak_and_state() {
        let mut health = ConnectionHealth::new();
        health.record_failure(
            ConnectionFailureKind::Transport,
            ConnectionHealthPolicy::default(),
        );
        health.record_success();
        assert_eq!(health.state, ConnectionState::Healthy);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.consecutive_successes, 1);
        assert!(health.last_failure.is_none());
        assert!(health.is_usable());
    }

    #[test]
    fn health_roundtrips_through_json() {
        let mut health = ConnectionHealth::new();
        health.record_failure(
            ConnectionFailureKind::RateLimit,
            ConnectionHealthPolicy::default(),
        );
        let text = serde_json::to_string(&health).unwrap();
        let restored: ConnectionHealth = serde_json::from_str(&text).unwrap();
        assert_eq!(restored, health);
    }
}
