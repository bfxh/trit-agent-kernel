use std::error::Error;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use trit_agent::tools::{LocalTools, SandboxCommand, WorkspaceConfig};
use trit_agent::{
    AgentContext, AgentContextPlanner, AgentContextTools, AgentHost, CacheLedger,
    CachePressurePolicy, CacheWindow, CheckpointStore, CheckpointingTools, ContextPolicy,
    ContextRequest, GitCheckpointStore, GitCheckpointingTools, HeadlessConfig, InMemoryContext,
    LowHitPolicy, Model, ModelProfile, OpenAiConfig, OpenAiProvider, Proposal, ProviderModel,
    ReasoningEffort, RecoveryContext, ResolvedContext, RewindScope, ScriptedModel, SearchOrder,
    SelectionPressure, Session, SupervisorConfig, SupervisorRuntime, ToolExecutor,
    recovery_context_for_session, rewind_session_git_with_policy, rewind_session_with_policy,
    run_headless,
};
use trit_reasonix::TurnSpec;

fn main() {
    if let Err(error) = run() {
        eprintln!("trit-agent: {error}");
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

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--task" => task = Some(next(&mut args, "--task")?),
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
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
    let workspace_root = workspace.clone();
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
            Some(SupervisorRuntime::start(supervisor_config, recovery)?)
        }
        None => None,
    };
    let (outcome, _cache_ledger) = if resume || checkpoint_dir.is_some() {
        let path = session_path.ok_or_else(|| fail("--resume requires --session"))?;
        let model = build_model(
            scripted,
            openai_config.as_ref(),
            context_agent.as_ref(),
            &shared_cache_ledger,
            &cache_options,
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
                    let mut tools = CheckpointingTools::new(store, workspace_root, local);
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
        )?;
        let local = LocalTools::new(config.workspace.clone())?;
        let tools = AgentContextTools::new(local, Arc::clone(shared))
            .with_max_chars(context_read_max)
            .with_read_budget(context_read_budget);
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
        )?;
        (run_headless(turn, model, config)?, None)
    };

    if let Some(runtime) = supervisor_runtime.as_ref() {
        let recovery =
            supervisor_recovery(supervisor_session_path.as_deref(), &shared_cache_ledger)?;
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
) -> Result<Box<dyn Model>, Box<dyn Error>> {
    if let Some(proposals) = scripted {
        return Ok(Box::new(ScriptedModel::new(proposals)));
    }
    let config = config.ok_or_else(|| fail("--provider openai requires OPENAI_* env"))?;
    let mut model = ProviderModel::new(OpenAiProvider::new(config.clone()));
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

fn usage() {
    eprintln!(
        "trit-agent - headless Rust agent host\n\n\
         usage:\n  \
         trit-agent run --task <text> --workspace <dir> \\\n    \
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
         [--supervisor-config <json>]]\n\n\
         scripted: deterministic replay from a JSON array of Proposal objects.\n\
         openai:   OpenAI-compatible /chat/completions; set OPENAI_API_KEY,\n\
         OPENAI_MODEL and optionally OPENAI_BASE_URL.\n\
         Writes are disabled unless --allow-write; links are rejected unless --allow-hardlinks.\n\
         Shell needs an OS sandbox; --allow-unsandboxed-shell is an explicit downgrade.\n\
         --resume reloads prior turns from --session and appends one new turn.\n\
         checkpoint: files (per-file snapshots) or git (shadow bare repo of the workspace).\n\
         context-catalog: JSON array of {{reference, document}} entries; enables agent-driven\n\
         context_search / context_add / context_read with dynamic-tail-only additions.\n\
         cache-hit-threshold: basis points (default 5000); low-hit-policy keep|demote (default keep).\n\
         cache-ledger: optional JSON file loaded before the run and rewritten after it.\n\
         cache-search-scale-bp: scales context_search candidates on low cache hit.\n\
         supervisor-config: JSON {{gateway:[ProcessSpec], mcp:[ProcessSpec],\n\
         bridges:[PortBridgeSpec], roots:[McpRoot], restart:{{...}}, interval_ms,\n\
         mcp_timeout_ms}}; starts supervised sidecars and keep-alive port bridges,\n\
         restarts them after exit, discovers MCP tools/resources/prompts, auto-refreshes\n\
         list_changed, and reconnects MCP with session state/cache recovery.\n\
         sampling/createMessage needs a host-registered SamplingHandler in library mode.\n\n\
         rewind: trit-agent rewind --session <jsonl> --checkpoint-dir <dir> --turn N \\\n\
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
    use trit_agent::{
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
}
