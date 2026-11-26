//! 子 agent 进程：spawn、帧交换与超时 kill（B7.6）。
//!
//! 契约由 [`trit_orch::subagent`] 定义；这一层负责把它落到真实进程：
//!
//! 1. spawn 子进程（stdin/stdout 管道，stderr 丢弃）；
//! 2. 写入请求帧并关闭 stdin（子进程看到 EOF）；
//! 3. 读线程有界收集 stdout，主线程 `recv_timeout` 等待；
//! 4. 超时 → `kill` + `wait`，返回 [`DelegationStatus::Aborted`]；
//! 5. 收到响应 → 强制校验 `task + epoch`，不匹配按协议错误拒绝；
//! 6. 无响应 → 用 [`trit_orch::exit_status`] 映射进程结局，绝不产生 `Completed`；
//! 7. 取消令牌置位 → kill 直接子进程并回 `Aborted`，不等到超时；
//! 8. stdout 超过 [`MAX_FRAME_BYTES`] + 4 → kill 子进程并返回 I/O 错误，不无限读。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use trit_orch::{
    CancelToken, DelegationResult, DelegationStatus, MAX_FRAME_BYTES, SubagentRequest,
    SubagentResponse, decode_frame, digest_hex, encode_frame, exit_status,
};

use crate::AgentError;

/// 取消轮询步长：保证 `kill` 不会等到整个超时才发生。
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// 子进程 stdout 最多读「4 字节长度 + 最大负载」再多 1 字节用于判定超限；
/// 更多字节不再读入。
const MAX_SUBAGENT_STDOUT_BYTES: usize = MAX_FRAME_BYTES + 4;

/// 读线程的结果：合法上限内的原始 stdout，或已超过帧上限。
enum StdoutRead {
    Frame(Vec<u8>),
    TooLarge,
}

/// 子 agent 进程配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentProcessConfig {
    /// 可执行文件路径。
    pub program: PathBuf,
    /// 传给可执行文件的参数。
    pub args: Vec<String>,
    /// 显式传给子进程的额外环境变量；其余宿主环境（含 provider key）不继承。
    pub env: Vec<(String, String)>,
    /// 等待响应的上限；超时即 kill。
    pub timeout: Duration,
}

impl SubagentProcessConfig {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args = args.into_iter().collect();
        self
    }

    /// 显式追加环境变量；不调用时子进程只拿到 [`crate::child_env`] 白名单。
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// 一次子 agent 交换的完整回包：结果元数据 + 原始内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentReply {
    pub result: DelegationResult,
    pub content: Vec<u8>,
}

/// spawn 子 agent 并完成一次请求/响应交换，只返回结果元数据。
pub fn run_subagent(
    config: &SubagentProcessConfig,
    request: &SubagentRequest,
) -> Result<DelegationResult, AgentError> {
    run_subagent_reply(config, request).map(|reply| reply.result)
}

/// spawn 子 agent 并完成一次请求/响应交换，保留响应内容。
pub fn run_subagent_reply(
    config: &SubagentProcessConfig,
    request: &SubagentRequest,
) -> Result<SubagentReply, AgentError> {
    run_subagent_reply_with_cancel(config, request, None)
}

/// 同 [`run_subagent_reply`]，但取消令牌置位时 kill 子进程并返回 `Aborted`。
///
/// 只 kill 直接子进程；如果子进程自己又起了外部进程，那些进程不在本层管理。
pub fn run_subagent_reply_cancellable(
    config: &SubagentProcessConfig,
    request: &SubagentRequest,
    cancel: &CancelToken,
) -> Result<SubagentReply, AgentError> {
    run_subagent_reply_with_cancel(config, request, Some(cancel))
}

fn run_subagent_reply_with_cancel(
    config: &SubagentProcessConfig,
    request: &SubagentRequest,
    cancel: Option<&CancelToken>,
) -> Result<SubagentReply, AgentError> {
    let mut command = Command::new(&config.program);
    crate::child_env::sanitize_with(&mut command, config.env.iter().cloned());
    let mut child = command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| AgentError::Io(error.to_string()))?;

    let frame = encode_frame(request)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&frame)
            .map_err(|error| AgentError::Io(error.to_string()))?;
        // stdin 在此 drop：子进程读到 EOF，知道请求结束。
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Io("subagent stdout unavailable".into()))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::with_capacity(64 * 1024);
        let mut limited = stdout.take((MAX_SUBAGENT_STDOUT_BYTES + 1) as u64);
        let _ = limited.read_to_end(&mut buffer);
        let outcome = if buffer.len() > MAX_SUBAGENT_STDOUT_BYTES {
            StdoutRead::TooLarge
        } else {
            StdoutRead::Frame(buffer)
        };
        let _ = sender.send(outcome);
    });

    let deadline = Instant::now() + config.timeout;
    let poll = config.timeout.min(CANCEL_POLL);
    let stdout = loop {
        match receiver.recv_timeout(poll) {
            Ok(stdout) => break stdout,
            Err(RecvTimeoutError::Timeout) => {
                if cancel.is_some_and(CancelToken::is_cancelled) || Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(aborted_reply(request));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(aborted_reply(request));
            }
        }
    };
    let buffer = match stdout {
        StdoutRead::Frame(buffer) => buffer,
        StdoutRead::TooLarge => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AgentError::Io(format!(
                "subagent stdout exceeds {MAX_SUBAGENT_STDOUT_BYTES} bytes"
            )));
        }
    };
    let exit = child.wait().ok().and_then(|status| status.code());

    let mut buffer = buffer;
    match decode_frame::<SubagentResponse>(&mut buffer) {
        Ok(Some(response)) => {
            response.validate_for(request)?;
            let content = response.content.clone();
            let result = response.into_delegation_result();
            Ok(SubagentReply { result, content })
        }
        Ok(None) => Ok(SubagentReply {
            result: DelegationResult {
                task: request.task.clone(),
                epoch: request.epoch,
                digest: digest_hex(b""),
                status: exit_status(exit),
                tokens: 0,
            },
            content: Vec::new(),
        }),
        Err(error) => Err(AgentError::Kernel(error)),
    }
}

/// 未确认完成（超时 / 取消 / 读出断开）时的保守回包。
fn aborted_reply(request: &SubagentRequest) -> SubagentReply {
    SubagentReply {
        result: DelegationResult {
            task: request.task.clone(),
            epoch: request.epoch,
            digest: digest_hex(b""),
            status: DelegationStatus::Aborted,
            tokens: 0,
        },
        content: Vec::new(),
    }
}
