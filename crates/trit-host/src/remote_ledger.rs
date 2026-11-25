//! 共享幂等账本：让多台远端在同一个 `task@epoch` 上达成至多一次执行。
//!
//! 协议（HTTP/1.1，一连接一请求，路径固定 `/claim` 与 `/complete`）：
//!
//! - `POST /claim`，头 `X-Trit-Idempotency: <task>@<epoch>`：
//!   - `200` + 空体：调用方获得执行权（[`Claim::New`]）；
//!   - `200` + 一帧：已有完成响应（[`Claim::Done`]），直接复用；
//!   - `409`：另一执行者持有该键（[`Claim::Pending`]）；
//!   - `503`：账本已满或内部错误。
//! - `POST /complete`，同一个头 + 一帧响应：登记完成；`204` 接受（重复完成幂等）。
//!
//! 占位有 TTL（默认 60s）：执行者崩溃后，过期占位可以被接管。账本只保存响应帧，
//! 不执行任务；它是跨机重试的协调点，不是任务队列。
//!
//! 安全边界：账本默认只应绑定 loopback 或置于内网；配置 token 后所有请求都要
//! `Authorization: Bearer <token>`。账本不校验任务内容，只校验 `complete` 的帧
//! 与幂等键一致。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use trit_orch::{MAX_FRAME_BYTES, RemoteEndpoint, SubagentResponse, decode_frame, encode_frame};

/// 连接上的读写超时：防止半开的客户端长期占住服务线程。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 默认占位 TTL：执行者崩溃后，多久允许其他节点接管。
const DEFAULT_PENDING_TTL: Duration = Duration::from_secs(60);

/// 默认账本容量上限。
const DEFAULT_MAX_ENTRIES: usize = 4096;

/// 单个响应体的上限（与远程 runner 一致）。
const MAX_LEDGER_BODY: usize = MAX_FRAME_BYTES + 4;

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
}

impl LedgerServerConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            token: None,
            pending_ttl: DEFAULT_PENDING_TTL,
            max_entries: DEFAULT_MAX_ENTRIES,
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
}

/// 一次 claim 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// 获得执行权；执行完成后必须调用 [`complete`]。
    New,
    /// 另一个执行者正在执行；稍后重试 claim。
    Pending,
    /// 已经有完成响应，直接回放这一帧。
    Done(Vec<u8>),
}

enum Entry {
    Pending { since: Instant },
    Done { frame: Vec<u8> },
}

#[derive(Default)]
struct LedgerState {
    entries: BTreeMap<String, Entry>,
}

/// 已绑定的账本服务端。
pub struct LedgerServer {
    listener: TcpListener,
    config: LedgerServerConfig,
    state: Arc<Mutex<LedgerState>>,
    completions: Arc<AtomicU64>,
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
        if config.pending_ttl.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger pending_ttl must be positive",
            ));
        }
        let listener = TcpListener::bind(config.bind.as_str())?;
        Ok(Self {
            listener,
            config,
            state: Arc::default(),
            completions: Arc::default(),
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
        let (stream, _) = self.listener.accept()?;
        handle(stream, &self.config, &self.state, &self.completions)
    }

    /// 每个连接一个线程；账本请求通常很短，但仍不阻塞其他连接。
    pub fn serve_forever(&self) -> std::io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept()?;
            let config = self.config.clone();
            let state = Arc::clone(&self.state);
            let completions = Arc::clone(&self.completions);
            std::thread::spawn(move || {
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
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return respond(&mut stream, 400, "Bad Request", b"");
    }
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
                Ok(length) => content_length = length,
                Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
            },
            "authorization" => authorization = Some(value.to_string()),
            "x-trit-idempotency" => idempotency = Some(value.to_string()),
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
        return Err(error);
    }

    let Some(key) = idempotency else {
        return respond(&mut stream, 400, "Bad Request", b"");
    };

    match path {
        "/claim" => {
            let server = LedgerServerView { state, config };
            match server.claim(&key) {
                Ok(Claim::New) => respond(&mut stream, 200, "OK", b""),
                Ok(Claim::Pending) => respond(&mut stream, 409, "Conflict", b""),
                Ok(Claim::Done(frame)) => respond(&mut stream, 200, "OK", &frame),
                Err(()) => respond(&mut stream, 503, "Service Unavailable", b""),
            }
        }
        "/complete" => {
            let mut buffer = body;
            let response: SubagentResponse = match decode_frame(&mut buffer) {
                Ok(Some(response)) => response,
                _ => return respond(&mut stream, 400, "Bad Request", b""),
            };
            if format!("{}@{}", response.task, response.epoch) != key {
                return respond(&mut stream, 400, "Bad Request", b"");
            }
            let frame = match encode_frame(&response) {
                Ok(frame) => frame,
                Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
            };
            let view = LedgerServerView { state, config };
            if view.complete(&key, frame) {
                completions.fetch_add(1, Ordering::SeqCst);
                respond(&mut stream, 204, "No Content", b"")
            } else {
                respond(&mut stream, 503, "Service Unavailable", b"")
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

impl LedgerServerView<'_> {
    fn claim(&self, key: &str) -> Result<Claim, ()> {
        let now = Instant::now();
        let mut state = self.state.lock().map_err(|_| ())?;
        if let Some(entry) = state.entries.get_mut(key) {
            match entry {
                Entry::Done { frame } => return Ok(Claim::Done(frame.clone())),
                Entry::Pending { since }
                    if now.duration_since(*since) < self.config.pending_ttl =>
                {
                    return Ok(Claim::Pending);
                }
                Entry::Pending { since } => {
                    *since = now;
                    return Ok(Claim::New);
                }
            }
        }
        if state.entries.len() >= self.config.max_entries {
            let victim = state
                .entries
                .iter()
                .find_map(|(key, entry)| matches!(entry, Entry::Done { .. }).then(|| key.clone()));
            match victim {
                Some(victim) => {
                    state.entries.remove(&victim);
                }
                None => return Err(()),
            }
        }
        state
            .entries
            .insert(key.to_string(), Entry::Pending { since: now });
        Ok(Claim::New)
    }

    fn complete(&self, key: &str, frame: Vec<u8>) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if !state.entries.contains_key(key) && state.entries.len() >= self.config.max_entries {
            let victim = state
                .entries
                .iter()
                .find_map(|(key, entry)| matches!(entry, Entry::Done { .. }).then(|| key.clone()));
            match victim {
                Some(victim) => {
                    state.entries.remove(&victim);
                }
                None => return false,
            }
        }
        state.entries.insert(key.to_string(), Entry::Done { frame });
        true
    }
}

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

/// 账本客户端：申请执行权。
pub fn claim(endpoint: &RemoteEndpoint, key: &str) -> Result<Claim, String> {
    let (status, body) = post(endpoint, "/claim", key, None)?;
    match status {
        200 if body.is_empty() => Ok(Claim::New),
        200 => Ok(Claim::Done(body)),
        409 => Ok(Claim::Pending),
        other => Err(format!("ledger claim returned HTTP {other}")),
    }
}

/// 账本客户端：登记完成响应。
pub fn complete(endpoint: &RemoteEndpoint, key: &str, frame: &[u8]) -> Result<(), String> {
    let (status, _) = post(endpoint, "/complete", key, Some(frame))?;
    match status {
        200 | 204 => Ok(()),
        other => Err(format!("ledger complete returned HTTP {other}")),
    }
}

fn post(
    endpoint: &RemoteEndpoint,
    path: &str,
    key: &str,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), String> {
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
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_LEDGER_BODY as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("read ledger response: {error}"))?;
    Ok((status, body))
}
