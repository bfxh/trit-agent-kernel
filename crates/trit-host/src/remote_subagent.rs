//! 远程子 agent：把 [`trit_orch::subagent`] 的帧经 HTTP POST 送到跨机器端点。
//!
//! 契约（[`RemoteEndpoint`]）只负责准入；本模块是宿主侧的**最小真实传输**：
//! 一次委派 = 一个 HTTP/1.1 `POST`，请求体是单帧（4 字节长度 + JSON），
//! 响应体也必须是单帧。服务端参考实现见 [`crate::remote_serve`]，
//! 命令行入口 `trit-host remote-echo`。
//!
//! 不变量（新增外连必须有据可查，见 `HANDOFF-SAFETY.md §2`）：
//!
//! 1. **先校验再连网**：`RemoteEndpoint::validate` 失败直接返回，绝不发请求；
//!    明文 `http://` 只允许 loopback，bearer token 不会走明文远程；
//! 2. **不跟随重定向**（`redirects(0)`）：3xx 可能把 `Authorization` 引到另一个
//!    host，所以 3xx 一律按“拒绝”处理，不做跳转；
//! 3. **响应体有界**：最多读 [`MAX_RESPONSE_BYTES`] + 1 字节，超限不解码；
//! 4. **没有合法帧就没有完成**：空体、截断帧、坏 JSON、非 2xx、传输失败
//!    都不产生 [`DelegationStatus::Completed`]；
//! 5. **不自动重试**：远程可能已经执行过，重试需要幂等键；本模块不做隐式重试。

use std::io::Read;
use std::time::Duration;

use trit_orch::{
    DelegationResult, DelegationStatus, MAX_FRAME_BYTES, RemoteEndpoint, SubagentRequest,
    SubagentResponse, decode_frame, digest_hex, encode_frame,
};

use crate::AgentError;

/// 远程端点交换的帧媒体类型；请求与响应都用它。
pub const FRAME_CONTENT_TYPE: &str = "application/x-trit-frame";

/// 响应体硬上限：单帧上限加 4 字节长度前缀。
pub const MAX_RESPONSE_BYTES: usize = MAX_FRAME_BYTES + 4;

/// 一次远程调用的完整产出：稳定引用 + 原始内容。
///
/// 内容只在宿主侧用于拼下游输入；跨机传递本身仍以 `task@epoch#digest` 为引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReply {
    pub result: DelegationResult,
    pub content: Vec<u8>,
}

/// 把请求帧 POST 到 `endpoint`，把合法响应映射成稳定结果引用。
///
/// 返回 `Ok` 的两种情况：
///
/// - 收到合法响应 → 对应 `Completed` / `Failed` / `Aborted`（由子 agent 自报，
///   宿主只信 `status` 字段）；
/// - 没有合法响应 → 非 2xx 记 `Failed`，传输失败记 `Aborted`。
///
/// 协议违约（解帧失败、`task + epoch` 不匹配）返回 [`AgentError::Kernel`]，
/// 与进程内路径保持一致：这类错误不会被降级成“完成”。
pub fn run_remote_subagent(
    endpoint: &RemoteEndpoint,
    request: &SubagentRequest,
) -> Result<DelegationResult, AgentError> {
    Ok(run_remote_subagent_reply(endpoint, request)?.result)
}

/// [`run_remote_subagent`] 的完整版本：同时返回响应内容，供跨机派发拼装下游输入。
pub fn run_remote_subagent_reply(
    endpoint: &RemoteEndpoint,
    request: &SubagentRequest,
) -> Result<RemoteReply, AgentError> {
    endpoint.validate()?;
    let frame = encode_frame(request)?;

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(endpoint.timeout_secs))
        .redirects(0)
        .build();
    let mut call = agent
        .post(endpoint.url.trim())
        .set("Content-Type", FRAME_CONTENT_TYPE)
        .set("Accept", FRAME_CONTENT_TYPE);
    if let Some(token) = endpoint.auth_token.as_deref() {
        call = call.set("Authorization", &format!("Bearer {token}"));
    }

    let response = match call.send_bytes(&frame) {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            return Ok(unresolved_reply(
                request,
                DelegationStatus::Failed {
                    message: format!("remote endpoint rejected the request with HTTP {code}"),
                },
            ));
        }
        // 连接 / DNS / 超时：对端是否执行过无法判定，只能按未确认（Aborted）处理。
        Err(ureq::Error::Transport(_)) => {
            return Ok(unresolved_reply(request, DelegationStatus::Aborted));
        }
    };

    // 3xx（redirects(0) 时不跳转）与其它非 2xx 都按拒绝处理。
    if !(200..300).contains(&response.status()) {
        return Ok(unresolved_reply(
            request,
            DelegationStatus::Failed {
                message: format!("remote endpoint answered HTTP {}", response.status()),
            },
        ));
    }

    let mut body = Vec::new();
    // 读到一半断开同样算传输失败：对端是否执行过无法判定，按 Aborted 处理。
    if response
        .into_reader()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .is_err()
    {
        return Ok(unresolved_reply(request, DelegationStatus::Aborted));
    }
    if body.len() > MAX_RESPONSE_BYTES {
        // 超限体不解码：避免畸形长度前缀造成内存放大。
        return Ok(unresolved_reply(
            request,
            DelegationStatus::Failed {
                message: format!("remote response exceeds {MAX_RESPONSE_BYTES} bytes"),
            },
        ));
    }

    match decode_frame::<SubagentResponse>(&mut body) {
        Ok(Some(response)) => {
            response.validate_for(request)?;
            let result = DelegationResult {
                task: response.task,
                epoch: response.epoch,
                digest: digest_hex(&response.content),
                status: response.status,
                tokens: response.tokens,
            };
            Ok(RemoteReply {
                result,
                content: response.content,
            })
        }
        Ok(None) if body.is_empty() => Ok(unresolved_reply(
            request,
            DelegationStatus::Failed {
                message: "remote endpoint returned no response frame".into(),
            },
        )),
        Ok(None) => Ok(unresolved_reply(
            request,
            DelegationStatus::Failed {
                message: "remote endpoint returned a truncated response frame".into(),
            },
        )),
        Err(error) => Err(AgentError::Kernel(error)),
    }
}

/// 没有合法响应时的保守结局：digest 为空，token 记 0，绝不伪装成完成。
fn unresolved(request: &SubagentRequest, status: DelegationStatus) -> DelegationResult {
    DelegationResult {
        task: request.task.clone(),
        epoch: request.epoch,
        digest: digest_hex(b""),
        status,
        tokens: 0,
    }
}

fn unresolved_reply(request: &SubagentRequest, status: DelegationStatus) -> RemoteReply {
    RemoteReply {
        result: unresolved(request, status),
        content: Vec::new(),
    }
}
