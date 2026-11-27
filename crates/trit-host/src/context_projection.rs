//! Append-only 上下文投影（B7.3）。
//!
//! 规范事实是 append-only 的 `canonical` 入口；模型可见投影只是
//! `canonical[visible_start..]`。压缩不会删除历史，只会移动可见起点、
//! 创建新的 epoch/generation，并把 freshness 降为 Unknown。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use trit_orch::Ternary;

use crate::ToolOutcome;
use crate::session::{SessionEvent, SessionRecord};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessRecovery {
    /// 新 epoch 内出现一条成功且有断言的证据后恢复 True。
    #[default]
    OnVerifiedEvidence,
    /// 宿主手动恢复；AgentHost 不自动改 freshness。
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectionError {
    #[error("cannot retain {retained} entries from {visible} visible entries")]
    RetainedExceedsVisible { retained: usize, visible: usize },
    #[error("cannot drop {dropped} entries from {visible} visible entries")]
    DroppedExceedsVisible { dropped: usize, visible: usize },
    #[error("projection {field} mismatch: expected {expected:?}, got {actual:?}")]
    EpochMismatch {
        field: &'static str,
        expected: Option<u64>,
        actual: u64,
    },
    #[error("projection freshness mismatch: expected {expected:?}, got {actual:?}")]
    FreshnessMismatch {
        expected: Option<Ternary>,
        actual: Ternary,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionEntry {
    pub call_id: String,
    pub tool: String,
    pub step: usize,
    /// 追加时的上下文 epoch。
    pub epoch: u64,
    pub passed: bool,
    pub in_doubt: bool,
    #[serde(default)]
    pub written_paths: Vec<String>,
    pub chars: usize,
    /// 输出内容的稳定指纹；空输出为 None。
    #[serde(default)]
    pub content_fingerprint: Option<String>,
    /// 同一可见窗口内已出现相同输出时，首个 `call_id`。
    #[serde(default)]
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionInput {
    pub step: usize,
    pub retained: usize,
    pub dropped: usize,
    pub truncated: usize,
    pub dropped_chars: usize,
    pub truncated_chars: usize,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionBoundary {
    pub step: usize,
    pub epoch_before: u64,
    pub epoch_after: u64,
    pub generation: u64,
    pub visible_before: usize,
    pub visible_after: usize,
    pub canonical_len: usize,
    pub retained: usize,
    pub dropped: usize,
    pub truncated: usize,
    pub dropped_chars: usize,
    pub truncated_chars: usize,
    pub freshness: Ternary,
    pub digest: Option<String>,
}

/// 送进 prompt 的轻量投影视图；不包含 canonical 历史。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionView {
    pub epoch: u64,
    pub generation: u64,
    pub freshness: Ternary,
    pub visible: Vec<ProjectionEntry>,
    #[serde(default)]
    pub last_compaction: Option<CompactionBoundary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextProjection {
    /// 规范日志：只追加，压缩不删除。
    canonical: Vec<ProjectionEntry>,
    /// 当前模型可见投影的起点；`begin_turn` 会把它推到当前末尾。
    visible_start: usize,
    epoch: u64,
    generation: u64,
    freshness: Ternary,
    boundaries: Vec<CompactionBoundary>,
    /// 当前可见窗口内 输出指纹 -> 首个 call_id；`begin_turn` 清空。
    visible_fingerprints: BTreeMap<String, String>,
}

impl Default for ContextProjection {
    fn default() -> Self {
        Self {
            canonical: Vec::new(),
            visible_start: 0,
            epoch: 0,
            generation: 0,
            freshness: Ternary::True,
            boundaries: Vec::new(),
            visible_fingerprints: BTreeMap::new(),
        }
    }
}

impl ContextProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// 新 turn 只让本 turn 新追加的 entry 进入可见投影；canonical 仍保留历史。
    pub fn begin_turn(&mut self) {
        self.visible_start = self.canonical.len();
        self.visible_fingerprints.clear();
    }

    pub fn append(&mut self, step: usize, outcome: &ToolOutcome) -> usize {
        let index = self.canonical.len();
        let fingerprint = (!outcome.output.is_empty())
            .then(|| crate::fingerprint::fnv1a64(outcome.output.as_bytes()));
        let duplicate_of = fingerprint
            .as_ref()
            .and_then(|fingerprint| self.visible_fingerprints.get(fingerprint))
            .filter(|existing| existing.as_str() != outcome.call_id)
            .cloned();
        if let (Some(fingerprint), None) = (&fingerprint, &duplicate_of) {
            self.visible_fingerprints
                .insert(fingerprint.clone(), outcome.call_id.clone());
        }
        self.canonical.push(ProjectionEntry {
            call_id: outcome.call_id.clone(),
            tool: outcome.tool.clone(),
            step,
            epoch: self.epoch,
            passed: outcome.passed,
            in_doubt: outcome.in_doubt,
            written_paths: outcome.written_paths.clone(),
            chars: outcome.output.chars().count(),
            content_fingerprint: fingerprint,
            duplicate_of,
        });
        index
    }

    /// 压缩当前可见投影；返回新的边界，旧 canonical 条目不删除。
    pub fn compact(
        &mut self,
        input: CompactionInput,
    ) -> Result<CompactionBoundary, ProjectionError> {
        let visible_before = self.canonical.len().saturating_sub(self.visible_start);
        if input.retained > visible_before {
            return Err(ProjectionError::RetainedExceedsVisible {
                retained: input.retained,
                visible: visible_before,
            });
        }
        if input.dropped > visible_before {
            return Err(ProjectionError::DroppedExceedsVisible {
                dropped: input.dropped,
                visible: visible_before,
            });
        }
        let epoch_before = self.epoch;
        self.epoch = self.epoch.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
        self.visible_start = self.canonical.len().saturating_sub(input.retained);
        self.freshness = Ternary::Unknown;
        let boundary = CompactionBoundary {
            step: input.step,
            epoch_before,
            epoch_after: self.epoch,
            generation: self.generation,
            visible_before,
            visible_after: input.retained,
            canonical_len: self.canonical.len(),
            retained: input.retained,
            dropped: input.dropped,
            truncated: input.truncated,
            dropped_chars: input.dropped_chars,
            truncated_chars: input.truncated_chars,
            freshness: self.freshness,
            digest: input.digest,
        };
        self.boundaries.push(boundary.clone());
        Ok(boundary)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn freshness(&self) -> Ternary {
        self.freshness
    }

    pub fn mark_freshness(&mut self, freshness: Ternary) {
        self.freshness = freshness;
    }

    pub fn canonical(&self) -> &[ProjectionEntry] {
        &self.canonical
    }

    pub fn visible(&self) -> &[ProjectionEntry] {
        &self.canonical[self.visible_start..]
    }

    pub fn boundaries(&self) -> &[CompactionBoundary] {
        &self.boundaries
    }

    pub fn last_compaction(&self) -> Option<&CompactionBoundary> {
        self.boundaries.last()
    }

    pub fn view(&self) -> ProjectionView {
        ProjectionView {
            epoch: self.epoch,
            generation: self.generation,
            freshness: self.freshness,
            visible: self.visible().to_vec(),
            last_compaction: self.last_compaction().cloned(),
        }
    }
}

/// 从 session 事件重建投影；用于回放一致性测试与迁移校验。
pub fn replay(records: &[SessionRecord]) -> Result<ContextProjection, ProjectionError> {
    let mut projection = ContextProjection::new();
    for record in records {
        match &record.event {
            SessionEvent::TurnStarted { .. } => projection.begin_turn(),
            SessionEvent::ToolCompleted { step, outcome } => {
                projection.append(*step, outcome);
            }
            SessionEvent::ContextCompacted {
                step,
                retained,
                dropped,
                truncated,
                dropped_chars,
                truncated_chars,
                epoch_before,
                epoch_after,
                freshness,
                digest,
            } => {
                // 仅截断、没有丢弃且没有 epoch 的旧事件不构成投影边界。
                if *dropped == 0 && epoch_before.is_none() && epoch_after.is_none() {
                    continue;
                }
                let visible = projection.visible().len();
                let retained = retained.unwrap_or_else(|| visible.saturating_sub(*dropped));
                let boundary = projection.compact(CompactionInput {
                    step: *step,
                    retained,
                    dropped: *dropped,
                    truncated: *truncated,
                    dropped_chars: *dropped_chars,
                    truncated_chars: *truncated_chars,
                    digest: digest.clone(),
                })?;
                if let Some(expected) = epoch_before
                    && boundary.epoch_before != *expected
                {
                    return Err(ProjectionError::EpochMismatch {
                        field: "epoch_before",
                        expected: Some(*expected),
                        actual: boundary.epoch_before,
                    });
                }
                if let Some(expected) = epoch_after
                    && boundary.epoch_after != *expected
                {
                    return Err(ProjectionError::EpochMismatch {
                        field: "epoch_after",
                        expected: Some(*expected),
                        actual: boundary.epoch_after,
                    });
                }
                if let Some(expected) = freshness
                    && boundary.freshness != *expected
                {
                    return Err(ProjectionError::FreshnessMismatch {
                        expected: Some(*expected),
                        actual: boundary.freshness,
                    });
                }
            }
            SessionEvent::ContextFreshnessRecovered {
                epoch, freshness, ..
            } => {
                if projection.epoch() != *epoch {
                    return Err(ProjectionError::EpochMismatch {
                        field: "freshness_epoch",
                        expected: Some(*epoch),
                        actual: projection.epoch(),
                    });
                }
                projection.mark_freshness(*freshness);
            }
            _ => {}
        }
    }
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(call_id: &str, chars: usize) -> ToolOutcome {
        ToolOutcome {
            call_id: call_id.into(),
            tool: "bash".into(),
            command: None,
            passed: true,
            assertions: 0,
            written_paths: Vec::new(),
            in_doubt: false,
            output: "x".repeat(chars),
        }
    }

    #[test]
    fn compaction_keeps_canonical_history_and_moves_visible_start() {
        let mut projection = ContextProjection::new();
        projection.begin_turn();
        projection.append(0, &outcome("a", 10));
        projection.append(0, &outcome("b", 20));
        projection.append(1, &outcome("c", 30));
        let boundary = projection
            .compact(CompactionInput {
                step: 1,
                retained: 1,
                dropped: 2,
                truncated: 0,
                dropped_chars: 30,
                truncated_chars: 0,
                digest: Some("digest".into()),
            })
            .unwrap();
        assert_eq!(boundary.visible_before, 3);
        assert_eq!(boundary.visible_after, 1);
        assert_eq!(boundary.epoch_before, 0);
        assert_eq!(boundary.epoch_after, 1);
        assert_eq!(projection.canonical().len(), 3);
        assert_eq!(projection.visible().len(), 1);
        assert_eq!(projection.visible()[0].call_id, "c");
        assert_eq!(projection.freshness(), Ternary::Unknown);

        projection.begin_turn();
        assert_eq!(projection.canonical().len(), 3);
        assert_eq!(projection.visible().len(), 0);
    }

    #[test]
    fn duplicate_tool_outputs_are_flagged_within_a_turn() {
        let mut projection = ContextProjection::new();
        projection.begin_turn();
        projection.append(0, &outcome("a", 10));
        projection.append(1, &outcome("b", 10));
        projection.append(2, &outcome("c", 20));

        assert_eq!(projection.canonical()[1].duplicate_of.as_deref(), Some("a"));
        assert!(projection.canonical()[2].duplicate_of.is_none());
        assert_eq!(
            projection.canonical()[0].content_fingerprint,
            projection.canonical()[1].content_fingerprint
        );

        // 跨 turn 的相同输出是合法重跑，不标记。
        projection.begin_turn();
        projection.append(3, &outcome("d", 10));
        assert!(projection.canonical()[3].duplicate_of.is_none());
    }

    #[test]
    fn empty_outputs_are_not_treated_as_duplicates() {
        let mut projection = ContextProjection::new();
        projection.begin_turn();
        projection.append(0, &outcome("a", 0));
        projection.append(1, &outcome("b", 0));
        assert!(projection.canonical()[0].content_fingerprint.is_none());
        assert!(projection.canonical()[1].duplicate_of.is_none());
    }

    #[test]
    fn compaction_rejects_retaining_more_than_visible() {
        let mut projection = ContextProjection::new();
        projection.append(0, &outcome("a", 1));
        assert!(matches!(
            projection.compact(CompactionInput {
                step: 0,
                retained: 2,
                ..CompactionInput::default()
            }),
            Err(ProjectionError::RetainedExceedsVisible {
                retained: 2,
                visible: 1
            })
        ));
    }

    #[test]
    fn replay_ignores_truncation_only_legacy_events() {
        let records = vec![
            SessionRecord {
                schema: crate::SESSION_SCHEMA,
                seq: 1,
                event: SessionEvent::TurnStarted {
                    task: "t".into(),
                    objective: "o".into(),
                    max_steps: 2,
                },
            },
            SessionRecord {
                schema: crate::SESSION_SCHEMA,
                seq: 2,
                event: SessionEvent::ToolCompleted {
                    step: 0,
                    outcome: outcome("a", 10),
                },
            },
            SessionRecord {
                schema: crate::SESSION_SCHEMA,
                seq: 3,
                event: SessionEvent::ContextCompacted {
                    step: 0,
                    dropped: 0,
                    truncated: 1,
                    retained: Some(1),
                    dropped_chars: 0,
                    truncated_chars: 5,
                    epoch_before: None,
                    epoch_after: None,
                    freshness: None,
                    digest: None,
                },
            },
        ];
        let projection = replay(&records).unwrap();
        assert_eq!(projection.epoch(), 0);
        assert_eq!(projection.canonical().len(), 1);
        assert_eq!(projection.visible().len(), 1);
        assert!(projection.boundaries().is_empty());
    }
}
