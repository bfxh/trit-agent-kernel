//! planner 与真实模型的接线（B7.6）：拦截 `submit_plan`、收集计划、交给
//! [`PlanHandoff`] 全量校验。
//!
//! planner 的模型（`ProviderModel`）通过工具目录看到 `submit_plan`；模型调用它时，
//! [`PlanCollectingTools`] 在本地接收内容并返回成功回执，**不会**把计划当成文件
//! 操作交给工作区执行器。宿主跑完一轮后用 [`plan_from_capture`] 取出并重校验。
//!
//! 组合方式：
//!
//! ```text
//! let (tools, captured) = planner_tools(ReadOnlyTools::new(LocalTools::new(ws)?));
//! let catalog = Arc::new(ReadOnlyCatalog::new(StaticCatalog(planner_catalog_specs())));
//! let model = ProviderModel::new(provider).with_tool_catalog(catalog);
//! let mut session = Session::open(model, tools, paths.planner)?;
//! let _ = session.run_turn(&turn);   // 计划已捕获；轮次可能以 ModelExhausted 结束
//! let handoff = plan_from_capture(&captured, &["goal"], 4096)?;
//! ```
//!
//! 「轮次以 `ModelExhausted` 结束」是预期语义：planner 在 `submit_plan` 之后不再
//! 产出提议，宿主不应该把“没有更多提议”当成失败——计划是否可执行由
//! [`plan_from_capture`] 的校验结果决定。

use std::sync::{Arc, Mutex};

use trit_orch::PlanHandoff;

use crate::provider::{ToolSpec, default_tools};
use crate::separation::{PLAN_TOOL, planner_tool_specs};
use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

/// 拦截 `submit_plan` 的工具包装器。
pub struct PlanCollectingTools<T> {
    inner: T,
    captured: Arc<Mutex<Option<String>>>,
}

impl<T> PlanCollectingTools<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            captured: Arc::new(Mutex::new(None)),
        }
    }

    /// 计划收集槽；可克隆给宿主在 session 外持有。
    pub fn captured(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.captured)
    }

    /// 取走已捕获的计划（只取一次）。
    pub fn take(&self) -> Option<String> {
        self.captured.lock().ok().and_then(|mut slot| slot.take())
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ToolExecutor> ToolExecutor for PlanCollectingTools<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if request.tool == PLAN_TOOL {
            if let Ok(mut slot) = self.captured.lock() {
                *slot = Some(request.content.clone().unwrap_or_default());
            }
            return Ok(ToolOutcome {
                call_id: request.call_id.clone(),
                tool: request.tool.clone(),
                passed: true,
                assertions: 1,
                ..ToolOutcome::default()
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

/// 装配 planner 工具：外层负责收集计划，内层只读执行器负责研究。
pub fn planner_tools<T>(inner: T) -> (PlanCollectingTools<T>, Arc<Mutex<Option<String>>>) {
    let tools = PlanCollectingTools::new(inner);
    let captured = tools.captured();
    (tools, captured)
}

/// planner 的工具目录：默认工具 + `submit_plan`，再按只读白名单过滤。
pub fn planner_catalog_specs() -> Vec<ToolSpec> {
    let mut tools = default_tools();
    tools.push(ToolSpec {
        name: PLAN_TOOL.into(),
        description: "Submit the structured plan as a PlanHandoff JSON payload.".into(),
        read_only: true,
        input_schema: None,
    });
    planner_tool_specs(&tools)
}

/// 取出捕获的计划并全量校验（胶囊白名单 + 依赖图 + 执行契约）。
pub fn plan_from_capture(
    captured: &Arc<Mutex<Option<String>>>,
    capsule_allowed: &[&str],
    capsule_budget: usize,
) -> Result<PlanHandoff, AgentError> {
    let content = captured
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or_else(|| AgentError::PlanProtocol {
            message: format!("planner did not call {PLAN_TOOL}"),
        })?;
    PlanHandoff::from_json(&content, capsule_allowed, capsule_budget).map_err(|error| {
        AgentError::PlanProtocol {
            message: format!("{}: {error}", error.kind()),
        }
    })
}
