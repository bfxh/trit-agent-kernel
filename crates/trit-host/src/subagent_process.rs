//! 子 agent 进程：spawn、帧交换与超时 kill（B7.6）。
//!
//! 契约由 [`trit_orch::subagent`] 定义；这一层负责把它落到真实进程：
//!
//! 1. spawn 子进程（stdin/stdout 管道，stderr 丢弃）；
//! 2. 写入请求帧并关闭 stdin（子进程看到 EOF）；
//! 3. 读线程收集 stdout，主线程 `recv_timeout` 等待；
//! 4. 超时 → `kill` + `wait`，返回 [`DelegationStatus::Aborted`]；
//! 5. 收到响应 → 强制校验 `task + epoch`，不匹配按协议错误拒绝；
//! 6. 无响应 → 用 [`trit_orch::exit_status`] 映射进程结局，绝不产生 `Completed`。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use trit_orch::{
    DelegationResult, DelegationStatus, SubagentRequest, SubagentResponse, decode_frame,
    digest_hex, encode_frame, exit_status,
};

use crate::AgentError;

/// 子 agent 进程配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentProcessConfig {
    /// 可执行文件路径。
    pub program: PathBuf,
    /// 传给可执行文件的参数。
    pub args: Vec<String>,
    /// 等待响应的上限；超时即 kill。
    pub timeout: Duration,
}

impl SubagentProcessConfig {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args = args.into_iter().collect();
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
    let mut child = Command::new(&config.program)
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

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Io("subagent stdout unavailable".into()))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = sender.send(buffer);
    });

    let buffer = match receiver.recv_timeout(config.timeout) {
        Ok(buffer) => buffer,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(SubagentReply {
                result: DelegationResult {
                    task: request.task.clone(),
                    epoch: request.epoch,
                    digest: digest_hex(b""),
                    status: DelegationStatus::Aborted,
                    tokens: 0,
                },
                content: Vec::new(),
            });
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
