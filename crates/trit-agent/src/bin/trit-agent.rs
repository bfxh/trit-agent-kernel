use std::error::Error;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use trit_agent::tools::{LocalTools, SandboxCommand, WorkspaceConfig};
use trit_agent::{
    CheckpointStore, CheckpointingTools, ContextPolicy, GitCheckpointStore, GitCheckpointingTools,
    HeadlessConfig, Model, OpenAiConfig, OpenAiProvider, Proposal, ProviderModel, RewindScope,
    ScriptedModel, Session, ToolExecutor, rewind_session_git_with_policy,
    rewind_session_with_policy, run_headless,
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
    let outcome = if resume || checkpoint_dir.is_some() {
        let path = session_path.ok_or_else(|| fail("--resume requires --session"))?;
        let model: Box<dyn Model> = match scripted {
            Some(proposals) => Box::new(ScriptedModel::new(proposals)),
            None => Box::new(ProviderModel::new(OpenAiProvider::new(
                OpenAiConfig::from_env()?,
            ))),
        };
        let local = LocalTools::new(config.workspace)?;
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
        let mut session = Session::open(model, tools, &path)?
            .with_max_steps(max_steps)
            .with_context(context);
        session.run_turn(&turn)?
    } else {
        match scripted {
            Some(proposals) => run_headless(turn, ScriptedModel::new(proposals), config)?,
            None => {
                let provider = OpenAiProvider::new(OpenAiConfig::from_env()?);
                run_headless(turn, ProviderModel::new(provider), config)?
            }
        }
    };

    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "{}",
        serde_json::to_string_pretty(&outcome).map_err(|error| fail(error.to_string()))?
    )?;
    if !outcome.judgment.is_complete() {
        std::process::exit(1);
    }
    Ok(())
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
         [--allow-hardlinks] [--context-receipts N] [--context-chars N] [--context-output N]\n\n\
         scripted: deterministic replay from a JSON array of Proposal objects.\n\
         openai:   OpenAI-compatible /chat/completions; set OPENAI_API_KEY,\n\
         OPENAI_MODEL and optionally OPENAI_BASE_URL.\n\
         Writes are disabled unless --allow-write; links are rejected unless --allow-hardlinks.\n\
         Shell needs an OS sandbox; --allow-unsandboxed-shell is an explicit downgrade.\n\
         --resume reloads prior turns from --session and appends one new turn.\n\
         checkpoint: files (per-file snapshots) or git (shadow bare repo of the workspace).\n\n\
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
