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
//! 4. **一连接一请求**：读满一帧即回包并关闭，便于宿主侧超时语义保持简单。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use trit_orch::{
    DelegationStatus, MAX_FRAME_BYTES, SubagentRequest, SubagentResponse, decode_frame,
    encode_frame,
};

/// 连接上的读写超时：防止半开的客户端长期占住服务线程。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 服务端应答模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMode {
    /// 回 `Completed`（默认）。
    Echo,
    /// 回 `Failed`：让宿主走“有响应但没完成”的分支。
    Fail,
    /// 回 200 空体：让宿主走“没有合法响应”的分支。
    Silent,
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
}

impl RemoteServerConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            token: None,
            mode: RemoteMode::Echo,
            sleep_ms: 0,
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
}

/// 已绑定的参考服务端。
pub struct RemoteServer {
    listener: TcpListener,
    config: RemoteServerConfig,
}

impl RemoteServer {
    /// 绑定监听地址；失败即返回，不重试。
    pub fn bind(config: RemoteServerConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind(config.bind.as_str())?;
        Ok(Self { listener, config })
    }

    /// 实际监听地址（`--bind 127.0.0.1:0` 时用它对外公布）。
    pub fn addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 服务一个连接后返回；测试与单次演示用。
    pub fn serve_once(&self) -> std::io::Result<()> {
        let (stream, _) = self.listener.accept()?;
        handle(stream, &self.config)
    }

    /// 持续服务，直到进程被终止；单连接错误只丢弃该连接。
    pub fn serve_forever(&self) -> std::io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept()?;
            let _ = handle(stream, &self.config);
        }
    }
}

/// 处理一个连接：解析请求头 → 鉴权 → 读一帧 → 回一帧 → 关闭。
fn handle(mut stream: TcpStream, config: &RemoteServerConfig) -> std::io::Result<()> {
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

    if config.sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(config.sleep_ms));
    }

    let status = match config.mode {
        RemoteMode::Echo => DelegationStatus::Completed,
        RemoteMode::Fail => DelegationStatus::Failed {
            message: "remote echo configured to fail".into(),
        },
        RemoteMode::Silent => return respond(&mut stream, 200, "OK", b""),
    };
    let response = SubagentResponse {
        task: request.task.clone(),
        epoch: request.epoch,
        status,
        content: format!("remote echo {}@{}", request.task, request.epoch).into_bytes(),
        tokens: 0,
    };
    let frame = match encode_frame(&response) {
        Ok(frame) => frame,
        Err(_) => return respond(&mut stream, 500, "Internal Server Error", b""),
    };
    respond(&mut stream, 200, "OK", &frame)
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
