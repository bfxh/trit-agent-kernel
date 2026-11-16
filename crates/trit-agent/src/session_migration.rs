//! 会话迁移：把会话、缓存、上下文版本和钩子一起迁移。
//!
//! 这是纯计划层：不调用模型、不启动进程。缓存只在 provider/model/prefix
//! 完全兼容时复用；上下文按版本合并，旧版本只保留已有摘要，避免额外模型成本；
//! 钩子按契约哈希迁移，不兼容的交给宿主重新注册。所有动作都可由 MigrationPolicy 关闭。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::context_layers::CacheLedger;

pub const SESSION_MIGRATION_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    #[default]
    Primary,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextVersion {
    pub id: String,
    pub version: u64,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub label: String,
    /// 已有的压缩摘要；迁移旧版本时只复用这个，不再调用模型。
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDescriptor {
    pub id: String,
    pub event: String,
    pub contract_hash: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationBundle {
    pub schema: u32,
    pub session_id: String,
    #[serde(default)]
    pub role: SessionRole,
    pub project: String,
    pub provider: String,
    pub model: String,
    pub prefix_fingerprint: String,
    #[serde(default)]
    pub contexts: Vec<ContextVersion>,
    #[serde(default)]
    pub hooks: Vec<HookDescriptor>,
    #[serde(default)]
    pub cache: CacheLedger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationTarget {
    pub provider: String,
    pub model: String,
    pub prefix_fingerprint: String,
    pub contexts: BTreeMap<String, ContextVersion>,
    /// hook id -> 目标端可接受的 contract hash。
    pub hook_contracts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextConflictPolicy {
    /// 同版本不同内容时采用迁入版本，并保留旧摘要。
    PreferIncoming,
    /// 保留目标版本，迁入版本记录为冲突。
    PreferTarget,
    /// 冲突时拒绝该上下文，其他上下文继续迁移。
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPolicy {
    pub enabled: bool,
    pub migrate_cache: bool,
    pub migrate_hooks: bool,
    pub conflict: ContextConflictPolicy,
    pub preserve_superseded: bool,
}

impl Default for MigrationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            migrate_cache: true,
            migrate_hooks: true,
            conflict: ContextConflictPolicy::PreferIncoming,
            preserve_superseded: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheDropReason {
    Disabled,
    ProviderMismatch,
    ModelMismatch,
    PrefixMismatch,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    pub imported: Vec<String>,
    pub replaced: Vec<String>,
    pub kept_target: Vec<String>,
    pub rejected: Vec<String>,
    pub superseded: Vec<String>,
    pub migrated_hooks: Vec<String>,
    pub rejected_hooks: Vec<String>,
    pub cache_dropped: Option<CacheDropReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    pub contexts: BTreeMap<String, ContextVersion>,
    /// 被替换的旧版本；只保留原有 summary，不调用模型生成新摘要。
    pub superseded: Vec<ContextVersion>,
    pub hooks: Vec<HookDescriptor>,
    pub cache: Option<CacheLedger>,
    pub report: MigrationReport,
}

/// 纯函数迁移计划；宿主负责把结果落到目标 session。
pub fn plan_migration(
    source: &SessionMigrationBundle,
    target: &MigrationTarget,
    policy: &MigrationPolicy,
) -> MigrationPlan {
    let mut report = MigrationReport::default();
    let mut contexts = target.contexts.clone();
    let mut superseded = Vec::new();
    if !policy.enabled {
        return MigrationPlan {
            contexts,
            superseded,
            hooks: Vec::new(),
            cache: None,
            report,
        };
    }

    for incoming in &source.contexts {
        match contexts.get(&incoming.id) {
            None => {
                report.imported.push(incoming.id.clone());
                contexts.insert(incoming.id.clone(), incoming.clone());
            }
            Some(existing) if existing.version < incoming.version => {
                report.replaced.push(incoming.id.clone());
                if policy.preserve_superseded && existing.summary.is_some() {
                    superseded.push(existing.clone());
                    report.superseded.push(incoming.id.clone());
                }
                contexts.insert(incoming.id.clone(), incoming.clone());
            }
            Some(existing) if existing.version > incoming.version => {
                report.kept_target.push(incoming.id.clone());
            }
            Some(existing)
                if existing.content_hash.is_some()
                    && existing.content_hash == incoming.content_hash =>
            {
                report.kept_target.push(incoming.id.clone());
            }
            Some(existing) => match policy.conflict {
                ContextConflictPolicy::PreferIncoming => {
                    report.replaced.push(incoming.id.clone());
                    if policy.preserve_superseded && existing.summary.is_some() {
                        superseded.push(existing.clone());
                        report.superseded.push(incoming.id.clone());
                    }
                    contexts.insert(incoming.id.clone(), incoming.clone());
                }
                ContextConflictPolicy::PreferTarget => {
                    report.kept_target.push(incoming.id.clone());
                    report.rejected.push(incoming.id.clone());
                }
                ContextConflictPolicy::Reject => {
                    report.rejected.push(incoming.id.clone());
                }
            },
        }
    }

    let mut hooks = Vec::new();
    if policy.migrate_hooks {
        for hook in &source.hooks {
            if !hook.enabled {
                continue;
            }
            match target.hook_contracts.get(&hook.id) {
                Some(contract) if *contract == hook.contract_hash => {
                    hooks.push(hook.clone());
                    report.migrated_hooks.push(hook.id.clone());
                }
                _ => report.rejected_hooks.push(hook.id.clone()),
            }
        }
    }

    let cache = if !policy.migrate_cache {
        report.cache_dropped = Some(CacheDropReason::Disabled);
        None
    } else if source.provider != target.provider {
        report.cache_dropped = Some(CacheDropReason::ProviderMismatch);
        None
    } else if source.model != target.model {
        report.cache_dropped = Some(CacheDropReason::ModelMismatch);
        None
    } else if source.prefix_fingerprint != target.prefix_fingerprint {
        report.cache_dropped = Some(CacheDropReason::PrefixMismatch);
        None
    } else {
        Some(source.cache.clone())
    };

    MigrationPlan {
        contexts,
        superseded,
        hooks,
        cache,
        report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_layers::CacheObservation;

    fn context(id: &str, version: u64, hash: &str, summary: &str) -> ContextVersion {
        ContextVersion {
            id: id.into(),
            version,
            content_hash: Some(hash.into()),
            label: id.into(),
            summary: Some(summary.into()),
        }
    }

    fn bundle() -> SessionMigrationBundle {
        SessionMigrationBundle {
            schema: SESSION_MIGRATION_SCHEMA,
            session_id: "s1".into(),
            role: SessionRole::Primary,
            project: "p1".into(),
            provider: "provider".into(),
            model: "model".into(),
            prefix_fingerprint: "fnv1a64:abc".into(),
            contexts: vec![context("doc", 2, "new", "new summary")],
            hooks: vec![HookDescriptor {
                id: "pre-tool".into(),
                event: "pre_tool".into(),
                contract_hash: "h1".into(),
                enabled: true,
            }],
            cache: CacheLedger::default(),
        }
    }

    fn target() -> MigrationTarget {
        MigrationTarget {
            provider: "provider".into(),
            model: "model".into(),
            prefix_fingerprint: "fnv1a64:abc".into(),
            contexts: BTreeMap::from([("doc".into(), context("doc", 1, "old", "old summary"))]),
            hook_contracts: BTreeMap::from([("pre-tool".into(), "h1".into())]),
        }
    }

    #[test]
    fn newer_context_replaces_old_and_keeps_compressed_old() {
        let plan = plan_migration(&bundle(), &target(), &MigrationPolicy::default());
        assert_eq!(plan.contexts["doc"].version, 2);
        assert_eq!(plan.superseded.len(), 1);
        assert_eq!(plan.superseded[0].summary.as_deref(), Some("old summary"));
        assert_eq!(plan.report.replaced, ["doc"]);
        assert_eq!(plan.report.migrated_hooks, ["pre-tool"]);
    }

    #[test]
    fn conflict_policy_can_keep_target_or_reject() {
        let mut source = bundle();
        source.contexts = vec![context("doc", 1, "different", "other")];
        let mut policy = MigrationPolicy {
            conflict: ContextConflictPolicy::PreferTarget,
            ..MigrationPolicy::default()
        };
        let plan = plan_migration(&source, &target(), &policy);
        assert_eq!(plan.contexts["doc"].content_hash.as_deref(), Some("old"));
        assert_eq!(plan.report.rejected, ["doc"]);
        policy.conflict = ContextConflictPolicy::Reject;
        let plan = plan_migration(&source, &target(), &policy);
        assert_eq!(plan.contexts["doc"].content_hash.as_deref(), Some("old"));
        assert_eq!(plan.report.rejected, ["doc"]);
    }

    #[test]
    fn cache_requires_provider_model_and_prefix_match() {
        let mut source = bundle();
        source.cache.observe(&CacheObservation {
            provider: "provider".into(),
            model: "model".into(),
            prefix_fingerprint: "fnv1a64:abc".into(),
            prompt_tokens: 10,
            cache_hit_tokens: 8,
            cache_miss_tokens: 2,
            estimated: false,
            observed_at_ms: None,
        });
        let plan = plan_migration(&source, &target(), &MigrationPolicy::default());
        assert!(plan.cache.is_some());
        assert!(plan.report.cache_dropped.is_none());

        let mut other = target();
        other.provider = "other".into();
        let plan = plan_migration(&source, &other, &MigrationPolicy::default());
        assert!(plan.cache.is_none());
        assert_eq!(
            plan.report.cache_dropped,
            Some(CacheDropReason::ProviderMismatch)
        );
    }

    #[test]
    fn policy_switch_disables_migration() {
        let plan = plan_migration(
            &bundle(),
            &target(),
            &MigrationPolicy {
                enabled: false,
                ..MigrationPolicy::default()
            },
        );
        assert_eq!(plan.contexts["doc"].version, 1);
        assert!(plan.hooks.is_empty());
        assert!(plan.cache.is_none());
    }

    #[test]
    fn hook_contract_mismatch_is_rejected() {
        let mut target = target();
        target
            .hook_contracts
            .insert("pre-tool".into(), "different".into());
        let plan = plan_migration(&bundle(), &target, &MigrationPolicy::default());
        assert!(plan.hooks.is_empty());
        assert_eq!(plan.report.rejected_hooks, ["pre-tool"]);
    }
}
