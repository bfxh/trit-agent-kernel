//! 三类 append-only JSONL 账本的布局兼容矩阵。
//!
//! 这是矩阵的单一事实来源：文档 [`docs/JSONL-COMPAT.md`] 必须与这里逐行一致
//! （由 `tests/jsonl_compat.rs` 校验），读取器行为也按这里的规则实现。
//!
//! 规则：
//! - 读取只接受 `schema ==` 当前版本；更大的版本 fail-closed，不猜布局。
//! - 同版本内后加字段一律 `#[serde(default)]`，旧文件缺失即取默认值。
//! - 写入只整行追加，不重写既有行；因此升级不会改动老字节。
//! - 崩溃尾行：mutation/rewind 账本容忍最后一行的半截 JSON；会话日志不容忍，
//!   它是权威记录，半行必须显式报错。

/// 一类 JSONL 账本的布局与读取策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonlLayout {
    /// 文件位置（相对 workspace 或 checkpoint 目录）。
    pub file: &'static str,
    /// 当前 schema 版本。
    pub schema: u32,
    /// 顶层必填字段。
    pub required: &'static [&'static str],
    /// 同版本内后加的可选字段（缺失时取默认值）。
    pub optional: &'static [&'static str],
    /// 是否容忍崩溃留下的半截尾行。
    pub tolerates_trailing_partial: bool,
}

/// mutation journal（工具写入对账）。
pub const WRITE_JOURNAL_LAYOUT: JsonlLayout = JsonlLayout {
    file: crate::journal::JOURNAL_PATH,
    schema: crate::journal::JOURNAL_SCHEMA,
    required: &["schema", "seq", "op_id", "phase", "tool", "at"],
    optional: &[
        "call_id",
        "paths",
        "lease_epoch",
        "lease_locks",
        "passed",
        "in_doubt",
        "output",
        "backups",
    ],
    tolerates_trailing_partial: true,
};

/// checkpoint rewind journal（回退进行时/未完成记录）。
pub const REWIND_JOURNAL_LAYOUT: JsonlLayout = JsonlLayout {
    file: crate::checkpoint::REWIND_JOURNAL,
    schema: crate::checkpoint::REWIND_SCHEMA,
    required: &["schema", "seq", "op_id", "phase", "turn", "at"],
    optional: &[
        "paths", "backups", "backend", "backup", "path", "restored", "deleted", "failures",
    ],
    tolerates_trailing_partial: true,
};

/// 无头会话事件日志。
pub const SESSION_LOG_LAYOUT: JsonlLayout = JsonlLayout {
    file: "<session>.jsonl",
    schema: crate::session::SESSION_SCHEMA,
    required: &["schema", "seq", "event"],
    optional: &[
        "event.proposal.context",
        "event.context_compacted.retained",
        "event.context_compacted.dropped_chars",
        "event.context_compacted.truncated_chars",
        "event.context_compacted.epoch_before",
        "event.context_compacted.epoch_after",
        "event.context_compacted.freshness",
        "event.turn_finished.metrics",
        "event.gate_chain.failed_at",
        "event.gate_chain.failures",
        "event.gate_chain.passed",
        "event.gate_chain.blocked",
        "event.gate_chain.unknown",
        "event.gate_chain.skipped",
        "event.gate_chain.report",
    ],
    tolerates_trailing_partial: false,
};

/// 全部已知布局；新增账本时在这里登记，并同步文档。
pub const JSONL_LAYOUTS: &[JsonlLayout] = &[
    WRITE_JOURNAL_LAYOUT,
    REWIND_JOURNAL_LAYOUT,
    SESSION_LOG_LAYOUT,
];

/// 按文件位置查布局；未登记的文件返回 `None`。
pub fn layout_for(file: &str) -> Option<&'static JsonlLayout> {
    JSONL_LAYOUTS.iter().find(|layout| layout.file == file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_layout_is_registered_and_lookupable() {
        for layout in JSONL_LAYOUTS {
            assert_eq!(layout_for(layout.file), Some(layout));
            assert!(layout.schema >= 1);
            assert!(!layout.required.is_empty());
            assert!(layout.required.contains(&"schema"));
        }
    }
}
