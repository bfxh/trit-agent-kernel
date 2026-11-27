use std::error::Error;
use std::io::{ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use trit_host::tools::{LocalTools, SandboxCommand, WorkspaceConfig};
use trit_host::{
    AgentContext, AgentContextPlanner, AgentContextTools, AgentHost, CacheLedger,
    CachePressurePolicy, CacheWindow, CheckpointStore, CheckpointingTools, ClaimingTools,
    ContextPolicy, ContextRequest, ExecutorSetup, GitCheckpointStore, GitCheckpointingTools,
    HeadlessConfig, InMemoryContext, LowHitPolicy, McpToolExecutor, Model, ModelProfile,
    MutationJournal, OpenAiConfig, OpenAiProvider, OpenAiSamplingHandler, OperationLease,
    PlanExecuteConfig, Proposal, ProviderModel, ReasoningEffort, RecoveryContext, ResolvedContext,
    RewindScope, SamplingHandler, ScriptedModel, SearchOrder, SelectionPressure, Session,
    SupervisorConfig, SupervisorRuntime, ToolCatalog, ToolExecutor, ToolSpec, WriteClaim,
    WriteGuard, default_tools, planner_catalog_specs, recovery_context_for_session,
    rewind_session_git_with_policy, rewind_session_with_policy, run_headless,
    run_plan_then_execute_with,
};
use trit_orch::alignment::AlignmentOptions;
use trit_orch::gate_chain::{
    self as gate_chain_mod, AlignmentStep, DeliveryStep, GateChainOptions,
};
use trit_reasonix::TurnSpec;

fn main() {
    if let Err(error) = run() {
        eprintln!("trit-host: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if command.is_empty() || command == "--help" || command == "-h" {
        usage();
        return Ok(());
    }
    if command == "rewind" {
        return run_rewind(args);
    }
    if command == "session-status" {
        return run_session_status(args);
    }
    if command == "session-index" {
        return run_session_index(args);
    }
    if command == "session-scan" {
        return run_session_scan(args);
    }
    if command == "diagnostics" {
        return run_diagnostics(args);
    }
    if command == "journal-status" {
        return run_journal_status(args);
    }
    if command == "rewind-status" {
        return run_rewind_status(args);
    }
    if command == "appcontainer-run" {
        return run_appcontainer(args);
    }
    if command == "appcontainer-prune" {
        return run_appcontainer_prune(args);
    }
    if command == "subagent-echo" {
        return run_subagent_echo(args);
    }
    if command == "subagent-run" {
        return run_subagent_run(args);
    }
    if command == "plan-exec" {
        return run_plan_exec(args);
    }
    if command == "remote-echo" {
        return run_remote_echo(args);
    }
    if command == "remote-plan" {
        return run_remote_plan_cli(args);
    }
    if command == "remote-health" {
        return run_remote_health(args);
    }
    if command == "remote-ledger" {
        return run_remote_ledger(args);
    }
    if command == "remote-exec" {
        return run_remote_exec(args);
    }
    if command != "run" {
        usage();
        return Err(fail(format!("unknown command: {command}")));
    }

    let mut task = None;
    let mut workspace = PathBuf::from(".");
    let mut repo_identity = false;
    let mut repo_expected = String::new();
    let mut task_kind = "agent".to_owned();
    let mut gate_chain = false;
    let mut handoff_packet: Option<PathBuf> = None;
    let mut handoff_state_hash = String::new();
    let mut handoff_attach = false;
    let mut alignment_manifest: Option<PathBuf> = None;
    let mut alignment_changed: Vec<String> = Vec::new();
    let mut alignment_changed_set = false;
    let mut alignment_no_git_changed = false;
    let mut gate_chain_post = false;
    let mut gate_chain_report: Option<PathBuf> = None;
    let mut gate_chain_post_report: Option<PathBuf> = None;
    let mut script = None;
    let mut max_steps = 8usize;
    let mut allow_write = false;
    let mut allow_shell = false;
    let mut session = None;
    let mut provider = "scripted".to_owned();
    let mut sandbox: Option<PathBuf> = None;
    let mut sandbox_args: Vec<String> = Vec::new();
    let mut allow_unsandboxed_shell = false;
    let mut allow_hardlinks = false;
    let mut write_claim_paths: Vec<PathBuf> = Vec::new();
    let mut write_lease_owner: Option<String> = None;
    let mut write_journal = false;
    let mut write_journal_rollback = false;
    let mut resume = false;
    let mut context = ContextPolicy::default();
    let mut checkpoint_dir: Option<PathBuf> = None;
    let mut checkpoint_backend = "files".to_owned();
    let mut checkpoint_retain: Option<usize> = None;
    let mut context_catalog: Option<PathBuf> = None;
    let mut context_project: Option<String> = None;
    let mut context_session_id: Option<String> = None;
    let mut context_budget: u64 = 0;
    let mut context_window: u64 = 128_000;
    let mut model_max_output: u64 = 8_192;
    let mut supports_reasoning = false;
    let mut reasoning = ReasoningEffort::Off;
    let mut context_read_budget: usize = 16 * 1024;
    let mut context_read_max: usize = 8 * 1024;
    let mut cache_hit_threshold: u32 = 5_000;
    let mut low_hit_policy = LowHitPolicy::Keep;
    let mut cache_window_samples: usize = 64;
    let mut cache_window_ms: Option<u64> = None;
    let mut cache_pressure_scale_bp: u32 = 10_000;
    let mut cache_search_scale_bp: u32 = 10_000;
    let mut cache_search_order = SearchOrder::Resolver;
    let mut cache_task_selection = SelectionPressure::Normal;
    let mut cache_ledger_path: Option<PathBuf> = None;
    let mut supervisor_config: Option<SupervisorConfig> = None;
    let mut mcp_sampling = false;
    let mut mcp_sampling_model: Option<String> = None;
    // 人类面默认输出 Markdown；需要机器消费时显式 `--output-format json`。
    let mut output_format = String::from("md");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--task" => task = Some(next(&mut args, "--task")?),
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--repo-identity" => repo_identity = true,
            "--repo-expected" => repo_expected = next(&mut args, "--repo-expected")?,
            "--task-kind" => task_kind = next(&mut args, "--task-kind")?,
            "--gate-chain" => gate_chain = true,
            "--handoff-packet" => {
                handoff_packet = Some(PathBuf::from(next(&mut args, "--handoff-packet")?));
            }
            "--handoff-state-hash" => {
                handoff_state_hash = next(&mut args, "--handoff-state-hash")?;
            }
            "--handoff-attach" => handoff_attach = true,
            "--alignment-manifest" => {
                alignment_manifest = Some(PathBuf::from(next(&mut args, "--alignment-manifest")?));
            }
            "--alignment-changed" => {
                alignment_changed.push(next(&mut args, "--alignment-changed")?);
            }
            "--alignment-changed-set" => alignment_changed_set = true,
            "--alignment-no-git-changed" => alignment_no_git_changed = true,
            "--gate-chain-post" => gate_chain_post = true,
            "--gate-chain-report" => {
                gate_chain_report = Some(PathBuf::from(next(&mut args, "--gate-chain-report")?));
            }
            "--gate-chain-post-report" => {
                gate_chain_post_report =
                    Some(PathBuf::from(next(&mut args, "--gate-chain-post-report")?));
            }
            "--script" => script = Some(PathBuf::from(next(&mut args, "--script")?)),
            "--max-steps" => {
                max_steps = next(&mut args, "--max-steps")?
                    .parse()
                    .map_err(|error| fail(format!("bad --max-steps: {error}")))?;
            }
            "--allow-write" => allow_write = true,
            "--allow-shell" => allow_shell = true,
            "--session" => session = Some(PathBuf::from(next(&mut args, "--session")?)),
            "--provider" => provider = next(&mut args, "--provider")?,
            "--sandbox" => sandbox = Some(PathBuf::from(next(&mut args, "--sandbox")?)),
            "--sandbox-arg" => sandbox_args.push(next(&mut args, "--sandbox-arg")?),
            "--allow-unsandboxed-shell" => allow_unsandboxed_shell = true,
            "--allow-hardlinks" => allow_hardlinks = true,
            "--output-format" => {
                output_format = match next(&mut args, "--output-format")?.as_str() {
                    "md" | "markdown" => "md".into(),
                    "json" => "json".into(),
                    other => return Err(fail(format!("bad --output-format: {other}"))),
                };
            }
            "--write-claim" => {
                write_claim_paths.push(PathBuf::from(next(&mut args, "--write-claim")?));
            }
            "--write-lease" => write_lease_owner = Some(next(&mut args, "--write-lease")?),
            "--write-journal" => write_journal = true,
            "--write-journal-rollback" => write_journal_rollback = true,
            "--resume" => resume = true,
            "--checkpoint-dir" => {
                checkpoint_dir = Some(PathBuf::from(next(&mut args, "--checkpoint-dir")?));
            }
            "--checkpoint-backend" => checkpoint_backend = next(&mut args, "--checkpoint-backend")?,
            "--checkpoint-retain" => {
                checkpoint_retain = Some(
                    next(&mut args, "--checkpoint-retain")?
                        .parse()
                        .map_err(|error| fail(format!("bad --checkpoint-retain: {error}")))?,
                );
            }
            "--context-receipts" => {
                context.max_receipts = next(&mut args, "--context-receipts")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-receipts: {error}")))?;
            }
            "--context-chars" => {
                context.max_chars = next(&mut args, "--context-chars")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-chars: {error}")))?;
            }
            "--context-output" => {
                context.max_output_chars = next(&mut args, "--context-output")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-output: {error}")))?;
            }
            "--context-catalog" => {
                context_catalog = Some(PathBuf::from(next(&mut args, "--context-catalog")?));
            }
            "--context-project" => context_project = Some(next(&mut args, "--context-project")?),
            "--context-session-id" => {
                context_session_id = Some(next(&mut args, "--context-session-id")?);
            }
            "--context-budget" => {
                context_budget = next(&mut args, "--context-budget")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-budget: {error}")))?;
            }
            "--context-window" => {
                context_window = next(&mut args, "--context-window")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-window: {error}")))?;
            }
            "--model-max-output" => {
                model_max_output = next(&mut args, "--model-max-output")?
                    .parse()
                    .map_err(|error| fail(format!("bad --model-max-output: {error}")))?;
            }
            "--supports-reasoning" => supports_reasoning = true,
            "--reasoning" => {
                reasoning = parse_reasoning(&next(&mut args, "--reasoning")?)?;
            }
            "--context-read-budget" => {
                context_read_budget = next(&mut args, "--context-read-budget")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-read-budget: {error}")))?;
            }
            "--context-read-max" => {
                context_read_max = next(&mut args, "--context-read-max")?
                    .parse()
                    .map_err(|error| fail(format!("bad --context-read-max: {error}")))?;
            }
            "--cache-hit-threshold" => {
                cache_hit_threshold = next(&mut args, "--cache-hit-threshold")?
                    .parse()
                    .map_err(|error| fail(format!("bad --cache-hit-threshold: {error}")))?;
            }
            "--cache-low-hit-policy" => {
                low_hit_policy = parse_low_hit_policy(&next(&mut args, "--cache-low-hit-policy")?)?;
            }
            "--cache-window-samples" => {
                cache_window_samples = next(&mut args, "--cache-window-samples")?
                    .parse()
                    .map_err(|error| fail(format!("bad --cache-window-samples: {error}")))?;
            }
            "--cache-window-ms" => {
                cache_window_ms = Some(
                    next(&mut args, "--cache-window-ms")?
                        .parse()
                        .map_err(|error| fail(format!("bad --cache-window-ms: {error}")))?,
                );
            }
            "--cache-pressure-scale-bp" => {
                cache_pressure_scale_bp = next(&mut args, "--cache-pressure-scale-bp")?
                    .parse()
                    .map_err(|error| fail(format!("bad --cache-pressure-scale-bp: {error}")))?;
            }
            "--cache-search-scale-bp" => {
                cache_search_scale_bp = next(&mut args, "--cache-search-scale-bp")?
                    .parse()
                    .map_err(|error| fail(format!("bad --cache-search-scale-bp: {error}")))?;
            }
            "--cache-search-order" => {
                cache_search_order = parse_search_order(&next(&mut args, "--cache-search-order")?)?;
            }
            "--cache-task-selection" => {
                cache_task_selection =
                    parse_task_selection(&next(&mut args, "--cache-task-selection")?)?;
            }
            "--cache-ledger" => {
                cache_ledger_path = Some(PathBuf::from(next(&mut args, "--cache-ledger")?));
            }
            "--supervisor-config" => {
                let path = PathBuf::from(next(&mut args, "--supervisor-config")?);
                let text = std::fs::read_to_string(&path).map_err(|error| {
                    fail(format!(
                        "read supervisor config {}: {error}",
                        path.display()
                    ))
                })?;
                supervisor_config = Some(
                    serde_json::from_str::<SupervisorConfig>(&text)
                        .map_err(|error| fail(format!("parse supervisor config: {error}")))?,
                );
            }
            "--mcp-sampling" => mcp_sampling = true,
            "--mcp-sampling-model" => {
                mcp_sampling_model = Some(next(&mut args, "--mcp-sampling-model")?);
            }
            "--help" | "-h" => {
                usage();
                return Ok(());
            }
            other => return Err(fail(format!("unknown argument: {other}"))),
        }
    }

    let turn = TurnSpec {
        task: "headless".into(),
        objective: task.unwrap_or_else(|| "headless task".into()),
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 1,
        max_tokens: 1_000_000,
    };
    if allow_shell && sandbox.is_none() && !allow_unsandboxed_shell {
        return Err(fail(
            "--allow-shell requires --sandbox <program> or --allow-unsandboxed-shell",
        ));
    }
    if checkpoint_dir.is_some() && session.is_none() {
        return Err(fail("--checkpoint-dir requires --session"));
    }
    match checkpoint_backend.as_str() {
        "files" | "git" => {}
        other => return Err(fail(format!("bad --checkpoint-backend: {other}"))),
    }
    if checkpoint_backend == "git" && checkpoint_dir.is_none() {
        return Err(fail("--checkpoint-backend git requires --checkpoint-dir"));
    }
    if context_catalog.is_some() && provider != "openai" {
        return Err(fail("--context-catalog requires --provider openai"));
    }
    if mcp_sampling_model.is_some() && !mcp_sampling {
        return Err(fail("--mcp-sampling-model requires --mcp-sampling"));
    }
    if mcp_sampling && provider != "openai" {
        return Err(fail("--mcp-sampling requires --provider openai"));
    }
    if handoff_packet.is_none() && !handoff_state_hash.trim().is_empty() {
        return Err(fail("--handoff-state-hash requires --handoff-packet"));
    }
    if handoff_attach && handoff_packet.is_none() {
        return Err(fail("--handoff-attach requires --handoff-packet"));
    }
    if write_lease_owner.is_some() && write_claim_paths.is_empty() {
        return Err(fail("--write-lease requires --write-claim"));
    }
    if write_journal_rollback && !write_journal {
        return Err(fail("--write-journal-rollback requires --write-journal"));
    }
    let workspace_root = workspace.clone();
    // 路径归一化在拿到 workspace 之后做：`.\src\a.py`、`src//a.py`、workspace 内
    // 绝对路径都折成 `src/a.py`；逃出 workspace 直接拒绝。
    let mut alignment_changed: Vec<String> = alignment_changed
        .iter()
        .map(|raw| normalize_changed(raw, &workspace_root))
        .collect::<Result<Vec<_>, _>>()?;
    alignment_changed.sort();
    alignment_changed.dedup();
    let manifest_path = alignment_manifest
        .clone()
        .unwrap_or_else(|| PathBuf::from(".agent/alignment.json"));
    let mut write_guard = if write_claim_paths.is_empty() {
        None
    } else {
        let claim = WriteClaim::new(&workspace_root, write_claim_paths.clone())
            .map_err(|error| fail(error.to_string()))?;
        let lease = match write_lease_owner.as_deref() {
            Some(owner) => Some(
                OperationLease::acquire_paths(&workspace_root, owner, &write_claim_paths)
                    .map_err(|error| fail(error.to_string()))?,
            ),
            None => None,
        };
        let journal = if write_journal {
            let mut journal =
                MutationJournal::open(&workspace_root).map_err(|error| fail(error.to_string()))?;
            let report = journal.report().map_err(|error| fail(error.to_string()))?;
            if report.has_in_doubt() {
                if write_journal_rollback {
                    let (outcomes, blocked) = rollback_in_doubt(
                        &mut journal,
                        &workspace_root,
                        &report.in_doubt,
                        lease.as_ref(),
                    )?;
                    for outcome in &outcomes {
                        eprintln!(
                            "write journal: rolled back op {} (restored={} removed={} skipped={} failed={})",
                            outcome.op_id,
                            outcome.restored.len(),
                            outcome.removed.len(),
                            outcome.skipped.len(),
                            outcome.failed.len()
                        );
                    }
                    if !blocked.is_empty() {
                        return Err(fail(format!(
                            "write journal: {} operation(s) still protected by a live lease; refusing rollback",
                            blocked.len()
                        )));
                    }
                } else {
                    eprintln!(
                        "write journal: {} operation(s) left in doubt from a previous run",
                        report.in_doubt.len()
                    );
                }
            }
            Some(journal)
        } else {
            None
        };
        Some(WriteGuard {
            claim,
            lease,
            journal,
        })
    };
    // guard 会随工具一起装箱；完成裁决需要同一份范围做校验，因此先留一份克隆。
    let write_claim = write_guard.as_ref().map(|guard| guard.claim.clone());
    let identity_path = workspace_root.join(trit_orch::repo::DEFAULT_PATH);
    let gate_chain_requested = gate_chain
        || handoff_packet.is_some()
        || alignment_manifest.is_some()
        || !alignment_changed.is_empty()
        || alignment_changed_set
        || alignment_no_git_changed
        || gate_chain_report.is_some();
    if gate_chain_requested {
        let delivery = handoff_packet.map(|packet| {
            let packet = if packet.is_absolute() {
                packet
            } else {
                workspace_root.join(packet)
            };
            DeliveryStep {
                packet,
                receiver_state_hash: handoff_state_hash.trim().to_owned(),
                at: 0,
                attach: handoff_attach,
            }
        });
        // 派发前的 changed 是“子集”语义：默认取当前 git 工作区变更，
        // 只检查已给出的变更有没有对齐，不要求声明的 changes 全部兑现。
        let changed = if !alignment_changed.is_empty() {
            Some(alignment_changed.clone())
        } else if alignment_no_git_changed {
            None
        } else {
            git_changed(&workspace_root)
        };
        let manifest = manifest_path.clone();
        let report = gate_chain_mod::run(&GateChainOptions {
            root: workspace_root.clone(),
            expected_repo: repo_expected.clone(),
            task_kind: task_kind.clone(),
            delivery,
            alignment: Some(AlignmentStep {
                manifest,
                options: AlignmentOptions {
                    changed: changed.clone(),
                    changed_known: alignment_changed_set && changed.is_some(),
                    strict: true,
                    executed: None,
                },
            }),
        });
        if let Some(path) = gate_chain_report.as_ref() {
            write_report(&workspace_root, path, &report)?;
        }
        if let Some(path) = session.as_deref() {
            append_session_event(
                path,
                trit_host::SessionEvent::GateChain {
                    phase: trit_host::GateChainPhase::Pre,
                    status: report.status.clone(),
                    exit: report.exit,
                    failed_at: report.failed_at.clone(),
                    failures: report.counts.blocked + report.counts.unknown,
                    passed: report.counts.passed,
                    blocked: report.counts.blocked,
                    unknown: report.counts.unknown,
                    skipped: report.counts.skipped,
                    report: gate_chain_report
                        .as_ref()
                        .map(|path| path.display().to_string()),
                },
            )?;
        }
        if report.status != "passed" {
            let failed_at = report
                .failed_at
                .clone()
                .unwrap_or_else(|| "unknown".to_owned());
            let reason = report
                .steps
                .iter()
                .find(|step| Some(step.id.as_str()) == report.failed_at.as_deref())
                .map(|step| step.reason.clone())
                .unwrap_or_default();
            return Err(fail(format!(
                "gate chain rejected at {failed_at}: {reason}"
            )));
        }
    } else if repo_identity || !repo_expected.trim().is_empty() || identity_path.exists() {
        let decision = trit_orch::repo::gate(&workspace_root, &repo_expected, &task_kind);
        if decision.gate != trit_orch::Ternary::True {
            return Err(fail(format!("repo gate rejected: {}", decision.reason)));
        }
    }
    let mut workspace = WorkspaceConfig::new(workspace);
    workspace.allow_write = allow_write;
    workspace.allow_shell = allow_shell;
    workspace.allow_unsandboxed_shell = allow_unsandboxed_shell;
    workspace.sandbox = match sandbox {
        Some(program) if program == Path::new("builtin") => Some(
            SandboxCommand::builtin(&workspace_root).map_err(|error| fail(error.to_string()))?,
        ),
        Some(program) => Some(SandboxCommand {
            program,
            args: sandbox_args,
        }),
        None => None,
    };
    workspace.forbid_hardlinks = !allow_hardlinks;
    let session_path = session.clone();
    let mut config = HeadlessConfig {
        workspace,
        max_steps,
        session,
        context,
        write_guard: None,
    };
    let scripted = match provider.as_str() {
        "scripted" => {
            let script =
                script.ok_or_else(|| fail("--script is required when --provider scripted"))?;
            Some(
                serde_json::from_str::<Vec<Proposal>>(
                    &std::fs::read_to_string(&script).map_err(|error| {
                        fail(format!("read script {}: {error}", script.display()))
                    })?,
                )
                .map_err(|error| fail(format!("parse script: {error}")))?,
            )
        }
        "openai" => {
            if script.is_some() {
                return Err(fail("--script is not used with --provider openai"));
            }
            None
        }
        other => return Err(fail(format!("unknown provider: {other}"))),
    };
    let openai_config = if provider == "openai" {
        Some(OpenAiConfig::from_env()?)
    } else {
        None
    };
    let sampling_handler: Option<Arc<dyn SamplingHandler>> = if mcp_sampling {
        let config = openai_config
            .as_ref()
            .ok_or_else(|| fail("--mcp-sampling requires --provider openai"))?;
        let mut sampling_config = config.clone();
        if sampling_config.max_output_tokens.is_none() {
            sampling_config.max_output_tokens = Some(model_max_output);
        }
        let handler = match mcp_sampling_model.as_ref() {
            Some(model) => OpenAiSamplingHandler::with_model(sampling_config, model.clone()),
            None => OpenAiSamplingHandler::new(sampling_config),
        };
        Some(Arc::new(handler))
    } else {
        None
    };
    let context_agent = match context_catalog.as_ref() {
        Some(path) => {
            let config = openai_config
                .as_ref()
                .ok_or_else(|| fail("--context-catalog requires --provider openai"))?;
            let catalog = load_context_catalog(path)?;
            let project = context_project
                .clone()
                .unwrap_or_else(|| workspace_root.display().to_string());
            let session_id = context_session_id.clone().unwrap_or_else(|| {
                session_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "cli".into())
            });
            let request = ContextRequest {
                project,
                session: session_id,
                query: turn.objective.clone(),
                selections: Vec::new(),
                budget_tokens: context_budget,
                model: ModelProfile {
                    name: config.model.clone(),
                    provider: config.base_url.clone(),
                    context_window,
                    max_output_tokens: model_max_output,
                    supports_tools: true,
                    supports_context_read: true,
                    supports_reasoning,
                },
                reasoning,
                allow_secret: false,
                autonomy: Default::default(),
                priors: Default::default(),
                calibration: None,
            };
            Some(Arc::new(Mutex::new(AgentContext::assemble(
                request, catalog,
            )?)))
        }
        None => None,
    };
    let loaded_cache_ledger = match cache_ledger_path.as_ref() {
        Some(path) if path.exists() => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| fail(format!("read cache ledger {}: {error}", path.display())))?;
            serde_json::from_str::<CacheLedger>(&text)
                .map_err(|error| fail(format!("parse cache ledger: {error}")))?
        }
        _ => CacheLedger::default(),
    };
    let shared_cache_ledger = Arc::new(Mutex::new(loaded_cache_ledger));
    let cache_options = CacheOptions {
        hit_threshold_bp: cache_hit_threshold,
        low_hit_policy,
        window: CacheWindow {
            max_samples: cache_window_samples,
            max_age_ms: cache_window_ms,
        },
        pressure: CachePressurePolicy {
            dynamic_budget_scale_bp: cache_pressure_scale_bp,
            search_limit_scale_bp: cache_search_scale_bp,
            search_order: cache_search_order,
            task_selection: cache_task_selection,
        },
    };
    let supervisor_session_path = session_path.clone();
    let mut supervisor_runtime = match supervisor_config.as_ref() {
        Some(supervisor_config) => {
            let recovery =
                supervisor_recovery(supervisor_session_path.as_deref(), &shared_cache_ledger)?;
            Some(SupervisorRuntime::start_with_sampling(
                supervisor_config,
                recovery,
                sampling_handler,
            )?)
        }
        None => None,
    };
    let mcp_tool_client = supervisor_runtime
        .as_ref()
        .map(|runtime| Arc::new(runtime.tool_client()))
        .filter(|client| !client.is_empty());
    let mcp_tool_catalog: Option<Arc<dyn ToolCatalog>> = mcp_tool_client
        .as_ref()
        .map(|client| Arc::clone(client) as Arc<dyn ToolCatalog>);
    let (outcome, _cache_ledger) = if resume || checkpoint_dir.is_some() {
        let path = session_path
            .as_ref()
            .ok_or_else(|| fail("--resume requires --session"))?
            .clone();
        let model = build_model(
            scripted,
            openai_config.as_ref(),
            context_agent.as_ref(),
            &shared_cache_ledger,
            &cache_options,
            mcp_tool_catalog.clone(),
        )?;
        let local = LocalTools::new(config.workspace.clone())?;
        let tools: Box<dyn ToolExecutor> = match checkpoint_dir {
            Some(dir) => match checkpoint_backend.as_str() {
                "git" => {
                    let store = GitCheckpointStore::open(dir, &workspace_root)?;
                    let mut tools = GitCheckpointingTools::new(store, local);
                    if let Some(retain) = checkpoint_retain {
                        tools = tools.with_retain(retain);
                    }
                    Box::new(tools)
                }
                _ => {
                    let store = CheckpointStore::open(dir)?;
                    let mut tools = CheckpointingTools::new(store, workspace_root.clone(), local);
                    if let Some(retain) = checkpoint_retain {
                        tools = tools.with_retain(retain);
                    }
                    Box::new(tools)
                }
            },
            None => Box::new(local),
        };
        let tools: Box<dyn ToolExecutor> = match context_agent.as_ref() {
            Some(shared) => Box::new(
                AgentContextTools::new(tools, Arc::clone(shared))
                    .with_max_chars(context_read_max)
                    .with_read_budget(context_read_budget),
            ),
            None => tools,
        };
        let tools: Box<dyn ToolExecutor> = match mcp_tool_client.as_ref() {
            Some(client) => Box::new(McpToolExecutor::new(client.clone(), tools)),
            None => tools,
        };
        let tools = apply_write_guard(tools, write_guard.take());
        let mut session = Session::open(model, tools, &path)?
            .with_max_steps(max_steps)
            .with_context(context)
            .with_write_claim(write_claim.clone())
            .with_cache_ledger(Arc::clone(&shared_cache_ledger));
        let outcome = session.run_turn(&turn)?;
        let ledger = session.cache_ledger();
        (outcome, Some(ledger))
    } else if let Some(shared) = context_agent.as_ref() {
        let model = build_model(
            scripted,
            openai_config.as_ref(),
            Some(shared),
            &shared_cache_ledger,
            &cache_options,
            mcp_tool_catalog.clone(),
        )?;
        let local = LocalTools::new(config.workspace.clone())?;
        let tools = AgentContextTools::new(local, Arc::clone(shared))
            .with_max_chars(context_read_max)
            .with_read_budget(context_read_budget);
        let tools: Box<dyn ToolExecutor> = match mcp_tool_client.as_ref() {
            Some(client) => Box::new(McpToolExecutor::new(client.clone(), tools)),
            None => Box::new(tools),
        };
        let tools = apply_write_guard(tools, write_guard.take());
        if let Some(path) = config.session.as_ref() {
            let mut session = Session::open(model, tools, path)?
                .with_max_steps(max_steps)
                .with_context(context)
                .with_write_claim(write_claim.clone())
                .with_cache_ledger(Arc::clone(&shared_cache_ledger));
            let outcome = session.run_turn(&turn)?;
            let ledger = session.cache_ledger();
            (outcome, Some(ledger))
        } else {
            let mut host = AgentHost::new(model, tools)
                .with_max_steps(max_steps)
                .with_context(context)
                .with_write_claim(write_claim.clone())
                .with_cache_ledger(Arc::clone(&shared_cache_ledger));
            let outcome = host.run_turn(&turn)?;
            let ledger = host.cache_ledger().clone();
            (outcome, Some(ledger))
        }
    } else {
        config.write_guard = write_guard.take();
        let model = build_model(
            scripted,
            openai_config.as_ref(),
            None,
            &shared_cache_ledger,
            &cache_options,
            mcp_tool_catalog.clone(),
        )?;
        if let Some(client) = mcp_tool_client.as_ref() {
            let local = LocalTools::new(config.workspace.clone())?;
            let tools = McpToolExecutor::new(client.clone(), local);
            let tools = apply_write_guard(Box::new(tools), config.write_guard.take());
            if let Some(path) = config.session.as_ref() {
                let mut session = Session::open(model, tools, path)?
                    .with_max_steps(max_steps)
                    .with_context(context)
                    .with_write_claim(write_claim.clone())
                    .with_cache_ledger(Arc::clone(&shared_cache_ledger));
                let outcome = session.run_turn(&turn)?;
                let ledger = session.cache_ledger();
                (outcome, Some(ledger))
            } else {
                let mut host = AgentHost::new(model, tools)
                    .with_max_steps(max_steps)
                    .with_context(context)
                    .with_write_claim(write_claim.clone())
                    .with_cache_ledger(Arc::clone(&shared_cache_ledger));
                let outcome = host.run_turn(&turn)?;
                let ledger = host.cache_ledger().clone();
                (outcome, Some(ledger))
            }
        } else {
            (run_headless(turn, model, config)?, None)
        }
    };

    if let Some(runtime) = supervisor_runtime.as_ref() {
        let recovery =
            supervisor_recovery(supervisor_session_path.as_deref(), &shared_cache_ledger)?;
        if recovery.gate_chain_blocked() {
            eprintln!(
                "supervisor: latest gate-chain verdict is not passed; \
                 review it before resuming autonomous work"
            );
        }
        runtime.refresh_recovery(recovery)?;
        eprintln!(
            "supervisor: gateways={} mcp={} bridges={}",
            runtime.gateway_count(),
            runtime.mcp_count(),
            runtime.bridge_count()
        );
    }

    if context_agent.is_some() {
        let ledger = shared_cache_ledger
            .lock()
            .map_err(|_| fail("cache ledger is poisoned"))?;
        let text = serde_json::to_string_pretty(&*ledger)
            .map_err(|error| fail(format!("encode cache ledger: {error}")))?;
        if let Some(path) = cache_ledger_path.as_ref() {
            std::fs::write(path, text.as_bytes())
                .map_err(|error| fail(format!("write cache ledger {}: {error}", path.display())))?;
        }
        eprintln!("cache ledger: {text}");
    }
    let mut stdout = std::io::stdout().lock();
    let rendered = if output_format == "json" {
        serde_json::to_string_pretty(&outcome).map_err(|error| fail(error.to_string()))?
    } else {
        outcome.render_markdown()
    };
    writeln!(stdout, "{rendered}")?;
    if gate_chain_post || gate_chain_post_report.is_some() {
        let mut exclude: Vec<&Path> = Vec::new();
        if let Some(path) = gate_chain_report.as_deref() {
            exclude.push(path);
        }
        if let Some(path) = gate_chain_post_report.as_deref() {
            exclude.push(path);
        }
        if let Some(path) = session_path.as_deref() {
            exclude.push(path);
        }
        verify_post_alignment(
            &workspace_root,
            &manifest_path,
            gate_chain_post_report.as_deref(),
            &exclude,
            session_path.as_deref(),
        )?;
    }
    if !outcome.judgment.is_complete() {
        if let Some(runtime) = supervisor_runtime.as_mut() {
            runtime.stop();
        }
        std::process::exit(1);
    }
    Ok(())
}

fn supervisor_recovery(
    session_path: Option<&Path>,
    cache: &Arc<Mutex<CacheLedger>>,
) -> Result<RecoveryContext, Box<dyn Error>> {
    if let Some(path) = session_path {
        return Ok(recovery_context_for_session(path)?);
    }
    let cache = cache
        .lock()
        .map_err(|_| fail("cache ledger is poisoned"))?
        .clone();
    Ok(RecoveryContext {
        cache: Some(cache),
        ..RecoveryContext::default()
    })
}

struct CacheOptions {
    hit_threshold_bp: u32,
    low_hit_policy: LowHitPolicy,
    window: CacheWindow,
    pressure: CachePressurePolicy,
}

fn build_model(
    scripted: Option<Vec<Proposal>>,
    config: Option<&OpenAiConfig>,
    context: Option<&Arc<Mutex<AgentContext<InMemoryContext>>>>,
    cache_ledger: &Arc<Mutex<CacheLedger>>,
    cache_options: &CacheOptions,
    tool_catalog: Option<Arc<dyn ToolCatalog>>,
) -> Result<Box<dyn Model>, Box<dyn Error>> {
    if let Some(proposals) = scripted {
        return Ok(Box::new(ScriptedModel::new(proposals)));
    }
    let config = config.ok_or_else(|| fail("--provider openai requires OPENAI_* env"))?;
    let mut model = ProviderModel::with_tools(OpenAiProvider::new(config.clone()), default_tools());
    if let Some(catalog) = tool_catalog {
        model = model.with_tool_catalog(catalog);
    }
    if let Some(shared) = context {
        model = model.with_context_planner(Box::new(
            AgentContextPlanner::new(Arc::clone(shared))
                .with_cache_ledger(Arc::clone(cache_ledger))
                .with_hit_threshold_bp(cache_options.hit_threshold_bp)
                .with_low_hit_policy(cache_options.low_hit_policy)
                .with_cache_window(cache_options.window)
                .with_cache_pressure_policy(cache_options.pressure),
        ));
    }
    Ok(Box::new(model))
}

fn load_context_catalog(path: &Path) -> Result<InMemoryContext, Box<dyn Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| fail(format!("read context catalog {}: {error}", path.display())))?;
    let entries: Vec<ResolvedContext> = serde_json::from_str(&text)
        .map_err(|error| fail(format!("parse context catalog: {error}")))?;
    let mut catalog = InMemoryContext::default();
    for entry in entries {
        catalog.insert(entry.reference, entry.document)?;
    }
    Ok(catalog)
}

fn parse_search_order(value: &str) -> Result<SearchOrder, Box<dyn Error>> {
    match value {
        "keep" => Ok(SearchOrder::Resolver),
        "dynamic-first" => Ok(SearchOrder::PreferDynamic),
        other => Err(fail(format!("bad --cache-search-order: {other}"))),
    }
}

fn parse_task_selection(value: &str) -> Result<SelectionPressure, Box<dyn Error>> {
    match value {
        "normal" => Ok(SelectionPressure::Normal),
        "low-cost" => Ok(SelectionPressure::LowCacheHit),
        other => Err(fail(format!("bad --cache-task-selection: {other}"))),
    }
}

fn parse_low_hit_policy(value: &str) -> Result<LowHitPolicy, Box<dyn Error>> {
    match value {
        "keep" => Ok(LowHitPolicy::Keep),
        "demote" => Ok(LowHitPolicy::DemotePrefix),
        other => Err(fail(format!("bad --cache-low-hit-policy: {other}"))),
    }
}

fn parse_reasoning(value: &str) -> Result<ReasoningEffort, Box<dyn Error>> {
    match value {
        "off" => Ok(ReasoningEffort::Off),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        other => Err(fail(format!("bad --reasoning: {other}"))),
    }
}

fn next(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| fail(format!("{flag} requires a value")))
}

fn apply_write_guard(
    tools: Box<dyn ToolExecutor>,
    guard: Option<WriteGuard>,
) -> Box<dyn ToolExecutor> {
    match guard {
        Some(guard) => {
            let mut claiming = ClaimingTools::new(tools, guard.claim);
            if let Some(lease) = guard.lease {
                claiming = claiming.with_lease(lease);
            }
            if let Some(journal) = guard.journal {
                claiming = claiming.with_journal(journal);
            }
            Box::new(claiming)
        }
        None => tools,
    }
}

fn fail(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::new(ErrorKind::InvalidInput, message.into()))
}

/// 单次 `git status` 最多接受的变更路径数；超过就放弃自动取值（fail-closed）。
const MAX_CHANGED_PATHS: usize = 10_000;

/// 空设备路径：用来屏蔽用户级 / 系统级 git 配置。
fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// 构造只读 git 命令：禁用 fsmonitor、屏蔽全局/系统配置、把工作树钉在 workspace。
///
/// 仓库自己的 `.git/config` 可以把 `core.fsmonitor` 指向一个程序（`git status`
/// 会执行它），也可以把 `core.worktree` 指到 workspace 之外（扫描别的目录）。
/// 这里用命令行配置与环境变量覆盖这两点，仓库配置仍会被读，但不能改变执行面。
fn git_base_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    trit_host::child_env::sanitize(&mut command);
    command
        .arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_WORK_TREE", root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// 解析 `git status --porcelain -z`：返回 workspace 相对路径。
///
/// 非 UTF-8 路径、超过上限都返回 `Err`，由调用方放弃自动取值。
fn parse_git_status(prefix: &str, stdout: &[u8]) -> Result<Vec<String>, String> {
    let mut entries = stdout.split(|byte| *byte == 0);
    let mut paths = Vec::new();
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let path = std::str::from_utf8(&entry[3..])
            .map_err(|_| "git status returned a non-UTF-8 path".to_owned())?;
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        // rename / copy 的下一条 NUL 是旧路径，属于同一处变更。
        if status.contains(&b'R') || status.contains(&b'C') {
            let _ = entries.next();
        }
        let Some(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        paths.push(rest.replace('\\', "/"));
        if paths.len() > MAX_CHANGED_PATHS {
            return Err(format!(
                "git status returned more than {MAX_CHANGED_PATHS} paths"
            ));
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// 取 workspace 当前 git 变更路径（相对 workspace，含未跟踪文件）。
///
/// 不是 git 仓、命令失败、路径不是 UTF-8 或输出读不出时返回 `None`：调用方据此
/// 保持“未给 changed”的语义，而不是把空集当成“没有变更”。
fn git_changed(root: &Path) -> Option<Vec<String>> {
    // `git status` 没有 `--relative`，用 `--show-prefix` 拿到当前目录相对 repo 根
    // 的前缀，再把 status 的 repo-relative 路径裁成 workspace-relative。
    let prefix = {
        let output = git_base_command(root, &["rev-parse", "--show-prefix"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let Ok(text) = std::str::from_utf8(&output.stdout) else {
            eprintln!(
                "trit-host: git rev-parse returned a non-UTF-8 prefix; \
                 skipping automatic changed set"
            );
            return None;
        };
        text.trim_end_matches(['\r', '\n']).to_owned()
    };
    let output = git_base_command(
        root,
        &["status", "--porcelain", "--untracked-files=all", "-z"],
    )
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    match parse_git_status(&prefix, &output.stdout) {
        Ok(paths) => Some(paths),
        Err(reason) => {
            eprintln!("trit-host: {reason}; skipping automatic changed set");
            None
        }
    }
}

/// 把 `--alignment-changed` 的输入折成 workspace 相对的 `/` 路径。
fn normalize_changed(raw: &str, root: &Path) -> Result<String, Box<dyn Error>> {
    let text = raw.trim().replace('\\', "/");
    if text.is_empty() {
        return Err(fail("--alignment-changed must not be empty"));
    }
    let mut path = PathBuf::from(&text);
    if path.is_absolute() {
        let root_abs = std::path::absolute(root)
            .map_err(|error| fail(format!("resolve workspace {}: {error}", root.display())))?;
        let path_abs = std::path::absolute(&path)
            .map_err(|error| fail(format!("resolve --alignment-changed {raw}: {error}")))?;
        let Ok(stripped) = path_abs.strip_prefix(&root_abs) else {
            return Err(fail(format!(
                "--alignment-changed is outside the workspace: {raw}"
            )));
        };
        path = stripped.to_path_buf();
    }
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(fail(format!(
                        "--alignment-changed escapes the workspace: {raw}"
                    )));
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            _ => {
                return Err(fail(format!(
                    "--alignment-changed is not a workspace relative path: {raw}"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(fail("--alignment-changed must name a file"));
    }
    Ok(parts.join("/"))
}

/// run 结束后用实际落盘变更复核对齐：`changed_known=true`，不再接受子集语义。
///
/// `report_path` 为 `None` 时不落盘；`exclude` 是 pre 报告路径，报告文件本身
/// 不应被当成“未对齐变更”。
fn verify_post_alignment(
    root: &Path,
    manifest: &Path,
    report_path: Option<&Path>,
    exclude: &[&Path],
    session: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let Some(mut changed) = git_changed(root) else {
        return Err(fail(
            "gate chain post: cannot read actual git changes (not a git repository, \
             or more than 10000 changed paths)",
        ));
    };
    for path in exclude {
        if let Some(relative) = relative_to_root(root, path) {
            changed.retain(|item| item != &relative);
        }
    }
    let report = trit_orch::alignment::check(
        root,
        manifest,
        &AlignmentOptions {
            changed: Some(changed),
            changed_known: true,
            strict: true,
            executed: None,
        },
    );
    if let Some(path) = report_path {
        write_report(root, path, &report)?;
    }
    if let Some(path) = session {
        append_session_event(
            path,
            trit_host::SessionEvent::GateChain {
                phase: trit_host::GateChainPhase::Post,
                status: report.status.clone(),
                exit: report.exit,
                failed_at: report.failures.first().map(|failure| failure.code.clone()),
                failures: report.failures.len(),
                passed: 0,
                blocked: 0,
                unknown: 0,
                skipped: 0,
                report: report_path.map(|path| path.display().to_string()),
            },
        )?;
    }
    eprintln!(
        "gate chain post: {} (must score {:.2}, {} failure(s))",
        report.status,
        report.must_score,
        report.failures.len()
    );
    if report.status != "aligned" {
        let reason = report
            .failures
            .first()
            .map(|failure| format!("{}: {}", failure.code, failure.message))
            .unwrap_or_else(|| format!("alignment {}", report.status));
        return Err(fail(format!("gate chain post rejected: {reason}")));
    }
    Ok(())
}

/// 把报告写到 `root` 相对（或绝对）路径；父目录不存在时创建。
fn write_report(
    root: &Path,
    path: &Path,
    value: &impl serde::Serialize,
) -> Result<(), Box<dyn Error>> {
    let path =
        gate_chain_mod::resolve_report_path(root, path).map_err(|error| fail(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| fail(format!("create {}: {error}", parent.display())))?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| fail(format!("serialize report: {error}")))?;
    gate_chain_mod::write_report_file(&path, &bytes).map_err(|error| fail(error.to_string()))?;
    Ok(())
}

/// 追加一条审计事件到 session JSONL；`--session` 未给时调用方不调用。
fn append_session_event(path: &Path, event: trit_host::SessionEvent) -> Result<(), Box<dyn Error>> {
    let mut log = trit_host::SessionLog::open(path)
        .map_err(|error| fail(format!("open session {}: {error}", path.display())))?;
    log.append(event)
        .map_err(|error| fail(format!("append session {}: {error}", path.display())))?;
    Ok(())
}

/// `path` 相对 `root` 的 `/` 路径；不在 root 下或无法解析时返回 `None`。
fn relative_to_root(root: &Path, path: &Path) -> Option<String> {
    let root_abs = std::path::absolute(root).ok()?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let path_abs = std::path::absolute(&candidate).ok()?;
    let stripped = path_abs.strip_prefix(&root_abs).ok()?;
    let text = stripped.to_string_lossy();
    (!text.is_empty()).then(|| text.replace('\\', "/"))
}

fn usage() {
    eprintln!(
        "trit-host - headless Rust agent host\n\n\
         usage:\n  \
         trit-host run --task <text> --workspace <dir> \\\n    \
         [--repo-identity [--repo-expected <owner/repo>] [--task-kind agent|ternary|...]] \\\n    \
         [--gate-chain [--handoff-packet <path>] [--handoff-state-hash <hash>] \\\n    \
         [--handoff-attach] [--alignment-manifest <path>] \\\n    \
         [--alignment-changed <path>] [--alignment-changed-set] \\\n    \
         [--alignment-no-git-changed] [--gate-chain-post] \\\n    \
         [--gate-chain-report <path>] [--gate-chain-post-report <path>]] \\\n    \
         [--provider scripted|openai] [--script <proposals.json>] \\\n    \
         [--max-steps N] [--allow-write] [--session <jsonl>] [--resume] \\\n    \
         [--checkpoint-dir <dir> [--checkpoint-backend files|git] [--checkpoint-retain N]] \\\n    \
         [--allow-shell (--sandbox <program> [--sandbox-arg <arg>...] | --allow-unsandboxed-shell)] \\\n    \
         [--allow-hardlinks] [--write-claim <path>]... [--write-lease <owner>] \\\n    \
         [--output-format md|json] \\\n    \
         [--write-journal [--write-journal-rollback]] \\\n    \
         [--context-receipts N] [--context-chars N] [--context-output N] \\\n    \
         [--context-catalog <json> [--context-project <id>] [--context-session-id <id>] \\\n    \
         [--context-budget N] [--context-window N] [--model-max-output N] \\\n    \
         [--supports-reasoning] [--reasoning off|low|medium|high] \\\n    \
         [--context-read-budget N] [--context-read-max N] \\\n    \
         [--cache-hit-threshold BP] [--cache-low-hit-policy keep|demote] \\\n    \
         [--cache-window-samples N] [--cache-window-ms N] \\\n    \
         [--cache-pressure-scale-bp N] [--cache-search-scale-bp N] \\\n    \
         [--cache-search-order keep|dynamic-first] \\\n    \
         [--cache-task-selection normal|low-cost] [--cache-ledger <json>] \\\n    \
         [--supervisor-config <json>] [--mcp-sampling [--mcp-sampling-model <name>]]]\n\n\
         trit-host plan-exec --task <text> --workspace <dir> \\\n    \
         --planner-script <proposals.json> --script <proposals.json> \\\n    \
         [--allow-write [--write-journal]] \\\n    \
         [--max-plan-steps N] [--max-exec-steps N] [--capsule-budget N] \\\n    \
         [--output-format md|json]\n\n\
         scripted: deterministic replay from a JSON array of Proposal objects.\n\
         openai:   OpenAI-compatible /chat/completions; set OPENAI_API_KEY,\n\
         OPENAI_MODEL and optionally OPENAI_BASE_URL.\n\
         Writes are disabled unless --allow-write; links are rejected unless --allow-hardlinks.\n\
         --output-format: md (default) renders a Markdown turn report; json keeps TurnOutcome.\n\
         --repo-identity: fail closed unless .agent/repo.json is valid; --repo-expected\n\
         additionally pins owner/repo, and agent tasks require owns_agent_runtime=true.\n\
         If the workspace already contains .agent/repo.json, the gate runs automatically.\n\
         --gate-chain: run the read-only repo -> delivery -> alignment chain before the run.\n\
         It needs a handoff packet and an alignment manifest (default .agent/alignment.json);\n\
         Unknown or False stops the chain and the run does not start.\n\
         --handoff-attach: also write sent + delivered when the delivery gate passes.\n\
         --alignment-changed: repeatable changed-path override (normalized to workspace\n\
         relative / paths); otherwise workspace git status is used as a subset.\n\
         --alignment-no-git-changed disables that lookup; --alignment-changed-set marks\n\
         the set complete and enforces declared changes.\n\
         --gate-chain-post: after the run, re-check alignment against actual git changes\n\
         (complete set); a failure exits 2 and does not roll back the run.\n\
         --gate-chain-report / --gate-chain-post-report: write the pre/post report JSON\n\
         to the given path (workspace relative); report files are excluded from post checks.\n\
         Shell needs an OS sandbox; --allow-unsandboxed-shell is an explicit downgrade.\n\
         --sandbox builtin: Linux bwrap / macOS sandbox-exec / Windows AppContainer.\n\
         --write-claim: restrict write_file to declared paths (globs allowed; quote in shell).\n\
         --write-lease: acquire <workspace>/.agent/write-lease before running\n\
         (requires --write-claim; exits 2 when another owner holds it).\n\
         --write-journal: append begin/finish records to\n\
         <workspace>/.agent/write-journal.jsonl and warn about in-doubt writes.\n\
         --write-journal-rollback: roll back in-doubt writes at startup\n\
         (requires --write-journal).\n\
         --resume reloads prior turns from --session and appends one new turn.\n\
         checkpoint: files (per-file snapshots) or git (shadow bare repo of the workspace).\n\
         context-catalog: JSON array of {{reference, document}} entries; enables agent-driven\n\
         context_search / context_add / context_read with dynamic-tail-only additions.\n\
         cache-hit-threshold: basis points (default 5000); low-hit-policy keep|demote (default keep).\n\
         cache-ledger: optional JSON file loaded before the run and rewritten after it.\n\
         cache-search-scale-bp: scales context_search candidates on low cache hit.\n\
         supervisor-config: JSON {{gateway:[ProcessSpec], mcp:[ProcessSpec],\n\
         bridges:[PortBridgeSpec], roots:[McpRoot], roots_watch:[path],\n\
         roots_watch_debounce_ms, log_level:debug|info|warning|error, restart:{{...}},\n\
         interval_ms, mcp_timeout_ms}}; starts supervised sidecars and\n\
         keep-alive port bridges, restarts them after exit, discovers MCP tools/resources/\n\
         prompts, auto-refreshes list_changed, and reconnects MCP with session state/cache\n\
         recovery. --mcp-sampling serves sampling/createMessage with the OpenAI-compatible\n\
         provider (optionally --mcp-sampling-model); roots_watch directories contribute\n\
         roots automatically and changes emit roots/list_changed.\n\n\
         session-status: trit-host session-status --session <jsonl>\n\
         prints turn count and the latest gate-chain verdict; exits 1 when blocked.\n\
         session-index: trit-host session-index --session <jsonl> --db <path>\n\
         [--rebuild|--verify]\n\
         maintains the SQLite projection of a session log; --rebuild deletes the old\n\
         projection and rebuilds it from JSONL, --verify rebuilds into a scratch db and\n\
         compares rows (exits 1 when the projection is missing or differs).\n\
         session-scan: trit-host session-scan --root <dir> [--compact]\n\
         scans session JSONL files under a directory and prints tasks, links and\n\
         per-session health (open turns, corrupt lines, in-doubt tools, last gate).\n\
         diagnostics: trit-host diagnostics --workspace <dir> [--session <jsonl>] [--db <path>]\n\
         prints a static, read-only capability report (kernel schema versions, built-in\n\
         tool catalog, write-journal reconciliation, optional session/projection summary).\n\
         it starts no MCP/gateway/subprocess and creates no files.\n\
         journal-status: trit-host journal-status --workspace <dir> [--rollback] [--prune]\n\
         prints the write-journal reconciliation; --rollback restores in-doubt writes\n\
         only after reacquiring their recorded lease locks (a live holder blocks it);\n\
         --prune deletes pre-image dirs whose operations closed successfully or have\n\
         no journal record, keeping open begins and failed operations;\n\
         exits 1 on remaining doubt, blocked rollback or rollback failure, 2 on prune errors.\n\
         rewind-status: trit-host rewind-status --checkpoint-dir <dir> [--checkpoint-backend files|git]\n\
         [--recover] [--workspace <root>]\n\
         prints the checkpoint rewind journal; --recover restores the pre-rewind state from\n\
         backups (git backend uses the whole-tree backup commit). git requires --workspace.\n\
         exits 1 when a rewind is still unfinished or rollback/recovery failed\n\
         (a new rewind will refuse until this is resolved).\n\
         appcontainer-prune: trit-host appcontainer-prune --workspace <dir>\n\
         deletes AppContainer profiles recorded for the workspace.\n\n\
         rewind: trit-host rewind --session <jsonl> --checkpoint-dir <dir> --turn N \\\n\
         [--checkpoint-backend files|git] [--workspace <dir>] [--scope code|conversation|both] \\\n\
         [--allow-unrewindable]\n\n\
         remote-echo: trit-host remote-echo [--bind <addr>] [--token <t>] [--once] \\\n\
         [--fail] [--silent] [--sleep-ms N] [--request-timeout-ms N] [--request-read-timeout-ms N] \\\n\
         [--max-connections N] [--max-cache-bytes N] [--cache-ttl-ms N] [--ledger <url>] [--ledger-token <t>] \\\n\
         [--ledger-timeout-secs N] [--ledger-renew-ms N]\n\
         reference remote endpoint: HTTP POST of one subagent frame per request;\n\
         prints {{\"bind\":\"127.0.0.1:PORT\"}} before accepting.\n\n\
         remote-ledger: trit-host remote-ledger [--bind <addr>] [--token <t>] \\\n\
         [--pending-ttl-ms N] [--max-entries N] [--max-state-bytes N] [--state <path>] \\\n\
         [--max-connections N] [--request-read-timeout-ms N] [--once]\n\
         shared idempotency ledger: POST /claim, /renew and /complete coordinate retries across nodes;\n\
         prints {{\"bind\":\"127.0.0.1:PORT\"}} before accepting.\n\n\
         remote-exec: trit-host remote-exec [--bind <addr>] [--token <t>] \\\n\
         (--workspace <dir> | --workspace-root <dir>) --script <proposals.json> \\\n\
         [--max-steps N] [--exec-timeout-secs N] [--request-timeout-ms N] [--request-read-timeout-ms N] \\\n\
         [--once] [--max-connections N] [--max-cache-bytes N] [--cache-ttl-ms N] [--ledger <url>] [--ledger-token <t>] \\\n\
         [--ledger-timeout-secs N] [--ledger-renew-ms N]\n\
         executable endpoint: spawns subagent-run per request (stdin/stdout frames).\n\n\
         remote-plan: trit-host remote-plan [--input <json|->] [--health <path>] [--probe]\n\
         [--probe-interval-ms N] [--probe-concurrency N] [--health-flush-ms N]\n\
         [--health-max-age-ms N]\n\
         cross-machine drill: reads {{revision,tasks,nodes,workers?,failure_threshold?,retry_limit?,scope?}},\n\
         dispatches tasks as read-only subagent requests to the static node table, prints JSON.\n\
         --probe-concurrency bounds parallel GET /health inside one scan (1..64, needs a probe mode).\n\
         --health-flush-ms defaults to 30000 when --health is set; 0 disables periodic rewrite.\n\
         --health-max-age-ms isolates healthy entries older than N ms until a probe readmits them.\n\n\
         remote-health: trit-host remote-health --nodes <json> --health <path> [--interval-ms N] [--once] [--probe-all]\n\
         [--probe-concurrency N]\n\
         standalone health monitor: reloads --nodes each sweep, probes isolated nodes,\n\
         rewrites the health file via temp+rename, takes an OS lock on <health>.lock;\n\
         probe concurrency defaults to 4 (max 64), events stay sorted by node id;\n\
         runs forever by default, --once for cron/Task Scheduler, one JSON line per sweep."
    );
}

/// `session-status --session <jsonl>`：只读输出 turn 数与最新门链裁决。
///
/// 退出码：`0` 正常（最新门链未阻塞），`1` 最新门链阻塞，`2` 参数/读取错误。
fn run_session_status(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut session = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => session = Some(PathBuf::from(next(&mut args, "--session")?)),
            other => return Err(fail(format!("unknown session-status argument: {other}"))),
        }
    }
    let session = session.ok_or_else(|| fail("--session is required"))?;
    let records = trit_host::SessionLog::open(&session)?.load()?;
    let state = trit_host::recover(&records)?;
    let latest = state.gate_chains.last();
    let blocked = latest.is_some_and(|record| !record.passed());
    let status = serde_json::json!({
        "session": session.display().to_string(),
        "turns": state.turns.len(),
        "latest_metrics": state.turns.last().and_then(|turn| turn.metrics.clone()),
        "gate_chains": {
            "history": state.gate_chains.len(),
            "latest": latest,
            "blocked": blocked,
        },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&status).map_err(|error| fail(error.to_string()))?
    );
    if blocked {
        std::process::exit(1);
    }
    Ok(())
}

/// `session-scan --root <dir> [--compact]`：只读扫描会话目录，输出清单/任务/链接。
fn run_session_scan(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut root = PathBuf::from(".");
    let mut compact = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(next(&mut args, "--root")?),
            "--compact" => compact = true,
            other => return Err(fail(format!("unknown session-scan argument: {other}"))),
        }
    }
    let report = trit_host::scan_sessions(&root)?;
    let text = if compact {
        serde_json::to_string(&report)
    } else {
        serde_json::to_string_pretty(&report)
    }
    .map_err(|error| fail(error.to_string()))?;
    println!("{text}");
    Ok(())
}

/// `session-index --session <jsonl> --db <path> [--rebuild|--verify]`：
/// 维护会话日志的 SQLite 投影。JSONL 是权威，投影可随时删掉重建。
fn run_session_index(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut session = None;
    let mut db = None;
    let mut rebuild = false;
    let mut verify = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => session = Some(PathBuf::from(next(&mut args, "--session")?)),
            "--db" => db = Some(PathBuf::from(next(&mut args, "--db")?)),
            "--rebuild" => rebuild = true,
            "--verify" => verify = true,
            other => return Err(fail(format!("unknown session-index argument: {other}"))),
        }
    }
    let session = session.ok_or_else(|| fail("session-index requires --session"))?;
    let db = db.ok_or_else(|| fail("session-index requires --db"))?;
    if rebuild && verify {
        return Err(fail("--rebuild and --verify are mutually exclusive"));
    }
    if rebuild {
        let report = trit_host::SessionIndex::rebuild(&session, &db)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "session": session.display().to_string(),
                "db": report.path,
                "replaced": report.replaced,
                "summary": report.summary,
            }))
            .map_err(|error| fail(error.to_string()))?
        );
        return Ok(());
    }
    if verify {
        let report = trit_host::SessionIndex::verify(&session, &db)?;
        let matches = report.matches;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| fail(error.to_string()))?
        );
        if !matches {
            std::process::exit(1);
        }
        return Ok(());
    }
    let summary = trit_host::SessionIndex::open(&db).summary()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "session": session.display().to_string(),
            "db": db.display().to_string(),
            "summary": summary,
        }))
        .map_err(|error| fail(error.to_string()))?
    );
    Ok(())
}

/// `diagnostics --workspace <dir> [--session <jsonl>] [--db <path>]`：
/// 只读静态诊断。不启动 MCP / gateway / 子进程，也不创建目录或账本。
fn run_diagnostics(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut workspace = PathBuf::from(".");
    let mut session = None;
    let mut db = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--session" => session = Some(PathBuf::from(next(&mut args, "--session")?)),
            "--db" => db = Some(PathBuf::from(next(&mut args, "--db")?)),
            other => return Err(fail(format!("unknown diagnostics argument: {other}"))),
        }
    }
    let report = trit_host::diagnose(&trit_host::DiagnosticsOptions {
        workspace: &workspace,
        session: session.as_deref(),
        projection: db.as_deref(),
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| fail(error.to_string()))?
    );
    Ok(())
}

/// `appcontainer-run --workspace <dir> -- <program> [args...]`：在 Windows
/// AppContainer 内运行命令，并把退出码原样返回；失败不回落。
fn run_appcontainer(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut workspace = PathBuf::from(".");
    let mut command: Vec<String> = Vec::new();
    let mut after_separator = false;
    while let Some(arg) = args.next() {
        if after_separator {
            command.push(arg);
            continue;
        }
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--" => after_separator = true,
            other => return Err(fail(format!("unknown appcontainer-run argument: {other}"))),
        }
    }
    if command.is_empty() {
        return Err(fail("appcontainer-run requires a command after --"));
    }
    #[cfg(windows)]
    {
        let code = match trit_host::appcontainer::run(&workspace, &command[0], &command[1..]) {
            Ok(code) => code,
            Err(error) => return Err(fail(error)),
        };
        std::process::exit(code);
    }
    #[cfg(not(windows))]
    {
        let _ = (workspace, command);
        Err(fail("appcontainer-run is Windows only"))
    }
}

/// `appcontainer-prune --workspace <dir>`：删除本 workspace 记录过的 AppContainer profile。
fn run_appcontainer_prune(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut workspace = PathBuf::from(".");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            other => {
                return Err(fail(format!(
                    "unknown appcontainer-prune argument: {other}"
                )));
            }
        }
    }
    #[cfg(windows)]
    {
        let removed = match trit_host::appcontainer::prune(&workspace) {
            Ok(removed) => removed,
            Err(error) => return Err(fail(error)),
        };
        println!("pruned {removed} AppContainer profile(s)");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = workspace;
        Err(fail("appcontainer-prune is Windows only"))
    }
}

/// 回滚 in-doubt 写操作；有租约记录时先拿到同一把锁。
///
/// `owned` 是当前进程已持有的租约：它覆盖的锁不必重复获取。仍被其他活进程
/// 占着的操作不会回滚，而是进入 `blocked`，由调用方决定退出或中止启动。
fn rollback_in_doubt(
    journal: &mut MutationJournal,
    workspace: &Path,
    records: &[trit_host::JournalRecord],
    owned: Option<&OperationLease>,
) -> Result<(Vec<trit_host::RollbackOutcome>, Vec<serde_json::Value>), Box<dyn Error>> {
    let owned_keys: Vec<String> = owned
        .map(|lease| {
            lease
                .lock_paths()
                .into_iter()
                .map(|path| lock_key(workspace, path))
                .collect()
        })
        .unwrap_or_default();
    let mut outcomes = Vec::new();
    let mut blocked = Vec::new();
    for record in records {
        let mut guard: Option<OperationLease> = None;
        if record.lease_locks.is_empty() {
            if record.lease_epoch.is_some() {
                blocked.push(serde_json::json!({
                    "op_id": record.op_id,
                    "reason": "lease epoch recorded without lock paths; refuse automatic rollback",
                }));
                continue;
            }
        } else {
            let missing: Vec<PathBuf> = record
                .lease_locks
                .iter()
                .filter(|lock| !owned_keys.contains(&lock_key(workspace, Path::new(lock))))
                .map(PathBuf::from)
                .collect();
            if !missing.is_empty() {
                match OperationLease::acquire_lock_files(workspace, "journal-rollback", &missing) {
                    Ok(lease) => guard = Some(lease),
                    Err(trit_host::LeaseError::Busy { owner, epoch }) => {
                        blocked.push(serde_json::json!({
                            "op_id": record.op_id,
                            "reason": format!("lease still held: owner={owner} epoch={epoch}"),
                        }));
                        continue;
                    }
                    Err(error) => {
                        return Err(fail(format!("journal rollback lease: {error}")));
                    }
                }
            }
        }
        let outcome = journal.rollback(record);
        journal
            .record_rollback(record.op_id, &outcome)
            .map_err(|error| fail(error.to_string()))?;
        drop(guard);
        outcomes.push(outcome);
    }
    Ok((outcomes, blocked))
}

fn lock_key(workspace: &Path, lock: &Path) -> String {
    let full = if lock.is_absolute() {
        lock.to_path_buf()
    } else {
        workspace.join(lock)
    };
    full.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// `journal-status --workspace <dir>`：只读输出 mutation journal 对账结果。
///
/// 退出码：`0` 没有未闭合写，`1` 有 `in_doubt`，`2` 参数/读取错误。
fn run_journal_status(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut workspace = PathBuf::from(".");
    let mut rollback = false;
    let mut prune = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--rollback" => rollback = true,
            "--prune" => prune = true,
            other => return Err(fail(format!("unknown journal-status argument: {other}"))),
        }
    }
    let path = workspace.join(trit_host::JOURNAL_PATH);
    if !path.is_file() {
        // 没有账本时输出空报告，且不创建 `.agent/`（只读命令不产生副作用）。
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "report": trit_host::JournalReport::default(),
                "rollbacks": [],
                "blocked": [],
                "prune": if prune {
                    Some(serde_json::json!({"removed": 0, "kept": 0, "failures": []}))
                } else {
                    None
                },
            }))
            .map_err(|error| fail(error.to_string()))?
        );
        return Ok(());
    }
    let mut journal = MutationJournal::open(&workspace).map_err(|error| fail(error.to_string()))?;
    let report = journal.report().map_err(|error| fail(error.to_string()))?;
    let (rollbacks, blocked) = if rollback {
        rollback_in_doubt(&mut journal, &workspace, &report.in_doubt, None)?
    } else {
        (Vec::new(), Vec::new())
    };
    let after = if rollbacks.is_empty() {
        report
    } else {
        journal.report().map_err(|error| fail(error.to_string()))?
    };
    let prune_report = if prune {
        Some(
            journal
                .prune_backups()
                .map_err(|error| fail(error.to_string()))?,
        )
    } else {
        None
    };
    let value = serde_json::json!({
        "report": after,
        "rollbacks": rollbacks,
        "blocked": blocked,
        "prune": prune_report,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| fail(error.to_string()))?
    );
    if after.has_in_doubt() || rollbacks.iter().any(|outcome| !outcome.ok()) || !blocked.is_empty()
    {
        std::process::exit(1);
    }
    if value["prune"]["failures"]
        .as_array()
        .is_some_and(|failures| !failures.is_empty())
    {
        std::process::exit(2);
    }
    Ok(())
}

/// `rewind-status --checkpoint-dir <dir> [--recover --workspace <root>]`：
/// 默认只读检查回退 journal；加 `--recover` 则按 pre-rewind 备份恢复半写工作区，
/// 恢复后仍有未闭合操作时退出 1。
fn run_rewind_status(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut checkpoint_dir: Option<PathBuf> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut backend = "files".to_owned();
    let mut recover = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--checkpoint-dir" => {
                checkpoint_dir = Some(PathBuf::from(next(&mut args, "--checkpoint-dir")?));
            }
            "--checkpoint-backend" => {
                backend = next(&mut args, "--checkpoint-backend")?;
                if backend != "files" && backend != "git" {
                    return Err(fail(format!(
                        "unknown rewind-status backend: {backend} (expected files or git)"
                    )));
                }
            }
            "--workspace" => {
                workspace = Some(PathBuf::from(next(&mut args, "--workspace")?));
            }
            "--recover" => recover = true,
            other => return Err(fail(format!("unknown rewind-status argument: {other}"))),
        }
    }
    let dir = checkpoint_dir.ok_or_else(|| fail("rewind-status requires --checkpoint-dir"))?;
    let workspace = match workspace {
        Some(path) => path,
        None if backend == "git" => {
            return Err(fail(
                "rewind-status --checkpoint-backend git requires --workspace",
            ));
        }
        None if recover => {
            return Err(fail("rewind-status --recover requires --workspace"));
        }
        None => PathBuf::from("."),
    };
    if backend == "git" {
        let store = GitCheckpointStore::open(dir, workspace)?;
        if recover {
            let recovery = store.recover_rewind()?;
            let status = store.rewind_status()?;
            let payload = serde_json::json!({ "recovery": recovery, "status": status });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).map_err(|error| fail(error.to_string()))?
            );
            if status.has_in_doubt() {
                std::process::exit(1);
            }
            return Ok(());
        }
        let report = store.rewind_status()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| fail(error.to_string()))?
        );
        if report.has_in_doubt() {
            std::process::exit(1);
        }
        return Ok(());
    }
    let store = CheckpointStore::open(dir)?;
    if recover {
        let recovery = store.recover_rewind(&workspace)?;
        let status = store.rewind_status()?;
        let payload = serde_json::json!({ "recovery": recovery, "status": status });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).map_err(|error| fail(error.to_string()))?
        );
        if status.has_in_doubt() {
            std::process::exit(1);
        }
        return Ok(());
    }
    let report = store.rewind_status()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| fail(error.to_string()))?
    );
    if report.has_in_doubt() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_rewind(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut session = None;
    let mut checkpoint_dir = None;
    let mut checkpoint_backend = "files".to_owned();
    let mut workspace = PathBuf::from(".");
    let mut turn = None;
    let mut scope = "both".to_owned();
    let mut allow_unrewindable = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => session = Some(PathBuf::from(next(&mut args, "--session")?)),
            "--checkpoint-dir" => {
                checkpoint_dir = Some(PathBuf::from(next(&mut args, "--checkpoint-dir")?));
            }
            "--checkpoint-backend" => checkpoint_backend = next(&mut args, "--checkpoint-backend")?,
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--turn" => {
                turn = Some(
                    next(&mut args, "--turn")?
                        .parse()
                        .map_err(|error| fail(format!("bad --turn: {error}")))?,
                );
            }
            "--scope" => scope = next(&mut args, "--scope")?,
            "--allow-unrewindable" => allow_unrewindable = true,
            "--help" | "-h" => {
                usage();
                return Ok(());
            }
            other => return Err(fail(format!("unknown argument: {other}"))),
        }
    }
    let scope = match scope.as_str() {
        "code" => RewindScope::Code,
        "conversation" => RewindScope::Conversation,
        "both" => RewindScope::Both,
        other => return Err(fail(format!("bad --scope: {other}"))),
    };
    let session = session.ok_or_else(|| fail("rewind requires --session"))?;
    let checkpoint_dir = checkpoint_dir.ok_or_else(|| fail("rewind requires --checkpoint-dir"))?;
    let turn = turn.ok_or_else(|| fail("rewind requires --turn"))?;
    let report = match checkpoint_backend.as_str() {
        "git" => rewind_session_git_with_policy(
            &session,
            &checkpoint_dir,
            &workspace,
            turn,
            scope,
            allow_unrewindable,
        )?,
        "files" => rewind_session_with_policy(
            &session,
            &checkpoint_dir,
            &workspace,
            turn,
            scope,
            allow_unrewindable,
        )?,
        other => return Err(fail(format!("bad --checkpoint-backend: {other}"))),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| fail(error.to_string()))?
    );
    Ok(())
}

/// `subagent-echo`：测试/演示用子 agent。读一个请求帧，回一个响应帧。
///
/// 选项：`--fail` 回 Failed；`--silent` 不回响应直接退出；`--sleep-ms N` 延迟
/// N 毫秒；`--task <id>` 用指定任务 ID 回显（用于构造引用不匹配）；
/// `--output-bytes N` 读完请求帧后不回应，直接写 N 个原始字节，用于验证宿主的
/// 有界读。
fn run_subagent_echo(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut fail_mode = false;
    let mut silent = false;
    let mut sleep_ms = 0u64;
    let mut task_override: Option<String> = None;
    let mut output_bytes = 0usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fail" => fail_mode = true,
            "--silent" => silent = true,
            "--sleep-ms" => sleep_ms = next(&mut args, "--sleep-ms")?.parse()?,
            "--task" => task_override = Some(next(&mut args, "--task")?),
            "--output-bytes" => output_bytes = next(&mut args, "--output-bytes")?.parse()?,
            other => return Err(fail(format!("unknown subagent-echo argument: {other}"))),
        }
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let request: trit_orch::SubagentRequest = trit_orch::decode_frame(&mut input)?
        .ok_or_else(|| fail("subagent-echo: no request frame"))?;
    if output_bytes > 0 {
        let mut output = std::io::stdout().lock();
        let chunk = [b'x'; 1024];
        let mut remaining = output_bytes;
        while remaining > 0 {
            let take = remaining.min(chunk.len());
            output.write_all(&chunk[..take])?;
            remaining -= take;
        }
        output.flush()?;
        return Ok(());
    }
    if sleep_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
    if silent {
        return Ok(());
    }
    let status = if fail_mode {
        trit_orch::DelegationStatus::Failed {
            message: "scripted failure".into(),
        }
    } else {
        trit_orch::DelegationStatus::Completed
    };
    let response = trit_orch::SubagentResponse {
        task: task_override.unwrap_or_else(|| request.task.clone()),
        epoch: request.epoch,
        status,
        content: request.task.as_bytes().to_vec(),
        tokens: 1,
    };
    let frame = trit_orch::encode_frame(&response)?;
    std::io::stdout().write_all(&frame)?;
    Ok(())
}

/// `remote-echo [--bind <addr>] [--token <t>] [--once] [--fail] [--silent] [--sleep-ms N]`
/// `[--request-timeout-ms N] [--request-read-timeout-ms N] [--max-connections N] [--max-cache-bytes N]`
/// `[--cache-ttl-ms N]`（0 关闭按时间淘汰）
/// `[--ledger <url>] [--ledger-token <t>]`
/// `[--ledger-timeout-secs N] [--ledger-renew-ms N]`：
/// 远程端点的参考服务端。HTTP、帧与鉴权规则见 `trit_host::remote_serve`。
///
/// 启动后先向 stdout 打一行 `{"bind":"127.0.0.1:PORT"}`（含系统分配的端口），
/// 再开始 accept；`--once` 服务一个连接后退出，供端到端测试使用。
fn run_remote_echo(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut config = trit_host::RemoteServerConfig::new("127.0.0.1:0");
    let mut once = false;
    let mut ledger_url: Option<String> = None;
    let mut ledger_token: Option<String> = None;
    let mut ledger_timeout_secs = 30u64;
    let mut ledger_renew_ms: Option<u64> = None;
    let mut request_timeout_ms: Option<u64> = None;
    let mut request_read_timeout_ms: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => config.bind = next(&mut args, "--bind")?,
            "--token" => config.token = Some(next(&mut args, "--token")?),
            "--once" => once = true,
            "--fail" => config.mode = trit_host::RemoteMode::Fail,
            "--silent" => config.mode = trit_host::RemoteMode::Silent,
            "--sleep-ms" => config.sleep_ms = next(&mut args, "--sleep-ms")?.parse()?,
            "--request-timeout-ms" => {
                request_timeout_ms = Some(next(&mut args, "--request-timeout-ms")?.parse()?)
            }
            "--request-read-timeout-ms" => {
                request_read_timeout_ms =
                    Some(next(&mut args, "--request-read-timeout-ms")?.parse()?)
            }
            "--max-connections" => {
                config.max_connections = next(&mut args, "--max-connections")?.parse()?
            }
            "--max-cache-bytes" => {
                config.max_cache_bytes = next(&mut args, "--max-cache-bytes")?.parse()?
            }
            "--cache-ttl-ms" => {
                config = config.with_cache_ttl(std::time::Duration::from_millis(
                    next(&mut args, "--cache-ttl-ms")?.parse()?,
                ))
            }
            "--ledger" => ledger_url = Some(next(&mut args, "--ledger")?),
            "--ledger-token" => ledger_token = Some(next(&mut args, "--ledger-token")?),
            "--ledger-timeout-secs" => {
                ledger_timeout_secs = next(&mut args, "--ledger-timeout-secs")?.parse()?
            }
            "--ledger-renew-ms" => {
                ledger_renew_ms = Some(next(&mut args, "--ledger-renew-ms")?.parse()?)
            }
            other => return Err(fail(format!("unknown remote-echo argument: {other}"))),
        }
    }
    if let Some(url) = ledger_url {
        config.ledger = Some(trit_orch::RemoteEndpoint {
            url,
            auth_token: ledger_token,
            timeout_secs: ledger_timeout_secs,
        });
    } else if ledger_token.is_some() {
        return Err(fail("--ledger-token requires --ledger <url>"));
    }
    if let Some(ms) = ledger_renew_ms {
        if config.ledger.is_none() {
            return Err(fail("--ledger-renew-ms requires --ledger <url>"));
        }
        config.ledger_renew_interval = std::time::Duration::from_millis(ms);
    }
    if let Some(ms) = request_timeout_ms {
        config = config.with_exec_timeout(std::time::Duration::from_millis(ms));
    }
    if let Some(ms) = request_read_timeout_ms {
        config = config.with_request_read_timeout(std::time::Duration::from_millis(ms));
    }
    let server = trit_host::RemoteServer::bind(config)?;
    let addr = server.addr()?;
    println!("{}", serde_json::json!({ "bind": addr.to_string() }));
    std::io::stdout().flush()?;
    if once {
        server.serve_once()?;
        return Ok(());
    }
    server.serve_forever()?;
    Ok(())
}

/// `remote-plan` 在提供 `--health` 时默认每 30s 把节点健康写回一次；长演练
/// 中途崩溃也只丢最近一个周期的变化。`--health-flush-ms 0` 关闭。
const DEFAULT_HEALTH_FLUSH_MS: u64 = 30_000;

/// `remote-plan [--input <json|->]`：跨机演练。
///
/// 输入是一个 JSON 文档：`{revision, tasks, nodes, workers?, failure_threshold?,
/// retry_limit?, scope?}`。`scope` 是计划作用域，多个计划共享账本/workspace 根时应
/// 使用不同值。任务按 `TaskSpec` 反序列化，节点按 `RemoteNode` 反序列化；
/// 每个任务都映射成只读 subagent 请求（capsule 的 `goal` 取任务引导或任务 ID），
/// 由 `run_remote_plan` 复用 fleet 调度并输出 JSON 报告（含每次尝试与节点快照）。
/// `--probe` 会在加载健康文件后对已隔离节点做一次 `GET /health`，成功才重新放行。
/// `--probe-interval-ms` 会在本次演练运行期间周期探测已隔离节点，成功后重新放行；
/// 它要求 `--health <path>`，且只在本进程内有效，不做常驻后台监控。
/// `--probe-concurrency N` 限制一次 preflight/周期扫描内的并行探测数（1..=64，
/// 需配合 `--probe` 或 `--probe-interval-ms`；未指定时沿用库级默认的串行探测）。
/// `--health` 还存在时默认每 30s 把当前节点健康写回文件（`--health-flush-ms` 可调，
/// 0 关闭），避免长演练中途崩溃只在正常结束才落盘；停止写回线程后才做最终写回。
/// `--health-max-age-ms N` 把健康文件里超过 N ms 未更新的 `healthy=true` 条目
/// 先按未知隔离，报告中列入 `stale`；它们只有经 `--probe` / `--probe-interval-ms`
/// 探测成功才会重新参与派发。
/// 两种探测都映射到库级 `RemotePlanConfig::with_health_probe_*`，事件来自报告的
/// `probes`；库级调用方不依赖 `--health` 文件也能使用同一能力。
/// 这是演练入口，不自带真实模型：远端需要能执行 subagent 帧的服务端。
fn run_remote_plan_cli(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut input_path: Option<String> = None;
    let mut health_path: Option<String> = None;
    let mut probe = false;
    let mut probe_interval_ms: Option<u64> = None;
    let mut probe_concurrency: Option<usize> = None;
    let mut health_flush_ms: Option<u64> = None;
    let mut health_max_age_ms: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" => input_path = Some(next(&mut args, "--input")?),
            "--health" => health_path = Some(next(&mut args, "--health")?),
            "--probe" => probe = true,
            "--probe-interval-ms" => {
                probe_interval_ms = Some(next(&mut args, "--probe-interval-ms")?.parse()?)
            }
            "--probe-concurrency" => {
                probe_concurrency = Some(next(&mut args, "--probe-concurrency")?.parse()?)
            }
            "--health-flush-ms" => {
                health_flush_ms = Some(next(&mut args, "--health-flush-ms")?.parse()?)
            }
            "--health-max-age-ms" => {
                health_max_age_ms = Some(next(&mut args, "--health-max-age-ms")?.parse()?)
            }
            other => return Err(fail(format!("unknown remote-plan argument: {other}"))),
        }
    }
    if probe && health_path.is_none() {
        return Err(fail("--probe requires --health <path>"));
    }
    if probe_interval_ms.is_some() && health_path.is_none() {
        return Err(fail("--probe-interval-ms requires --health <path>"));
    }
    if health_flush_ms.is_some() && health_path.is_none() {
        return Err(fail("--health-flush-ms requires --health <path>"));
    }
    if health_max_age_ms.is_some() && health_path.is_none() {
        return Err(fail("--health-max-age-ms requires --health <path>"));
    }
    if health_max_age_ms == Some(0) {
        return Err(fail("--health-max-age-ms must be positive"));
    }
    if health_max_age_ms.is_some() && !probe && probe_interval_ms.is_none() {
        return Err(fail(
            "--health-max-age-ms requires --probe or --probe-interval-ms",
        ));
    }
    if probe_concurrency.is_some() && !probe && probe_interval_ms.is_none() {
        return Err(fail(
            "--probe-concurrency requires --probe or --probe-interval-ms",
        ));
    }
    if probe_interval_ms == Some(0) {
        return Err(fail("--probe-interval-ms must be positive"));
    }
    if let Some(concurrency) = probe_concurrency {
        if concurrency == 0 {
            return Err(fail("--probe-concurrency must be positive"));
        }
        if concurrency > trit_host::MAX_HEALTH_PROBE_CONCURRENCY {
            return Err(fail(format!(
                "--probe-concurrency must be <= {}",
                trit_host::MAX_HEALTH_PROBE_CONCURRENCY
            )));
        }
    }
    let probe_interval = probe_interval_ms.map(std::time::Duration::from_millis);
    let health_flush_interval =
        std::time::Duration::from_millis(health_flush_ms.unwrap_or(DEFAULT_HEALTH_FLUSH_MS));
    let text = match input_path.as_deref() {
        None | Some("-") => {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| fail(format!("read remote plan input {path}: {error}")))?,
    };
    let input: RemotePlanInput = serde_json::from_str(&text)
        .map_err(|error| fail(format!("parse remote plan input: {error}")))?;

    let plan = trit_orch::Plan::new(input.revision, input.tasks)
        .map_err(|error| fail(format!("invalid plan: {error}")))?;
    let mut config = trit_host::RemotePlanConfig::new(input.workers)
        .with_failure_threshold(input.failure_threshold)
        .with_retry_limit(input.retry_limit);
    if let Some(scope) = input.scope {
        config = config.with_scope(scope);
    }
    if probe {
        config = config.with_health_probe_on_start();
    }
    if let Some(interval) = probe_interval {
        config = config.with_health_probe_interval(interval);
    }
    if let Some(concurrency) = probe_concurrency {
        config = config.with_health_probe_concurrency(concurrency);
    }
    if let Some(max_age_ms) = health_max_age_ms {
        config = config.with_health_max_age(std::time::Duration::from_millis(max_age_ms));
    }
    let mut table =
        trit_orch::NodeTable::new(input.nodes)?.with_failure_threshold(input.failure_threshold);
    if let Some(path) = health_path.as_deref()
        && Path::new(path).exists()
    {
        let text = std::fs::read_to_string(path)
            .map_err(|error| fail(format!("read health file {path}: {error}")))?;
        let snapshots: Vec<trit_orch::NodeSnapshot> = serde_json::from_str(&text)
            .map_err(|error| fail(format!("parse health file {path}: {error}")))?;
        table = table.with_snapshots(&snapshots);
    }
    let table = Arc::new(table);
    let flusher = match health_path.as_deref() {
        Some(path) if !health_flush_interval.is_zero() => {
            Some(trit_host::spawn_remote_health_flusher(
                Arc::clone(&table),
                Path::new(path),
                health_flush_interval,
            )?)
        }
        _ => None,
    };
    let result = trit_host::run_remote_plan_with_table(
        &plan,
        Arc::clone(&table),
        config,
        trit_orch::CancelToken::new(),
        remote_request_for,
    );
    if let Some(mut flusher) = flusher {
        flusher.stop_and_join();
        if let Some(error) = flusher.last_error() {
            eprintln!("remote-plan health flush warning: {error}");
        }
    }
    let report = result?;
    if let Some(path) = health_path.as_deref() {
        trit_host::save_remote_health_file(Path::new(path), &report.nodes)?;
    }

    let failed = report
        .report
        .failed
        .as_ref()
        .map(|failure| serde_json::json!({ "task": failure.task, "message": failure.message }));
    let value = serde_json::json!({
        "ok": true,
        "completed": report.report.completed,
        "failed": failed,
        "deferred": report.report.deferred,
        "cancelled": report.report.cancelled,
        "results": report.results,
        "assignments": report.assignments,
        "attempts": report.attempts,
        "nodes": report.nodes,
        "stale": report.stale_nodes,
        "probes": report.probes,
        "metrics": report.report.metrics,
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// `remote-health` 单轮默认并行探测 4 个节点；`--probe-concurrency 1` 恢复串行。
const DEFAULT_HEALTH_PROBE_CONCURRENCY: usize = 4;

/// `remote-health --nodes <path> --health <path> [--interval-ms N] [--once]
/// [--probe-concurrency N] [--probe-all]`：
/// 独立健康守护。每轮重读节点表，默认只探已隔离节点，成功即写回健康文件；
/// `--probe-all` 连健康节点一起探，成功刷新时间戳、失败就地隔离。
/// 默认常驻，`--once` 适合 cron / 任务计划程序。持有 `<health>.lock` 的单实例
/// OS 锁，进程退出（包括崩溃）时自动释放。探测按 `--probe-concurrency` 并行，
/// 默认 4、上限 64；事件与快照仍按节点 id 升序输出。
fn run_remote_health(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut nodes_path: Option<String> = None;
    let mut health_path: Option<String> = None;
    let mut interval_ms: u64 = 30_000;
    let mut probe_concurrency = DEFAULT_HEALTH_PROBE_CONCURRENCY;
    let mut once = false;
    let mut scope = trit_host::RemoteHealthScope::IsolatedOnly;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--nodes" => nodes_path = Some(next(&mut args, "--nodes")?),
            "--health" => health_path = Some(next(&mut args, "--health")?),
            "--interval-ms" => {
                interval_ms = next(&mut args, "--interval-ms")?.parse()?;
                if interval_ms == 0 {
                    return Err(fail("--interval-ms must be greater than zero"));
                }
            }
            "--probe-concurrency" => {
                probe_concurrency = next(&mut args, "--probe-concurrency")?.parse()?
            }
            "--once" => once = true,
            "--probe-all" => scope = trit_host::RemoteHealthScope::All,
            other => return Err(fail(format!("unknown remote-health argument: {other}"))),
        }
    }
    if probe_concurrency == 0 {
        return Err(fail("--probe-concurrency must be positive"));
    }
    if probe_concurrency > trit_host::MAX_HEALTH_PROBE_CONCURRENCY {
        return Err(fail(format!(
            "--probe-concurrency must be <= {}",
            trit_host::MAX_HEALTH_PROBE_CONCURRENCY
        )));
    }
    let nodes_path = nodes_path.ok_or_else(|| fail("remote-health requires --nodes <path>"))?;
    let health_path = health_path.ok_or_else(|| fail("remote-health requires --health <path>"))?;
    let lock_path = PathBuf::from(format!("{health_path}.lock"));
    let _lock = trit_host::SingleInstanceLock::acquire(&lock_path)
        .map_err(|error| fail(format!("lock {}: {error}", lock_path.display())))?
        .ok_or_else(|| {
            fail(format!(
                "another remote-health monitor holds {}",
                lock_path.display()
            ))
        })?;

    let mut stdout = std::io::stdout().lock();
    loop {
        // 每轮重读节点表：增删节点或轮换 token 不需要重启守护。
        let nodes = load_remote_nodes(&nodes_path)?;
        let report = trit_host::probe_remote_health_file_scoped(
            &nodes,
            Path::new(&health_path),
            probe_concurrency,
            scope,
        )?;
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "ok": true,
                "checked": report.checked,
                "readmitted": report.readmitted,
                "still_isolated": report.still_isolated,
                "events": report.events,
                "nodes": report.nodes,
                "ignored": report.ignored,
            }))?
        )?;
        stdout.flush()?;
        if once {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }
    Ok(())
}

fn load_remote_nodes(path: &str) -> Result<Vec<trit_orch::RemoteNode>, Box<dyn Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| fail(format!("read nodes {path}: {error}")))?;
    serde_json::from_str(&text).map_err(|error| fail(format!("parse nodes {path}: {error}")))
}

#[derive(serde::Deserialize)]
struct RemotePlanInput {
    revision: u64,
    tasks: Vec<trit_orch::TaskSpec>,
    #[serde(default = "default_workers")]
    workers: usize,
    #[serde(default = "default_failure_threshold")]
    failure_threshold: u32,
    #[serde(default)]
    retry_limit: u32,
    /// 计划作用域；不同计划应使用不同值，隔离共享账本与 workspace。
    #[serde(default)]
    scope: Option<String>,
    nodes: Vec<trit_orch::RemoteNode>,
}

fn default_workers() -> usize {
    1
}

fn default_failure_threshold() -> u32 {
    1
}

/// 把任务定义映射成远程 subagent 请求：只读、无写根，工具限定 `read_file`。
///
/// 这是演练用的保守映射；真实执行器可以在调用 `run_remote_plan` 时换成自己的
/// `build_request`，权限与 capsule 由调用方决定。
fn remote_request_for(task: &trit_orch::TaskSpec) -> trit_orch::SubagentRequest {
    let mut goal = if task.guidance.objective.trim().is_empty() {
        task.task.id.clone()
    } else {
        task.guidance.objective.clone()
    };
    if goal.chars().count() > 256 {
        goal = goal.chars().take(256).collect();
    }
    let capsule = trit_orch::ContextCapsule::inherit(
        vec![trit_orch::CapsuleEntry {
            key: "goal".into(),
            value: goal,
        }],
        &["goal"],
        512,
    )
    .unwrap_or_default();
    let profile = trit_orch::SubagentProfile {
        name: format!("{}@remote", task.task.id),
        access: trit_orch::AccessMode::ReadOnly,
        write_roots: Vec::new(),
        tools: vec!["read_file".into()],
        max_tokens: task.max_tokens,
    };
    trit_orch::SubagentRequest::new(task, capsule, profile)
}

/// `remote-ledger [--bind <addr>] [--token <t>] [--pending-ttl-ms N] [--max-entries N]`
/// `[--max-state-bytes N] [--state <path>] [--max-connections N] [--request-read-timeout-ms N]`
/// `[--once]`：共享幂等账本服务端；
/// 协议见 `trit_host::remote_ledger`。
fn run_remote_ledger(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut config = trit_host::LedgerServerConfig::new("127.0.0.1:0");
    let mut once = false;
    let mut request_read_timeout_ms: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => config.bind = next(&mut args, "--bind")?,
            "--token" => config.token = Some(next(&mut args, "--token")?),
            "--pending-ttl-ms" => {
                config.pending_ttl =
                    std::time::Duration::from_millis(next(&mut args, "--pending-ttl-ms")?.parse()?);
            }
            "--max-entries" => config.max_entries = next(&mut args, "--max-entries")?.parse()?,
            "--max-state-bytes" => {
                config.max_state_bytes = next(&mut args, "--max-state-bytes")?.parse()?
            }
            "--state" => config.state_path = Some(PathBuf::from(next(&mut args, "--state")?)),
            "--max-connections" => {
                config.max_connections = next(&mut args, "--max-connections")?.parse()?
            }
            "--request-read-timeout-ms" => {
                request_read_timeout_ms =
                    Some(next(&mut args, "--request-read-timeout-ms")?.parse()?)
            }
            "--once" => once = true,
            other => return Err(fail(format!("unknown remote-ledger argument: {other}"))),
        }
    }
    if let Some(ms) = request_read_timeout_ms {
        config = config.with_request_read_timeout(std::time::Duration::from_millis(ms));
    }
    let server = trit_host::LedgerServer::bind(config)?;
    let addr = server.addr()?;
    println!("{}", serde_json::json!({ "bind": addr.to_string() }));
    std::io::stdout().flush()?;
    if once {
        server.serve_once()?;
        return Ok(());
    }
    server.serve_forever()?;
    Ok(())
}

/// `remote-exec [--bind <addr>] [--token <t>] (--workspace <dir> | --workspace-root <dir>)`
/// `--script <proposals.json>` `[--max-steps N] [--exec-timeout-secs N]`
/// `[--request-timeout-ms N] [--request-read-timeout-ms N] [--max-connections N] [--max-cache-bytes N]`
/// `[--cache-ttl-ms N] [--once]`
/// `[--ledger <url> ...]`：可执行的远程端点。每个请求 spawn 一个 `subagent-run`
/// 子进程，帧走 stdin/stdout。`--workspace-root` 会按任务 ID 建独立子目录；
/// args/`--script` 中的 `{task}` 与 `{workspace}` 在每次请求前展开。
fn run_remote_exec(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut config = trit_host::RemoteServerConfig::new("127.0.0.1:0");
    let mut once = false;
    let mut workspace: Option<PathBuf> = None;
    let mut workspace_root: Option<PathBuf> = None;
    let mut script: Option<PathBuf> = None;
    let mut max_steps = 20usize;
    let mut exec_timeout_secs = 30u64;
    let mut request_timeout_ms: Option<u64> = None;
    let mut request_read_timeout_ms: Option<u64> = None;
    let mut ledger_url: Option<String> = None;
    let mut ledger_token: Option<String> = None;
    let mut ledger_timeout_secs = 30u64;
    let mut ledger_renew_ms: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => config.bind = next(&mut args, "--bind")?,
            "--token" => config.token = Some(next(&mut args, "--token")?),
            "--once" => once = true,
            "--workspace" => workspace = Some(PathBuf::from(next(&mut args, "--workspace")?)),
            "--workspace-root" => {
                workspace_root = Some(PathBuf::from(next(&mut args, "--workspace-root")?))
            }
            "--script" => script = Some(PathBuf::from(next(&mut args, "--script")?)),
            "--max-steps" => max_steps = next(&mut args, "--max-steps")?.parse()?,
            "--max-connections" => {
                config.max_connections = next(&mut args, "--max-connections")?.parse()?
            }
            "--max-cache-bytes" => {
                config.max_cache_bytes = next(&mut args, "--max-cache-bytes")?.parse()?
            }
            "--cache-ttl-ms" => {
                config = config.with_cache_ttl(std::time::Duration::from_millis(
                    next(&mut args, "--cache-ttl-ms")?.parse()?,
                ))
            }
            "--exec-timeout-secs" => {
                exec_timeout_secs = next(&mut args, "--exec-timeout-secs")?.parse()?
            }
            "--request-timeout-ms" => {
                request_timeout_ms = Some(next(&mut args, "--request-timeout-ms")?.parse()?)
            }
            "--request-read-timeout-ms" => {
                request_read_timeout_ms =
                    Some(next(&mut args, "--request-read-timeout-ms")?.parse()?)
            }
            "--ledger" => ledger_url = Some(next(&mut args, "--ledger")?),
            "--ledger-token" => ledger_token = Some(next(&mut args, "--ledger-token")?),
            "--ledger-timeout-secs" => {
                ledger_timeout_secs = next(&mut args, "--ledger-timeout-secs")?.parse()?
            }
            "--ledger-renew-ms" => {
                ledger_renew_ms = Some(next(&mut args, "--ledger-renew-ms")?.parse()?)
            }
            other => return Err(fail(format!("unknown remote-exec argument: {other}"))),
        }
    }
    let (workspace, workspace_root) = match (workspace, workspace_root) {
        (Some(workspace), None) => (workspace, None),
        (None, Some(root)) => (PathBuf::from("{workspace}"), Some(root)),
        (Some(_), Some(_)) => {
            return Err(fail(
                "--workspace and --workspace-root are mutually exclusive",
            ));
        }
        (None, None) => {
            return Err(fail(
                "remote-exec requires --workspace <dir> or --workspace-root <dir>",
            ));
        }
    };
    let script = script.ok_or_else(|| fail("remote-exec requires --script <proposals.json>"))?;
    let program = std::env::current_exe()
        .map_err(|error| fail(format!("resolve current executable: {error}")))?;
    let exec = trit_host::SubagentProcessConfig::new(program)
        .with_args(vec![
            "subagent-run".into(),
            "--workspace".into(),
            workspace.display().to_string(),
            "--script".into(),
            script.display().to_string(),
            "--max-steps".into(),
            max_steps.to_string(),
        ])
        .with_timeout(std::time::Duration::from_secs(exec_timeout_secs));
    if let Some(root) = workspace_root {
        config.exec_workspace_root = Some(root);
    }
    config = config.with_exec(exec);
    if let Some(url) = ledger_url {
        config.ledger = Some(trit_orch::RemoteEndpoint {
            url,
            auth_token: ledger_token,
            timeout_secs: ledger_timeout_secs,
        });
    } else if ledger_token.is_some() {
        return Err(fail("--ledger-token requires --ledger <url>"));
    }
    if let Some(ms) = ledger_renew_ms {
        if config.ledger.is_none() {
            return Err(fail("--ledger-renew-ms requires --ledger <url>"));
        }
        config.ledger_renew_interval = std::time::Duration::from_millis(ms);
    }
    if let Some(ms) = request_timeout_ms {
        config = config.with_exec_timeout(std::time::Duration::from_millis(ms));
    }
    if let Some(ms) = request_read_timeout_ms {
        config = config.with_request_read_timeout(std::time::Duration::from_millis(ms));
    }

    let server = trit_host::RemoteServer::bind(config)?;
    let addr = server.addr()?;
    println!("{}", serde_json::json!({ "bind": addr.to_string() }));
    std::io::stdout().flush()?;
    if once {
        server.serve_once()?;
        return Ok(());
    }
    server.serve_forever()?;
    Ok(())
}

/// `plan-exec --task <t> --workspace <dir> (--planner-script P --script E | --provider openai)`：
///
/// 一键跑 planner → executor 两阶段。planner 只读产出 `PlanHandoff`；executor 的工具与
/// `WriteClaim` 在计划校验通过后才按 `plan.profile.write_roots` 构造，计划和缓存都不跨阶段共享。
fn run_plan_exec(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut task: Option<String> = None;
    let mut workspace = PathBuf::from(".");
    let mut planner_script: Option<PathBuf> = None;
    let mut executor_script: Option<PathBuf> = None;
    let mut provider = "scripted".to_owned();
    let mut planner_model_name: Option<String> = None;
    let mut executor_model_name: Option<String> = None;
    let mut allow_write = false;
    let mut write_journal = false;
    let mut plan_max_steps = 8usize;
    let mut exec_max_steps = 8usize;
    let mut capsule_budget = 4096usize;
    let mut output_format = String::from("md");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--task" => task = Some(next(&mut args, "--task")?),
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--planner-script" => {
                planner_script = Some(PathBuf::from(next(&mut args, "--planner-script")?));
            }
            "--script" | "--executor-script" => {
                executor_script = Some(PathBuf::from(next(&mut args, "--script")?));
            }
            "--provider" => provider = next(&mut args, "--provider")?,
            "--planner-model" => planner_model_name = Some(next(&mut args, "--planner-model")?),
            "--executor-model" => executor_model_name = Some(next(&mut args, "--executor-model")?),
            "--allow-write" => allow_write = true,
            "--write-journal" => write_journal = true,
            "--max-plan-steps" => {
                plan_max_steps = next(&mut args, "--max-plan-steps")?
                    .parse()
                    .map_err(|error| fail(format!("bad --max-plan-steps: {error}")))?;
            }
            "--max-exec-steps" => {
                exec_max_steps = next(&mut args, "--max-exec-steps")?
                    .parse()
                    .map_err(|error| fail(format!("bad --max-exec-steps: {error}")))?;
            }
            "--capsule-budget" => {
                capsule_budget = next(&mut args, "--capsule-budget")?
                    .parse()
                    .map_err(|error| fail(format!("bad --capsule-budget: {error}")))?;
            }
            "--output-format" => {
                output_format = match next(&mut args, "--output-format")?.as_str() {
                    "md" | "markdown" => "md".into(),
                    "json" => "json".into(),
                    other => return Err(fail(format!("bad --output-format: {other}"))),
                };
            }
            other => return Err(fail(format!("plan-exec: unknown argument {other}"))),
        }
    }
    let task = task.ok_or_else(|| fail("plan-exec requires --task"))?;
    if provider != "scripted" && provider != "openai" {
        return Err(fail(format!("plan-exec: bad --provider {provider}")));
    }

    // planner 侧只读：即便脚本里塞了写调用，也会被 ReadOnlyTools 拒绝。
    let mut planner_config = WorkspaceConfig::new(&workspace);
    planner_config.allow_write = false;
    let planner_tools = trit_host::ReadOnlyTools::new(LocalTools::new(planner_config)?);

    let planner_model = plan_exec_model(
        &provider,
        planner_script.as_deref(),
        "--planner-script",
        planner_model_name.as_deref(),
        planner_catalog_specs(),
    )?;
    let executor_model = plan_exec_model(
        &provider,
        executor_script.as_deref(),
        "--script",
        executor_model_name.as_deref(),
        default_tools(),
    )?;

    let workspace_for_exec = workspace.clone();
    let executor =
        move |plan: &trit_orch::PlanHandoff| -> Result<ExecutorSetup<Box<dyn ToolExecutor>>, trit_host::AgentError> {
            let mut config = WorkspaceConfig::new(&workspace_for_exec);
            config.allow_write = allow_write;
            let local = LocalTools::new(config)?;
            let tools: Box<dyn ToolExecutor> = if allow_write {
                Box::new(local)
            } else {
                Box::new(trit_host::ReadOnlyTools::new(local))
            };
            if !allow_write {
                return Ok(ExecutorSetup {
                    tools,
                    write_claim: None,
                });
            }
            // 计划里写哪里，执行侧就只能写哪里；同一份 claim 也交给完成裁决。
            let claim = WriteClaim::new(
                &workspace_for_exec,
                plan.profile
                    .write_roots
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
            )?;
            let journal = if write_journal {
                Some(
                    MutationJournal::open(&workspace_for_exec)
                        .map_err(|error| trit_host::AgentError::Io(error.to_string()))?,
                )
            } else {
                None
            };
            let claim_for_host = claim.clone();
            Ok(ExecutorSetup {
                tools: apply_write_guard(
                    tools,
                    Some(WriteGuard {
                        claim,
                        lease: None,
                        journal,
                    }),
                ),
                write_claim: Some(claim_for_host),
            })
        };

    let turn = TurnSpec {
        task: task.clone(),
        objective: task,
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 0,
        max_tokens: 0,
    };
    let config = PlanExecuteConfig {
        plan_max_steps,
        exec_max_steps,
        capsule_allowed: vec!["goal".to_owned()],
        capsule_budget,
    };
    let outcome = run_plan_then_execute_with(
        planner_model,
        planner_tools,
        executor_model,
        executor,
        &turn,
        &config,
    )?;

    let complete = outcome.execution.judgment.is_complete();
    let plan_ref = outcome.plan.reference();
    if output_format == "json" {
        let value = serde_json::json!({
            "plan": outcome.plan,
            "plan_ref": plan_ref,
            "complete": complete,
            "execution": outcome.execution,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("plan: {plan_ref}");
        print!("{}", outcome.execution.render_markdown());
    }
    if !complete {
        std::process::exit(1);
    }
    Ok(())
}

fn plan_exec_model(
    provider: &str,
    script: Option<&Path>,
    flag: &str,
    model_name: Option<&str>,
    tools: Vec<ToolSpec>,
) -> Result<Box<dyn Model>, Box<dyn Error>> {
    if provider == "scripted" {
        let path = script.ok_or_else(|| fail(format!("plan-exec requires {flag}")))?;
        let text = std::fs::read_to_string(path)?;
        let proposals: Vec<Proposal> = serde_json::from_str(&text)?;
        return Ok(Box::new(ScriptedModel::new(proposals)));
    }
    let mut config = OpenAiConfig::from_env().map_err(|error| fail(error.to_string()))?;
    if let Some(model) = model_name {
        config.model = model.to_owned();
    }
    Ok(Box::new(ProviderModel::with_tools(
        OpenAiProvider::new(config),
        tools,
    )))
}

/// `subagent-run --workspace <dir> --script <proposals.json> [--max-steps N]`：
/// 可执行子 agent 参考实现。读一个请求帧，按 profile 权限跑一轮，回响应帧。
///
/// 权限映射：`ReadOnly` → `ReadOnlyTools` 包 `LocalTools` 且 `allow_write = false`；
/// `ReadWrite` → 直接用 `LocalTools` 且 `allow_write = true`。响应状态只有
/// `judgment.is_complete()` 为 true 时才是 `Completed`。
fn run_subagent_run(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut workspace = PathBuf::from(".");
    let mut script: Option<PathBuf> = None;
    let mut max_steps = 20usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--script" => script = Some(PathBuf::from(next(&mut args, "--script")?)),
            "--max-steps" => max_steps = next(&mut args, "--max-steps")?.parse()?,
            other => return Err(fail(format!("unknown subagent-run argument: {other}"))),
        }
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let request: trit_orch::SubagentRequest = trit_orch::decode_frame(&mut input)?
        .ok_or_else(|| fail("subagent-run: no request frame"))?;

    let mut config = WorkspaceConfig::new(&workspace);
    config.allow_write = request.profile.access == trit_orch::AccessMode::ReadWrite;
    let local = LocalTools::new(config)?;
    let tools: Box<dyn ToolExecutor> = if request.profile.access == trit_orch::AccessMode::ReadOnly
    {
        Box::new(trit_host::ReadOnlyTools::new(local))
    } else {
        Box::new(local)
    };
    // profile 的工具白名单同样要在执行侧生效。
    let tools: Box<dyn ToolExecutor> = Box::new(trit_host::ToolWhitelist::new(
        tools,
        request.profile.tools.clone(),
    ));
    // profile 的 write_roots 必须在执行侧真正生效：只在 validate() 里检查形状
    // 不够，否则子 agent 仍能写到声明的子树之外。
    let write_guard = if request.profile.access == trit_orch::AccessMode::ReadWrite {
        let claim = WriteClaim::new(
            &workspace,
            request
                .profile
                .write_roots
                .iter()
                .map(PathBuf::from)
                .collect(),
        )
        .map_err(|error| fail(format!("subagent write roots: {error}")))?;
        Some(WriteGuard {
            claim,
            lease: None,
            journal: None,
        })
    } else {
        None
    };
    let claim_for_host = write_guard.as_ref().map(|guard| guard.claim.clone());
    let tools = apply_write_guard(tools, write_guard);

    let script_path =
        script.ok_or_else(|| fail("subagent-run requires --script <proposals.json>"))?;
    let text = std::fs::read_to_string(&script_path)?;
    let proposals: Vec<Proposal> = serde_json::from_str(&text)?;
    let model = ScriptedModel::new(proposals);

    let objective = request
        .capsule
        .get("objective")
        .unwrap_or(request.task.as_str())
        .to_owned();
    let turn = TurnSpec {
        task: request.task.clone(),
        objective,
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 0,
        max_tokens: request.profile.max_tokens,
    };
    let mut host = AgentHost::new(model, tools)
        .with_max_steps(max_steps)
        .with_write_claim(claim_for_host);
    let outcome = host.run_turn(&turn)?;
    let status = if outcome.judgment.is_complete() {
        trit_orch::DelegationStatus::Completed
    } else {
        trit_orch::DelegationStatus::Failed {
            message: "task did not complete".into(),
        }
    };
    let response = trit_orch::SubagentResponse {
        task: request.task,
        epoch: request.epoch,
        status,
        content: outcome.render_markdown().into_bytes(),
        tokens: outcome.usage.total()?,
    };
    let frame = trit_orch::encode_frame(&response)?;
    std::io::stdout().write_all(&frame)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use trit_host::{
        CacheClass, ContextDocument, ContextKind, ContextRef, ContextScope, Sensitivity,
    };

    #[test]
    fn loads_context_catalog_json() {
        let entry = ResolvedContext {
            reference: ContextRef {
                id: "doc".into(),
                label: "doc".into(),
                kind: ContextKind::Doc,
                scope: ContextScope::Project,
                project: Some("p1".into()),
                session: None,
                revision: "rev1".into(),
                content_hash: None,
                sensitivity: Sensitivity::Internal,
                cache: CacheClass::DynamicTail,
            },
            document: ContextDocument {
                id: "doc".into(),
                full: "body".into(),
                summary: None,
                tokens: Some(1),
            },
        };
        let path = std::env::temp_dir().join(format!(
            "trit-cli-catalog-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, serde_json::to_string(&vec![entry]).unwrap()).unwrap();
        let catalog = load_context_catalog(&path).unwrap();
        assert_eq!(catalog.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_git_status_entries_and_rejects_non_utf8() {
        let stdout = b" M src/a.py\0?? src/b.py\0";
        assert_eq!(
            parse_git_status("", stdout).unwrap(),
            ["src/a.py", "src/b.py"]
        );

        // rename：下一条 NUL 是旧路径，不应进 changed。
        let renamed = b"R  new.py\0old.py\0";
        assert_eq!(parse_git_status("", renamed).unwrap(), ["new.py"]);

        // workspace 是 repo 子目录：前缀要裁掉。
        let nested = b"?? sub/x.py\0";
        assert_eq!(parse_git_status("sub/", nested).unwrap(), ["x.py"]);

        // 非 UTF-8 路径：放弃自动取值，而不是替换字符。
        let bad = b"?? non-utf8-\xFF.txt\0";
        assert!(parse_git_status("", bad).is_err());

        // 排序去重。
        let dup = b"?? b.py\0?? a.py\0?? a.py\0";
        assert_eq!(parse_git_status("", dup).unwrap(), ["a.py", "b.py"]);
    }

    #[test]
    fn supervisor_recovery_surfaces_blocked_gate_chain() {
        let path = std::env::temp_dir().join(format!(
            "trit-supervisor-recovery-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut log = trit_host::SessionLog::open(&path).unwrap();
        log.append(trit_host::SessionEvent::GateChain {
            phase: trit_host::GateChainPhase::Pre,
            status: "blocked".into(),
            exit: 1,
            failed_at: Some("repo".into()),
            failures: 1,
            passed: 0,
            blocked: 1,
            unknown: 0,
            skipped: 2,
            report: None,
        })
        .unwrap();
        let cache = std::sync::Arc::new(std::sync::Mutex::new(trit_host::CacheLedger::default()));
        let recovery = supervisor_recovery(Some(&path), &cache).unwrap();
        assert!(recovery.gate_chain_blocked());
        assert_eq!(
            recovery
                .latest_gate_chain()
                .map(|record| record.status.as_str()),
            Some("blocked")
        );
        let _ = std::fs::remove_file(&path);
    }
}
