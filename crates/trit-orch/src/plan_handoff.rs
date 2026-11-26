//! 计划交接（B7.6）：planner 产出给 executor 的结构化计划。
//!
//! 交接物同时携带四样东西，缺一不可：
//!
//! 1. **计划**：`revision + tasks`，构造时用 [`Plan::new`] 校验依赖图（未知依赖、环、
//!    重复 ID 全部 fail-closed），存的是排序后的规范形式；
//! 2. **上下文胶囊**：[`ContextCapsule`]，只有显式允许的键；
//! 3. **执行契约**：[`SubagentProfile`]，executor 的写权限/工具/预算；
//! 4. **稳定引用**：`plan@revision#digest`，同一交接物在不同进程得到同一引用。
//!
//! planner 侧用只读工具与只读目录（`trit-host` 的 `ReadOnlyTools` /
//! `ReadOnlyCatalog`）研究并产出本结构；executor 侧 `into_plan()` 后交给
//! `fleet::execute` 执行。本模块只做契约与校验，不发请求、不写文件。

use serde::{Deserialize, Serialize};

use crate::{ContextCapsule, Error, Plan, SubagentProfile, TaskSpec, digest_hex};

/// planner 交给 executor 的完整交接物。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHandoff {
    /// 计划版本号。
    pub revision: u64,
    /// 排序后的任务定义；构造时已通过依赖图校验。
    pub tasks: Vec<TaskSpec>,
    /// 显式继承的上下文胶囊。
    #[serde(default)]
    pub capsule: ContextCapsule,
    /// executor 的权限契约。
    pub profile: SubagentProfile,
}

impl PlanHandoff {
    /// 构造并校验交接物。
    ///
    /// 计划先过 [`Plan::new`]（依赖图、效果冲突、任务数上限），再用
    /// [`SubagentProfile::validate`] 校验执行契约；任一步失败都不产生交接物。
    /// 通过后存排序后的规范形式，保证同一组任务的 digest 与输入顺序无关。
    pub fn new(
        revision: u64,
        tasks: Vec<TaskSpec>,
        capsule: ContextCapsule,
        profile: SubagentProfile,
    ) -> Result<Self, Error> {
        let plan = Plan::new(revision, tasks)?;
        profile.validate()?;
        Ok(Self {
            revision: plan.revision(),
            tasks: plan.tasks().to_vec(),
            capsule,
            profile,
        })
    }

    /// 还原成可执行/可选择的计划。
    pub fn into_plan(self) -> Result<Plan, Error> {
        Plan::new(self.revision, self.tasks)
    }

    /// 从 JSON 解析并**全量重校验**：结构 → 胶囊白名单 → 计划依赖图 → 执行契约。
    ///
    /// 不能信任输入：手写 JSON 可以直接构造出绕过构造器校验的字段，所以这里把字段
    /// 拆开重跑 [`ContextCapsule::inherit`] 与 [`PlanHandoff::new`]。胶囊只接受
    /// `capsule_allowed` 列出的键，超出预算同样拒绝。
    pub fn from_json(
        text: &str,
        capsule_allowed: &[&str],
        capsule_budget: usize,
    ) -> Result<Self, Error> {
        let raw: Self = serde_json::from_str(text).map_err(|error| Error::PlanHandoffSchema {
            message: error.to_string(),
        })?;
        let capsule = ContextCapsule::inherit(
            raw.capsule.entries().to_vec(),
            capsule_allowed,
            capsule_budget,
        )?;
        Self::new(raw.revision, raw.tasks, capsule, raw.profile)
    }

    /// 交接物内容的稳定摘要（16 位十六进制）。
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        digest_hex(&bytes)
    }

    /// 稳定引用 `plan@revision#digest`。
    pub fn reference(&self) -> String {
        format!("plan@{}#{}", self.revision, self.digest())
    }
}
