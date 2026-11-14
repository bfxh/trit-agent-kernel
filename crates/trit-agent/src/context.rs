//! 无头 turn 的上下文压缩。
//!
//! 只压缩传给模型的 tool receipts：保留最近结果，旧结果按确定性的摘要折叠。
//! 全量回执仍留在 session / TurnOutcome 里，压缩不改变审计事实。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ToolOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicy {
    /// 最多保留多少条完整回执；至少为 1。
    pub max_receipts: usize,
    /// 回执文本总字符预算；至少为 1。
    pub max_chars: usize,
    /// 单条回执输出截断上限。
    pub max_output_chars: usize,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            max_receipts: 12,
            max_chars: 32 * 1024,
            max_output_chars: 4 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactedContext {
    pub receipts: Vec<ToolOutcome>,
    pub digest: Option<String>,
    pub dropped: usize,
    pub truncated: usize,
}

pub fn compact_receipts(receipts: &[ToolOutcome], policy: &ContextPolicy) -> CompactedContext {
    let max_receipts = policy.max_receipts.max(1);
    let max_chars = policy.max_chars.max(1);
    let mut kept = Vec::with_capacity(receipts.len());
    let mut truncated = 0;
    for receipt in receipts {
        let mut receipt = receipt.clone();
        let (output, did_truncate) = truncate(receipt.output, policy.max_output_chars);
        receipt.output = output;
        truncated += usize::from(did_truncate);
        kept.push(receipt);
    }

    let mut dropped_receipts = Vec::new();
    while kept.len() > max_receipts {
        dropped_receipts.push(kept.remove(0));
    }
    while total_chars(&kept) > max_chars && kept.len() > 1 {
        dropped_receipts.push(kept.remove(0));
    }
    if kept.len() == 1 && total_chars(&kept) > max_chars {
        let receipt = &mut kept[0];
        let budget = max_chars.saturating_sub(receipt.tool.len());
        let (output, did_truncate) = truncate(std::mem::take(&mut receipt.output), budget);
        receipt.output = output;
        truncated += usize::from(did_truncate);
    }

    let dropped = dropped_receipts.len();
    let digest = if dropped > 0 {
        Some(summarize(&dropped_receipts, dropped))
    } else {
        None
    };
    CompactedContext {
        receipts: kept,
        digest,
        dropped,
        truncated,
    }
}

fn summarize(dropped: &[ToolOutcome], count: usize) -> String {
    let tools: BTreeSet<&str> = dropped
        .iter()
        .map(|receipt| receipt.tool.as_str())
        .collect();
    let failed = dropped.iter().filter(|receipt| !receipt.passed).count();
    let in_doubt = dropped.iter().filter(|receipt| receipt.in_doubt).count();
    let paths: usize = dropped
        .iter()
        .map(|receipt| receipt.written_paths.len())
        .sum();
    format!(
        "compacted {count} earlier tool results; tools={tools:?}; failed={failed}; in_doubt={in_doubt}; written_paths={paths}"
    )
}

fn total_chars(receipts: &[ToolOutcome]) -> usize {
    receipts
        .iter()
        .map(|receipt| receipt.tool.chars().count() + receipt.output.chars().count())
        .sum()
}

fn truncate(mut text: String, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text, false);
    }
    let end = text
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(end);
    text.push_str("...(truncated)");
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(call: &str, tool: &str, output: &str) -> ToolOutcome {
        ToolOutcome {
            call_id: call.into(),
            tool: tool.into(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: output.into(),
        }
    }

    #[test]
    fn keeps_recent_receipts_and_digests_old_ones() {
        let receipts = vec![
            outcome("c1", "read_file", "one"),
            outcome("c2", "bash", "two"),
            outcome("c3", "write_file", "three"),
            outcome("c4", "read_file", "four"),
        ];
        let compacted = compact_receipts(
            &receipts,
            &ContextPolicy {
                max_receipts: 2,
                max_chars: 10_000,
                max_output_chars: 1_000,
            },
        );
        assert_eq!(compacted.receipts.len(), 2);
        assert_eq!(compacted.receipts[0].call_id, "c3");
        assert_eq!(compacted.receipts[1].call_id, "c4");
        assert_eq!(compacted.dropped, 2);
        let digest = compacted.digest.unwrap();
        assert!(digest.contains("compacted 2 earlier"));
        assert!(digest.contains("bash"));
    }

    #[test]
    fn enforces_the_char_budget() {
        let receipts = vec![
            outcome("c1", "bash", &"a".repeat(100)),
            outcome("c2", "bash", &"b".repeat(100)),
        ];
        let compacted = compact_receipts(
            &receipts,
            &ContextPolicy {
                max_receipts: 10,
                max_chars: 50,
                max_output_chars: 1_000,
            },
        );
        assert_eq!(compacted.receipts.len(), 1);
        assert_eq!(compacted.receipts[0].call_id, "c2");
        assert_eq!(compacted.dropped, 1);
        assert!(compacted.digest.is_some());
    }
}
