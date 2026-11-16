//! 低资源档位：面向低配机器、长会话和国产系统适配的统一预算。
//!
//! 这些值只影响宿主侧的步骤/输出/搜索/上下文预算，不改变裁决语义。

use crate::context::ContextPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceProfile {
    Full,
    Low,
}

impl ResourceProfile {
    pub fn detect() -> Self {
        let requested = std::env::var("TRIT_LOW_RESOURCE")
            .map(|value| !value.is_empty() && value != "0")
            .unwrap_or(false);
        if requested { Self::Low } else { Self::Full }
    }

    pub fn max_steps(self) -> usize {
        match self {
            Self::Full => 8,
            Self::Low => 5,
        }
    }

    pub fn max_output_bytes(self) -> usize {
        match self {
            Self::Full => 32 * 1024,
            Self::Low => 8 * 1024,
        }
    }

    pub fn max_search_file_bytes(self) -> usize {
        match self {
            Self::Full => 1024 * 1024,
            Self::Low => 256 * 1024,
        }
    }

    pub fn max_search_results(self) -> usize {
        match self {
            Self::Full => 100,
            Self::Low => 40,
        }
    }

    pub fn max_glob_results(self) -> usize {
        match self {
            Self::Full => 200,
            Self::Low => 80,
        }
    }

    pub fn context_policy(self) -> ContextPolicy {
        match self {
            Self::Full => ContextPolicy::default(),
            Self::Low => ContextPolicy {
                max_receipts: 6,
                max_chars: 12 * 1024,
                max_output_chars: 2 * 1024,
            },
        }
    }

    pub fn context_read_budget(self) -> usize {
        match self {
            Self::Full => 16 * 1024,
            Self::Low => 4 * 1024,
        }
    }

    pub fn context_read_max(self) -> usize {
        match self {
            Self::Full => 8 * 1024,
            Self::Low => 2 * 1024,
        }
    }

    pub fn cache_window_samples(self) -> usize {
        match self {
            Self::Full => 64,
            Self::Low => 32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_profile_is_strictly_smaller() {
        let full = ResourceProfile::Full;
        let low = ResourceProfile::Low;
        assert!(low.max_steps() < full.max_steps());
        assert!(low.max_output_bytes() < full.max_output_bytes());
        assert!(low.max_search_file_bytes() < full.max_search_file_bytes());
        assert!(low.max_search_results() < full.max_search_results());
        assert!(low.max_glob_results() < full.max_glob_results());
        assert!(low.context_policy().max_chars < full.context_policy().max_chars);
        assert!(low.context_policy().max_receipts < full.context_policy().max_receipts);
        assert!(low.context_read_budget() < full.context_read_budget());
        assert!(low.context_read_max() < full.context_read_max());
        assert!(low.cache_window_samples() < full.cache_window_samples());
    }

    #[test]
    fn low_profile_keeps_safe_minimums() {
        let low = ResourceProfile::Low;
        assert!(low.max_steps() >= 1);
        assert!(low.max_output_bytes() >= 1024);
        assert!(low.max_search_results() >= 1);
        assert!(low.context_policy().max_receipts >= 1);
        assert!(low.context_policy().max_chars >= 1);
    }
}
