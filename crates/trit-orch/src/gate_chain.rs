//! 门链：仓库身份 → 交接包 → 对齐清单，一条默认只读、stop-first 的链。
//!
//! 三扇门各自回答一个很窄的问题：
//!
//! - `repo-gate`：这是哪个仓，这个仓能不能接智能体任务；
//! - `delivery check`：交接包还是不是发送方钉住的那一份，账本还能不能续；
//! - `alignment`：这份“目标 → 需求 → 产物/证据/变更”清单有没有断链。
//!
//! 分开跑三次容易出两类错误：前一道门没过，后一道门已经跑过；或者把
//! `Unknown` 当成“差不多通过”。门链把它们固定成一条 **默认只读**、
//! **stop-first** 的链：只有 `True` 放行，`Unknown` / `False` 立即停链，
//! 后续门在报告里记为 `skipped`；只有显式 `attach` 才补 `sent + delivered`。
//!
//! 默认不补 `sent/delivered` 账本；显式 `attach` 才补前两态，`read/acted` 仍走
//! `trit-orch delivery`。

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::Ternary;
use crate::alignment::{self, AlignmentOptions};
use crate::delivery;
use crate::repo;

/// 报告 schema。
pub const SCHEMA: &str = "gate-chain/1";
/// 第一道门：仓库身份。
pub const STEP_REPO: &str = "repo";
/// 第二道门：交接包只读预检。
pub const STEP_DELIVERY: &str = "delivery";
/// 第三道门：对齐清单。
pub const STEP_ALIGNMENT: &str = "alignment";

/// `delivery check` 的输入；`at=0` 由 delivery 侧取当前 Unix 秒。
#[derive(Debug, Clone)]
pub struct DeliveryStep {
    pub packet: PathBuf,
    pub receiver_state_hash: String,
    pub at: u64,
    /// 门通过后是否补 `sent + delivered`。默认 `false`：门链保持只读。
    pub attach: bool,
}

/// `delivery` 门的完整结果：只读预检 + 可选落账。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryOutcome {
    pub check: delivery::DeliveryCheck,
    /// 本次是否真的新推进了 `delivered`；已到中间态或未启用时为 `false`。
    pub attached: bool,
}

/// `alignment` 的输入。
#[derive(Debug, Clone)]
pub struct AlignmentStep {
    pub manifest: PathBuf,
    pub options: AlignmentOptions,
}

/// 一次门链运行的输入。
#[derive(Debug, Clone)]
pub struct GateChainOptions {
    pub root: PathBuf,
    pub expected_repo: String,
    pub task_kind: String,
    pub delivery: Option<DeliveryStep>,
    pub alignment: Option<AlignmentStep>,
}

/// 一步的结果。`gate` 是三值裁决，`state` 是它在链上的语义：
/// `passed` / `blocked` / `unknown` / `skipped`。
#[derive(Debug, Clone, Serialize)]
pub struct GateStep {
    pub id: String,
    pub gate: Ternary,
    pub state: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// 各状态的步数，便于 CI 直接读计数而不是翻 `steps`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct GateChainCounts {
    pub passed: usize,
    pub blocked: usize,
    pub unknown: usize,
    pub skipped: usize,
}

/// 一次门链运行的报告。
#[derive(Debug, Clone, Serialize)]
pub struct GateChainReport {
    pub schema: String,
    /// `passed` / `blocked` / `unknown`。
    pub status: String,
    /// `0` 全链通过，`1` 有明确拒绝，`3` 有无法判定的门。
    pub exit: i32,
    pub root: String,
    pub steps: Vec<GateStep>,
    /// 第一个没有放行的门；全通过时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<String>,
    pub counts: GateChainCounts,
}

/// 按固定顺序跑门链；遇到第一个非 `True` 就停，后续门记 `skipped`。
pub fn run(options: &GateChainOptions) -> GateChainReport {
    let mut steps = Vec::with_capacity(3);

    let repo_step = repo_step(options);
    let stopped = repo_step.gate != Ternary::True;
    steps.push(repo_step);
    if stopped {
        steps.push(skipped(STEP_DELIVERY, STEP_REPO));
        steps.push(skipped(STEP_ALIGNMENT, STEP_REPO));
        return finish(options, steps);
    }

    let delivery_step = delivery_step(options);
    let stopped = delivery_step.gate != Ternary::True;
    steps.push(delivery_step);
    if stopped {
        steps.push(skipped(STEP_ALIGNMENT, STEP_DELIVERY));
        return finish(options, steps);
    }

    steps.push(alignment_step(options));
    finish(options, steps)
}

fn repo_step(options: &GateChainOptions) -> GateStep {
    let decision = repo::gate(&options.root, &options.expected_repo, &options.task_kind);
    GateStep {
        id: STEP_REPO.to_owned(),
        gate: decision.gate,
        state: state_of(decision.gate).to_owned(),
        reason: decision.reason.clone(),
        detail: serde_json::to_value(&decision).ok(),
    }
}

fn delivery_step(options: &GateChainOptions) -> GateStep {
    let Some(input) = &options.delivery else {
        return GateStep {
            id: STEP_DELIVERY.to_owned(),
            gate: Ternary::Unknown,
            state: state_of(Ternary::Unknown).to_owned(),
            reason: "delivery packet is not configured".to_owned(),
            detail: None,
        };
    };
    match delivery::check(
        &options.root,
        &input.packet,
        &input.receiver_state_hash,
        input.at,
    ) {
        Ok(check) => {
            // `ready` 已经是“门通过且账本没有坏”；三值仍然保留 `Unknown`
            // 与 `False` 的区别，便于调用方决定先对账还是重新生成包。
            let mut gate = if check.ready {
                Ternary::True
            } else if check.gate == Ternary::Unknown {
                Ternary::Unknown
            } else {
                Ternary::False
            };
            let mut reason = if check.ready {
                format!("handoff gate passed; receipt {}", check.receipt.status)
            } else {
                format!(
                    "handoff gate {}; receipt {} ({})",
                    ternary_name(check.gate),
                    check.receipt.status,
                    check.receipt.reason
                )
            };
            let mut attached = false;
            if gate == Ternary::True && input.attach {
                match delivery::attach(&options.root, &input.packet, input.at) {
                    Ok((_, wrote)) => {
                        attached = wrote;
                        if wrote {
                            reason.push_str("; attached sent+delivered");
                        }
                    }
                    Err(error) => {
                        // 门本身通过了，但落账失败仍然是 Unknown：不能对外
                        // 宣称账本已经推进。
                        gate = Ternary::Unknown;
                        reason = format!("delivery attach failed: {error}");
                    }
                }
            }
            let outcome = DeliveryOutcome { check, attached };
            GateStep {
                id: STEP_DELIVERY.to_owned(),
                gate,
                state: state_of(gate).to_owned(),
                reason,
                detail: serde_json::to_value(&outcome).ok(),
            }
        }
        Err(error) => GateStep {
            id: STEP_DELIVERY.to_owned(),
            gate: Ternary::Unknown,
            state: state_of(Ternary::Unknown).to_owned(),
            reason: format!("delivery check failed: {error}"),
            detail: None,
        },
    }
}

fn alignment_step(options: &GateChainOptions) -> GateStep {
    let Some(input) = &options.alignment else {
        return GateStep {
            id: STEP_ALIGNMENT.to_owned(),
            gate: Ternary::Unknown,
            state: state_of(Ternary::Unknown).to_owned(),
            reason: "alignment manifest is not configured".to_owned(),
            detail: None,
        };
    };
    let report = alignment::check(&options.root, &input.manifest, &input.options);
    let gate = match report.status.as_str() {
        "aligned" => Ternary::True,
        "unaligned" => Ternary::False,
        _ => Ternary::Unknown,
    };
    let reason = match report.failures.first() {
        Some(failure) => format!("{}: {}", failure.code, failure.message),
        None => format!(
            "alignment {} (must score {:.2}, {} failure(s))",
            report.status,
            report.must_score,
            report.failures.len()
        ),
    };
    GateStep {
        id: STEP_ALIGNMENT.to_owned(),
        gate,
        state: state_of(gate).to_owned(),
        reason,
        detail: serde_json::to_value(&report).ok(),
    }
}

fn skipped(id: &str, after: &str) -> GateStep {
    GateStep {
        id: id.to_owned(),
        gate: Ternary::Unknown,
        state: "skipped".to_owned(),
        reason: format!("not run: {after} gate did not pass"),
        detail: None,
    }
}

fn finish(options: &GateChainOptions, steps: Vec<GateStep>) -> GateChainReport {
    let mut counts = GateChainCounts::default();
    for step in &steps {
        match step.state.as_str() {
            "passed" => counts.passed += 1,
            "blocked" => counts.blocked += 1,
            "unknown" => counts.unknown += 1,
            _ => counts.skipped += 1,
        }
    }
    let failed_at = steps
        .iter()
        .find(|step| step.state == "blocked" || step.state == "unknown")
        .map(|step| step.id.clone());
    let (status, exit) = if counts.blocked > 0 {
        ("blocked", 1)
    } else if counts.unknown > 0 {
        ("unknown", 3)
    } else {
        ("passed", 0)
    };
    GateChainReport {
        schema: SCHEMA.to_owned(),
        status: status.to_owned(),
        exit,
        root: options.root.to_string_lossy().to_string(),
        steps,
        failed_at,
        counts,
    }
}

fn state_of(gate: Ternary) -> &'static str {
    match gate {
        Ternary::True => "passed",
        Ternary::Unknown => "unknown",
        Ternary::False => "blocked",
    }
}

fn ternary_name(gate: Ternary) -> &'static str {
    match gate {
        Ternary::True => "True",
        Ternary::Unknown => "Unknown",
        Ternary::False => "False",
    }
}
