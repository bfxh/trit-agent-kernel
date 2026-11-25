use std::error::Error;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use trit_host::tools::{LocalTools, SandboxCommand, WorkspaceConfig};
use trit_host::{
    AgentContext, AgentContextPlanner, AgentContextTools, AgentHost, CacheLedger,
    CachePressurePolicy, CacheWindow, CheckpointStore, CheckpointingTools, ContextPolicy,
    ContextRequest, GitCheckpointStore, GitCheckpointingTools, HeadlessConfig, InMemoryContext,
    LowHitPolicy, McpToolExecutor, Model, ModelProfile, OpenAiConfig, OpenAiProvider,
    OpenAiSamplingHandler, Proposal, ProviderModel, ReasoningEffort, RecoveryContext,
    ResolvedContext, RewindScope, SamplingHandler, ScriptedModel, SearchOrder, SelectionPressure,
    Session, SupervisorConfig, SupervisorRuntime, ToolCatalog, ToolExecutor, default_tools,
    recovery_context_for_session, rewind_session_git_with_policy, rewind_session_with_policy,
    run_headless,
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
    workspace.sandbox = sandbox.map(|program| SandboxCommand {
        program,
        args: sandbox_args,
    });
    workspace.forbid_hardlinks = !allow_hardlinks;
    let session_path = session.clone();
    let config = HeadlessConfig {
        workspace,
        max_steps,
        session,
        context,
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
        let mut session = Session::open(model, tools, &path)?
            .with_max_steps(max_steps)
            .with_context(context)
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
        if let Some(path) = config.session.as_ref() {
            let mut session = Session::open(model, tools, path)?
                .with_max_steps(max_steps)
                .with_context(context)
                .with_cache_ledger(Arc::clone(&shared_cache_ledger));
            let outcome = session.run_turn(&turn)?;
            let ledger = session.cache_ledger();
            (outcome, Some(ledger))
        } else {
            let mut host = AgentHost::new(model, tools)
                .with_max_steps(max_steps)
                .with_context(context)
                .with_cache_ledger(Arc::clone(&shared_cache_ledger));
            let outcome = host.run_turn(&turn)?;
            let ledger = host.cache_ledger().clone();
            (outcome, Some(ledger))
        }
    } else {
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
            if let Some(path) = config.session.as_ref() {
                let mut session = Session::open(model, tools, path)?
                    .with_max_steps(max_steps)
                    .with_context(context)
                    .with_cache_ledger(Arc::clone(&shared_cache_ledger));
                let outcome = session.run_turn(&turn)?;
                let ledger = session.cache_ledger();
                (outcome, Some(ledger))
            } else {
                let mut host = AgentHost::new(model, tools)
                    .with_max_steps(max_steps)
                    .with_context(context)
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
    writeln!(
        stdout,
        "{}",
        serde_json::to_string_pretty(&outcome).map_err(|error| fail(error.to_string()))?
    )?;
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
         [--allow-hardlinks] [--context-receipts N] [--context-chars N] [--context-output N] \\\n    \
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
         scripted: deterministic replay from a JSON array of Proposal objects.\n\
         openai:   OpenAI-compatible /chat/completions; set OPENAI_API_KEY,\n\
         OPENAI_MODEL and optionally OPENAI_BASE_URL.\n\
         Writes are disabled unless --allow-write; links are rejected unless --allow-hardlinks.\n\
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
         rewind: trit-host rewind --session <jsonl> --checkpoint-dir <dir> --turn N \\\n\
         [--checkpoint-backend files|git] [--workspace <dir>] [--scope code|conversation|both] \\\n\
         [--allow-unrewindable]"
    );
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
