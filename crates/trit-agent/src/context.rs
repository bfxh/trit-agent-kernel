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
    pub dropped_chars: usize,
    pub truncated_chars: usize,
}

const MAX_SUMMARY_CHARS: usize = 1_024;
const MAX_SUMMARY_EXCERPT_CHARS: usize = 120;

pub fn compact_receipts(receipts: &[ToolOutcome], policy: &ContextPolicy) -> CompactedContext {
    let max_receipts = policy.max_receipts.max(1);
    let max_chars = policy.max_chars.max(1);
    let mut kept = Vec::with_capacity(receipts.len());
    let mut truncated = 0;
    let mut truncated_chars: usize = 0;
    for receipt in receipts {
        let mut receipt = receipt.clone();
        let (output, omitted) = truncate(receipt.output, policy.max_output_chars);
        receipt.output = output;
        if omitted > 0 {
            truncated += 1;
            truncated_chars = truncated_chars.saturating_add(omitted);
        }
        kept.push(receipt);
    }

    // 需要丢弃旧回执时，为摘要预留预算；单条回执不需要丢弃，不预留。
    let will_drop = kept.len() > max_receipts || (kept.len() > 1 && total_chars(&kept) > max_chars);
    let summary_budget = if will_drop {
        (max_chars / 4).min(MAX_SUMMARY_CHARS)
    } else {
        0
    };
    let body_budget = max_chars.saturating_sub(summary_budget);

    let mut dropped_receipts = Vec::new();
    while kept.len() > max_receipts {
        dropped_receipts.push(kept.remove(0));
    }
    while total_chars(&kept) > body_budget && kept.len() > 1 {
        dropped_receipts.push(kept.remove(0));
    }
    if kept.len() == 1 && total_chars(&kept) > body_budget {
        let receipt = &mut kept[0];
        let budget = body_budget.saturating_sub(receipt.tool.chars().count());
        let (output, omitted) = truncate(std::mem::take(&mut receipt.output), budget);
        receipt.output = output;
        if omitted > 0 {
            truncated += 1;
            truncated_chars = truncated_chars.saturating_add(omitted);
        }
    }

    let dropped = dropped_receipts.len();
    let dropped_chars = dropped_receipts
        .iter()
        .map(|receipt| receipt.tool.chars().count() + receipt.output.chars().count())
        .fold(0usize, usize::saturating_add);
    let digest = if dropped > 0 && summary_budget > 0 {
        Some(summarize(
            &dropped_receipts,
            dropped,
            dropped_chars,
            summary_budget,
        ))
    } else {
        None
    };
    CompactedContext {
        receipts: kept,
        digest,
        dropped,
        truncated,
        dropped_chars,
        truncated_chars,
    }
}

fn summarize(
    dropped: &[ToolOutcome],
    count: usize,
    dropped_chars: usize,
    max_chars: usize,
) -> String {
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
    let mut summary = String::new();
    push_bounded(
        &mut summary,
        &format!(
            "compacted {count} earlier tool results; tools={tools:?}; failed={failed}; in_doubt={in_doubt}; written_paths={paths}; dropped_chars={dropped_chars}\n"
        ),
        max_chars,
    );
    for receipt in dropped {
        let excerpt = excerpt(&receipt.output, MAX_SUMMARY_EXCERPT_CHARS);
        push_bounded(
            &mut summary,
            &format!(
                "- {} {} passed={} in_doubt={} paths={} output: {}\n",
                receipt.call_id,
                receipt.tool,
                receipt.passed,
                receipt.in_doubt,
                receipt.written_paths.len(),
                excerpt
            ),
            max_chars,
        );
        if summary.chars().count() >= max_chars {
            break;
        }
    }
    summary
}

fn push_bounded(output: &mut String, text: &str, max_chars: usize) {
    let used = output.chars().count();
    if used >= max_chars {
        return;
    }
    let remaining = max_chars - used;
    if text.chars().count() <= remaining {
        output.push_str(text);
    } else {
        let (part, _) = truncate(text.to_owned(), remaining);
        output.push_str(&part);
    }
}

fn excerpt(output: &str, max_chars: usize) -> String {
    let line = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let (excerpt, _) = truncate(collapsed, max_chars);
    excerpt
}

fn total_chars(receipts: &[ToolOutcome]) -> usize {
    receipts
        .iter()
        .map(|receipt| receipt.tool.chars().count() + receipt.output.chars().count())
        .sum()
}

fn truncate(mut text: String, max: usize) -> (String, usize) {
    let chars = text.chars().count();
    if chars <= max {
        return (text, 0);
    }
    const MARKER: &str = "...(truncated)";
    let marker_chars = MARKER.chars().count();
    let body = if max > marker_chars {
        max - marker_chars
    } else {
        max
    };
    let end = text
        .char_indices()
        .nth(body)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(end);
    if max > marker_chars {
        text.push_str(MARKER);
    }
    (text, chars.saturating_sub(body))
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
        let digest = compacted.digest.as_deref().unwrap();
        assert!(total_chars(&compacted.receipts) + digest.chars().count() <= 50);
    }

    #[test]
    fn digest_includes_bounded_excerpts_and_char_counts() {
        let receipts = vec![
            outcome("c1", "bash", "first line\nsecond line"),
            outcome("c2", "read_file", &"x".repeat(300)),
            outcome("c3", "bash", "latest"),
        ];
        let compacted = compact_receipts(
            &receipts,
            &ContextPolicy {
                max_receipts: 1,
                max_chars: 10_000,
                max_output_chars: 200,
            },
        );
        assert_eq!(compacted.receipts.len(), 1);
        assert_eq!(compacted.dropped, 2);
        assert!(compacted.dropped_chars > 0);
        assert!(compacted.truncated_chars > 0);
        let digest = compacted.digest.unwrap();
        assert!(digest.contains("first line"));
        assert!(digest.contains("c1"));
        assert!(digest.contains("c2"));
        assert!(digest.chars().count() <= MAX_SUMMARY_CHARS);
    }

    #[test]
    fn truncation_respects_budget_including_marker() {
        let receipts = vec![outcome("c1", "bash", &"a".repeat(100))];
        let compacted = compact_receipts(
            &receipts,
            &ContextPolicy {
                max_receipts: 10,
                max_chars: 20,
                max_output_chars: 20,
            },
        );
        assert_eq!(compacted.receipts.len(), 1);
        assert!(compacted.truncated >= 1);
        assert!(total_chars(&compacted.receipts) <= 20);
        assert!(compacted.receipts[0].output.ends_with("...(truncated)"));
    }
}
