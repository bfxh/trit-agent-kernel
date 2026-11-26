//! 远程子 agent 的**参考服务端**：HTTP/1.1 单帧 POST。
//!
//! 只实现一条最小、可审计的线：`POST <path>`，请求体是一帧，回一帧，
//! `Connection: close`。不是通用 HTTP 服务器，也不做长连接 / 分块 / 重定向。
//!
//! 不变量：
//!
//! 1. **有界读**：`Content-Length` 缺失、超过 [`MAX_FRAME_BYTES`] + 4、或实际
//!    字节数不足，一律拒绝，绝不无限读；
//! 2. **鉴权先于执行**：配置了 token 时，缺 `Authorization: Bearer <token>`
//!    直接 401，帧不解码；
//! 3. **只回合法帧**：响应体永远是 `trit_orch::encode_frame` 的产物；
//!    失败模式回 `Failed` 帧，而不是让宿主把错误当成完成；
//! 4. **一连接一请求**：读满一帧即回包并关闭，便于宿主侧超时语义保持简单；
//! 5. **带键才去重**：`X-Trit-Idempotency` 必须等于帧里的 `task@epoch`，否则 400；
//!    命中缓存直接回首次响应、不再执行（`sleep_ms` 也不生效），缓存上限
//!    [`MAX_CACHED_RESPONSES`]，按 FIFO 淘汰。没有头就按普通请求处理；
//! 6. **账本优先**：配置 `ledger` 后，带键请求先向共享账本 claim；账本返回完成帧
//!    就直接回放，返回占位则轮询等待，拿到执行权才执行，执行后必须 complete。
//!    账本不可达时回 503，宁可不执行也不冒重复执行的风险。`Silent` 模式不产生
//!    响应帧，无法登记完成，账本占位会留到 TTL 过期；
//! 7. **Exec 模式真实执行**：spawn `subagent-run`（或等价命令），请求帧走 stdin、
//!    响应帧走 stdout；只有子进程判定完成才回 `Completed`，进程错误/超时按
//!    `Failed`/`Aborted` 回帧，不误报完成。配 `exec_workspace_root` 时按任务 ID
//!    建 `<root>/<task>` 并展开 args 里的 `{workspace}` / `{task}`；任务 ID 含
//!    路径分隔符或为 `.`/`..` 时直接回 `Failed`，不落盘；
//! 8. **取消传播**：`POST /cancel`（origin 上的固定路径）+ `X-Trit-Idempotency`
//!    按幂等键置位在途请求；等待账本、`sleep_ms` 与执行前都会检查，Exec 用可
//!    取消进程跑法直接 kill 子进程。取消回合法 `Aborted` 帧，并照常登记
//!    账本/本地缓存，避免后续重试再次执行同一键；
//! 9. **连接有界**：`max_connections` 默认 64；超额连接直接 503，不排队、不 spawn，
//!    防止慢客户端把服务端线程与内存堆爆。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use trit_orch::{
    CancelToken, DelegationStatus, MAX_FRAME_BYTES, RemoteEndpoint, SubagentRequest,
    SubagentResponse, decode_frame, encode_frame,
};

use crate::remote_ledger::{self, Claim};
use crate::remote_subagent::CANCEL_PATH;
use crate::subagent_process::{
    SubagentProcessConfig, run_subagent_reply, run_subagent_reply_cancellable,
};

/// 等待容量/执行期间检查取消的步长。
const CANCEL_CHECK_STEP: Duration = Duration::from_millis(25);

/// 预取消但尚未到达的键上限，防止 `/cancel` 被用来无限占用内存。
const MAX_PRE_CANCELLED: usize = 1024;

/// 同时在处理的连接上限（参考实现；可通过 `--max-connections` 调整）。
const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// 连接上的读写超时：防止半开的客户端长期占住服务线程。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 幂等缓存上限：参考实现按 FIFO 淘汰，不做 LRU。
const MAX_CACHED_RESPONSES: usize = 1024;

/// 幂等缓存：`"<task>@<epoch>" -> 首次响应`。
type ResponseCache = Arc<Mutex<BTreeMap<String, SubagentResponse>>>;

/// 在途请求的取消登记：按幂等键。
#[derive(Default)]
struct ActiveCancels {
    inner: Mutex<CancelRegistry>,
}

#[derive(Default)]
struct CancelRegistry {
    active: BTreeMap<String, CancelToken>,
    /// 取消先于请求到达的键；请求登记时立即继承取消状态。
    pre: BTreeSet<String>,
}

impl ActiveCancels {
    /// 登记一个请求并返回自动注销的守卫。
    fn scope(&self, key: &str) -> CancelScope<'_> {
        let token = CancelToken::new();
        if let Ok(mut registry) = self.inner.lock() {
            if registry.pre.remove(key) {
                token.cancel();
            }
            registry.active.insert(key.to_string(), token.clone());
        }
        CancelScope {
            registry: self,
            key: key.to_string(),
            token,
        }
    }

    /// 取消一个键；在途请求立即置位，尚未到达的键先暂存。
    fn cancel(&self, key: &str) {
        if let Ok(mut registry) = self.inner.lock() {
            if let Some(token) = registry.active.get(key) {
                token.cancel();
                return;
            }
            if registry.pre.len() >= MAX_PRE_CANCELLED
                && let Some(oldest) = registry.pre.iter().next().cloned()
            {
                registry.pre.remove(&oldest);
            }
            registry.pre.insert(key.to_string());
        }
    }
}

struct CancelScope<'a> {
    registry: &'a ActiveCancels,
    key: String,
    token: CancelToken,
}

impl CancelScope<'_> {
    fn token(&self) -> &CancelToken {
        &self.token
    }
}

impl Drop for CancelScope<'_> {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.registry.inner.lock() {
            registry.active.remove(&self.key);
        }
    }
}

/// 连接并发上限：超额连接回 503，不再创建线程。
struct ConnectionSlots {
    limit: usize,
    active: Mutex<usize>,
}

struct ConnectionSlot {
    slots: Arc<ConnectionSlots>,
}

impl ConnectionSlots {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(1),
            active: Mutex::new(0),
        })
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionSlot> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *active >= self.limit {
            return None;
        }
        *active += 1;
        Some(ConnectionSlot {
            slots: Arc::clone(self),
        })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut active = self
            .slots
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
    }
}

/// 服务端应答模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMode {
    /// 回 `Completed`（默认）。
    Echo,
    /// 回 `Failed`：让宿主走“有响应但没完成”的分支。
    Fail,
    /// 回 200 空体：让宿主走“没有合法响应”的分支。
    Silent,
    /// 真实执行：按配置 spawn `subagent-run`（或等价命令）并回它的响应帧。
    Exec,
}

/// 参考服务端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteServerConfig {
    /// 监听地址；`127.0.0.1:0` 表示由系统分配端口。
    pub bind: String,
    /// 可选 bearer token；配置后所有请求都要带同一个值。
    pub token: Option<String>,
    /// 应答模式。
    pub mode: RemoteMode,
    /// 回包前的人为延迟（毫秒），用于验证宿主超时。
    pub sleep_ms: u64,
    /// 可选的共享幂等账本；配置后带键请求以账本为准，本地缓存不再参与。
    pub ledger: Option<RemoteEndpoint>,
    /// `Exec` 模式下的子 agent 进程配置；其他模式必须为空。
    pub exec: Option<SubagentProcessConfig>,
    /// `Exec` 模式下按任务隔离的 workspace 根：请求的任务 ID 作为子目录，
    /// args 里的 `{workspace}` 会在执行前替换成该目录。
    pub exec_workspace_root: Option<PathBuf>,
    /// 同时在处理的连接上限；超限直接回 503，不再 spawn 线程。
    pub max_connections: usize,
}

impl RemoteServerConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            token: None,
            mode: RemoteMode::Echo,
            sleep_ms: 0,
            ledger: None,
            exec: None,
            exec_workspace_root: None,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_mode(mut self, mode: RemoteMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_sleep_ms(mut self, sleep_ms: u64) -> Self {
        self.sleep_ms = sleep_ms;
        self
    }

    pub fn with_ledger(mut self, ledger: RemoteEndpoint) -> Self {
        self.ledger = Some(ledger);
        self
    }

    pub fn with_exec(mut self, exec: SubagentProcessConfig) -> Self {
        self.mode = RemoteMode::Exec;
        self.exec = Some(exec);
        self
    }

    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections.max(1);
        self
    }

    /// 按任务隔离 workspace：`<root>/<sanitized task id>`。
    pub fn with_exec_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.exec_workspace_root = Some(root.into());
        self
    }
}

/// 已绑定的参考服务端。
pub struct RemoteServer {
    listener: TcpListener,
    config: RemoteServerConfig,
    cache: ResponseCache,
    cancels: Arc<ActiveCancels>,
    slots: Arc<ConnectionSlots>,
}

impl RemoteServer {
    /// 绑定监听地址；失败即返回，不重试。
    pub fn bind(config: RemoteServerConfig) -> std::io::Result<Self> {
        if let Some(ledger) = config.ledger.as_ref() {
            ledger.validate().map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
            })?;
        }
        match (config.mode, config.exec.is_some()) {
            (RemoteMode::Exec, false) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "exec mode requires exec process config",
                ));
            }
            (mode, true) if mode != RemoteMode::Exec => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "exec process config is only valid in exec mode",
                ));
            }
            _ => {}
        }
        if config.exec_workspace_root.is_some() && config.mode != RemoteMode::Exec {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "exec workspace root is only valid in exec mode",
            ));
        }
        if let Some(exec) = config.exec.as_ref()
            && exec.args.iter().any(|arg| arg.contains("{workspace}"))
            && config.exec_workspace_root.is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "exec args use {workspace} but exec workspace root is not configured",
            ));
        }
        let listener = TcpListener::bind(config.bind.as_str())?;
        let slots = ConnectionSlots::new(config.max_connections);
        Ok(Self {
            listener,
            config,
            cache: Arc::default(),
            cancels: Arc::default(),
            slots,
        })
    }

    /// 实际监听地址（`--bind 127.0.0.1:0` 时用它对外公布）。
    pub fn addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 服务一个连接后返回；测试与单次演示用。
    pub fn serve_once(&self) -> std::io::Result<()> {
        let (mut stream, _) = self.listener.accept()?;
        let Some(_slot) = self.slots.try_acquire() else {
            return respond(&mut stream, 503, "Service Unavailable", b"");
        };
        handle(stream, &self.config, &self.cache, &self.cancels)
    }

    /// 持续服务，直到进程被终止；单连接错误只丢弃该连接。
    ///
    /// 每个连接一个线程（参考实现不做线程池）：跨机 fleet 会从多个 worker
    /// 同时打到一个端点，串行处理会让 `max_in_flight > 1` 的容量形同虚设。
    pub fn serve_forever(&self) -> std::io::Result<()> {
        loop {
            let (mut stream, _) = self.listener.accept()?;
            let Some(slot) = self.slots.try_acquire() else {
                // 超额连接不排队、不 spawn：直接 503，保护服务端线程与内存。
                let _ = respond(&mut stream, 503, "Service Unavailable", b"");
                continue;
            };
            let config = self.config.clone();
            let cache = Arc::clone(&self.cache);
            let cancels = Arc::clone(&self.cancels);
            std::thread::spawn(move || {
                let _slot = slot;
                let _ = handle(stream, &config, &cache, &cancels);
            });
        }
    }
}

/// 处理一个连接：解析请求头 → 鉴权 → 读一帧 → 幂等查缓存/执行 → 回一帧 → 关闭。
fn handle(
    mut stream: TcpStream,
    config: &RemoteServerConfig,
    cache: &ResponseCache,
    cancels: &ActiveCancels,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if method.is_empty() || path.is_empty() {
        return respond(&mut stream, 400, "Bad Request", b"");
    }
    if method != "POST" {
        return respond(&mut stream, 405, "Method Not Allowed", b"");
    }

    let mut content_length: Option<usize> = None;
    let mut authorization: Option<String> = None;
    let mut idempotency: Option<String> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return respond(&mut stream, 400, "Bad Request", b"");
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => match value.parse::<usize>() {
                Ok(length) => content_length = Some(length),
                Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
            },
            "authorization" => authorization = Some(value.to_string()),
            "x-trit-idempotency" => idempotency = Some(value.to_string()),
            _ => {}
        }
    }

    let Some(length) = content_length else {
        return respond(&mut stream, 411, "Length Required", b"");
    };
    if length > MAX_FRAME_BYTES + 4 {
        return respond(&mut stream, 413, "Payload Too Large", b"");
    }

    if let Some(token) = config.token.as_deref() {
        // 参考实现：按字节相等比较；旁路/时序防护不在本层。
        if authorization.as_deref() != Some(&format!("Bearer {token}")) {
            return respond(&mut stream, 401, "Unauthorized", b"");
        }
    }

    if path == CANCEL_PATH {
        if length != 0 {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        let Some(key) = idempotency.as_deref() else {
            return respond(&mut stream, 400, "Bad Request", b"");
        };
        // 未知键与重复取消都是 no-op；鉴权已在上面完成。
        cancels.cancel(key);
        return respond(&mut stream, 204, "No Content", b"");
    }

    let mut body = vec![0u8; length];
    if let Err(error) = reader.read_exact(&mut body) {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        return Err(error);
    }

    let request: SubagentRequest = match decode_frame(&mut body) {
        Ok(Some(request)) => request,
        Ok(None) | Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
    };

    // 幂等键必须与帧里的 task@epoch 一致；只有带键的请求才去重。
    let key = format!("{}@{}", request.task, request.epoch);
    let dedupe = match idempotency.as_deref() {
        None => false,
        Some(value) if value == key => true,
        Some(_) => return respond(&mut stream, 400, "Bad Request", b""),
    };
    let _cancel_scope = dedupe.then(|| cancels.scope(&key));
    let token = _cancel_scope.as_ref().map(|scope| scope.token());
    let mut ledger_owned = false;
    if dedupe {
        if let Some(endpoint) = config.ledger.as_ref() {
            // 账本是跨机权威：先 claim，再执行；等待期内轮询完成响应与取消。
            let deadline = std::time::Instant::now() + Duration::from_secs(endpoint.timeout_secs);
            loop {
                if token.is_some_and(CancelToken::is_cancelled) {
                    return respond_aborted(&mut stream, &request);
                }
                match remote_ledger::claim(endpoint, &key) {
                    Ok(Claim::New) => {
                        ledger_owned = true;
                        break;
                    }
                    Ok(Claim::Done(frame)) => return respond(&mut stream, 200, "OK", &frame),
                    Ok(Claim::Pending) => {
                        if std::time::Instant::now() >= deadline {
                            return respond(&mut stream, 409, "Conflict", b"");
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => return respond(&mut stream, 503, "Service Unavailable", b""),
                }
            }
        } else if let Some(cached) = cache.lock().ok().and_then(|guard| guard.get(&key).cloned()) {
            let frame = match encode_frame(&cached) {
                Ok(frame) => frame,
                Err(_) => return respond(&mut stream, 500, "Internal Server Error", b""),
            };
            return respond(&mut stream, 200, "OK", &frame);
        }
    }

    let cancelled = if config.sleep_ms > 0 {
        sleep_unless_cancelled(config.sleep_ms, token)
    } else {
        token.is_some_and(CancelToken::is_cancelled)
    };

    let (status, content, tokens) = if cancelled {
        (DelegationStatus::Aborted, Vec::new(), 0)
    } else {
        match config.mode {
            RemoteMode::Echo => (
                DelegationStatus::Completed,
                format!("remote echo {}@{}", request.task, request.epoch).into_bytes(),
                0,
            ),
            RemoteMode::Fail => (
                DelegationStatus::Failed {
                    message: "remote echo configured to fail".into(),
                },
                format!("remote echo {}@{}", request.task, request.epoch).into_bytes(),
                0,
            ),
            RemoteMode::Silent => return respond(&mut stream, 200, "OK", b""),
            RemoteMode::Exec => {
                let Some(exec) = config.exec.as_ref() else {
                    return respond(&mut stream, 500, "Internal Server Error", b"");
                };
                match exec_for_request(exec, config.exec_workspace_root.as_deref(), &request.task) {
                    Ok(exec) => {
                        let reply = match token {
                            Some(token) => run_subagent_reply_cancellable(&exec, &request, token),
                            None => run_subagent_reply(&exec, &request),
                        };
                        match reply {
                            Ok(reply) => (reply.result.status, reply.content, reply.result.tokens),
                            Err(error) => (
                                DelegationStatus::Failed {
                                    message: error.to_string(),
                                },
                                Vec::new(),
                                0,
                            ),
                        }
                    }
                    Err(message) => (DelegationStatus::Failed { message }, Vec::new(), 0),
                }
            }
        }
    };
    let response = SubagentResponse {
        task: request.task.clone(),
        epoch: request.epoch,
        status,
        content,
        tokens,
    };
    let frame = match encode_frame(&response) {
        Ok(frame) => frame,
        Err(_) => return respond(&mut stream, 500, "Internal Server Error", b""),
    };
    if ledger_owned && let Some(endpoint) = config.ledger.as_ref() {
        // 执行后必须登记；登记失败时回 5xx，宁可让调用方失败也不谎报完成。
        if remote_ledger::complete(endpoint, &key, &frame).is_err() {
            return respond(&mut stream, 503, "Service Unavailable", b"");
        }
    }
    if dedupe
        && !ledger_owned
        && let Ok(mut guard) = cache.lock()
    {
        if guard.len() >= MAX_CACHED_RESPONSES
            && let Some(oldest) = guard.keys().next().cloned()
        {
            guard.remove(&oldest);
        }
        guard.insert(key, response);
    }
    respond(&mut stream, 200, "OK", &frame)
}

/// 把 `{task}` / `{workspace}` 占位符按请求展开；配了 workspace 根时先建目录。
fn exec_for_request(
    config: &SubagentProcessConfig,
    workspace_root: Option<&Path>,
    task: &str,
) -> Result<SubagentProcessConfig, String> {
    let uses_task = config.args.iter().any(|arg| arg.contains("{task}"));
    let uses_workspace = config.args.iter().any(|arg| arg.contains("{workspace}"));
    if !uses_task && !uses_workspace && workspace_root.is_none() {
        return Ok(config.clone());
    }
    let component = safe_task_component(task)?;
    let workspace = workspace_root.map(|root| root.join(&component));
    if uses_workspace && workspace.is_none() {
        return Err("exec args use {workspace} but no workspace root is configured".into());
    }
    if let Some(workspace) = workspace.as_deref() {
        std::fs::create_dir_all(workspace).map_err(|error| {
            format!("create per-task workspace {}: {error}", workspace.display())
        })?;
    }
    let mut materialized = config.clone();
    materialized.args = config
        .args
        .iter()
        .map(|arg| {
            let mut arg = arg.replace("{task}", &component);
            if let Some(workspace) = workspace.as_deref() {
                arg = arg.replace("{workspace}", &workspace.to_string_lossy());
            }
            arg
        })
        .collect();
    Ok(materialized)
}

/// 只允许单段、无路径分隔符的任务 ID 作为目录名，拒绝 `.` / `..`。
fn safe_task_component(task: &str) -> Result<String, String> {
    if task.is_empty() || task == "." || task == ".." {
        return Err(format!(
            "task id {task:?} is not a safe workspace directory"
        ));
    }
    if !task
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!(
            "task id {task:?} contains characters unsafe for a workspace directory"
        ));
    }
    Ok(task.to_string())
}

/// 分片睡眠，取消置位时立即返回 `true`。
fn sleep_unless_cancelled(ms: u64, token: Option<&CancelToken>) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if token.is_some_and(CancelToken::is_cancelled) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(CANCEL_CHECK_STEP));
    }
}

/// 未开始执行就被取消：回一个合法 `Aborted` 帧，而不是空体。
fn respond_aborted(stream: &mut TcpStream, request: &SubagentRequest) -> std::io::Result<()> {
    let response = SubagentResponse {
        task: request.task.clone(),
        epoch: request.epoch,
        status: DelegationStatus::Aborted,
        content: Vec::new(),
        tokens: 0,
    };
    let frame = encode_frame(&response).map_err(std::io::Error::other)?;
    respond(stream, 200, "OK", &frame)
}

/// 写一个带长度与 `Connection: close` 的响应。
fn respond(stream: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        crate::remote_subagent::FRAME_CONTENT_TYPE,
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}
