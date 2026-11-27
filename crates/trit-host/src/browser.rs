//! Chrome/Chromium 浏览器控制（CDP，headless）。
//!
//! 通过 DevTools Protocol 驱动本机 Chrome/Chromium（含 Edge），不嵌入渲染引擎：
//! 启动 `--headless=new --remote-debugging-port=0`，读 `DevToolsActivePort`，
//! 再用最小 WebSocket 客户端发 `Target.*` / `Page.*` / `Runtime.*` 命令。
//! 只支持文本帧与 ping/pong；不处理二进制帧。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::AgentError;

/// 浏览器启动配置。
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// 显式 Chrome/Chromium 路径；None 时按常见路径与 PATH 探测。
    pub chrome: Option<PathBuf>,
    /// 是否 headless；默认 true。
    pub headless: bool,
    /// 启动/命令超时。
    pub timeout: Duration,
    /// 额外的 Chrome 参数。
    pub extra_args: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            chrome: None,
            headless: true,
            timeout: Duration::from_secs(15),
            extra_args: Vec::new(),
        }
    }
}

/// 一个受控的浏览器实例。
pub struct Browser {
    child: Option<Child>,
    socket: WebSocket,
    session_id: Option<String>,
    target_id: Option<String>,
    next_id: u64,
    user_data_dir: Option<PathBuf>,
}

impl Browser {
    /// 启动 headless 浏览器并连接浏览器级 target。
    pub fn launch(config: &BrowserConfig) -> Result<Self, AgentError> {
        let chrome = find_chrome(config.chrome.as_deref()).ok_or_else(|| {
            AgentError::Tool(
                "no Chrome/Chromium found; set TRIT_CHROME or install Chrome/Edge".into(),
            )
        })?;
        let user_data_dir = unique_temp_dir("trit-chrome");
        std::fs::create_dir_all(&user_data_dir)
            .map_err(|error| AgentError::Io(format!("chrome user-data-dir: {error}")))?;
        let mut command = Command::new(&chrome);
        command
            .arg(if config.headless {
                "--headless=new"
            } else {
                "--headless"
            })
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-gpu")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for arg in &config.extra_args {
            command.arg(arg);
        }
        let child = command
            .spawn()
            .map_err(|error| AgentError::Io(format!("spawn {}: {error}", chrome.display())))?;
        // 启动中途失败时 kill 子进程并删临时目录；Child 的 Drop 不会 kill。
        let mut guard = LaunchGuard {
            child: Some(child),
            dir: Some(user_data_dir.clone()),
        };
        let (port, ws_path) = read_devtools_port(&user_data_dir, config.timeout)?;
        let socket = WebSocket::connect("127.0.0.1", port, &ws_path)?;
        Ok(Self {
            child: guard.child.take(),
            socket,
            session_id: None,
            target_id: None,
            next_id: 0,
            user_data_dir: guard.dir.take(),
        })
    }

    /// 打开 URL：创建 target、附加 flatten session，再导航。
    pub fn open(&mut self, url: &str) -> Result<(), AgentError> {
        if let Some(target) = self.target_id.take() {
            let _ = self.command("Target.closeTarget", json!({"targetId": target}));
        }
        self.session_id = None;
        let created = self.command("Target.createTarget", json!({"url": "about:blank"}))?;
        let target_id = created["targetId"]
            .as_str()
            .ok_or_else(|| AgentError::Tool("CDP createTarget returned no targetId".into()))?
            .to_owned();
        let attached = self.command(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
        )?;
        let session_id = attached["sessionId"]
            .as_str()
            .ok_or_else(|| AgentError::Tool("CDP attachToTarget returned no sessionId".into()))?
            .to_owned();
        self.target_id = Some(target_id);
        self.session_id = Some(session_id);
        self.command("Page.enable", json!({}))?;
        self.command("Runtime.enable", json!({}))?;
        self.command("Page.navigate", json!({"url": url}))?;
        Ok(())
    }

    /// 在页面执行表达式，返回 `returnByValue` 的结果。
    pub fn eval(&mut self, expression: &str) -> Result<Value, AgentError> {
        let result = self.command(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )?;
        if let Some(exception) = result.get("exceptionDetails") {
            return Err(AgentError::Tool(format!(
                "browser eval failed: {exception}"
            )));
        }
        Ok(result
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// 截图到 `path`（PNG）。
    pub fn screenshot(&mut self, path: &Path) -> Result<(), AgentError> {
        let result = self.command("Page.captureScreenshot", json!({"format": "png"}))?;
        let data = result["data"]
            .as_str()
            .ok_or_else(|| AgentError::Tool("CDP screenshot returned no data".into()))?;
        let bytes = base64_decode(data)
            .ok_or_else(|| AgentError::Tool("invalid base64 screenshot".into()))?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| AgentError::Io(format!("screenshot dir: {error}")))?;
        }
        std::fs::write(path, bytes)
            .map_err(|error| AgentError::Io(format!("write {}: {error}", path.display())))?;
        Ok(())
    }

    fn command(&mut self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = &self.session_id {
            message["sessionId"] = json!(session);
        }
        self.socket.send_text(&message.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                return Err(AgentError::Tool(format!("CDP {method} timed out")));
            }
            let text = self.socket.read_text()?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| AgentError::Tool(format!("CDP frame: {error}")))?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(AgentError::Tool(format!("CDP {method}: {error}")));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(dir) = self.user_data_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// 启动中途失败的清理器：kill Chrome 并删除临时 user-data-dir。
struct LaunchGuard {
    child: Option<Child>,
    dir: Option<PathBuf>,
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn find_chrome(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return path.is_file().then(|| path.to_path_buf());
    }
    if let Ok(path) = std::env::var("TRIT_CHROME") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates = vec![
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
    ];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe"));
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        );
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

fn read_devtools_port(dir: &Path, timeout: Duration) -> Result<(u16, String), AgentError> {
    let path = dir.join("DevToolsActivePort");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut lines = text.lines();
            if let (Some(port), Some(ws)) = (lines.next(), lines.next())
                && let Ok(port) = port.trim().parse::<u16>()
            {
                return Ok((port, ws.trim().to_owned()));
            }
        }
        if Instant::now() > deadline {
            return Err(AgentError::Tool(format!(
                "Chrome did not expose DevToolsActivePort under {}",
                dir.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct WebSocket {
    stream: TcpStream,
}

impl WebSocket {
    fn connect(host: &str, port: u16, path: &str) -> Result<Self, AgentError> {
        let mut stream = TcpStream::connect((host, port))
            .map_err(|error| AgentError::Io(format!("websocket connect {host}:{port}: {error}")))?;
        let timeout = Some(Duration::from_secs(30));
        stream
            .set_read_timeout(timeout)
            .map_err(|error| AgentError::Io(error.to_string()))?;
        stream
            .set_write_timeout(timeout)
            .map_err(|error| AgentError::Io(error.to_string()))?;
        let key = websocket_key();
        let host_header = if port == 80 {
            host.to_owned()
        } else {
            format!("{host}:{port}")
        };
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .map_err(|error| AgentError::Io(error.to_string()))?;
        stream
            .flush()
            .map_err(|error| AgentError::Io(error.to_string()))?;
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            stream
                .read_exact(&mut byte)
                .map_err(|error| AgentError::Io(format!("websocket handshake: {error}")))?;
            response.push(byte[0]);
            if response.len() > 8192 {
                return Err(AgentError::Tool("websocket handshake too large".into()));
            }
        }
        let head = String::from_utf8_lossy(&response);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(AgentError::Tool(format!(
                "websocket upgrade rejected: {}",
                head.lines().next().unwrap_or_default()
            )));
        }
        Ok(Self { stream })
    }

    fn send_text(&mut self, text: &str) -> Result<(), AgentError> {
        self.send_frame(0x1, text.as_bytes())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), AgentError> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let mask = [0xA5u8, 0x5A, 0x0F, 0xF0];
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        self.stream
            .write_all(&frame)
            .and_then(|_| self.stream.flush())
            .map_err(|error| AgentError::Io(format!("websocket write: {error}")))
    }

    fn read_text(&mut self) -> Result<String, AgentError> {
        let mut payload = Vec::new();
        loop {
            let (fin, opcode, data) = self.read_frame()?;
            match opcode {
                0x1 | 0x0 => {
                    payload.extend_from_slice(&data);
                    if fin {
                        return String::from_utf8(payload)
                            .map_err(|error| AgentError::Tool(format!("websocket text: {error}")));
                    }
                }
                0x9 => self.send_frame(0xA, &data)?,
                0xA => {}
                0x8 => return Err(AgentError::Tool("websocket closed by peer".into())),
                _ => {}
            }
        }
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), AgentError> {
        let mut head = [0u8; 2];
        self.stream
            .read_exact(&mut head)
            .map_err(|error| AgentError::Io(format!("websocket read: {error}")))?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let masked = head[1] & 0x80 != 0;
        let len = match head[1] & 0x7F {
            126 => {
                let mut bytes = [0u8; 2];
                self.stream
                    .read_exact(&mut bytes)
                    .map_err(|error| AgentError::Io(error.to_string()))?;
                u16::from_be_bytes(bytes) as usize
            }
            127 => {
                let mut bytes = [0u8; 8];
                self.stream
                    .read_exact(&mut bytes)
                    .map_err(|error| AgentError::Io(error.to_string()))?;
                u64::from_be_bytes(bytes) as usize
            }
            other => other as usize,
        };
        if len > 64 * 1024 * 1024 {
            return Err(AgentError::Tool("websocket frame too large".into()));
        }
        let mut mask = [0u8; 4];
        if masked {
            self.stream
                .read_exact(&mut mask)
                .map_err(|error| AgentError::Io(error.to_string()))?;
        }
        let mut payload = vec![0u8; len];
        self.stream
            .read_exact(&mut payload)
            .map_err(|error| AgentError::Io(error.to_string()))?;
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok((fin, opcode, payload))
    }
}

fn websocket_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&nanos.to_le_bytes()[..8]);
    bytes[8..12].copy_from_slice(&((nanos >> 64) as u32).to_le_bytes());
    bytes[12..].copy_from_slice(&(pid as u32).to_le_bytes());
    base64_encode(&bytes)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = BASE64.iter().position(|&candidate| candidate == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for payload in [b"" as &[u8], b"a", b"ab", b"abc", b"\x00\xff\x10"] {
            let encoded = base64_encode(payload);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(payload));
        }
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn websocket_frames_round_trip_over_tcp() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&request);
            let key = head
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                .unwrap()
                .trim()
                .to_owned();
            let accept = {
                use ring::digest;
                let mut input = key.into_bytes();
                input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
                let digest = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &input);
                base64_encode(digest.as_ref())
            };
            write!(
                stream,
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();
            // 读一个客户端文本帧（带掩码），回显。
            let mut head = [0u8; 2];
            stream.read_exact(&mut head).unwrap();
            let len = (head[1] & 0x7F) as usize;
            let mut mask = [0u8; 4];
            stream.read_exact(&mut mask).unwrap();
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).unwrap();
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
            let mut frame = vec![0x81];
            frame.push(payload.len() as u8);
            frame.extend_from_slice(&payload);
            stream.write_all(&frame).unwrap();
        });
        let mut client = WebSocket::connect("127.0.0.1", address.port(), "/").unwrap();
        client.send_text("ping").unwrap();
        assert_eq!(client.read_text().unwrap(), "ping");
        server.join().unwrap();
    }

    #[test]
    #[ignore = "requires Chrome/Chromium (set TRIT_CHROME or install Chrome/Edge)"]
    fn launches_chrome_and_evaluates_expression() {
        let mut browser = Browser::launch(&BrowserConfig::default()).expect("launch chrome");
        browser
            .open("data:text/html,<title>trit</title><h1>hello</h1>")
            .expect("open page");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            browser.eval("document.title").unwrap(),
            serde_json::json!("trit")
        );
        assert_eq!(
            browser
                .eval("document.querySelector('h1').textContent")
                .unwrap(),
            serde_json::json!("hello")
        );
    }
}
