//! 共享幂等账本：让多台远端在同一个 `task@epoch` 上最多登记一个完成结果，重试优先回放。
//!
//! 协议（HTTP/1.1，一连接一请求，路径固定 `/claim`、`/renew` 与 `/complete`）：
//!
//! - `POST /claim`，头 `X-Trit-Idempotency: <task>@<epoch>` 或
//!   `<scope>|<task>@<epoch>`：
//!   - `200` + 空体 + `X-Trit-Lease`：调用方获得执行权（[`Claim::New`]），租约令牌
//!     必须用于后续 `/renew` 与 `/complete`；
//!   - `200` + 一帧：已有完成响应（[`Claim::Done`]），直接复用；
//!   - `409`：另一执行者持有该键（[`Claim::Pending`]）；
//!   - `503`：账本已满或内部错误。
//! - `POST /complete`，同一个头 + `X-Trit-Lease` + 一帧响应：登记完成；`204`
//!   接受（同一持有者重复登记为幂等 no-op，不覆盖原帧；不同持有者 `403`，
//!   避免旧执行者拿到“自己的结果获胜”的假象）。租约已失效或键未由该令牌
//!   claim 时也返回 `403`。
//! - `POST /renew`，同一个头 + `X-Trit-Lease`、空体：把该键的占位租约重置为
//!   当前时间，`204`；已完成返回 `204` + `X-Trit-Lease-Done: 1`（当前令牌已
//!   不再拥有该任务，调用方应立即停止在途执行），未知键 `404`，令牌不匹配 `403`。
//!   用于执行时间超过 `pending_ttl` 的任务，避免另一个节点误判过期而并发执行。
//!
//! 占位有 TTL（默认 60s）：执行者崩溃后，过期占位可以被接管。账本只保存响应帧，
//! 不执行任务；它是跨机重试的协调点，不是任务队列。租约令牌是随机生成的持有者
//! 凭证，不是加密身份：传输仍应放在 loopback 或可信内网，并配置 bearer token。
//!
//! 容量有键数与字节双重上限（默认 4096 / 64 MiB）：键数包含占位与完成项，字节按
//! 完成帧与持有者令牌估算；完成记录也保存写入者令牌（旧格式没有 `lease`，仍按
//! 幂等成功兼容），占位令牌与绝对到期时间同样写入 JSONL，另受键数与单行长度约束。
//! 物理文件到逻辑上限的 1.5 倍时压缩为当前内存快照（完成项 + 未过期占位，
//! 临时文件 + 原子替换）；启动时若目标缺失会从 `.compact` 临时文件恢复。
//!
//! 连接并发默认 64（`with_max_connections` / `--max-connections`），超额连接直接
//! `503`、不排队也不创建线程；账本是跨机协调的单点，不能让慢客户端耗尽线程。
//! 请求读取同样有界：请求行/头单行 8 KiB、总计 64 KiB、最多 64 个，超限 `431`；
//! 请求行 + 头部 + 请求体共享总读取时限（默认 60s，`with_request_read_timeout` /
//! `--request-read-timeout-ms`，0 关闭），慢速滴流到点回 `408`。
//!
//! 安全边界：账本默认只应绑定 loopback 或置于内网；配置 token 后所有请求都要
//! `Authorization: Bearer <token>`。状态文件同时包含完成帧与租约令牌，应按敏感
//! 数据保护。账本不校验任务内容，只校验 `complete` 的帧与幂等键一致，并要求
//! `/renew`、`/complete` 携带当前持有者的租约令牌。

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use trit_orch::{MAX_FRAME_BYTES, RemoteEndpoint, SubagentResponse, decode_frame, encode_frame};

use crate::connection_limit::{ConnectionSlots, DEFAULT_MAX_CONNECTIONS};
use crate::http_read::{
    DEFAULT_REQUEST_READ_TIMEOUT, DeadlineStream, HeaderReadError, IO_TIMEOUT, MAX_HEADER_BYTES,
    MAX_HEADER_LINE_BYTES, MAX_HEADERS, is_timeout, read_http_line,
};
use crate::remote_subagent::idempotency_scope;

/// 默认占位 TTL：执行者崩溃后，多久允许其他节点接管。
const DEFAULT_PENDING_TTL: Duration = Duration::from_secs(60);

/// 默认账本容量上限。
const DEFAULT_MAX_ENTRIES: usize = 4096;

/// 默认状态字节上限（按已完成帧的十六进制 JSONL 记录估算）。
const DEFAULT_MAX_STATE_BYTES: usize = 64 * 1024 * 1024;

/// 单个响应体的上限（与远程 runner 一致）。
const MAX_LEDGER_BODY: usize = MAX_FRAME_BYTES + 4;

/// 状态文件单行上限：帧的十六进制编码再留 key 与 JSON 转义空间。
const MAX_STATE_LINE_BYTES: usize = MAX_LEDGER_BODY * 2 + 16 * 1024;

/// 租约持有者令牌头：`/claim` 成功时返回，`/renew` 与 `/complete` 必须携带。
const LEASE_HEADER: &str = "X-Trit-Lease";

/// `/renew` 命中已完成键时的标记头：持有者应停止在途执行，当前结果已由别人提交。
const LEASE_DONE_HEADER: &str = "X-Trit-Lease-Done";

/// 租约令牌长度上限；正常令牌是 16 位十六进制。
const MAX_LEASE_LEN: usize = 128;

static LEASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 账本服务端配置。
#[derive(Debug, Clone)]
pub struct LedgerServerConfig {
    pub bind: String,
    /// 可选 bearer token；配置后 claim/complete 都必须带。
    pub token: Option<String>,
    /// 占位过期时间：超过后允许接管。
    pub pending_ttl: Duration,
    /// 最多保存多少个键（含占位与完成）。
    pub max_entries: usize,
    /// 已完成记录的逻辑字节上限；超出时按 key 顺序淘汰并压缩日志。
    pub max_state_bytes: usize,
    /// 可选持久化文件：追加完成帧与占位租约（含令牌/到期时间），重启时回放；
    /// 过期占位会被丢弃。
    pub state_path: Option<PathBuf>,
    /// 同时在处理的连接上限；超限直接回 503，避免单点账本被慢连接拖垮。
    pub max_connections: usize,
    /// 请求行 + 头部 + 请求体的总读取时限；`None` 不限制。
    /// 到点回 `408`，避免慢速滴流客户端长期占住连接。
    pub request_read_timeout: Option<Duration>,
}

impl LedgerServerConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            token: None,
            pending_ttl: DEFAULT_PENDING_TTL,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_state_bytes: DEFAULT_MAX_STATE_BYTES,
            state_path: None,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            request_read_timeout: Some(DEFAULT_REQUEST_READ_TIMEOUT),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_pending_ttl(mut self, pending_ttl: Duration) -> Self {
        self.pending_ttl = pending_ttl;
        self
    }

    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self
    }

    pub fn with_max_state_bytes(mut self, max_state_bytes: usize) -> Self {
        self.max_state_bytes = max_state_bytes.max(1);
        self
    }

    pub fn with_state_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_path = Some(path.into());
        self
    }

    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections.max(1);
        self
    }

    /// 设置请求行 + 头部 + 请求体的总读取时限；`Duration::ZERO` 表示不限制。
    pub fn with_request_read_timeout(mut self, timeout: Duration) -> Self {
        self.request_read_timeout = (!timeout.is_zero()).then_some(timeout);
        self
    }
}

/// 一次 claim 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// 获得执行权；`lease` 必须用于后续 [`renew`] 与 [`complete`]。
    New { lease: String },
    /// 另一个执行者正在执行；稍后重试 claim。
    Pending,
    /// 已经有完成响应，直接回放这一帧。
    Done(Vec<u8>),
}

enum Entry {
    Pending {
        expires_at: Instant,
        lease: String,
    },
    /// `lease` 是写入该完成帧的持有者；旧版状态没有这一列，回放后为 `None`。
    Done {
        frame: Vec<u8>,
        lease: Option<String>,
    },
}

/// 生成新的随机持有者令牌；时间 + 随机哈希种子 + 进程内序号，避免不同 claim 重复。
fn fresh_lease() -> String {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.write_u64(LEASE_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", hasher.finish())
}

/// 令牌必须是有限长的安全 ASCII；拒绝空值、控制字符、冒号与超长头。
fn valid_lease(lease: &str) -> bool {
    !lease.is_empty()
        && lease.len() <= MAX_LEASE_LEN
        && lease
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 把内存里的单调时钟截止点换算成可落盘的绝对毫秒时间。
fn expiry_unix_ms(expires_at: Instant) -> u64 {
    let remaining = expires_at.saturating_duration_since(Instant::now());
    unix_millis_now().saturating_add(remaining.as_millis() as u64)
}

#[derive(Default)]
struct LedgerState {
    entries: BTreeMap<String, Entry>,
    /// 所有 `Done` 记录的逻辑字节数（按保守估算，见 `record_bytes`）。
    bytes: usize,
}

/// 已绑定的账本服务端。
pub struct LedgerServer {
    listener: TcpListener,
    config: LedgerServerConfig,
    state: Arc<Mutex<LedgerState>>,
    completions: Arc<AtomicU64>,
    slots: Arc<ConnectionSlots>,
}

impl LedgerServer {
    pub fn bind(config: LedgerServerConfig) -> std::io::Result<Self> {
        if config
            .token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger token is blank",
            ));
        }
        if config.max_entries == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger max_entries must be positive",
            ));
        }
        if config.max_state_bytes == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger max_state_bytes must be positive",
            ));
        }
        if config.pending_ttl.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger pending_ttl must be positive",
            ));
        }
        let listener = TcpListener::bind(config.bind.as_str())?;
        let state = match config.state_path.as_deref() {
            Some(path) => {
                recover_compaction(path)?;
                let state = load_entries(
                    path,
                    config.pending_ttl,
                    config.max_entries,
                    config.max_state_bytes,
                )?;
                if std::fs::metadata(path)
                    .is_ok_and(|meta| meta.len() > config.max_state_bytes as u64)
                {
                    compact_state(path, &state.entries)?;
                }
                Arc::new(Mutex::new(state))
            }
            None => Arc::default(),
        };
        let slots = ConnectionSlots::new(config.max_connections);
        Ok(Self {
            listener,
            config,
            state,
            completions: Arc::default(),
            slots,
        })
    }

    pub fn addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 已观察到的 `complete` 调用次数；诊断用，用于验证“只执行一次”。
    pub fn completions(&self) -> u64 {
        self.completions.load(Ordering::SeqCst)
    }

    pub fn serve_once(&self) -> std::io::Result<()> {
        let (mut stream, _) = self.listener.accept()?;
        let Some(_slot) = self.slots.try_acquire() else {
            return respond(&mut stream, 503, "Service Unavailable", b"");
        };
        handle(stream, &self.config, &self.state, &self.completions)
    }

    /// 每个连接一个线程，但受 `max_connections` 限制；超额连接直接 503。
    pub fn serve_forever(&self) -> std::io::Result<()> {
        loop {
            let (mut stream, _) = self.listener.accept()?;
            let Some(slot) = self.slots.try_acquire() else {
                let _ = respond(&mut stream, 503, "Service Unavailable", b"");
                continue;
            };
            let config = self.config.clone();
            let state = Arc::clone(&self.state);
            let completions = Arc::clone(&self.completions);
            std::thread::spawn(move || {
                let _slot = slot;
                let _ = handle(stream, &config, &state, &completions);
            });
        }
    }
}

fn handle(
    mut stream: TcpStream,
    config: &LedgerServerConfig,
    state: &Arc<Mutex<LedgerState>>,
    completions: &Arc<AtomicU64>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(DeadlineStream::new(
        stream.try_clone()?,
        config.request_read_timeout,
    )?);

    let request_line = match read_http_line(&mut reader, MAX_HEADER_LINE_BYTES) {
        Ok(Some(line)) => line,
        Ok(None) | Err(HeaderReadError::Invalid) => {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        Err(HeaderReadError::TooLarge) => {
            return respond(&mut stream, 431, "Request Header Fields Too Large", b"");
        }
        Err(HeaderReadError::Io(error)) if is_timeout(&error) => {
            return respond(&mut stream, 408, "Request Timeout", b"");
        }
        Err(HeaderReadError::Io(error)) => return Err(error),
    };
    let mut header_bytes = request_line.len();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    if method != "POST" {
        return respond(&mut stream, 405, "Method Not Allowed", b"");
    }

    let mut content_length = 0usize;
    let mut authorization: Option<String> = None;
    let mut idempotency: Option<String> = None;
    let mut lease_header: Option<String> = None;
    let mut header_count = 0;
    loop {
        let line = match read_http_line(&mut reader, MAX_HEADER_LINE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) | Err(HeaderReadError::Invalid) => {
                return respond(&mut stream, 400, "Bad Request", b"");
            }
            Err(HeaderReadError::TooLarge) => {
                return respond(&mut stream, 431, "Request Header Fields Too Large", b"");
            }
            Err(HeaderReadError::Io(error)) if is_timeout(&error) => {
                return respond(&mut stream, 408, "Request Timeout", b"");
            }
            Err(HeaderReadError::Io(error)) => return Err(error),
        };
        header_count += 1;
        header_bytes = header_bytes.saturating_add(line.len());
        if header_count > MAX_HEADERS || header_bytes > MAX_HEADER_BYTES {
            return respond(&mut stream, 431, "Request Header Fields Too Large", b"");
        }
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
                Ok(length) => content_length = length,
                Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
            },
            "authorization" => authorization = Some(value.to_string()),
            "x-trit-idempotency" => idempotency = Some(value.to_string()),
            "x-trit-lease" => lease_header = Some(value.to_string()),
            _ => {}
        }
    }
    if content_length > MAX_LEDGER_BODY {
        return respond(&mut stream, 413, "Payload Too Large", b"");
    }
    if let Some(token) = config.token.as_deref()
        && authorization.as_deref() != Some(&format!("Bearer {token}"))
    {
        return respond(&mut stream, 401, "Unauthorized", b"");
    }

    let mut body = vec![0u8; content_length];
    if let Err(error) = reader.read_exact(&mut body) {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        if is_timeout(&error) {
            return respond(&mut stream, 408, "Request Timeout", b"");
        }
        return Err(error);
    }

    let Some(key) = idempotency else {
        return respond(&mut stream, 400, "Bad Request", b"");
    };

    match path {
        "/renew" => {
            if !body.is_empty() {
                return respond(&mut stream, 400, "Bad Request", b"");
            }
            let Some(lease) = lease_header.as_deref().filter(|lease| valid_lease(lease)) else {
                return respond(&mut stream, 400, "Bad Request", b"");
            };
            let server = LedgerServerView { state, config };
            match server.renew(&key, lease) {
                Ok(RenewOutcome::Renewed) => respond(&mut stream, 204, "No Content", b""),
                Ok(RenewOutcome::AlreadyDone) => respond_lease_done(&mut stream, 204, "No Content"),
                Ok(RenewOutcome::NotOwner) => respond(&mut stream, 403, "Forbidden", b""),
                Ok(RenewOutcome::Unknown) => respond(&mut stream, 404, "Not Found", b""),
                Err(()) => respond(&mut stream, 503, "Service Unavailable", b""),
            }
        }
        "/claim" => {
            let server = LedgerServerView { state, config };
            match server.claim(&key) {
                Ok(Claim::New { lease }) => respond_with_lease(&mut stream, 200, "OK", b"", &lease),
                Ok(Claim::Pending) => respond(&mut stream, 409, "Conflict", b""),
                Ok(Claim::Done(frame)) => respond(&mut stream, 200, "OK", &frame),
                Err(()) => respond(&mut stream, 503, "Service Unavailable", b""),
            }
        }
        "/complete" => {
            let Some(lease) = lease_header.as_deref().filter(|lease| valid_lease(lease)) else {
                return respond(&mut stream, 400, "Bad Request", b"");
            };
            let mut buffer = body;
            let response: SubagentResponse = match decode_frame(&mut buffer) {
                Ok(Some(response)) => response,
                _ => return respond(&mut stream, 400, "Bad Request", b""),
            };
            if idempotency_scope(&key, &response.task, response.epoch).is_err() {
                return respond(&mut stream, 400, "Bad Request", b"");
            }
            let frame = match encode_frame(&response) {
                Ok(frame) => frame,
                Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
            };
            let view = LedgerServerView { state, config };
            match view.complete(&key, lease, frame) {
                Ok(CompleteOutcome::Completed) => {
                    completions.fetch_add(1, Ordering::SeqCst);
                    respond(&mut stream, 204, "No Content", b"")
                }
                Ok(CompleteOutcome::Duplicate) => respond(&mut stream, 204, "No Content", b""),
                Ok(CompleteOutcome::NotOwner) => respond(&mut stream, 403, "Forbidden", b""),
                Err(()) => respond(&mut stream, 503, "Service Unavailable", b""),
            }
        }
        _ => respond(&mut stream, 404, "Not Found", b""),
    }
}

/// 只借 `state/config` 做判定的轻量视图，避免把 `LedgerServer` 整体借进 `handle`。
struct LedgerServerView<'a> {
    state: &'a Arc<Mutex<LedgerState>>,
    config: &'a LedgerServerConfig,
}

enum RenewOutcome {
    Renewed,
    AlreadyDone,
    NotOwner,
    Unknown,
}

enum CompleteOutcome {
    /// 首次写入完成帧。
    Completed,
    /// 同一持有者重复登记：幂等接受，但不重复计数。
    Duplicate,
    NotOwner,
}

impl LedgerServerView<'_> {
    /// 延长占位租约；已完成视为成功，未知键返回 404，非持有者返回 403。
    fn renew(&self, key: &str, lease: &str) -> Result<RenewOutcome, ()> {
        let now = Instant::now();
        let mut state = self.state.lock().map_err(|_| ())?;
        let outcome = match state.entries.get_mut(key) {
            Some(Entry::Pending {
                expires_at,
                lease: owner,
            }) => {
                if owner != lease {
                    return Ok(RenewOutcome::NotOwner);
                }
                let new_expiry = now + self.config.pending_ttl;
                if let Some(path) = self.config.state_path.as_deref()
                    && append_pending_record(path, key, lease, expiry_unix_ms(new_expiry)).is_err()
                {
                    // 续租不落盘就不生效，避免重启后退回旧期限。
                    return Err(());
                }
                *expires_at = new_expiry;
                Ok(RenewOutcome::Renewed)
            }
            Some(Entry::Done { .. }) => Ok(RenewOutcome::AlreadyDone),
            None => Ok(RenewOutcome::Unknown),
        };
        if matches!(&outcome, Ok(RenewOutcome::Renewed))
            && let Some(path) = self.config.state_path.as_deref()
        {
            compact_if_bloated(path, &state.entries, self.config.max_state_bytes);
        }
        outcome
    }

    fn claim(&self, key: &str) -> Result<Claim, ()> {
        let now = Instant::now();
        let mut state = self.state.lock().map_err(|_| ())?;
        if let Some(entry) = state.entries.get_mut(key) {
            match entry {
                Entry::Done { frame, .. } => return Ok(Claim::Done(frame.clone())),
                Entry::Pending { expires_at, .. } if now < *expires_at => {
                    return Ok(Claim::Pending);
                }
                Entry::Pending { expires_at, lease } => {
                    let new_lease = fresh_lease();
                    let new_expiry = now + self.config.pending_ttl;
                    if let Some(path) = self.config.state_path.as_deref()
                        && append_pending_record(path, key, &new_lease, expiry_unix_ms(new_expiry))
                            .is_err()
                    {
                        return Err(());
                    }
                    *expires_at = new_expiry;
                    *lease = new_lease.clone();
                    return Ok(Claim::New { lease: new_lease });
                }
            }
        }
        let victim = if state.entries.len() >= self.config.max_entries {
            state
                .entries
                .iter()
                .find_map(|(key, entry)| matches!(entry, Entry::Done { .. }).then(|| key.clone()))
        } else {
            None
        };
        if state.entries.len() >= self.config.max_entries && victim.is_none() {
            return Err(());
        }
        let lease = fresh_lease();
        let expires_at = now + self.config.pending_ttl;
        if let Some(path) = self.config.state_path.as_deref()
            && append_pending_record(path, key, &lease, expiry_unix_ms(expires_at)).is_err()
        {
            return Err(());
        }
        if let Some(victim) = victim
            && let Some(Entry::Done { frame, lease }) = state.entries.remove(&victim)
        {
            state.bytes =
                state
                    .bytes
                    .saturating_sub(record_bytes(&victim, &frame, lease.as_deref()));
        }
        state.entries.insert(
            key.to_string(),
            Entry::Pending {
                expires_at,
                lease: lease.clone(),
            },
        );
        if let Some(path) = self.config.state_path.as_deref() {
            compact_if_bloated(path, &state.entries, self.config.max_state_bytes);
        }
        Ok(Claim::New { lease })
    }

    fn complete(&self, key: &str, lease: &str, frame: Vec<u8>) -> Result<CompleteOutcome, ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        // 已完成过就不再重复落盘；同一持有者重复 complete 视为幂等成功。
        match state.entries.get(key) {
            Some(Entry::Done { lease: owner, .. }) => {
                // 旧版 Done 没有持有者信息，只能按幂等成功处理；新记录必须同令牌。
                if owner.as_deref().is_some_and(|owner| owner != lease) {
                    return Ok(CompleteOutcome::NotOwner);
                }
                return Ok(CompleteOutcome::Duplicate);
            }
            Some(Entry::Pending { lease: owner, .. }) if owner != lease => {
                return Ok(CompleteOutcome::NotOwner);
            }
            Some(Entry::Pending { .. }) => {}
            None => return Ok(CompleteOutcome::NotOwner),
        }
        let new_bytes = record_bytes(key, &frame, Some(lease));
        let Some(victims) = capacity_victims(
            &state,
            key,
            new_bytes,
            self.config.max_entries,
            self.config.max_state_bytes,
        ) else {
            return Err(());
        };
        if let Some(path) = self.config.state_path.as_deref()
            && append_done_record(path, key, &frame, lease).is_err()
        {
            // 写盘失败就不承认完成，调用方拿到 5xx。
            return Err(());
        }
        for victim in &victims {
            if let Some(Entry::Done { frame, lease }) = state.entries.remove(victim) {
                state.bytes =
                    state
                        .bytes
                        .saturating_sub(record_bytes(victim, &frame, lease.as_deref()));
            }
        }
        state.entries.insert(
            key.to_string(),
            Entry::Done {
                frame,
                lease: Some(lease.to_string()),
            },
        );
        state.bytes = state.bytes.saturating_add(new_bytes);
        if let Some(path) = self.config.state_path.as_deref() {
            compact_if_bloated(path, &state.entries, self.config.max_state_bytes);
        }
        Ok(CompleteOutcome::Completed)
    }
}

fn respond(stream: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    write_response(stream, code, reason, &[], body)
}

fn respond_with_lease(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &[u8],
    lease: &str,
) -> std::io::Result<()> {
    write_response(stream, code, reason, &[(LEASE_HEADER, lease)], body)
}

fn respond_lease_done(stream: &mut TcpStream, code: u16, reason: &str) -> std::io::Result<()> {
    write_response(stream, code, reason, &[(LEASE_DONE_HEADER, "1")], b"")
}

fn write_response(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
        crate::remote_subagent::FRAME_CONTENT_TYPE,
        body.len()
    )?;
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    stream.write_all(body)?;
    stream.flush()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum StoredRecord {
    /// 已完成记录；旧格式没有 `kind` 字段，仍按这种形态解析。
    Done {
        key: String,
        /// 响应帧的十六进制编码；避免为字节引入额外依赖。
        frame: String,
        /// 写入该帧的持有者令牌；旧格式没有此字段，回放为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<String>,
    },
    /// 占位租约：claim 成功或续租时追加，重启后仍能识别当前持有者。
    Pending {
        key: String,
        lease: String,
        /// 绝对到期时间（Unix 毫秒），重启时据此恢复剩余 TTL。
        expires_unix_ms: u64,
    },
}

/// 从追加日志逐行回放；后出现的记录覆盖同 key 的旧状态，最终按键数与字节上限
/// 裁剪，过期占位不再保留。不能只保留“最后 N 条”，否则高频续租会挤掉完成帧。
fn load_entries(
    path: &Path,
    pending_ttl: Duration,
    max_entries: usize,
    max_state_bytes: usize,
) -> std::io::Result<LedgerState> {
    if !path.exists() {
        return Ok(LedgerState::default());
    }
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let now = Instant::now();
    let now_unix_ms = unix_millis_now();
    let mut state = LedgerState::default();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| {
            invalid_data(format!(
                "{}:{}: read failed: {error}",
                path.display(),
                index + 1
            ))
        })?;
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_STATE_LINE_BYTES {
            return Err(invalid_data(format!(
                "{}:{}: state line exceeds {MAX_STATE_LINE_BYTES} bytes",
                path.display(),
                index + 1
            )));
        }
        let record: StoredRecord = serde_json::from_str(&line)
            .map_err(|error| invalid_data(format!("{}:{}: {error}", path.display(), index + 1)))?;
        let event_key = match &record {
            StoredRecord::Done { key, .. } | StoredRecord::Pending { key, .. } => key.clone(),
        };
        match record {
            StoredRecord::Done { key, frame, lease } => {
                if let Some(owner) = lease.as_deref()
                    && !valid_lease(owner)
                {
                    return Err(invalid_data(format!(
                        "{}: stored completion lease for {key} is invalid",
                        path.display()
                    )));
                }
                let frame = hex_decode(&frame)
                    .ok_or_else(|| invalid_data(format!("{}: bad hex frame", path.display())))?;
                let mut buffer = frame.clone();
                let response: SubagentResponse = match decode_frame(&mut buffer) {
                    Ok(Some(response)) => response,
                    _ => {
                        return Err(invalid_data(format!(
                            "{}: stored frame is not a valid response",
                            path.display()
                        )));
                    }
                };
                if idempotency_scope(&key, &response.task, response.epoch).is_err() {
                    return Err(invalid_data(format!(
                        "{}: stored frame does not match key {key}",
                        path.display()
                    )));
                }
                if let Some(Entry::Done { frame, lease }) = state.entries.get(&key) {
                    state.bytes =
                        state
                            .bytes
                            .saturating_sub(record_bytes(&key, frame, lease.as_deref()));
                }
                state.bytes =
                    state
                        .bytes
                        .saturating_add(record_bytes(&key, &frame, lease.as_deref()));
                state.entries.insert(key, Entry::Done { frame, lease });
            }
            StoredRecord::Pending {
                key,
                lease,
                expires_unix_ms,
            } => {
                if !valid_lease(&lease) {
                    return Err(invalid_data(format!(
                        "{}: stored lease for {key} is invalid",
                        path.display()
                    )));
                }
                // 新的占位事件覆盖同 key 的旧完成/占位状态。
                if let Some(Entry::Done { frame, lease }) = state.entries.remove(&key) {
                    state.bytes =
                        state
                            .bytes
                            .saturating_sub(record_bytes(&key, &frame, lease.as_deref()));
                }
                if expires_unix_ms > now_unix_ms {
                    // 以当前配置的 TTL 为上限，避免损坏/时钟漂移把租约无限延长。
                    let remaining =
                        Duration::from_millis(expires_unix_ms - now_unix_ms).min(pending_ttl);
                    state.entries.insert(
                        key,
                        Entry::Pending {
                            expires_at: now + remaining,
                            lease,
                        },
                    );
                } else {
                    state.entries.remove(&key);
                }
            }
        }
        trim_state(&mut state, max_entries, max_state_bytes, Some(&event_key));
    }
    Ok(state)
}

/// 按键数/字节上限裁剪回放后的状态：优先淘汰别的已完成项，保留刚回放的 key。
fn trim_state(
    state: &mut LedgerState,
    max_entries: usize,
    max_state_bytes: usize,
    preserve: Option<&str>,
) {
    while state.entries.len() > max_entries || state.bytes > max_state_bytes {
        if let Some(victim) = first_done_key_excluding(&state.entries, preserve) {
            if let Some(Entry::Done { frame, lease }) = state.entries.remove(&victim) {
                state.bytes =
                    state
                        .bytes
                        .saturating_sub(record_bytes(&victim, &frame, lease.as_deref()));
            }
            continue;
        }
        if state.entries.len() > max_entries
            && let Some(victim) = first_pending_key_excluding(&state.entries, preserve)
        {
            state.entries.remove(&victim);
            continue;
        }
        // 没有别的可淘汰项：为了尊重上限，只能放弃刚回放的完成项（正常日志不会走到）。
        if let Some(preserve) = preserve
            && let Some(Entry::Done { frame, lease }) = state.entries.remove(preserve)
        {
            state.bytes =
                state
                    .bytes
                    .saturating_sub(record_bytes(preserve, &frame, lease.as_deref()));
            continue;
        }
        break;
    }
}

/// 保守估算一条 JSONL 记录占用的字节数：帧按十六进制翻倍，key/lease 预留转义空间。
fn record_bytes(key: &str, frame: &[u8], lease: Option<&str>) -> usize {
    key.len()
        .saturating_mul(6)
        .saturating_add(frame.len().saturating_mul(2))
        .saturating_add(lease.map_or(0, |lease| lease.len().saturating_mul(6)))
        .saturating_add(32)
}

/// 按 key 顺序找第一个已完成项；`max_entries` 与字节淘汰共用这个顺序。
fn first_done_key_excluding(
    entries: &BTreeMap<String, Entry>,
    exclude: Option<&str>,
) -> Option<String> {
    entries
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != exclude)
        .find_map(|(key, entry)| matches!(entry, Entry::Done { .. }).then(|| key.clone()))
}

/// 占位键额超限时，优先放弃最早到期（最接近自然失效）的租约。
fn first_pending_key_excluding(
    entries: &BTreeMap<String, Entry>,
    exclude: Option<&str>,
) -> Option<String> {
    entries
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != exclude)
        .filter_map(|(key, entry)| match entry {
            Entry::Pending { expires_at, .. } => Some((*expires_at, key.clone())),
            Entry::Done { .. } => None,
        })
        .min_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)))
        .map(|(_, key)| key)
}

/// 预先计算完成 `key` 需要淘汰的已完成项；淘汰后仍装不下时返回 `None`。
fn capacity_victims(
    state: &LedgerState,
    key: &str,
    new_bytes: usize,
    max_entries: usize,
    max_state_bytes: usize,
) -> Option<BTreeSet<String>> {
    if new_bytes > max_state_bytes {
        return None;
    }
    let mut victims = BTreeSet::new();
    let mut projected_entries = state.entries.len() + usize::from(!state.entries.contains_key(key));
    let mut projected_bytes = state.bytes.checked_add(new_bytes)?;
    while projected_entries > max_entries || projected_bytes > max_state_bytes {
        let victim = state.entries.iter().find_map(|(other, entry)| {
            (other != key && !victims.contains(other) && matches!(entry, Entry::Done { .. }))
                .then(|| other.clone())
        })?;
        if let Some(Entry::Done { frame, lease }) = state.entries.get(&victim) {
            projected_bytes =
                projected_bytes.saturating_sub(record_bytes(&victim, frame, lease.as_deref()));
        }
        victims.insert(victim);
        projected_entries = projected_entries.saturating_sub(1);
    }
    Some(victims)
}

/// 追加一条已完成记录并落盘。
fn append_done_record(path: &Path, key: &str, frame: &[u8], lease: &str) -> std::io::Result<()> {
    let record = StoredRecord::Done {
        key: key.to_string(),
        frame: hex_encode(frame),
        lease: Some(lease.to_string()),
    };
    append_line(path, &record)
}

/// 追加一条占位记录并落盘；claim/续租在改内存状态前必须先成功。
fn append_pending_record(
    path: &Path,
    key: &str,
    lease: &str,
    expires_unix_ms: u64,
) -> std::io::Result<()> {
    let record = StoredRecord::Pending {
        key: key.to_string(),
        lease: lease.to_string(),
        expires_unix_ms,
    };
    append_line(path, &record)
}

fn append_line(path: &Path, record: &StoredRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(record)
        .map_err(|error| invalid_data(format!("serialize ledger record: {error}")))?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    file.sync_all()
}

/// 压缩临时文件路径：始终追加 `.compact`，避免覆盖 `state.compact` 这类原路径。
fn compaction_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".compact");
    PathBuf::from(name)
}

/// 压缩日志：写入当前内存里的完成项与未过期占位，临时文件落盘后原子替换。
fn compact_state(path: &Path, entries: &BTreeMap<String, Entry>) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = compaction_path(path);
    if temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    let mut writer = BufWriter::new(std::fs::File::create(&temporary)?);
    let now = Instant::now();
    for (key, entry) in entries {
        let record = match entry {
            Entry::Done { frame, lease } => StoredRecord::Done {
                key: key.clone(),
                frame: hex_encode(frame),
                lease: lease.clone(),
            },
            Entry::Pending { expires_at, lease } if now < *expires_at => StoredRecord::Pending {
                key: key.clone(),
                lease: lease.clone(),
                expires_unix_ms: expiry_unix_ms(*expires_at),
            },
            Entry::Pending { .. } => continue,
        };
        let line = serde_json::to_string(&record)
            .map_err(|error| invalid_data(format!("serialize ledger record: {error}")))?;
        writeln!(writer, "{line}")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    std::fs::rename(&temporary, path)
}

/// 启动时处理上次压缩留下的临时文件：目标存在说明压缩未完成，删临时文件；
/// 目标缺失说明替换前崩溃，用临时文件恢复。
fn recover_compaction(path: &Path) -> std::io::Result<()> {
    let temporary = compaction_path(path);
    if !temporary.exists() {
        return Ok(());
    }
    if path.exists() {
        std::fs::remove_file(temporary)
    } else {
        std::fs::rename(temporary, path)
    }
}

/// 物理日志超过逻辑上限的 1.5 倍时压缩一次，避免每次完成都重写大文件。
fn compact_if_bloated(path: &Path, entries: &BTreeMap<String, Entry>, max_state_bytes: usize) {
    let threshold = max_state_bytes.saturating_mul(3).saturating_div(2) as u64;
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > threshold) {
        // 压缩是物理文件治理；失败时保留追加日志，不把已确认的完成判成失败。
        let _ = compact_state(path, entries);
    }
}

fn invalid_data(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        output.push(high << 4 | low);
    }
    Some(output)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 账本客户端：申请执行权；成功时返回当前持有者租约令牌。
pub fn claim(endpoint: &RemoteEndpoint, key: &str) -> Result<Claim, String> {
    let reply = post(endpoint, "/claim", key, None, None)?;
    match reply.status {
        200 if reply.body.is_empty() => match reply.lease.filter(|lease| valid_lease(lease)) {
            Some(lease) => Ok(Claim::New { lease }),
            None => Err("ledger claim: missing or invalid lease header".into()),
        },
        200 => Ok(Claim::Done(reply.body)),
        409 => Ok(Claim::Pending),
        other => Err(format!("ledger claim returned HTTP {other}")),
    }
}

/// 账本客户端：延长一个占位租约。已完成或未知键都不会延长，返回错误。
pub fn renew(endpoint: &RemoteEndpoint, key: &str, lease: &str) -> Result<(), String> {
    renew_checked(endpoint, key, lease).map_err(|error| error.to_string())
}

/// 续租的结构化错误；心跳用它区分“租约已丢”和普通网络故障。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewError {
    InvalidLease,
    /// 403：租约已被其他持有者接管。
    LeaseLost,
    /// 404：账本已不认识该键（例如重启后未持久化占位）。
    UnknownKey,
    /// `/renew` 命中已完成键：当前结果已由别人提交，本机执行应立即停止。
    Completed,
    Http(u16),
    Transport(String),
}

impl std::fmt::Display for RenewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLease => write!(formatter, "ledger renew: invalid lease"),
            Self::LeaseLost => write!(formatter, "ledger renew: lease not held"),
            Self::UnknownKey => write!(formatter, "ledger renew: unknown key"),
            Self::Completed => write!(formatter, "ledger renew: already completed"),
            Self::Http(status) => write!(formatter, "ledger renew returned HTTP {status}"),
            Self::Transport(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for RenewError {}

/// 带结构化错误的续租：403 表示租约已被接管，404 表示账本已不认识该键。
pub fn renew_checked(endpoint: &RemoteEndpoint, key: &str, lease: &str) -> Result<(), RenewError> {
    if !valid_lease(lease) {
        return Err(RenewError::InvalidLease);
    }
    let reply = post(endpoint, "/renew", key, Some(lease), None).map_err(RenewError::Transport)?;
    let completed = reply.done;
    match reply.status {
        200 | 204 if completed => Err(RenewError::Completed),
        200 | 204 => Ok(()),
        403 => Err(RenewError::LeaseLost),
        404 => Err(RenewError::UnknownKey),
        other => Err(RenewError::Http(other)),
    }
}

/// 登记完成的结构化错误；服务端用它把“令牌已失效”转成 `Aborted`，而不是 5xx。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteError {
    InvalidLease,
    /// 403：占位已属于别人，或该键的完成帧已由另一个持有者写入。
    LeaseLost,
    Http(u16),
    Transport(String),
}

impl std::fmt::Display for CompleteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLease => write!(formatter, "ledger complete: invalid lease"),
            Self::LeaseLost => write!(formatter, "ledger complete: lease not held"),
            Self::Http(status) => write!(formatter, "ledger complete returned HTTP {status}"),
            Self::Transport(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CompleteError {}

/// 账本客户端：登记完成响应；只接受当前持有者的租约令牌。
pub fn complete(
    endpoint: &RemoteEndpoint,
    key: &str,
    lease: &str,
    frame: &[u8],
) -> Result<(), String> {
    complete_checked(endpoint, key, lease, frame).map_err(|error| error.to_string())
}

/// 带结构化错误的完成登记：403 表示租约已被接管或完成帧已由别人写入。
pub fn complete_checked(
    endpoint: &RemoteEndpoint,
    key: &str,
    lease: &str,
    frame: &[u8],
) -> Result<(), CompleteError> {
    if !valid_lease(lease) {
        return Err(CompleteError::InvalidLease);
    }
    let reply = post(endpoint, "/complete", key, Some(lease), Some(frame))
        .map_err(CompleteError::Transport)?;
    match reply.status {
        200 | 204 => Ok(()),
        403 => Err(CompleteError::LeaseLost),
        other => Err(CompleteError::Http(other)),
    }
}

struct LedgerReply {
    status: u16,
    body: Vec<u8>,
    lease: Option<String>,
    done: bool,
}

fn post(
    endpoint: &RemoteEndpoint,
    path: &str,
    key: &str,
    lease: Option<&str>,
    body: Option<&[u8]>,
) -> Result<LedgerReply, String> {
    endpoint.validate().map_err(|error| error.to_string())?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(endpoint.timeout_secs))
        .redirects(0)
        .build();
    let url = format!("{}{path}", endpoint.url.trim().trim_end_matches('/'));
    let mut request = agent
        .post(&url)
        .set("X-Trit-Idempotency", key)
        .set("Accept", crate::remote_subagent::FRAME_CONTENT_TYPE);
    if let Some(token) = endpoint.auth_token.as_deref() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    if let Some(lease) = lease {
        request = request.set(LEASE_HEADER, lease);
    }
    let result = match body {
        Some(bytes) => request
            .set("Content-Type", crate::remote_subagent::FRAME_CONTENT_TYPE)
            .send_bytes(bytes),
        None => request.send_bytes(&[]),
    };
    let (status, response) = match result {
        Ok(response) => (response.status(), response),
        Err(ureq::Error::Status(status, response)) => (status, response),
        Err(error) => return Err(format!("ledger transport: {error}")),
    };
    let lease = response.header(LEASE_HEADER).map(str::to_string);
    let done = response.header(LEASE_DONE_HEADER) == Some("1");
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_LEDGER_BODY as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("read ledger response: {error}"))?;
    Ok(LedgerReply {
        status,
        body,
        lease,
        done,
    })
}
