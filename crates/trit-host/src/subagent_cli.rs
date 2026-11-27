//! 把外部编码智能体 CLI 当作子智能体驱动。
//!
//! 这些 CLI（codex / qoder / zcode / 豆包 / Claude Code）不实现本仓库的帧协议，
//! 所以适配层负责：组装命令行、传 prompt、读输出，并把可选的异议块转成 [`Dissent`]。
//!
//! 默认调用约定是 `<program> <args...> <prompt>`（或 stdin），实际参数差异用
//! `TRIT_SUBAGENT_<ID>_PROGRAM` / `_ARGS` / `_AUTO_ARGS` 覆盖，不靠猜。

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use trit_orch::Dissent;

use crate::AgentError;

/// prompt 传给 CLI 的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTransport {
    /// 作为最后一个命令行参数。
    Arg,
    /// 写进 stdin 后关闭。
    Stdin,
}

/// 一个外部编码智能体 CLI 的默认调用约定。
#[derive(Debug, Clone, Copy)]
pub struct CliAgentSpec {
    pub id: &'static str,
    pub program: &'static str,
    /// 固定参数，位于 prompt 之前。
    pub args: &'static [&'static str],
    /// 非交互 / 自动批准参数；可用环境变量覆盖。
    pub auto_args: &'static [&'static str],
    pub prompt: PromptTransport,
}

/// 先支持这五个；新增后端只需往这里加一条并补文档。
pub const CLI_AGENTS: &[CliAgentSpec] = &[
    CliAgentSpec {
        id: "codex",
        program: "codex",
        args: &["exec"],
        auto_args: &["--full-auto"],
        prompt: PromptTransport::Arg,
    },
    CliAgentSpec {
        id: "claude-code",
        program: "claude",
        args: &["-p"],
        auto_args: &[],
        prompt: PromptTransport::Arg,
    },
    CliAgentSpec {
        id: "qoder",
        program: "qoder",
        args: &["run"],
        auto_args: &[],
        prompt: PromptTransport::Arg,
    },
    CliAgentSpec {
        id: "zcode",
        program: "zcode",
        args: &["run"],
        auto_args: &[],
        prompt: PromptTransport::Arg,
    },
    CliAgentSpec {
        id: "doubao",
        program: "doubao",
        args: &[],
        auto_args: &[],
        prompt: PromptTransport::Stdin,
    },
];

/// 异议块：`<<<DISSENT>>>{json}<<<END>>>`，可出现在输出任意位置。
pub const DISSENT_OPEN: &str = "<<<DISSENT>>>";
pub const DISSENT_CLOSE: &str = "<<<END>>>";
/// 单个 CLI 输出上限，防止恶意/异常输出拖垮进程。
pub const MAX_CLI_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

impl CliAgentSpec {
    pub fn resolve(id: &str) -> Option<&'static Self> {
        let id = id.trim();
        CLI_AGENTS.iter().find(|spec| spec.id == id)
    }

    pub fn ids() -> Vec<&'static str> {
        CLI_AGENTS.iter().map(|spec| spec.id).collect()
    }

    fn env_key(&self, suffix: &str) -> String {
        format!(
            "TRIT_SUBAGENT_{}_{}",
            self.id.to_ascii_uppercase().replace('-', "_"),
            suffix
        )
    }

    /// 可执行文件：`TRIT_SUBAGENT_<ID>_PROGRAM` 覆盖默认值。
    pub fn program(&self) -> String {
        std::env::var(self.env_key("PROGRAM"))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.program.to_owned())
    }

    /// 固定参数：`TRIT_SUBAGENT_<ID>_ARGS`（空白分隔，允许空串清空）。
    pub fn args(&self) -> Vec<String> {
        match std::env::var(self.env_key("ARGS")) {
            Ok(value) => split_args(&value),
            Err(_) => self.args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    /// 自动批准参数：`TRIT_SUBAGENT_<ID>_AUTO_ARGS` 覆盖。
    pub fn auto_args(&self) -> Vec<String> {
        match std::env::var(self.env_key("AUTO_ARGS")) {
            Ok(value) => split_args(&value),
            Err(_) => self.auto_args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    /// 组装一次调用：`(program, argv, stdin_prompt)`。
    pub fn command(&self, prompt: &str) -> (String, Vec<String>, Option<String>) {
        let mut argv = self.args();
        argv.extend(self.auto_args());
        match self.prompt {
            PromptTransport::Arg => {
                argv.push(prompt.to_owned());
                (self.program(), argv, None)
            }
            PromptTransport::Stdin => (self.program(), argv, Some(prompt.to_owned())),
        }
    }
}

fn split_args(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_owned).collect()
}

/// 解析 CLI 输出：取出异议块，其余作为结果内容。
pub fn parse_cli_output(stdout: &str) -> (String, Option<Dissent>) {
    let Some(start) = stdout.find(DISSENT_OPEN) else {
        return (stdout.trim_end().to_owned(), None);
    };
    let after = start + DISSENT_OPEN.len();
    let Some(end_rel) = stdout[after..].find(DISSENT_CLOSE) else {
        // 标记不完整：按普通输出处理，不猜。
        return (stdout.trim_end().to_owned(), None);
    };
    let end = after + end_rel;
    let payload = stdout[after..end].trim();
    let Some(dissent) = serde_json::from_str::<Dissent>(payload).ok() else {
        // 块存在但内容不是合法异议：不猜、不剥离，按普通输出原样返回。
        return (stdout.trim_end().to_owned(), None);
    };
    let mut content = String::new();
    content.push_str(stdout[..start].trim_end());
    let tail = stdout[end + DISSENT_CLOSE.len()..].trim_start();
    if !tail.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(tail.trim_end());
    }
    (content, Some(dissent))
}

/// 解析 `<<<STACK>>>{json}<<<END>>>` 块；不存在或非法时返回 None。
pub fn parse_stack_block(stdout: &str) -> Option<trit_orch::StackReport> {
    let start = stdout.find("<<<STACK>>>")?;
    let after = start + "<<<STACK>>>".len();
    let end_rel = stdout[after..].find(DISSENT_CLOSE)?;
    serde_json::from_str::<trit_orch::StackReport>(stdout[after..after + end_rel].trim()).ok()
}

/// 一次外部 CLI 子智能体的运行结果。
#[derive(Debug, Clone, Default)]
pub struct CliAgentRun {
    pub agent: String,
    pub content: String,
    pub dissent: Option<Dissent>,
    pub exit: Option<i32>,
    /// 超时被强制结束时为 true。
    pub timed_out: bool,
    /// stderr 末尾（截断），便于排障。
    pub stderr_tail: String,
}

/// 跑一次外部 CLI 子智能体。
///
/// 超时后主动结束进程；stdout 超过上限直接失败；stderr 只保留末尾。
pub fn run_cli_agent(
    spec: &CliAgentSpec,
    prompt: &str,
    workspace: &Path,
    timeout: Duration,
) -> Result<CliAgentRun, AgentError> {
    let (program, argv, stdin_prompt) = spec.command(prompt);
    let mut command = Command::new(&program);
    // 外部 CLI 不继承父进程环境：避免把 provider 凭据带出去。
    crate::child_env::sanitize_with(&mut command, std::iter::empty::<(String, String)>());
    command
        .args(&argv)
        .current_dir(workspace)
        .stdin(if stdin_prompt.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| AgentError::Io(format!("spawn subagent {}: {error}", spec.id)))?;

    if let Some(prompt) = stdin_prompt
        && let Some(mut stdin) = child.stdin.take()
    {
        // 大 prompt 不能阻塞主线程：写 stdin 放到独立线程，写完 drop 关闭。
        std::thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }

    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let result = stdout
                .take(MAX_CLI_OUTPUT_BYTES + 1)
                .read_to_end(&mut buffer);
            let _ = sender.send(("out", result, buffer));
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let result = stderr
                .take(MAX_CLI_OUTPUT_BYTES + 1)
                .read_to_end(&mut buffer);
            let _ = sender.send(("err", result, buffer));
        });
    }
    drop(sender);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => return Err(AgentError::Io(format!("wait subagent: {error}"))),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let drain_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < drain_deadline {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(("out", Ok(_), buffer)) => stdout = buffer,
            Ok(("out", Err(error), _)) => {
                return Err(AgentError::Io(format!("read subagent stdout: {error}")));
            }
            Ok((_, _, buffer)) => stderr = buffer,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(("out", Ok(_), buffer)) = receiver.try_recv() {
        stdout = buffer;
    }
    while let Ok((_, _, buffer)) = receiver.try_recv() {
        stderr = buffer;
    }

    if stdout.len() as u64 > MAX_CLI_OUTPUT_BYTES {
        return Err(AgentError::Io(format!(
            "subagent {} output exceeds {MAX_CLI_OUTPUT_BYTES} bytes",
            spec.id
        )));
    }
    let text = String::from_utf8_lossy(&stdout);
    let (content, dissent) = parse_cli_output(&text);
    // 子智能体不得自行引入新技术栈：默认允许集合为空，越权即失败。
    if let Some(stack) = parse_stack_block(&text) {
        trit_orch::validate_stack(&stack, &trit_orch::StackPolicy::default())
            .map_err(|error| AgentError::Provider(error.to_string()))?;
    }
    let stderr_text = String::from_utf8_lossy(&stderr);
    let stderr_tail: String = {
        let mut tail: Vec<String> = stderr_text
            .lines()
            .rev()
            .take(20)
            .map(|line| crate::diagnostics::redact_line(line, workspace))
            .collect();
        tail.reverse();
        tail.join("\n")
    };
    Ok(CliAgentRun {
        agent: spec.id.to_owned(),
        content,
        dissent,
        exit: status.and_then(|status| status.code()),
        timed_out,
        stderr_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_backends_cover_the_requested_clis() {
        for id in ["codex", "claude-code", "qoder", "zcode", "doubao"] {
            assert!(CliAgentSpec::resolve(id).is_some(), "missing backend {id}");
        }
        assert!(CliAgentSpec::resolve("unknown").is_none());
    }

    #[test]
    fn command_places_prompt_by_transport() {
        let codex = CliAgentSpec::resolve("codex").unwrap();
        let (program, argv, stdin) = codex.command("fix it");
        assert_eq!(program, "codex");
        assert_eq!(argv.last().map(String::as_str), Some("fix it"));
        assert!(stdin.is_none());

        let doubao = CliAgentSpec::resolve("doubao").unwrap();
        let (_program, argv, stdin) = doubao.command("fix it");
        assert!(!argv.iter().any(|arg| arg == "fix it"));
        assert_eq!(stdin.as_deref(), Some("fix it"));
    }

    #[test]
    fn env_overrides_program_and_args() {
        let spec = CliAgentSpec::resolve("zcode").unwrap();
        // SAFETY: 测试内单线程设置/清理同名变量。
        unsafe {
            std::env::set_var("TRIT_SUBAGENT_ZCODE_PROGRAM", "/opt/zcode/bin/zed");
            std::env::set_var("TRIT_SUBAGENT_ZCODE_ARGS", "--json run");
        }
        assert_eq!(spec.program(), "/opt/zcode/bin/zed");
        assert_eq!(spec.args(), vec!["--json".to_owned(), "run".to_owned()]);
        unsafe {
            std::env::remove_var("TRIT_SUBAGENT_ZCODE_PROGRAM");
            std::env::remove_var("TRIT_SUBAGENT_ZCODE_ARGS");
        }
    }

    #[test]
    fn parses_dissent_block_and_keeps_content() {
        let output = "changed 2 files\n<<<DISSENT>>>{\"target\":\"schema change\",\"reason\":\"breaks old logs\",\"suggestion\":\"add a versioned field\"}<<<END>>>\ndone";
        let (content, dissent) = parse_cli_output(output);
        assert_eq!(content, "changed 2 files\ndone");
        let dissent = dissent.expect("dissent");
        assert_eq!(dissent.target, "schema change");
        assert_eq!(dissent.reason, "breaks old logs");
        assert_eq!(dissent.suggestion, "add a versioned field");
    }

    #[test]
    fn malformed_dissent_is_not_guessed() {
        let output = "plain <<<DISSENT>>>not-json<<<END>>>";
        let (content, dissent) = parse_cli_output(output);
        assert!(dissent.is_none());
        assert!(content.contains("not-json"), "{content}");
    }

    #[test]
    fn output_without_dissent_is_untouched() {
        let (content, dissent) = parse_cli_output("  ok  \n");
        assert_eq!(content, "  ok");
        assert!(dissent.is_none());
    }
}
