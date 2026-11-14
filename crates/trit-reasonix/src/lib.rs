//! DeepSeek-Reasonix 的 Rust 宿主适配层。
//!
//! 只做两件事：把宿主 usage / criterion / tool call 映射成 `trit-orch` 的事实，
//! 以及把完成声明裁决成三值 / Belnap 判定。不做 I/O、不调用模型、不读系统时钟。

use serde::{Deserialize, Serialize};
use trit_orch::{
    AdherenceReport, CheckReport, CheckResult, EffectSpec, EvidenceRails, EvidenceSpec,
    EvidenceVerdict, Observation, Phase, Resources, Task, TaskBrief, TaskGuidance, TaskSpec,
    Ternary, TokenUsage, check_adherence,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("completion claim task {claim} does not match brief task {brief}")]
    TaskMismatch { claim: String, brief: String },
}

/// Reasonix 的 usage 事件；cache hit/miss 来自供应商。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    /// 供应商未返回精确用量时为 true。
    #[serde(default)]
    pub estimated: bool,
}

impl HostUsage {
    /// 把宿主 usage 映射为内核口径。
    ///
    /// 有 hit/miss 时以它们为准（prompt 可能是聚合值）；没有时把 prompt 全部当 fresh。
    pub fn into_token_usage(self) -> TokenUsage {
        let (fresh, cached) = if self.cache_hit_tokens == 0 && self.cache_miss_tokens == 0 {
            (self.prompt_tokens, 0)
        } else {
            (self.cache_miss_tokens, self.cache_hit_tokens)
        };
        TokenUsage {
            fresh,
            cached,
            output: self.completion_tokens,
            estimated: self.estimated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CriterionKind {
    Verification {
        command: String,
    },
    Diff {
        paths: Vec<String>,
    },
    /// 人工说明不能自证；只能作为 Inconclusive。
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionClaim {
    pub id: String,
    pub kind: CriterionKind,
    /// 宿主回执是否真的记录了这条 criterion（命令跑过 / 路径被观察）。
    pub backed: bool,
    /// 回执本身是否成功；未背书时忽略。
    pub passed: bool,
}

/// 模型提交的完成请求；`backed/passed` 由宿主回执填写，模型不能自证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub criteria: Vec<CriterionRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionRequest {
    pub id: String,
    pub kind: CriterionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionClaim {
    pub task: String,
    pub criteria: Vec<CriterionClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionJudgment {
    pub verdict: EvidenceVerdict,
    /// `Proved -> True`、`Refuted -> False`、`Neither/Conflicted -> Unknown`。
    pub ternary: Ternary,
    pub rails: EvidenceRails,
    pub adherence: AdherenceReport,
}

impl CompletionJudgment {
    pub fn is_complete(&self) -> bool {
        self.verdict == EvidenceVerdict::Proved
    }

    pub fn is_refuted(&self) -> bool {
        self.verdict == EvidenceVerdict::Refuted
    }

    pub fn is_conflicted(&self) -> bool {
        self.verdict == EvidenceVerdict::Conflicted
    }
}

/// 把完成声明逐条映射为双轨证据；未背书 / manual 只进 inconclusive。
pub fn claim_rails(claim: &CompletionClaim, epoch: u64) -> EvidenceRails {
    let mut rails = EvidenceRails::default();
    for criterion in &claim.criteria {
        let result = match criterion.kind {
            CriterionKind::Manual => CheckResult::Inconclusive,
            _ if !criterion.backed => CheckResult::Inconclusive,
            _ if criterion.passed => CheckResult::Passed,
            _ => CheckResult::Refuted,
        };
        rails.push(
            format!("reasonix:{}", criterion.id),
            CheckReport {
                epoch,
                checks: 1,
                result,
            },
        );
    }
    rails
}

/// 对完成声明做三值 / Belnap 裁决，并顺带跑一次遵循度检查。
pub fn judge_completion(
    brief: &TaskBrief,
    claim: &CompletionClaim,
) -> Result<CompletionJudgment, AdapterError> {
    if claim.task != brief.task {
        return Err(AdapterError::TaskMismatch {
            claim: claim.task.clone(),
            brief: brief.task.clone(),
        });
    }
    let rails = claim_rails(claim, brief.epoch);
    let observation = Observation {
        phase: Phase::Settled,
        epoch: brief.epoch,
        authorization: Ternary::True,
        freshness: Ternary::True,
        evidence: None,
        evidence_rails: rails.clone(),
        usage: TokenUsage::default(),
    };
    let verdict = observation.judgment(brief.epoch);
    let adherence = check_adherence(brief, &observation);
    Ok(CompletionJudgment {
        verdict,
        ternary: verdict.value(),
        rails,
        adherence,
    })
}

/// 一个用户 turn / 子智能体运行的宿主描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSpec {
    pub task: String,
    pub objective: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub stop_when: Vec<String>,
    #[serde(default = "default_min_assertions")]
    pub min_assertions: u32,
    pub max_tokens: u64,
}

fn default_min_assertions() -> u32 {
    1
}

impl TurnSpec {
    /// 生成内核任务定义；效果由后续 tool call 通过 PlanPatch 追加。
    pub fn task_spec(&self) -> TaskSpec {
        TaskSpec {
            task: Task {
                id: self.task.clone(),
                priority: 0,
                duration: 1,
                deps: Vec::new(),
            },
            guidance: TaskGuidance {
                objective: self.objective.clone(),
                constraints: self.constraints.clone(),
                stop_when: self.stop_when.clone(),
            },
            produces: EvidenceSpec {
                min_assertions: self.min_assertions.max(1),
            },
            requirements: Resources::new(1, 1),
            max_tokens: self.max_tokens,
            ..TaskSpec::default()
        }
    }
}

/// 宿主的工具调用；read-only 不产生效果，写工具必须有稳定的 operation ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostToolCall {
    pub task: String,
    pub call_id: String,
    pub tool: String,
    pub read_only: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub operation: Option<String>,
}

impl HostToolCall {
    pub fn effect_spec(&self) -> Option<EffectSpec> {
        if self.read_only {
            return None;
        }
        Some(EffectSpec::new(self.operation.clone().unwrap_or_else(
            || format!("reasonix:{}:{}:{}", self.task, self.tool, self.call_id),
        )))
    }

    pub fn claim_paths(&self) -> &[String] {
        &self.paths
    }
}
