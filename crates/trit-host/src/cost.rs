//! 成本模型：把 token 用量按模型单价折算成美元，并对预算给出告警。
//!
//! 单价单位是美元 / 百万 token。内置表只覆盖常见模型；未知模型返回 `None`，
//! 调用方必须显式处理，绝不按 0 计价静默通过。
//!
//! 覆盖方式：设置 `TRIT_MODEL_PRICES=<json 文件>`，内容形如
//! `{"my-gateway-model": {"input": 1.0, "output": 2.0, "cached_input": 0.5}}`。
//! 同名条目覆盖内置表，新名字追加；`cached_input` 可省略（按 `input` 计）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use trit_orch::TokenUsage;

pub const COST_SCHEMA: u32 = 1;
/// 预算告警默认阈值：已花达到预算的 80% 即告警。
pub const DEFAULT_ALERT_THRESHOLD: f64 = 0.8;
/// 覆盖内置价目表的环境变量，值是 JSON 文件路径。
pub const PRICES_ENV: &str = "TRIT_MODEL_PRICES";
/// 默认美元兑人民币汇率；成本默认以人民币展示。
pub const DEFAULT_USD_CNY: f64 = 7.2;
/// 覆盖美元兑人民币汇率的处理变量名。
pub const USD_CNY_ENV: &str = "TRIT_USD_CNY";

/// 当前汇率；处理变量缺失、非法或非正时回退默认值。
pub fn usd_cny_rate() -> f64 {
    std::env::var(USD_CNY_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(DEFAULT_USD_CNY)
}

/// 单一模型的单价，单位：美元 / 百万 token。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// 输入（cache miss）单价。
    pub input: f64,
    /// 输出单价。
    pub output: f64,
    /// 缓存命中输入单价；缺省时按 `input` 计。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
}

/// 一次用量的折算结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub model: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
    /// 人民币金额；按 `TRIT_USD_CNY` 或默认汇率折算。
    pub cny: f64,
    /// 用量本身来自供应商估算（`TokenUsage::estimated`）时为 false。
    pub exact: bool,
}

/// 预算告警；只有跨过阈值才会生成。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetAlert {
    pub budget_usd: f64,
    pub spent_usd: f64,
    /// 预算的人民币金额（按当前汇率折算）。
    pub budget_cny: f64,
    /// 已花的人民币金额（按当前汇率折算）。
    pub spent_cny: f64,
    pub threshold: f64,
    /// 已花达到或超过预算本身。
    pub exceeded: bool,
}

impl BudgetAlert {
    /// 便于写进报告 warnings 的一行文本；默认以人民币展示。
    pub fn warning(&self) -> String {
        let percent = (self.spent_cny / self.budget_cny * 100.0).round();
        if self.exceeded {
            format!(
                "预算超支：已花 ¥{:.2} / ¥{:.2}（{}%）",
                self.spent_cny, self.budget_cny, percent
            )
        } else {
            format!(
                "预算告警：已花 ¥{:.2} / ¥{:.2}（{}%，阈值 {}%）",
                self.spent_cny,
                self.budget_cny,
                percent,
                (self.threshold * 100.0).round()
            )
        }
    }
}

/// 模型单价表；支持精确名与 `-`/`@`/`:`/`/` 分隔的版本前缀。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PriceTable {
    prices: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// 内置常见模型单价（美元 / 百万 token）。
    pub fn builtin() -> Self {
        let mut prices = BTreeMap::new();
        let mut add = |model: &str, input: f64, output: f64, cached_input: f64| {
            prices.insert(
                model.to_owned(),
                ModelPrice {
                    input,
                    output,
                    cached_input: Some(cached_input),
                },
            );
        };
        add("gpt-4o", 2.50, 10.00, 1.25);
        add("gpt-4o-mini", 0.15, 0.60, 0.075);
        add("gpt-4.1", 2.00, 8.00, 0.50);
        add("gpt-4.1-mini", 0.40, 1.60, 0.10);
        add("gpt-4.1-nano", 0.10, 0.40, 0.025);
        add("o3", 2.00, 8.00, 0.50);
        add("o4-mini", 1.10, 4.40, 0.275);
        add("claude-sonnet-4", 3.00, 15.00, 0.30);
        add("claude-opus-4", 15.00, 75.00, 1.50);
        add("claude-3-5-haiku", 0.80, 4.00, 0.08);
        add("deepseek-chat", 0.27, 1.10, 0.07);
        add("deepseek-reasoner", 0.55, 2.19, 0.14);
        add("gemini-2.0-flash", 0.10, 0.40, 0.025);
        add("gemini-2.5-pro", 1.25, 10.00, 0.31);
        Self { prices }
    }

    /// 内置表 + `TRIT_MODEL_PRICES` 覆盖；设置了变量但读不到/解析失败时 fail closed。
    pub fn from_env() -> Result<Self, String> {
        let mut table = Self::builtin();
        let Ok(path) = std::env::var(PRICES_ENV) else {
            return Ok(table);
        };
        let path = path.trim();
        if path.is_empty() {
            return Ok(table);
        }
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{PRICES_ENV} {path}: {error}"))?;
        table.apply_json(&text)?;
        Ok(table)
    }

    /// 合并一份价目表 JSON；同名覆盖，新名字追加。
    pub fn apply_json(&mut self, text: &str) -> Result<(), String> {
        let extra: BTreeMap<String, ModelPrice> =
            serde_json::from_str(text).map_err(|error| format!("pricing json: {error}"))?;
        for (model, price) in extra {
            if model.trim().is_empty() {
                return Err("pricing json: empty model name".to_owned());
            }
            if !valid_price(price.input) || !valid_price(price.output) {
                return Err(format!("pricing json: invalid price for {model}"));
            }
            if let Some(cached_input) = price.cached_input
                && !valid_price(cached_input)
            {
                return Err(format!("pricing json: invalid cached_input for {model}"));
            }
            self.prices.insert(model, price);
        }
        Ok(())
    }

    /// 精确名优先；否则取最长的、且后面跟版本分隔符的前缀。
    pub fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(price) = self.prices.get(model) {
            return Some(price);
        }
        self.prices
            .iter()
            .filter(|(key, _)| {
                model.len() > key.len()
                    && model.starts_with(key.as_str())
                    && model[key.len()..]
                        .chars()
                        .next()
                        .is_some_and(|next| matches!(next, '-' | '@' | ':' | '/'))
            })
            .max_by_key(|(key, _)| key.len())
            .map(|(_, price)| price)
    }

    /// 按模型单价折算用量；未知模型返回 `None`。
    pub fn estimate(&self, model: &str, usage: &TokenUsage) -> Option<CostEstimate> {
        let price = self.lookup(model)?;
        let cached_rate = price.cached_input.unwrap_or(price.input);
        let usd = (usage.fresh as f64 * price.input
            + usage.cached as f64 * cached_rate
            + usage.output as f64 * price.output)
            / 1_000_000.0;
        Some(CostEstimate {
            model: model.to_owned(),
            input_tokens: usage.fresh,
            cached_input_tokens: usage.cached,
            output_tokens: usage.output,
            usd,
            cny: usd * usd_cny_rate(),
            exact: usage.is_exact(),
        })
    }
}

fn valid_price(price: f64) -> bool {
    price.is_finite() && price >= 0.0
}

/// 已花费用达到预算的 `threshold` 时返回告警；`budget_usd <= 0` 视为不限预算。
pub fn budget_alert(spent_usd: f64, budget_usd: f64, threshold: f64) -> Option<BudgetAlert> {
    if !(budget_usd.is_finite() && budget_usd > 0.0) {
        return None;
    }
    let threshold = if threshold.is_finite() && threshold > 0.0 {
        threshold
    } else {
        DEFAULT_ALERT_THRESHOLD
    };
    if spent_usd < budget_usd * threshold {
        return None;
    }
    Some(BudgetAlert {
        budget_usd,
        spent_usd,
        budget_cny: budget_usd * usd_cny_rate(),
        spent_cny: spent_usd * usd_cny_rate(),
        threshold,
        exceeded: spent_usd >= budget_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(fresh: u64, cached: u64, output: u64) -> TokenUsage {
        TokenUsage {
            fresh,
            cached,
            output,
            estimated: false,
        }
    }

    #[test]
    fn lookup_prefers_exact_then_longest_prefix() {
        let table = PriceTable::builtin();
        assert_eq!(table.lookup("gpt-4o").unwrap().input, 2.50);
        assert_eq!(table.lookup("gpt-4o-2024-08-06").unwrap().input, 2.50);
        assert_eq!(table.lookup("gpt-4o-mini").unwrap().input, 0.15);
        assert_eq!(table.lookup("gpt-4o-mini-2024-07-18").unwrap().input, 0.15);
        // 前缀后必须是版本分隔符，`gpt-4o-whatever` 可以，`gpt-4oo` 不行。
        assert!(table.lookup("gpt-4oo").is_none());
        assert!(table.lookup("claude-sonnet-4-20250514").is_some());
        assert!(table.lookup("unknown-model").is_none());
    }

    #[test]
    fn estimate_uses_cached_rate_and_reports_exactness() {
        let table = PriceTable::builtin();
        let cost = table
            .estimate("gpt-4o", &usage(1_000_000, 1_000_000, 1_000_000))
            .unwrap();
        assert!((cost.usd - (2.50 + 1.25 + 10.00)).abs() < 1e-9);
        assert!((cost.cny - cost.usd * usd_cny_rate()).abs() < 1e-9);
        assert!(cost.exact);
        let mut estimated = usage(0, 0, 1_000_000);
        estimated.estimated = true;
        assert!(!table.estimate("gpt-4o", &estimated).unwrap().exact);
    }

    #[test]
    fn override_json_replaces_and_adds_models() {
        let mut table = PriceTable::builtin();
        table
            .apply_json(
                r#"{
                    "gpt-4o": {"input": 1.0, "output": 2.0},
                    "gateway-x": {"input": 0.5, "output": 1.5, "cached_input": 0.1}
                }"#,
            )
            .unwrap();
        assert_eq!(table.lookup("gpt-4o").unwrap().input, 1.0);
        assert_eq!(table.lookup("gpt-4o").unwrap().cached_input, None);
        assert_eq!(table.lookup("gateway-x").unwrap().cached_input, Some(0.1));
        // 覆盖项之后仍保留内置的其他模型。
        assert!(table.lookup("gpt-4.1").is_some());
    }

    #[test]
    fn invalid_override_fails_closed() {
        let mut table = PriceTable::builtin();
        assert!(
            table
                .apply_json(r#"{"x": {"input": -1.0, "output": 1.0}}"#)
                .is_err()
        );
        assert!(
            table
                .apply_json(r#"{"": {"input": 1.0, "output": 1.0}}"#)
                .is_err()
        );
        assert!(table.apply_json("{not json").is_err());
    }

    #[test]
    fn budget_alert_crosses_threshold_and_flags_exceeded() {
        assert!(budget_alert(0.79, 1.0, DEFAULT_ALERT_THRESHOLD).is_none());
        let alert = budget_alert(0.80, 1.0, DEFAULT_ALERT_THRESHOLD).unwrap();
        assert!(!alert.exceeded);
        assert!(alert.warning().contains("80%"));
        let exceeded = budget_alert(1.20, 1.0, DEFAULT_ALERT_THRESHOLD).unwrap();
        assert!(exceeded.exceeded);
        assert!(exceeded.warning().contains("预算超支"));
        assert!((exceeded.budget_cny - exceeded.budget_usd * usd_cny_rate()).abs() < 1e-9);
        // 不限预算与非法阈值。
        assert!(budget_alert(100.0, 0.0, DEFAULT_ALERT_THRESHOLD).is_none());
        assert!(budget_alert(100.0, 1.0, f64::NAN).is_some());
    }
}
