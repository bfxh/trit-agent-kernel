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
//!    `/cancel` 时远端仍可能执行完，调用方不能把 `Aborted` 当作“一定没写”；
//! 7. **租约丢失不是节点故障**：服务端因账本租约被接管而中止时，会在合法
//!    `Aborted` 帧旁带 `X-Trit-Lease-Lost: 1`；[`RemoteReply::lease_lost`] 供
//!    调度层区分“节点不可用”与“任务已由其他持有者接手”。
//! 8. **执行时限不是节点故障**：服务端墙钟到点会回带 `X-Trit-Deadline-Exceeded: 1`
//!    的合法 `Aborted`；[`RemoteReply::deadline_exceeded`] 供调度层区分
//!    “节点失联”与“节点健康但本次请求超预算”。
//! 9. **健康探测只读且同鉴权**：`GET <origin>/health` 沿用 Bearer、不跳转、
//!    不派发任务；只有 2xx 才算可达，供显式恢复隔离节点使用。

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use trit_orch::{
    DelegationResult, DelegationStatus, MAX_FRAME_BYTES, RemoteEndpoint, SubagentRequest,
    SubagentResponse, decode_frame, digest_hex, encode_frame,
};

use crate::AgentError;

/// 远程端点交换的帧媒体类型；请求与响应都用它。
pub const FRAME_CONTENT_TYPE: &str = "application/x-trit-frame";

/// 按 timeout 复用 ureq 连接池；send/cancel/health 共享同一个 agent。
fn agent_for(timeout_secs: u64) -> ureq::Agent {
    static AGENTS: OnceLock<Mutex<BTreeMap<u64, ureq::Agent>>> = OnceLock::new();
    let agents = AGENTS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut agents = agents
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    agents
        .entry(timeout_secs)
        .or_insert_with(|| {
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(timeout_secs))
                .redirects(0)
                .build()
        })
        .clone()
}

/// 幂等键头：值是 `"<task>@<epoch>"`，配了计划作用域时是
/// `"<scope>|<task>@<epoch>"`。
///
/// 跨机重试必须复用同一个键；服务端可据此去重（参考实现 `remote-echo` 会缓存
/// 首次响应）。没有这个头时服务端按普通请求处理，不去重。不同计划即使任务 ID
/// 相同，也通过 `scope` 隔离，避免账本/缓存互相回放。
pub const IDEMPOTENCY_HEADER: &str = "X-Trit-Idempotency";

/// 租约丢失标记头：服务端因账本租约被接管而中止本机执行时置 `1`。
/// 客户端据此区分“节点故障”与“同一任务已由其他持有者接手”，后者不应隔离节点。
pub const LEASE_LOST_HEADER: &str = "X-Trit-Lease-Lost";

/// 服务端执行时限到点标记：超时返回的 `Aborted` 帧旁带 `1`。
/// 客户端据此区分“节点不可用”与“节点健康但请求超出本次墙钟预算”。
pub const DEADLINE_EXCEEDED_HEADER: &str = "X-Trit-Deadline-Exceeded";

/// 计划作用域与帧键之间的分隔符；作用域本身不允许包含它。
pub const IDEMPOTENCY_SCOPE_SEPARATOR: char = '|';

/// 计划作用域的最大字节数（只允许 ASCII，因此等于字符数）。
pub const MAX_IDEMPOTENCY_SCOPE_LEN: usize = 128;

/// 判断值能否作为单段命名空间目录（作用域、任务 ID）。
pub(crate) fn is_safe_namespace_component(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// 校验计划作用域：非空、长度有界、只含安全字符、不是 `.`/`..`。
pub fn validate_idempotency_scope(scope: &str) -> Result<(), AgentError> {
    if scope.len() > MAX_IDEMPOTENCY_SCOPE_LEN || !is_safe_namespace_component(scope) {
        return Err(AgentError::Kernel(trit_orch::Error::SubagentProtocol {
            message: format!("idempotency scope {scope:?} is not a safe namespace"),
        }));
    }
    Ok(())
}

/// 组装幂等键：`scope|task@epoch`，无作用域时保持旧的 `task@epoch`。
pub fn idempotency_key(scope: Option<&str>, task: &str, epoch: u64) -> String {
    match scope {
        Some(scope) => format!("{scope}{IDEMPOTENCY_SCOPE_SEPARATOR}{task}@{epoch}"),
        None => format!("{task}@{epoch}"),
    }
}

/// 从请求头里的键还原计划作用域，并校验 `task@epoch` 后缀。
pub fn idempotency_scope<'a>(
    key: &'a str,
    task: &str,
    epoch: u64,
) -> Result<Option<&'a str>, AgentError> {
    let frame_key = format!("{task}@{epoch}");
    if key == frame_key {
        return Ok(None);
    }
    let Some((scope, rest)) = key.split_once(IDEMPOTENCY_SCOPE_SEPARATOR) else {
        return Err(AgentError::Kernel(trit_orch::Error::SubagentProtocol {
            message: "idempotency key does not match task@epoch".into(),
        }));
    };
    if rest != frame_key
        || scope.len() > MAX_IDEMPOTENCY_SCOPE_LEN
        || !is_safe_namespace_component(scope)
    {
        return Err(AgentError::Kernel(trit_orch::Error::SubagentProtocol {
            message: "idempotency key does not match task@epoch".into(),
        }));
    }
    Ok(Some(scope))
}

/// 响应体硬上限：单帧上限加 4 字节长度前缀。
pub const MAX_RESPONSE_BYTES: usize = MAX_FRAME_BYTES + 4;

/// 一次远程调用的完整产出：稳定引用 + 原始内容。
///
/// 内容只在宿主侧用于拼下游输入；跨机传递本身仍以 `task@epoch#digest` 为引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReply {
    pub result: DelegationResult,
    pub content: Vec<u8>,
    /// 服务端因账本租约被其他持有者接管而中止：节点仍健康，任务应由当前持有者完成。
    pub lease_lost: bool,
    /// 服务端执行时限到点而中止：节点仍健康，可按重试预算换节点。
    pub deadline_exceeded: bool,
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
    run_remote_subagent_reply_scoped(endpoint, request, None)
}

/// 带计划作用域的 [`run_remote_subagent_reply`]：幂等键变为 `scope|task@epoch`。
pub fn run_remote_subagent_reply_scoped(
    endpoint: &RemoteEndpoint,
    request: &SubagentRequest,
    scope: Option<&str>,
) -> Result<RemoteReply, AgentError> {
    if let Some(scope) = scope {
        validate_idempotency_scope(scope)?;
    }
    endpoint.validate()?;
    let frame = encode_frame(request)?;

    let agent = agent_for(endpoint.timeout_secs);
    let mut call = agent
        .post(endpoint.url.trim())
        .set("Content-Type", FRAME_CONTENT_TYPE)
        .set("Accept", FRAME_CONTENT_TYPE)
        // 重试必须复用同一个幂等键；服务端据此去重。
        .set(
            IDEMPOTENCY_HEADER,
            &idempotency_key(scope, request.task.as_str(), request.epoch),
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
    let lease_lost_header = response.header(LEASE_LOST_HEADER) == Some("1");
    let deadline_exceeded_header = response.header(DEADLINE_EXCEEDED_HEADER) == Some("1");

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
            let lease_lost =
                lease_lost_header && matches!(&response.status, DelegationStatus::Aborted);
            let deadline_exceeded =
                deadline_exceeded_header && matches!(&response.status, DelegationStatus::Aborted);
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
                lease_lost,
                deadline_exceeded,
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
    run_remote_subagent_reply_cancellable_scoped(endpoint, request, cancel, None)
}

/// 带计划作用域的 [`run_remote_subagent_reply_cancellable`]；取消请求复用同一个
/// `scope|task@epoch` 键。
pub fn run_remote_subagent_reply_cancellable_scoped(
    endpoint: &RemoteEndpoint,
    request: &SubagentRequest,
    cancel: &trit_orch::CancelToken,
    scope: Option<&str>,
) -> Result<RemoteReply, AgentError> {
    if let Some(scope) = scope {
        validate_idempotency_scope(scope)?;
    }
    if cancel.is_cancelled() {
        return Ok(unresolved_reply(request, DelegationStatus::Aborted));
    }
    let key = idempotency_key(scope, request.task.as_str(), request.epoch);
    let endpoint_for_call = endpoint.clone();
    let request_for_call = request.clone();
    let scope_for_call = scope.map(str::to_string);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(run_remote_subagent_reply_scoped(
            &endpoint_for_call,
            &request_for_call,
            scope_for_call.as_deref(),
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

    // 取消是 best-effort：通知失败也要按 Aborted 返回，不能让调用方卡住。
    let _ = cancel_remote_subagent(endpoint, &key);
    match receiver.recv_timeout(CLIENT_CANCEL_GRACE) {
        Ok(result) => result,
        Err(_) => Ok(unresolved_reply(request, DelegationStatus::Aborted)),
    }
}

/// 把端点 URL 折叠到 **origin** 上的固定路径：`http://host:port/run` →
/// `http://host:port<path>`。
fn endpoint_origin_url(endpoint_url: &str, path: &str) -> String {
    let trimmed = endpoint_url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return format!("{trimmed}{path}");
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    format!("{scheme}://{authority}{path}")
}

/// 取消入口固定在端点 **origin** 上：`http://host:port/run` → `http://host:port/cancel`。
fn cancel_endpoint_url(endpoint_url: &str) -> String {
    endpoint_origin_url(endpoint_url, CANCEL_PATH)
}

/// 通知远程端点取消一个在途请求；未知键按服务端语义视为 no-op。
///
/// 返回 `Ok(())` 只代表通知被端点接受，不代表远端已经停止执行。
pub fn cancel_remote_subagent(endpoint: &RemoteEndpoint, key: &str) -> Result<(), String> {
    endpoint.validate().map_err(|error| error.to_string())?;
    let agent = agent_for(endpoint.timeout_secs.clamp(1, CANCEL_REQUEST_TIMEOUT_SECS));
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

/// 探测远程端点是否还能服务：`GET <origin>/health`，带同一 Bearer。
///
/// 只验证可达与鉴权，不派发任务、不触碰幂等账本。成功返回 `Ok(())`；
/// 非 2xx、3xx、传输失败都返回错误。超时限制在 1–5s，避免恢复流程被慢节点拖住。
pub fn probe_remote_health(endpoint: &RemoteEndpoint) -> Result<(), String> {
    endpoint.validate().map_err(|error| error.to_string())?;
    let agent = agent_for(endpoint.timeout_secs.clamp(1, HEALTH_REQUEST_TIMEOUT_SECS));
    let url = endpoint_origin_url(&endpoint.url, HEALTH_PATH);
    let mut call = agent.get(&url);
    if let Some(token) = endpoint.auth_token.as_deref() {
        call = call.set("Authorization", &format!("Bearer {token}"));
    }
    match call.call() {
        Ok(response) if (200..300).contains(&response.status()) => Ok(()),
        Ok(response) => Err(format!("health rejected with HTTP {}", response.status())),
        Err(ureq::Error::Status(code, _)) => Err(format!("health rejected with HTTP {code}")),
        Err(ureq::Error::Transport(error)) => Err(format!("health transport: {error}")),
    }
}

/// 服务端取消入口：`POST <endpoint>/cancel` + `X-Trit-Idempotency: <task>@<epoch>`。
pub const CANCEL_PATH: &str = "/cancel";

/// 服务端健康探测入口：`GET <endpoint>/health`，沿用同一 Bearer 鉴权。
pub const HEALTH_PATH: &str = "/health";

/// 取消轮询步长。
const CLIENT_CANCEL_POLL: Duration = Duration::from_millis(25);

/// 发出取消后等待对端收尾的上限；超时即按未确认完成返回。
const CLIENT_CANCEL_GRACE: Duration = Duration::from_secs(2);

/// 取消请求自身的短超时，避免取消动作把调用方拖住。
const CANCEL_REQUEST_TIMEOUT_SECS: u64 = 5;

/// 健康探测自身的短超时；恢复流程不应被慢节点拖住。
const HEALTH_REQUEST_TIMEOUT_SECS: u64 = 5;

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
        lease_lost: false,
        deadline_exceeded: false,
    }
}
