//! HTTP MCP 客户端（Streamable HTTP 子集）：JSON-RPC 2.0 over `POST /mcp`。
//!
//! 只实现工具发现与调用：`initialize` → `tools/list` → `tools/call`。
//! 响应可以是普通 JSON，也可以是 SSE（`data:` 行）；工具列表首次成功后缓存，
//! 失败不缓存，下次重试。响应体有上限，避免异常服务端拖垮进程。

use std::io::{Cursor, Read};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AgentError;
use crate::provider_stream::read_sse_events;
use crate::supervisor::McpTool;

/// 单次响应体上限；工具列表与调用结果都应远小于此值。
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// 工具列表缓存上限：异常服务端返回超长列表时截断。
const MAX_CACHED_TOOLS: usize = 4096;

/// HTTP MCP server 配置；FluxDown 的 `/mcp` 即此形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpMcpSpec {
    pub name: String,
    pub url: String,
    /// 附加请求头，例如 `Authorization: Bearer <token>`。
    pub headers: Vec<(String, String)>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for HttpMcpSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            headers: Vec::new(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

fn default_timeout_secs() -> u64 {
    30
}

pub struct HttpMcpClient {
    spec: HttpMcpSpec,
    /// 复用连接池；不要每次请求新建 Agent。
    agent: ureq::Agent,
    next_id: AtomicU64,
    initialized: AtomicBool,
    tools: Mutex<Option<Vec<McpTool>>>,
}

impl HttpMcpClient {
    pub fn new(spec: HttpMcpSpec) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(spec.timeout_secs.max(1)))
            .build();
        Self {
            spec,
            agent,
            next_id: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
            tools: Mutex::new(None),
        }
    }

    pub fn name(&self) -> &str {
        if self.spec.name.trim().is_empty() {
            self.spec.url.as_str()
        } else {
            self.spec.name.as_str()
        }
    }

    pub fn spec(&self) -> &HttpMcpSpec {
        &self.spec
    }

    /// 发现工具；成功后缓存，失败不缓存。
    pub fn tools(&self) -> Result<Vec<McpTool>, AgentError> {
        if let Some(cached) = self
            .tools
            .lock()
            .map_err(|_| AgentError::Tool("HTTP MCP tool cache is poisoned".into()))?
            .clone()
        {
            return Ok(cached);
        }
        self.initialize()?;
        let result = self.rpc("tools/list", json!({}))?;
        let mut tools: Vec<McpTool> = serde_json::from_value(
            result
                .get("tools")
                .cloned()
                .ok_or_else(|| AgentError::Tool("HTTP MCP tools/list returned no tools".into()))?,
        )
        .map_err(|error| AgentError::Tool(format!("HTTP MCP tools/list: {error}")))?;
        if tools.len() > MAX_CACHED_TOOLS {
            tools.truncate(MAX_CACHED_TOOLS);
        }
        *self
            .tools
            .lock()
            .map_err(|_| AgentError::Tool("HTTP MCP tool cache is poisoned".into()))? =
            Some(tools.clone());
        Ok(tools)
    }

    pub fn call(&self, tool: &str, arguments: Value) -> Result<Value, AgentError> {
        self.initialize()?;
        let result = self.rpc("tools/call", json!({"name": tool, "arguments": arguments}))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(AgentError::Tool(format!(
                "HTTP MCP {} returned isError: {}",
                self.name(),
                result
            )));
        }
        Ok(result)
    }

    fn initialize(&self) -> Result<(), AgentError> {
        if self.initialized.load(Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "trit-host", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut request = self
            .agent
            .post(self.spec.url.trim())
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        for (key, value) in &self.spec.headers {
            request = request.set(key, value);
        }
        let response = match request.send_string(&body.to_string()) {
            Ok(response) => response,
            Err(ureq::Error::Status(code, response)) => {
                let text = read_body(response);
                return Err(AgentError::Tool(format!(
                    "HTTP MCP {method} returned HTTP {code}: {}",
                    text.chars().take(512).collect::<String>()
                )));
            }
            Err(error) => {
                return Err(AgentError::Tool(format!(
                    "HTTP MCP {} {method} transport: {error}",
                    self.name()
                )));
            }
        };
        let content_type = response
            .header("Content-Type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let text = read_body(response);
        let payload =
            if content_type.contains("text/event-stream") || !text.trim_start().starts_with('{') {
                collect_sse_data(&text)?
            } else {
                text
            };
        let value: Value = serde_json::from_str(payload.trim())
            .map_err(|error| AgentError::Tool(format!("HTTP MCP {method}: {error}")))?;
        if let Some(error) = value.get("error") {
            return Err(AgentError::Tool(format!("HTTP MCP {method}: {error}")));
        }
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(AgentError::Tool(format!(
                "HTTP MCP {method}: response id mismatch"
            )));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn read_body(response: ureq::Response) -> String {
    let mut bytes = Vec::new();
    let _ = response
        .into_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut bytes);
    if bytes.len() as u64 > MAX_BODY_BYTES {
        bytes.truncate(MAX_BODY_BYTES as usize);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 从 SSE 流里取最后一条 `data:` 负载；没有事件头时原样返回。
fn collect_sse_data(text: &str) -> Result<String, AgentError> {
    let mut last = String::new();
    read_sse_events(Cursor::new(text), |event| {
        if !event.data.trim().is_empty() {
            last = event.data.clone();
        }
        Ok(())
    })?;
    if last.is_empty() {
        return Err(AgentError::Tool("HTTP MCP returned no SSE data".into()));
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn mock_server() -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let id = request["id"].clone();
                let method = request["method"].as_str().unwrap_or_default();
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mock", "version": "1"}
                    }),
                    "tools/list" => json!({
                        "tools": [{
                            "name": "download_add",
                            "description": "add a download",
                            "inputSchema": {"type": "object"}
                        }]
                    }),
                    "tools/call" => json!({"content": [], "isError": false}),
                    _ => json!({}),
                };
                let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
                let body = serde_json::to_string(&response).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        (port, handle)
    }

    #[test]
    fn discovers_and_calls_tools() {
        let (port, handle) = mock_server();
        let client = HttpMcpClient::new(HttpMcpSpec {
            name: "mock".into(),
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: Vec::new(),
            timeout_secs: 5,
        });
        let tools = client.tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "download_add");
        client
            .call("download_add", json!({"url": "https://example.com/a.zip"}))
            .unwrap();
        handle.join().unwrap();
    }
}
