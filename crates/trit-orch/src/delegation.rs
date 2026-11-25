//! 委派契约（B7.6）：子智能体 profile、上下文胶囊、结果引用与委派收益门槛。
//!
//! 三条不变量，全部由本模块的纯函数保证，可离线机器验证：
//!
//! 1. **权限单调**：子 profile 的写权限、工具白名单、token 预算都是父 profile 的
//!    子集；只读 profile 不得声明写根，任何一次 `permits_write` 都会拒绝越权路径。
//! 2. **显式继承**：上下文胶囊只携带调用方显式列出的键。白名单外的键是错误而不是
//!    静默丢弃——静默丢弃会把“子 agent 隐式继承父上下文”藏起来。
//! 3. **结果可引用**：委派结果带稳定引用 `task@epoch#digest`，失败/中止/延迟三值
//!    可辨，只有 `Completed` 可被复用（失败结果不得被当成完成证据）。
//!
//! 本模块不做调度、不发请求：并行执行由 `trit-schedule` 的 runtime 负责，
//! 这里只定义“能不能委派、能带走什么、结果怎么引用”。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::Error;

/// 子智能体的权限模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    /// 只读：不得声明写根，`permits_write` 恒为 false。
    ReadOnly,
    /// 可写：只能写 `write_roots` 列出的 workspace 相对子树。
    ReadWrite,
}

/// 一个子智能体的显式契约。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentProfile {
    /// 子智能体名字；同一父会话内唯一。
    pub name: String,
    /// 权限模式。
    pub access: AccessMode,
    /// 允许写入的 workspace 相对子树；只读 profile 必须为空。
    #[serde(default)]
    pub write_roots: Vec<String>,
    /// 允许调用的工具名白名单；必须非空且不重复。
    pub tools: Vec<String>,
    /// 该子智能体的 token 预算。
    pub max_tokens: u64,
}

impl SubagentProfile {
    /// 校验 profile 自身合法：字段非空、写根是规范相对子树、工具不重复。
    pub fn validate(&self) -> Result<(), Error> {
        if self.name.trim().is_empty() {
            return Err(Error::DelegateBlankField {
                profile: self.name.clone(),
                field: "name".into(),
            });
        }
        if self.tools.is_empty() {
            return Err(Error::DelegateBlankField {
                profile: self.name.clone(),
                field: "tools".into(),
            });
        }
        if self.max_tokens == 0 {
            return Err(Error::DelegateBlankField {
                profile: self.name.clone(),
                field: "max_tokens".into(),
            });
        }
        let mut seen = BTreeSet::new();
        for tool in &self.tools {
            if tool.trim().is_empty() {
                return Err(Error::DelegateBlankField {
                    profile: self.name.clone(),
                    field: "tool".into(),
                });
            }
            if !seen.insert(tool.as_str()) {
                return Err(Error::DelegateDuplicateTool {
                    profile: self.name.clone(),
                    tool: tool.clone(),
                });
            }
        }
        if self.access == AccessMode::ReadOnly && !self.write_roots.is_empty() {
            return Err(Error::DelegateReadOnlyWrites {
                profile: self.name.clone(),
            });
        }
        for root in &self.write_roots {
            normalize_relative(root).ok_or_else(|| Error::DelegateWriteRoot {
                profile: self.name.clone(),
                root: root.clone(),
            })?;
        }
        Ok(())
    }

    /// 该 profile 是否允许写 `path`（workspace 相对路径）。
    ///
    /// 只读恒 false；路径必须规范化成功且落在某个写根子树内。
    pub fn permits_write(&self, path: &str) -> bool {
        if self.access != AccessMode::ReadWrite {
            return false;
        }
        let Some(path) = normalize_relative(path) else {
            return false;
        };
        self.write_roots.iter().any(|root| {
            normalize_relative(root)
                .is_some_and(|root| path == root || path.starts_with(&format!("{root}/")))
        })
    }

    /// 子 profile 的权限必须是父 profile 的子集：写权限、工具、预算都不能放大。
    ///
    /// 这是“子 agent 无隐式继承”的反向约束：子只能比父更窄。
    pub fn is_subset_of(&self, parent: &SubagentProfile) -> Result<(), Error> {
        let escalate = |field: &str| Error::DelegatePrivilegeEscalation {
            profile: self.name.clone(),
            parent: parent.name.clone(),
            field: field.into(),
        };
        if self.access == AccessMode::ReadWrite && parent.access == AccessMode::ReadOnly {
            return Err(escalate("access"));
        }
        let parent_roots: Vec<String> = parent
            .write_roots
            .iter()
            .filter_map(|root| normalize_relative(root))
            .collect();
        for root in &self.write_roots {
            let Some(root) = normalize_relative(root) else {
                return Err(escalate("write_roots"));
            };
            if !parent_roots.iter().any(|parent_root| {
                root == *parent_root || root.starts_with(&format!("{parent_root}/"))
            }) {
                return Err(escalate("write_roots"));
            }
        }
        let parent_tools: BTreeSet<&str> = parent.tools.iter().map(String::as_str).collect();
        if !self
            .tools
            .iter()
            .all(|tool| parent_tools.contains(tool.as_str()))
        {
            return Err(escalate("tools"));
        }
        if self.max_tokens > parent.max_tokens {
            return Err(escalate("max_tokens"));
        }
        Ok(())
    }
}

/// 胶囊里的一条显式上下文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleEntry {
    pub key: String,
    pub value: String,
}

/// 交给子智能体的上下文胶囊：只含有显式允许的键。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCapsule {
    entries: Vec<CapsuleEntry>,
}

impl ContextCapsule {
    /// 按白名单过滤上下文。
    ///
    /// - 白名单外的键 → `DelegateCapsuleKey`（fail-closed，不静默丢弃）；
    /// - 重复键 → `DelegateCapsuleDuplicate`；
    /// - 总量超过 `budget_bytes` → `DelegateCapsuleTooLarge`（不截断）；
    /// - 通过后按键排序，保证同一输入得到同一胶囊。
    pub fn inherit(
        entries: Vec<CapsuleEntry>,
        allowed: &[&str],
        budget_bytes: usize,
    ) -> Result<Self, Error> {
        let allowed: BTreeSet<&str> = allowed.iter().copied().collect();
        let mut seen = BTreeSet::new();
        let mut kept = Vec::with_capacity(entries.len());
        let mut bytes = 0usize;
        for entry in entries {
            if !allowed.contains(entry.key.as_str()) {
                return Err(Error::DelegateCapsuleKey { key: entry.key });
            }
            if !seen.insert(entry.key.clone()) {
                return Err(Error::DelegateCapsuleDuplicate { key: entry.key });
            }
            bytes = bytes.saturating_add(entry.key.len() + entry.value.len());
            if bytes > budget_bytes {
                return Err(Error::DelegateCapsuleTooLarge {
                    bytes,
                    max: budget_bytes,
                });
            }
            kept.push(entry);
        }
        kept.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(Self { entries: kept })
    }

    /// 胶囊占用的字节数（键 + 值）。
    pub fn len_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.key.len() + entry.value.len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    /// 按键升序的键列表。
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.key.as_str())
    }
}

/// 委派结果状态。三值语义：完成 / 失败（含中止）/ 延迟。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum DelegationStatus {
    /// 完成：只有这个状态可被复用。
    Completed,
    /// 失败：带原因，不得计入完成证据。
    Failed { message: String },
    /// 被取消/中止。
    Aborted,
    /// 依赖未知或预算不足被延迟。
    Deferred,
}

/// 一次子任务的结果引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationResult {
    /// 子任务 ID。
    pub task: String,
    /// 该任务定义的 epoch；epoch 变化后旧结果自动失效。
    pub epoch: u64,
    /// 结果内容的稳定摘要（见 [`digest_hex`]）。
    pub digest: String,
    pub status: DelegationStatus,
    /// 实际消耗的 token。
    pub tokens: u64,
}

impl DelegationResult {
    /// 稳定引用 `task@epoch#digest`：宿主与父会话用它指代这次子结果。
    pub fn reference(&self) -> String {
        format!("{}@{}#{}", self.task, self.epoch, self.digest)
    }

    /// 只有完成的结果可作为下游输入。
    pub fn reusable(&self) -> bool {
        matches!(self.status, DelegationStatus::Completed)
    }
}

/// FNV-1a 64 位摘要，输出 16 位小写十六进制。
///
/// 用途是**稳定引用**而不是密码学校验：同一字节序列在不同进程、不同版本得到同一
/// 结果即可。需要抗碰撞的场合由调用方换成 SHA-256。
pub fn digest_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 委派收益的输入指标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationInput {
    /// 可并行的子任务数。
    pub parallel_tasks: usize,
    /// 不委派时父会话需要的串行轮数。
    pub serial_steps: u32,
    /// 每个子任务的预估轮数。
    pub steps_per_task: u32,
    /// 委派的固定协调开销（父会话里汇总/裁决的轮数）。
    pub coordination_steps: u32,
    /// 胶囊实际字节数。
    pub capsule_bytes: usize,
    /// 胶囊预算字节数。
    pub capsule_budget_bytes: usize,
}

/// 是否值得委派，以及可复算的理由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationDecision {
    pub delegate: bool,
    pub reason: String,
}

/// 用指标判断是否值得委派，不凭感觉并行。
///
/// 判据（全部可复算）：
/// 1. 少于两个并行子任务 → 不委派；
/// 2. 胶囊超过预算 → 不委派（先裁上下文，不是先并行）；
/// 3. 并行路径的轮数（`steps_per_task + coordination_steps`）不短于串行轮数 → 不委派。
pub fn should_delegate(input: &DelegationInput) -> DelegationDecision {
    if input.parallel_tasks < 2 {
        return DelegationDecision {
            delegate: false,
            reason: "fewer than two parallel tasks".into(),
        };
    }
    if input.capsule_bytes > input.capsule_budget_bytes {
        return DelegationDecision {
            delegate: false,
            reason: "context capsule over budget".into(),
        };
    }
    let parallel_steps = input
        .steps_per_task
        .saturating_add(input.coordination_steps);
    if parallel_steps >= input.serial_steps {
        return DelegationDecision {
            delegate: false,
            reason: "parallel path is not shorter than serial".into(),
        };
    }
    DelegationDecision {
        delegate: true,
        reason: format!("saves {} steps", input.serial_steps - parallel_steps),
    }
}

/// 把 workspace 相对路径规范化成 `a/b/c` 形式；绝对路径、盘符、`..`、`.`、
/// 反斜杠与空段一律返回 `None`。
fn normalize_relative(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.contains('\\') {
        return None;
    }
    if trimmed.contains(':') {
        return None;
    }
    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        segments.push(segment);
    }
    Some(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, access: AccessMode, roots: &[&str], tools: &[&str]) -> SubagentProfile {
        SubagentProfile {
            name: name.into(),
            access,
            write_roots: roots.iter().map(|root| (*root).to_owned()).collect(),
            tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
            max_tokens: 1000,
        }
    }

    #[test]
    fn read_only_profile_rejects_write_roots() {
        let read_only = profile("scout", AccessMode::ReadOnly, &["src"], &["read_file"]);
        let error = read_only.validate().unwrap_err();
        assert_eq!(error.kind(), "delegate_read_only_writes");
    }

    #[test]
    fn write_roots_must_be_normalized_relative_subtrees() {
        for root in [
            "", "/abs", "../up", "a/../b", "a//b", r"a\b", "C:/win", "./a",
        ] {
            let bad = profile("writer", AccessMode::ReadWrite, &[root], &["write_file"]);
            let error = bad.validate().unwrap_err();
            assert_eq!(error.kind(), "delegate_write_root", "root {root:?}");
        }
        let good = profile(
            "writer",
            AccessMode::ReadWrite,
            &["src/lib"],
            &["write_file"],
        );
        assert!(good.validate().is_ok());
    }

    #[test]
    fn permits_write_is_subtree_scoped_and_rejects_escapes() {
        let writer = profile("writer", AccessMode::ReadWrite, &["src"], &["write_file"]);
        assert!(writer.permits_write("src"));
        assert!(writer.permits_write("src/lib.rs"));
        assert!(writer.permits_write("src/a/b.rs"));
        assert!(!writer.permits_write("srclib.rs"));
        assert!(!writer.permits_write("docs/readme.md"));
        assert!(!writer.permits_write("/etc/passwd"));
        assert!(!writer.permits_write("src/../../etc"));
        assert!(!writer.permits_write(r"src\lib.rs"));

        let reader = profile("reader", AccessMode::ReadOnly, &[], &["read_file"]);
        assert!(!reader.permits_write("src/lib.rs"));
    }

    #[test]
    fn child_profile_cannot_exceed_parent() {
        let parent = profile(
            "parent",
            AccessMode::ReadWrite,
            &["src", "docs"],
            &["read_file", "write_file"],
        );

        let narrower = profile("child", AccessMode::ReadWrite, &["src"], &["read_file"]);
        assert!(narrower.is_subset_of(&parent).is_ok());

        let wider_root = profile(
            "child",
            AccessMode::ReadWrite,
            &["src", "tests"],
            &["read_file"],
        );
        assert_eq!(
            wider_root.is_subset_of(&parent).unwrap_err().kind(),
            "delegate_privilege_escalation"
        );

        let wider_tools = profile("child", AccessMode::ReadWrite, &["src"], &["shell"]);
        assert_eq!(
            wider_tools.is_subset_of(&parent).unwrap_err().kind(),
            "delegate_privilege_escalation"
        );

        let mut richer = profile("child", AccessMode::ReadWrite, &["src"], &["read_file"]);
        richer.max_tokens = parent.max_tokens + 1;
        assert_eq!(
            richer.is_subset_of(&parent).unwrap_err().kind(),
            "delegate_privilege_escalation"
        );

        let reader_parent = profile("parent", AccessMode::ReadOnly, &[], &["read_file"]);
        let writer_child = profile("child", AccessMode::ReadWrite, &["src"], &["read_file"]);
        assert_eq!(
            writer_child
                .is_subset_of(&reader_parent)
                .unwrap_err()
                .kind(),
            "delegate_privilege_escalation"
        );
    }

    #[test]
    fn capsule_rejects_keys_outside_the_allow_list() {
        let entries = vec![CapsuleEntry {
            key: "secret".into(),
            value: "x".into(),
        }];
        let error = ContextCapsule::inherit(entries, &["goal"], 128).unwrap_err();
        assert_eq!(error.kind(), "delegate_capsule_key");
    }

    #[test]
    fn capsule_rejects_duplicates_and_oversize_without_truncating() {
        let entries = vec![
            CapsuleEntry {
                key: "goal".into(),
                value: "a".into(),
            },
            CapsuleEntry {
                key: "goal".into(),
                value: "b".into(),
            },
        ];
        assert_eq!(
            ContextCapsule::inherit(entries, &["goal"], 128)
                .unwrap_err()
                .kind(),
            "delegate_capsule_duplicate"
        );

        let big = vec![CapsuleEntry {
            key: "goal".into(),
            value: "x".repeat(64),
        }];
        assert_eq!(
            ContextCapsule::inherit(big, &["goal"], 8)
                .unwrap_err()
                .kind(),
            "delegate_capsule_too_large"
        );
    }

    #[test]
    fn capsule_keeps_explicit_keys_sorted_and_counts_bytes() {
        let entries = vec![
            CapsuleEntry {
                key: "goal".into(),
                value: "ship b7.6".into(),
            },
            CapsuleEntry {
                key: "facts".into(),
                value: "runtime exists".into(),
            },
        ];
        let capsule = ContextCapsule::inherit(entries, &["facts", "goal"], 256).unwrap();
        assert_eq!(capsule.keys().collect::<Vec<_>>(), vec!["facts", "goal"]);
        assert_eq!(capsule.get("goal"), Some("ship b7.6"));
        assert_eq!(capsule.len_bytes(), 4 + 9 + 5 + 14);
        assert!(!capsule.is_empty());
    }

    #[test]
    fn delegation_reference_is_stable_and_only_completed_is_reusable() {
        let digest = digest_hex(b"hello");
        assert_eq!(digest.len(), 16);
        assert_eq!(digest, digest_hex(b"hello"));
        assert_ne!(digest, digest_hex(b"hellO"));

        let done = DelegationResult {
            task: "build".into(),
            epoch: 3,
            digest: digest.clone(),
            status: DelegationStatus::Completed,
            tokens: 42,
        };
        assert_eq!(done.reference(), format!("build@3#{digest}"));
        assert!(done.reusable());

        for status in [
            DelegationStatus::Failed {
                message: "boom".into(),
            },
            DelegationStatus::Aborted,
            DelegationStatus::Deferred,
        ] {
            let result = DelegationResult {
                status,
                ..done.clone()
            };
            assert!(!result.reusable());
        }
    }

    #[test]
    fn delegation_decision_uses_recomputable_metrics() {
        let base = DelegationInput {
            parallel_tasks: 3,
            serial_steps: 12,
            steps_per_task: 2,
            coordination_steps: 1,
            capsule_bytes: 64,
            capsule_budget_bytes: 128,
        };
        let decision = should_delegate(&base);
        assert!(decision.delegate);
        assert_eq!(decision.reason, "saves 9 steps");

        let single = DelegationInput {
            parallel_tasks: 1,
            ..base
        };
        assert!(!should_delegate(&single).delegate);
        assert_eq!(
            should_delegate(&single).reason,
            "fewer than two parallel tasks"
        );

        let oversize = DelegationInput {
            capsule_bytes: 129,
            ..base
        };
        assert!(!should_delegate(&oversize).delegate);
        assert_eq!(
            should_delegate(&oversize).reason,
            "context capsule over budget"
        );

        let no_gain = DelegationInput {
            serial_steps: 3,
            ..base
        };
        assert!(!should_delegate(&no_gain).delegate);
        assert_eq!(
            should_delegate(&no_gain).reason,
            "parallel path is not shorter than serial"
        );
    }
}
