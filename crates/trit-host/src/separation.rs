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

use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

/// planner 可用的只读工具白名单。
pub const READ_ONLY_TOOLS: &[&str] = &["read_file", "grep", "glob", "ls"];

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
