//! `trit-subagent`：子 agent 独立发布物。
//!
//! 与 `trit-host subagent-run` 行为一致，但作为独立可执行文件分发，宿主不必
//! 依赖整个 `trit-host` 二进制：
//!
//! ```text
//! trit-subagent --workspace <dir> --script <proposals.json> [--max-steps N]
//! ```
//!
//! stdin 收一个 `SubagentRequest` 帧，stdout 回一个 `SubagentResponse` 帧。
//! 权限映射：`ReadOnly` → `ReadOnlyTools` + `allow_write = false`；
//! `ReadWrite` → `LocalTools` + `allow_write = true`。只有
//! `judgment.is_complete()` 为 true 时状态才是 `Completed`。

use std::error::Error;
use std::io::{Read, Write};
use std::path::PathBuf;

use trit_host::tools::{LocalTools, WorkspaceConfig};
use trit_host::{AgentHost, Proposal, ReadOnlyTools, ScriptedModel, ToolExecutor};
use trit_reasonix::TurnSpec;

fn main() {
    if let Err(error) = run() {
        eprintln!("trit-subagent: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut workspace = PathBuf::from(".");
    let mut script: Option<PathBuf> = None;
    let mut max_steps = 20usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(next(&mut args, "--workspace")?),
            "--script" => script = Some(PathBuf::from(next(&mut args, "--script")?)),
            "--max-steps" => max_steps = next(&mut args, "--max-steps")?.parse()?,
            other => return Err(fail(format!("unknown argument: {other}"))),
        }
    }

    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let request: trit_orch::SubagentRequest =
        trit_orch::decode_frame(&mut input)?.ok_or_else(|| fail("no request frame on stdin"))?;

    let mut config = WorkspaceConfig::new(&workspace);
    config.allow_write = request.profile.access == trit_orch::AccessMode::ReadWrite;
    let local = LocalTools::new(config)?;
    let tools: Box<dyn ToolExecutor> = if request.profile.access == trit_orch::AccessMode::ReadOnly
    {
        Box::new(ReadOnlyTools::new(local))
    } else {
        Box::new(local)
    };

    let script_path = script.ok_or_else(|| fail("requires --script <proposals.json>"))?;
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
    let mut host = AgentHost::new(model, tools).with_max_steps(max_steps);
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
        model: None,
        usage: Some(trit_orch::SubagentUsage {
            fresh: outcome.usage.fresh,
            cached: outcome.usage.cached,
            output: outcome.usage.output,
            estimated: outcome.usage.estimated,
        }),
        dissent: None,
    };
    let frame = trit_orch::encode_frame(&response)?;
    std::io::stdout().write_all(&frame)?;
    Ok(())
}

fn next(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| fail(format!("{flag} requires a value")))
}

fn fail(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message.into(),
    ))
}
