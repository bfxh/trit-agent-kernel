//! planner → executor 两阶段编排（B7.6 第三刀）。
//!
//! planner 在只读工具上产出一份 [`PlanHandoff`]，executor 在**独立的
//! [`AgentHost`] 与独立 [`CacheLedger`]** 中执行。两阶段不共享任何宿主状态：
//! planner 的前缀缓存、回执与上下文投影都不会进入执行侧。
//!
//! 调用方仍负责把 `plan.profile.write_roots` 接成 executor 的 `WriteClaim`
//! （见 `SubagentProfile` 与 `ToolWhitelist`），本函数只保证编排与隔离。

use std::sync::{Arc, Mutex};

use trit_orch::PlanHandoff;

use crate::context_layers::CacheLedger;
use crate::{AgentError, AgentHost, Model, ToolExecutor, TurnOutcome, TurnSpec};

/// 两阶段的预算与胶囊白名单。
#[derive(Debug, Clone)]
pub struct PlanExecuteConfig {
    pub plan_max_steps: usize,
    pub exec_max_steps: usize,
    /// 允许从计划胶囊里带出的键。
    pub capsule_allowed: Vec<String>,
    pub capsule_budget: usize,
}

impl Default for PlanExecuteConfig {
    fn default() -> Self {
        Self {
            plan_max_steps: 8,
            exec_max_steps: 8,
            capsule_allowed: vec!["goal".to_owned()],
            capsule_budget: 4096,
        }
    }
}

pub struct PlanExecuteOutcome {
    pub plan: PlanHandoff,
    pub execution: TurnOutcome,
}

/// 跑 planner 一轮，拿到计划，再在新宿主里跑 executor 一轮。
///
/// planner 在 `submit_plan` 之后通常会以 [`AgentError::ModelExhausted`] 结束，
/// 这是预期语义，不算失败；计划是否成立由 `plan_from_capture` 的校验决定。
pub fn run_plan_then_execute<PM, PT, EM, ET>(
    planner_model: PM,
    planner_tools_inner: PT,
    executor_model: EM,
    executor_tools: ET,
    turn: &TurnSpec,
    config: &PlanExecuteConfig,
) -> Result<PlanExecuteOutcome, AgentError>
where
    PM: Model,
    PT: ToolExecutor,
    EM: Model,
    ET: ToolExecutor,
{
    let (tools, captured) = crate::planner::planner_tools(planner_tools_inner);
    let mut planner = AgentHost::new(planner_model, tools)
        .with_max_steps(config.plan_max_steps)
        .with_cache_ledger(Arc::new(Mutex::new(CacheLedger::default())));
    match planner.run_turn(turn) {
        Ok(_) => {}
        Err(AgentError::ModelExhausted) => {}
        Err(error) => return Err(error),
    }

    let allowed: Vec<&str> = config.capsule_allowed.iter().map(String::as_str).collect();
    let plan = crate::planner::plan_from_capture(&captured, &allowed, config.capsule_budget)?;
    let plan_json =
        serde_json::to_string_pretty(&plan).map_err(|error| AgentError::PlanProtocol {
            message: format!("plan encode: {error}"),
        })?;

    let exec_turn = TurnSpec {
        task: turn.task.clone(),
        objective: format!(
            "{}\n\n## 批准的计划（PlanHandoff）\n```json\n{plan_json}\n```",
            turn.objective
        ),
        constraints: turn.constraints.clone(),
        stop_when: turn.stop_when.clone(),
        min_assertions: turn.min_assertions,
        max_tokens: turn.max_tokens,
    };
    let mut executor = AgentHost::new(executor_model, executor_tools)
        .with_max_steps(config.exec_max_steps)
        .with_cache_ledger(Arc::new(Mutex::new(CacheLedger::default())));
    let execution = executor.run_turn(&exec_turn)?;

    Ok(PlanExecuteOutcome { plan, execution })
}
