//! planner / executor 分离（B7.6）：只读 planner 工具集、独立 session、独立缓存账本。
//!
//! 三种隔离各自独立可验证：
//!
//! 1. **工具隔离**：`ReadOnlyTools` 把任意 [`ToolExecutor`] 收窄成只读白名单；非白名单
//!    工具或 `read_only == false` 的请求在到达内层之前就被拒绝，不会产生副作用。
//! 2. **会话隔离**：[`SessionPaths`] 给 planner / executor 各自一个 JSONL 文件；
//!    `Session::open` 只从自己的记录重建状态，互不可见。
//! 3. **缓存隔离**：两个 session 各自持有独立 `CacheLedger`（不同 `Arc`），planner 的
//!    缓存前缀不会被 executor 命中，反之亦然——避免“planner 研究上下文”污染 executor
//!    的缓存命中率统计。
//!
//! 装配示例：
//!
//! ```text
//! let paths = SessionPaths::under(session_dir);
//! let planner = Session::open(planner_model, ReadOnlyTools::new(planner_tools), paths.planner)?;
//! let executor = Session::open(executor_model, executor_tools, paths.executor)?;
//! ```
//!
//! planner 只做研究并产出结构化计划；执行由 executor 在自己的 session 里完成。

use std::path::{Path, PathBuf};

use crate::provider::{ToolCatalog, ToolSpec};
use crate::{AgentError, Proposal, ToolExecutor, ToolOutcome, ToolRequest};

/// planner 可用的只读工具白名单。
///
/// `submit_plan` 也在这里：它是 planner 唯一的输出通道，只提交结构化计划，
/// 不读写工作区文件。
pub const READ_ONLY_TOOLS: &[&str] = &["read_file", "grep", "glob", "ls", "submit_plan"];

/// planner 提交结构化计划时调用的工具名。
pub const PLAN_TOOL: &str = "submit_plan";

/// 该工具名是否属于 planner 的只读白名单。
pub fn is_read_only_tool(name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&name)
}

/// 把任意工具执行器收窄成 planner 的只读视图。
///
/// 拒绝发生在转发之前：内层执行器看不到写请求，因此不存在“先执行再撤销”的窗口。
pub struct ReadOnlyTools<T> {
    inner: T,
}

impl<T> ReadOnlyTools<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ToolExecutor> ToolExecutor for ReadOnlyTools<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !request.read_only || !is_read_only_tool(&request.tool) {
            return Err(AgentError::ToolDenied {
                tool: request.tool.clone(),
                reason: "planner is read-only".into(),
            });
        }
        self.inner.execute(request)
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

/// planner 与 executor 的 session 文件对：必须不同，否则状态会互相污染。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    pub planner: PathBuf,
    pub executor: PathBuf,
}

impl SessionPaths {
    /// 在 `dir` 下生成一对固定名字的 session 文件路径。
    pub fn under(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self {
            planner: dir.join("planner.jsonl"),
            executor: dir.join("executor.jsonl"),
        }
    }

    /// 两个路径必须不同（构造器保证，供调用方断言）。
    pub fn are_distinct(&self) -> bool {
        self.planner != self.executor
    }
}

/// 从完整工具目录筛出 planner 可用的只读子集。
///
/// 两个条件同时满足才保留：名字在 [`READ_ONLY_TOOLS`] 白名单里，且
/// `ToolSpec.read_only` 为 true。目录是给"模型能调什么"的清单，不能靠模型自我约束；
/// 过滤后的目录与 [`ReadOnlyTools`] 的执行期拒绝是同一份契约的两端。
pub fn planner_tool_specs(tools: &[ToolSpec]) -> Vec<ToolSpec> {
    tools
        .iter()
        .filter(|tool| tool.read_only && is_read_only_tool(&tool.name))
        .cloned()
        .collect()
}

/// 把任意工具目录收窄成 planner 的只读视图，供 `ProviderModel` 使用。
pub struct ReadOnlyCatalog<C> {
    inner: C,
}

impl<C> ReadOnlyCatalog<C> {
    pub fn new(inner: C) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &C {
        &self.inner
    }

    pub fn into_inner(self) -> C {
        self.inner
    }
}

impl<C: ToolCatalog> ToolCatalog for ReadOnlyCatalog<C> {
    fn tools(&self) -> Vec<ToolSpec> {
        planner_tool_specs(&self.inner.tools())
    }
}

/// 从 planner 的 proposal 里提取并校验计划交接物。
///
/// 协议：planner 必须**恰好调用一次** [`PLAN_TOOL`]，`content` 是
/// [`trit_orch::PlanHandoff`] 的 JSON。解析后由 `PlanHandoff::from_json` 全量
/// 重校验，模型输出不能绕过依赖图、胶囊白名单与执行契约。
pub fn plan_handoff_from_proposal(
    proposal: &Proposal,
    capsule_allowed: &[&str],
    capsule_budget: usize,
) -> Result<trit_orch::PlanHandoff, AgentError> {
    let mut calls = proposal
        .tool_calls
        .iter()
        .filter(|call| call.tool == PLAN_TOOL);
    let call = calls.next().ok_or_else(|| AgentError::PlanProtocol {
        message: format!("planner did not call {PLAN_TOOL}"),
    })?;
    if calls.next().is_some() {
        return Err(AgentError::PlanProtocol {
            message: format!("{PLAN_TOOL} called more than once"),
        });
    }
    let content = call
        .content
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| AgentError::PlanProtocol {
            message: format!("{PLAN_TOOL} content is empty"),
        })?;
    trit_orch::PlanHandoff::from_json(content, capsule_allowed, capsule_budget).map_err(|error| {
        AgentError::PlanProtocol {
            message: format!("{}: {error}", error.kind()),
        }
    })
}
