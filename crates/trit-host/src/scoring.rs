//! 前后端评分：把一轮结果折算成可解释的分数。
//!
//! 分数只描述"这一轮做得好不好"，不改变证据裁决；是否让它参与决策由设置
//! `scoring.participate_in_decision` 控制（默认关闭）。

use serde::{Deserialize, Serialize};

use crate::TurnOutcome;

/// 评分对象：后端产物、前端产物，或两者混合的一轮。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Backend,
    Frontend,
    Both,
}

/// 一条可解释的评分结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceScore {
    pub surface: Surface,
    /// 0..=100。
    pub score: u8,
    /// `score < BLOCK_THRESHOLD`。
    pub blocked: bool,
    /// 扣分原因；空表示满分。
    pub reasons: Vec<String>,
}

/// 低于该分数视为不合格；是否据此阻断由 `participate_in_decision` 决定。
pub const BLOCK_THRESHOLD: u8 = 60;

/// 对一轮结果打分。
///
/// 规则刻意保守且可解释：未证明完成扣 40、被否证扣 30、冲突扣 20；
/// 存疑回执每个扣 5（上限 20）、失败回执每个扣 10（上限 30）、重复校验每次扣 2（上限 10）；
/// 前端面没有写出任何前端产物再扣 10。
pub fn score_turn(outcome: &TurnOutcome, surface: Surface) -> SurfaceScore {
    let mut score: i32 = 100;
    let mut reasons = Vec::new();

    if !outcome.judgment.is_complete() {
        score -= 40;
        reasons.push("completion not proved".to_owned());
    }
    if outcome.judgment.is_refuted() {
        score -= 30;
        reasons.push("completion refuted".to_owned());
    }
    if outcome.judgment.is_conflicted() {
        score -= 20;
        reasons.push("completion conflicted".to_owned());
    }

    let in_doubt = outcome
        .receipts
        .iter()
        .filter(|receipt| receipt.in_doubt)
        .count();
    if in_doubt > 0 {
        score -= (in_doubt as i32 * 5).min(20);
        reasons.push(format!("{in_doubt} in-doubt tool result(s)"));
    }

    let failed = outcome
        .receipts
        .iter()
        .filter(|receipt| !receipt.passed && !receipt.in_doubt)
        .count();
    if failed > 0 {
        score -= (failed as i32 * 10).min(30);
        reasons.push(format!("{failed} failed tool result(s)"));
    }

    let repeated = outcome.metrics.repeated_verifications;
    if repeated > 0 {
        score -= (repeated as i32 * 2).min(10);
        reasons.push(format!("{repeated} repeated verification(s)"));
    }

    if matches!(surface, Surface::Frontend) {
        let wrote_frontend = outcome
            .receipts
            .iter()
            .flat_map(|receipt| receipt.written_paths.iter())
            .any(|path| {
                let lower = path.to_ascii_lowercase();
                [".tsx", ".jsx", ".vue", ".svelte", ".css", ".scss", ".html"]
                    .iter()
                    .any(|ext| lower.ends_with(ext))
            });
        if !wrote_frontend {
            score -= 10;
            reasons.push("no frontend artifact written".to_owned());
        }
    }

    let score = score.clamp(0, 100) as u8;
    SurfaceScore {
        surface,
        score,
        blocked: score < BLOCK_THRESHOLD,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_and_surface_are_stable_contract() {
        assert_eq!(BLOCK_THRESHOLD, 60);
        let json = serde_json::to_string(&Surface::Both).unwrap();
        assert_eq!(json, "\"both\"");
    }
}
