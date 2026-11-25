//! 子 agent 进程与 IPC 契约（B7.6）：帧格式、请求/响应、引用校验与结局映射。
//!
//! 这一层只定义**契约**，不负责 spawn 进程：宿主用任意传输（stdin/stdout 管道、
//! socket、测试替身）承载同一套帧。
//!
//! 帧格式：4 字节大端长度 + UTF-8 JSON，单帧上限 [`MAX_FRAME_BYTES`]。
//! 两条不变量：
//!
//! 1. **响应必须匹配请求**：`task + epoch` 对不上就拒绝——防止旧响应串到新计划，
//!    或两个子 agent 的输出互相顶替；
//! 2. **没有合法响应就没有完成**：进程零退出但没发响应、非零退出、被超时杀掉，
//!    一律不产生 `Completed`（见 [`exit_status`]）。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{
    ContextCapsule, DelegationResult, DelegationStatus, Error, SubagentProfile, TaskSpec,
    digest_hex,
};

/// 单帧上限：8 MiB。超过即拒绝，避免畸形长度前缀导致的内存放大。
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 宿主发给子 agent 的请求帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// 子任务 ID。
    pub task: String,
    /// 任务定义 epoch；响应必须回显同一个值。
    pub epoch: u64,
    /// 显式继承的上下文胶囊。
    #[serde(default)]
    pub capsule: ContextCapsule,
    /// 子 agent 的权限契约。
    pub profile: SubagentProfile,
}

impl SubagentRequest {
    pub fn new(task: &TaskSpec, capsule: ContextCapsule, profile: SubagentProfile) -> Self {
        Self {
            task: task.task.id.clone(),
            epoch: task.epoch,
            capsule,
            profile,
        }
    }
}

/// 子 agent 返回的响应帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentResponse {
    /// 回显请求的任务 ID。
    pub task: String,
    /// 回显请求的 epoch。
    pub epoch: u64,
    /// 子 agent 自报的状态；宿主只信这个字段，不因“收到响应”就当成完成。
    pub status: DelegationStatus,
    /// 结果内容；digest 由它计算。
    #[serde(default)]
    pub content: Vec<u8>,
    /// 实际消耗的 token。
    #[serde(default)]
    pub tokens: u64,
}

impl SubagentResponse {
    /// 响应的 `task + epoch` 必须与请求一致，否则视为协议违约。
    pub fn validate_for(&self, request: &SubagentRequest) -> Result<(), Error> {
        if self.task != request.task || self.epoch != request.epoch {
            return Err(Error::SubagentProtocol {
                message: format!(
                    "response {}@{} does not match request {}@{}",
                    self.task, self.epoch, request.task, request.epoch
                ),
            });
        }
        Ok(())
    }

    /// 转成宿主的稳定结果引用。
    pub fn into_delegation_result(self) -> DelegationResult {
        DelegationResult {
            task: self.task,
            epoch: self.epoch,
            digest: digest_hex(&self.content),
            status: self.status,
            tokens: self.tokens,
        }
    }
}

/// 把值编码成一帧（4 字节大端长度 + JSON）。
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let payload = serde_json::to_vec(value).map_err(|error| Error::SubagentProtocol {
        message: error.to_string(),
    })?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(Error::SubagentProtocol {
            message: format!("frame {} exceeds {MAX_FRAME_BYTES}", payload.len()),
        });
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// 从缓冲区头部取一帧；不足一帧返回 `Ok(None)`，成功后把该帧从缓冲区移除。
///
/// 支持粘包与半包：调用方可以持续 `read` 进同一个缓冲区再反复调用。
pub fn decode_frame<T: DeserializeOwned>(buffer: &mut Vec<u8>) -> Result<Option<T>, Error> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(Error::SubagentProtocol {
            message: format!("frame {len} exceeds {MAX_FRAME_BYTES}"),
        });
    }
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let value =
        serde_json::from_slice(&buffer[4..4 + len]).map_err(|error| Error::SubagentProtocol {
            message: error.to_string(),
        })?;
    buffer.drain(..4 + len);
    Ok(Some(value))
}

/// 没有收到合法响应时，用进程结局决定委派状态。
///
/// - 被信号/超时终止（`None`）→ `Aborted`；
/// - 零退出但没发响应 → `Failed`（协议违约）；
/// - 非零退出 → `Failed`（带退出码）。
pub fn exit_status(exit: Option<i32>) -> DelegationStatus {
    match exit {
        None => DelegationStatus::Aborted,
        Some(0) => DelegationStatus::Failed {
            message: "subagent exited without a response".into(),
        },
        Some(code) => DelegationStatus::Failed {
            message: format!("subagent exited with status {code}"),
        },
    }
}

/// 跨机器执行的端点描述。
///
/// 只做**契约校验**，不做真实网络传输：载荷仍是本模块的帧，宿主决定用什么传输
/// （HTTP POST、长连接、消息队列）。校验的目标是保证“认证 token 不会走明文远程”。
///
/// 宿主侧的最小真实传输在 `trit-host`：`remote_subagent::run_remote_subagent`
/// 把一帧 POST 到 `url`，`remote_serve` / `trit-host remote-echo` 是对应的参考
/// 服务端。节点发现、跨机调度与跨机失败恢复不在契约层范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEndpoint {
    /// 端点地址；必须 `https://`，或 loopback 的 `http://`。
    pub url: String,
    /// 可选 bearer token；空白视为错误。
    #[serde(default)]
    pub auth_token: Option<String>,
    /// 请求超时（秒）；必须大于 0。
    #[serde(default = "default_remote_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_remote_timeout_secs() -> u64 {
    30
}

impl RemoteEndpoint {
    /// 校验端点：远程必须 https；明文 http 只允许 loopback。
    pub fn validate(&self) -> Result<(), Error> {
        if self.url.trim().is_empty() {
            return Err(Error::SubagentProtocol {
                message: "remote endpoint url is blank".into(),
            });
        }
        if self.timeout_secs == 0 {
            return Err(Error::SubagentProtocol {
                message: "remote endpoint timeout must be positive".into(),
            });
        }
        let url = self.url.trim();
        if !url.starts_with("https://") && !is_loopback_http(url) {
            return Err(Error::SubagentProtocol {
                message: format!("remote endpoint must use https (or loopback http): {url}"),
            });
        }
        if authority_host(url).is_empty() {
            return Err(Error::SubagentProtocol {
                message: "remote endpoint has no host".into(),
            });
        }
        if self
            .auth_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            return Err(Error::SubagentProtocol {
                message: "remote auth token is blank".into(),
            });
        }
        Ok(())
    }
}

fn is_loopback_http(url: &str) -> bool {
    url.strip_prefix("http://").is_some()
        && matches!(authority_host(url), "127.0.0.1" | "localhost" | "::1")
}

/// 从 `scheme://authority/path` 里取出 host（处理 IPv6 方括号）。
fn authority_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let authority = rest.split('/').next().unwrap_or("");
    if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    }
}
