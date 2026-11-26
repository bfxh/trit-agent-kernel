//! `trit-orch` 的宿主桥：stdin 收一份 JSON，stdout 回一份 JSON。
//!
//! 子命令：`plan` / `select` / `dispatch` / `finish` / `recover` / `shadow` / `compact-gate`
//! 以及协调面 `claim` / `renew` / `release` / `claim-gate` / `receipt` / `handoff-gate` /
//! `admit-with-claim`，交接账本 `delivery`，仓库身份门 `repo-gate`，对齐门 `alignment`，
//! 对齐骨架 `alignment-scaffold`，三门串联 `gate-chain`，fleet 演练 `fleet-run`。
//! 成功退出码 0，普通失败退出码 1；`alignment` 与 `gate-chain` 另有 1=unaligned/blocked、
//! 3=unknown，`alignment-scaffold` 输入/写入拒绝退出 2，并输出
//! `{"ok":false,"kind":"...","error":"..."}`；成功输出永远带 `"ok":true`。

mod cli_delivery;
mod cli_gate_chain;

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use trit_orch::{
    ClaimStore, ContextFacts, HandoffPacket, Ledger, Limits, ObligationBudget, ObligationPolicy,
    Plan, PlanPatch, Progress, ReceiptLedger, ReceiptState, RoundReport, Selection, Snapshot,
    TaskSpec, Ternary, TraceStep, TraceStore, admit_with_claim, builtin, compaction_gate,
    dispatch_ready, handoff_gate, handoff_obligation, receipt_obligation, replay,
};

#[derive(Deserialize)]
struct PlanInput {
    revision: u64,
    tasks: Vec<TaskSpec>,
}

/// `fleet-run` 的输入：确定性演练，不执行外部命令。
///
/// `fail` 指定必然失败的任务；`cancel_after` 在执行满 N 个任务后触发取消，
/// 用来复现“中途中断”时下游任务的 Aborted / Deferred 归类。
#[derive(Deserialize)]
struct FleetInput {
    revision: u64,
    tasks: Vec<TaskSpec>,
    #[serde(default = "default_workers")]
    workers: usize,
    #[serde(default)]
    fail: Vec<String>,
    #[serde(default)]
    cancel_after: Option<usize>,
}

fn default_workers() -> usize {
    1
}

#[derive(Deserialize)]
struct SelectInput {
    revision: u64,
    tasks: Vec<TaskSpec>,
    snapshot: Snapshot,
    limits: Limits,
    #[serde(default)]
    patch: Option<PlanPatch>,
    /// patch 的在途保护必须看到**打补丁前**的快照。
    #[serde(default)]
    base_snapshot: Option<Snapshot>,
    #[serde(default)]
    ledger: Ledger,
}

#[derive(Deserialize)]
struct DispatchInput {
    selection: Selection,
    tasks: Vec<TaskSpec>,
    ledger: Ledger,
}

#[derive(Deserialize)]
struct ObligationInput {
    selection: Selection,
    #[serde(default)]
    policy: ObligationPolicy,
    now: u64,
    budget: ObligationBudget,
}

#[derive(Deserialize)]
struct ShadowInput {
    policy: String,
    steps: Vec<TraceStep>,
}

#[derive(Deserialize)]
struct FinishInput {
    tasks: Vec<TaskSpec>,
    snapshot: Snapshot,
    #[serde(default)]
    rounds: Vec<RoundReport>,
}

#[derive(Deserialize)]
struct ClaimInput {
    #[serde(default)]
    store: ClaimStore,
    owner: String,
    paths: Vec<String>,
    intent: String,
    now: u64,
    ttl: u64,
}

#[derive(Deserialize)]
struct RenewInput {
    #[serde(default)]
    store: ClaimStore,
    id: String,
    owner: String,
    fencing: u64,
    now: u64,
    ttl: u64,
}

#[derive(Deserialize)]
struct ReleaseInput {
    #[serde(default)]
    store: ClaimStore,
    id: String,
    owner: String,
    fencing: u64,
}

#[derive(Deserialize)]
struct ClaimGateInput {
    #[serde(default)]
    store: ClaimStore,
    owner: String,
    paths: Vec<String>,
    now: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RepoGateInput {
    root: String,
    expected: String,
    task_kind: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AlignmentInput {
    root: String,
    manifest: String,
    strict: Option<bool>,
    changed: Option<Vec<String>>,
    changed_known: bool,
    executed: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct AlignmentScaffoldInput {
    root: String,
    manifest: String,
    goal: String,
    requirements: Vec<String>,
    force: bool,
}

#[derive(Deserialize)]
struct ReceiptInput {
    #[serde(default)]
    ledger: ReceiptLedger,
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct HandoffGateInput {
    packet: HandoffPacket,
    receiver_state_hash: String,
    now: u64,
}

#[derive(Deserialize)]
struct AdmitWithClaimInput {
    dependencies: String,
    authorization: String,
    resources: String,
    freshness: String,
    claim: String,
}

fn parse_ternary(raw: &str) -> Result<Ternary, trit_orch::Error> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "+1" | "1" => Ok(Ternary::True),
        "unknown" | "u" | "0" => Ok(Ternary::Unknown),
        "false" | "f" | "-1" => Ok(Ternary::False),
        other => Err(trit_orch::Error::InputParse {
            message: format!("非法三值 {other:?}（True/Unknown/False 或 +1/0/-1）"),
        }),
    }
}

fn parse_receipt_state(raw: &str) -> Result<ReceiptState, trit_orch::Error> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "sent" => Ok(ReceiptState::Sent),
        "delivered" => Ok(ReceiptState::Delivered),
        "read" => Ok(ReceiptState::Read),
        "acted" => Ok(ReceiptState::Acted),
        other => Err(trit_orch::Error::InputParse {
            message: format!("未知回执状态 {other:?}（sent/delivered/read/acted）"),
        }),
    }
}

fn main() {
    let command = std::env::args().nth(1).unwrap_or_default();
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        fail(&trit_orch::Error::InputParse {
            message: error.to_string(),
        });
    }
    match run(&command, &input) {
        Ok(value) => println!("{value}"),
        Err(error) => fail(&error),
    }
}

fn fail(error: &trit_orch::Error) -> ! {
    println!(
        "{}",
        json!({"ok": false, "kind": error.kind(), "error": error.to_string()})
    );
    let exit = match error {
        trit_orch::Error::AlignmentScaffold { .. }
        | trit_orch::Error::AlignmentScaffoldIo { .. } => 2,
        _ => 1,
    };
    std::process::exit(exit);
}

/// `select --capture <path>`：把这一步追加进轨迹文件（路径由宿主给，模型无法注入）。
fn capture_path() -> Option<String> {
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--capture" {
            return args.next();
        }
    }
    None
}

/// `shadow-jsonl <policy> <path>`。
fn jsonl_args() -> (String, String) {
    let mut args = std::env::args().skip(2);
    (
        args.next().unwrap_or_default(),
        args.next().unwrap_or_default(),
    )
}

fn parse<T: for<'de> Deserialize<'de>>(input: &str) -> Result<T, trit_orch::Error> {
    serde_json::from_str(input).map_err(|error| trit_orch::Error::InputParse {
        message: error.to_string(),
    })
}

fn run(command: &str, input: &str) -> Result<Value, trit_orch::Error> {
    match command {
        "plan" => {
            let request: PlanInput = parse(input)?;
            let plan = Plan::new(request.revision, request.tasks)?;
            Ok(json!({
                "ok": true,
                "revision": plan.revision(),
                "tasks": plan.tasks().len(),
            }))
        }
        "select" => {
            let request: SelectInput = parse(input)?;
            let capture = capture_path();
            let plan = Plan::new(request.revision, request.tasks)?;
            let plan = match request.patch {
                Some(patch) => {
                    let base = request.base_snapshot.as_ref().ok_or_else(|| {
                        trit_orch::Error::InputParse {
                            message: "带 patch 时必须提供 base_snapshot（打补丁前的快照）".into(),
                        }
                    })?;
                    plan.patch(&patch, base)?
                }
                None => plan,
            };
            let selection =
                plan.select_with_effects(&request.snapshot, &request.limits, &request.ledger)?;
            if let Some(path) = &capture {
                TraceStore::new(path).append(&TraceStep {
                    revision: selection.revision,
                    // 记录**补丁后**的计划，这样单步回放不依赖 patch。
                    tasks: plan.tasks().to_vec(),
                    snapshot: request.snapshot.clone(),
                    limits: request.limits.clone(),
                    ledger: Ledger::new(),
                })?;
            }
            Ok(json!({
                "ok": true,
                "brief": selection.brief(),
                "selection": selection,
            }))
        }
        "dispatch" => {
            let request: DispatchInput = parse(input)?;
            let attempts = dispatch_ready(&request.selection, &request.tasks, &request.ledger)?;
            Ok(json!({
                "ok": true,
                "ready": attempts,
            }))
        }
        "obligations" => {
            let request: ObligationInput = parse(input)?;
            let batch = request.selection.schedule_obligations(
                &request.policy,
                request.now,
                &request.budget,
            );
            Ok(json!({
                "ok": true,
                "batch": batch,
            }))
        }
        "finish" => {
            let request: FinishInput = parse(input)?;
            let progress = Progress::from_rounds(request.rounds);
            let summary = progress.finish(&request.tasks, &request.snapshot)?;
            Ok(json!({
                "ok": true,
                "summary": summary,
                "brief": progress.brief(),
            }))
        }
        "shadow" => {
            let request: ShadowInput = parse(input)?;
            let policy = builtin(&request.policy).ok_or_else(|| trit_orch::Error::InputParse {
                message: format!(
                    "未知策略 {:?}（可用：baseline/longest_first/eager/critical_path）",
                    request.policy
                ),
            })?;
            let report = replay(policy.as_ref(), &request.steps)?;
            Ok(json!({
                "ok": true,
                "safe": report.is_safe(),
                "report": report,
            }))
        }
        "shadow-jsonl" => {
            let (policy_name, path) = jsonl_args();
            let policy = builtin(&policy_name).ok_or_else(|| trit_orch::Error::InputParse {
                message: format!("未知策略 {policy_name:?}"),
            })?;
            let steps = TraceStore::new(&path).load()?;
            let report = replay(policy.as_ref(), &steps)?;
            Ok(json!({
                "ok": true,
                "safe": report.is_safe(),
                "trace": path,
                "report": report,
            }))
        }
        "recover" => {
            let ledger: Ledger = parse(input)?;
            Ok(json!({
                "ok": true,
                "recovery": ledger.recover(),
            }))
        }
        "claim" => {
            let mut request: ClaimInput = parse(input)?;
            let outcome = request.store.claim(
                &request.owner,
                &request.paths,
                &request.intent,
                request.now,
                request.ttl,
            )?;
            Ok(json!({
                "ok": true,
                "store": request.store,
                "outcome": outcome,
            }))
        }
        "renew" => {
            let mut request: RenewInput = parse(input)?;
            let claim = request.store.renew(
                &request.id,
                &request.owner,
                request.fencing,
                request.now,
                request.ttl,
            )?;
            Ok(json!({
                "ok": true,
                "store": request.store,
                "claim": claim,
            }))
        }
        "release" => {
            let mut request: ReleaseInput = parse(input)?;
            let claim = request
                .store
                .release(&request.id, &request.owner, request.fencing)?;
            Ok(json!({
                "ok": true,
                "store": request.store,
                "claim": claim,
            }))
        }
        "claim-gate" => {
            let request: ClaimGateInput = parse(input)?;
            let gate = request
                .store
                .gate(&request.paths, &request.owner, request.now)?;
            Ok(json!({
                "ok": true,
                "gate": format!("{gate:?}"),
            }))
        }
        "receipt" => {
            let mut request: ReceiptInput = parse(input)?;
            let receipt = match request.action.as_str() {
                "record" => request.ledger.record(&request.id, &request.key)?,
                "advance" => {
                    let state = parse_receipt_state(request.state.as_deref().unwrap_or(""))?;
                    request.ledger.advance(&request.key, state)?
                }
                "reject" => {
                    let reason = request.reason.as_deref().unwrap_or("");
                    request.ledger.reject(&request.key, reason)?
                }
                "expire" => request.ledger.expire(&request.key)?,
                other => {
                    return Err(trit_orch::Error::InputParse {
                        message: format!(
                            "未知 receipt action {other:?}（record/advance/reject/expire）"
                        ),
                    });
                }
            };
            let obligation = receipt_obligation(&receipt);
            Ok(json!({
                "ok": true,
                "ledger": request.ledger,
                "receipt": receipt,
                "obligation": obligation,
            }))
        }
        "handoff-gate" => {
            let request: HandoffGateInput = parse(input)?;
            let gate = handoff_gate(&request.packet, &request.receiver_state_hash, request.now);
            let obligation =
                handoff_obligation(&request.packet, &request.receiver_state_hash, request.now);
            Ok(json!({
                "ok": true,
                "gate": format!("{gate:?}"),
                "obligation": obligation,
            }))
        }
        "admit-with-claim" => {
            let request: AdmitWithClaimInput = parse(input)?;
            let gate = admit_with_claim(
                parse_ternary(&request.dependencies)?,
                parse_ternary(&request.authorization)?,
                parse_ternary(&request.resources)?,
                parse_ternary(&request.freshness)?,
                parse_ternary(&request.claim)?,
            );
            Ok(json!({
                "ok": true,
                "gate": format!("{gate:?}"),
            }))
        }
        "compact-gate" => {
            let facts: ContextFacts = parse(input)?;
            Ok(json!({
                "ok": true,
                "decision": compaction_gate(&facts)?,
            }))
        }
        "delivery" => cli_delivery::run(input),
        "gate-chain" => {
            let (value, exit) = cli_gate_chain::run(input)?;
            println!("{value}");
            std::process::exit(exit);
        }
        "alignment" => {
            let request: AlignmentInput = parse(input)?;
            let root = if request.root.trim().is_empty() {
                PathBuf::from(".")
            } else {
                PathBuf::from(request.root.trim())
            };
            let manifest = if request.manifest.trim().is_empty() {
                Path::new(".agent/alignment.json").to_path_buf()
            } else {
                PathBuf::from(request.manifest.trim())
            };
            let options = trit_orch::alignment::AlignmentOptions {
                changed: request.changed,
                changed_known: request.changed_known,
                strict: request.strict.unwrap_or(true),
                executed: request.executed,
            };
            let report = trit_orch::alignment::check(&root, &manifest, &options);
            let value =
                serde_json::to_value(&report).map_err(|error| trit_orch::Error::InputParse {
                    message: format!("serialize alignment report: {error}"),
                })?;
            println!("{value}");
            std::process::exit(report.exit);
        }
        "alignment-scaffold" => {
            let request: AlignmentScaffoldInput = parse(input)?;
            let root = if request.root.trim().is_empty() {
                PathBuf::from(".")
            } else {
                PathBuf::from(request.root.trim())
            };
            let manifest = if request.manifest.trim().is_empty() {
                Path::new(".agent/alignment.json").to_path_buf()
            } else {
                PathBuf::from(request.manifest.trim())
            };
            let outcome = trit_orch::alignment::scaffold(
                &root,
                &manifest,
                &trit_orch::alignment::ScaffoldOptions {
                    goal: request.goal,
                    requirements: request.requirements,
                    force: request.force,
                },
            )?;
            Ok(json!({
                "ok": true,
                "manifest": outcome.manifest,
                "goals": outcome.goals,
                "requirements": outcome.requirements,
            }))
        }
        "repo-gate" => {
            let request: RepoGateInput = parse(input)?;
            let root = if request.root.trim().is_empty() {
                PathBuf::from(".")
            } else {
                PathBuf::from(request.root.trim())
            };
            let decision = trit_orch::repo_gate(&root, &request.expected, &request.task_kind);
            if decision.gate != Ternary::True {
                return Err(trit_orch::Error::RepoScopeRejected {
                    reason: decision.reason,
                });
            }
            Ok(json!({
                "ok": true,
                "gate": "True",
                "identity": decision.identity,
            }))
        }
        "fleet-run" => {
            let request: FleetInput = parse(input)?;
            let plan = Plan::new(request.revision, request.tasks)?;
            let fail: std::collections::BTreeSet<String> = request.fail.into_iter().collect();
            let cancel = trit_schedule::runtime::CancelToken::new();
            let cancel_job = cancel.clone();
            let executed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let executed_job = std::sync::Arc::clone(&executed);
            let cancel_after = request.cancel_after;
            let report = trit_orch::execute(
                &plan,
                trit_orch::FleetConfig::new(request.workers),
                cancel,
                move |spec, _cancel| {
                    let done = executed_job.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    if cancel_after.is_some_and(|limit| done >= limit) {
                        cancel_job.cancel();
                    }
                    if fail.contains(&spec.task.id) {
                        return Err(format!("simulated failure: {}", spec.task.id));
                    }
                    Ok(trit_orch::FleetOutput {
                        content: format!("simulated {}", spec.task.id).into_bytes(),
                        tokens: 0,
                    })
                },
            )?;
            Ok(json!({
                "ok": true,
                "completed": report.completed,
                "failed": report.failed.as_ref().map(|failure| json!({
                    "task": failure.task,
                    "message": failure.message,
                })),
                "deferred": report.deferred,
                "cancelled": report.cancelled,
                "references": report.references(),
                "metrics": report.metrics,
                "metrics_prometheus": report.metrics.to_prometheus(),
            }))
        }
        other => Err(trit_orch::Error::InputParse {
            message: format!(
                "未知子命令 {other:?}（可用：plan/select/dispatch/obligations/finish/recover/shadow/shadow-jsonl/compact-gate/delivery/alignment/alignment-scaffold/repo-gate/gate-chain/fleet-run/claim/renew/release/claim-gate/receipt/handoff-gate/admit-with-claim）"
            ),
        }),
    }
}
