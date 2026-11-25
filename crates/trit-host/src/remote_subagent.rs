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
//! 5. **不自动重试**：远程可能已经执行过，重试需要幂等键；本模块不做隐式重试；
//! 6. **取消是通知**：可取消版本等待期间发现令牌置位，会向端点 origin 的
//!    `/cancel` 发同一幂等键；最多等 2s 收尾后按 `Aborted` 返回。对端未实现
//!    `/cancel` 时远端仍可能执行完，调用方不能把 `Aborted` 当作“一定没写”。

use std::io::Read;
use std::time::Duration;

use trit_orch::{
    DelegationResult, DelegationStatus, MAX_FRAME_BYTES, RemoteEndpoint, SubagentRequest,
    SubagentResponse, decode_frame, digest_hex, encode_frame,
};

use crate::AgentError;

/// 远程端点交换的帧媒体类型；请求与响应都用它。
pub const FRAME_CONTENT_TYPE: &str = "application/x-trit-frame";

/// 幂等键头：值是 `"<task>@<epoch>"`。
///
/// 跨机重试必须复用同一个键；服务端可据此去重（参考实现 `remote-echo` 会缓存
/// 首次响应）。没有这个头时服务端按普通请求处理，不去重。
pub const IDEMPOTENCY_HEADER: &str = "X-Trit-Idempotency";

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
        .set("Accept", FRAME_CONTENT_TYPE)
        // 重试必须复用同一个幂等键；服务端据此去重。
        .set(
            IDEMPOTENCY_HEADER,
            &format!("{}@{}", request.task, request.epoch),
        );
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

/// [`run_remote_subagent_reply`] 的可取消版本。
///
/// 请求在独立线程里执行，调用线程每 25ms 检查一次取消令牌；令牌置位后向端点
/// `POST /cancel`（带同一幂等键），最多再等 2s 收尾，之后返回 `Aborted`。
/// 对端若不支持 `/cancel`，请求线程会继续到端点超时；因此远端可能仍执行完，
/// 调用方不应把 `Aborted` 当成“远端一定没写”。
pub fn run_remote_subagent_reply_cancellable(
    endpoint: &RemoteEndpoint,
    request: &SubagentRequest,
    cancel: &trit_orch::CancelToken,
) -> Result<RemoteReply, AgentError> {
    if cancel.is_cancelled() {
        return Ok(unresolved_reply(request, DelegationStatus::Aborted));
    }
    let endpoint_for_call = endpoint.clone();
    let request_for_call = request.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(run_remote_subagent_reply(
            &endpoint_for_call,
            &request_for_call,
        ));
    });

    loop {
        match receiver.recv_timeout(CLIENT_CANCEL_POLL) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AgentError::Io(
                    "remote request worker ended without a reply".into(),
                ));
            }
        }
    }

    let key = format!("{}@{}", request.task, request.epoch);
    // 取消是 best-effort：通知失败也要按 Aborted 返回，不能让调用方卡住。
    let _ = cancel_remote_subagent(endpoint, &key);
    match receiver.recv_timeout(CLIENT_CANCEL_GRACE) {
        Ok(result) => result,
        Err(_) => Ok(unresolved_reply(request, DelegationStatus::Aborted)),
    }
}

/// 取消入口固定在端点 **origin** 上：`http://host:port/run` → `http://host:port/cancel`。
fn cancel_endpoint_url(endpoint_url: &str) -> String {
    let trimmed = endpoint_url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return format!("{trimmed}{CANCEL_PATH}");
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    format!("{scheme}://{authority}{CANCEL_PATH}")
}

/// 通知远程端点取消一个在途请求；未知键按服务端语义视为 no-op。
///
/// 返回 `Ok(())` 只代表通知被端点接受，不代表远端已经停止执行。
pub fn cancel_remote_subagent(endpoint: &RemoteEndpoint, key: &str) -> Result<(), String> {
    endpoint.validate().map_err(|error| error.to_string())?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(
            endpoint.timeout_secs.clamp(1, CANCEL_REQUEST_TIMEOUT_SECS),
        ))
        .redirects(0)
        .build();
    let url = cancel_endpoint_url(&endpoint.url);
    let mut call = agent.post(&url).set(IDEMPOTENCY_HEADER, key);
    if let Some(token) = endpoint.auth_token.as_deref() {
        call = call.set("Authorization", &format!("Bearer {token}"));
    }
    match call.send_bytes(&[]) {
        Ok(response) if (200..300).contains(&response.status()) => Ok(()),
        Ok(response) => Err(format!("cancel rejected with HTTP {}", response.status())),
        Err(ureq::Error::Status(code, _)) => Err(format!("cancel rejected with HTTP {code}")),
        Err(ureq::Error::Transport(error)) => Err(format!("cancel transport: {error}")),
    }
}

/// 服务端取消入口：`POST <endpoint>/cancel` + `X-Trit-Idempotency: <task>@<epoch>`。
pub const CANCEL_PATH: &str = "/cancel";

/// 取消轮询步长。
const CLIENT_CANCEL_POLL: Duration = Duration::from_millis(25);

/// 发出取消后等待对端收尾的上限；超时即按未确认完成返回。
const CLIENT_CANCEL_GRACE: Duration = Duration::from_secs(2);

/// 取消请求自身的短超时，避免取消动作把调用方拖住。
const CANCEL_REQUEST_TIMEOUT_SECS: u64 = 5;

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
