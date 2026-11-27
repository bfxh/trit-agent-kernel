//! 把发现的 MCP 工具桥接进内核工具循环。
//!
//! 路由名使用 `mcp__<server>__<tool>` 命名空间，避免与本地工具冲突；
//! 执行仍走 `ToolExecutor`，因此会话、检查点与会话回放无需特殊分支。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::mcp_http::HttpMcpClient;
use crate::provider::{ToolCatalog, ToolSpec};
use crate::supervisor::{McpSupervisor, RecoveryContext};
use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

#[derive(Debug, Clone)]
pub struct McpToolRoute {
    pub exposed_name: String,
    pub server: String,
    pub tool: String,
    pub description: String,
    pub input_schema: Value,
    pub declared_read_only: bool,
    /// stdio supervisor 下标；HTTP 客户端为 0 且 `http` 为 Some。
    supervisor: usize,
    /// HTTP MCP 客户端下标；stdio 路由为 None。
    http: Option<usize>,
}

#[derive(Clone)]
pub struct McpToolClient {
    supervisors: Vec<Arc<Mutex<McpSupervisor>>>,
    http: Vec<Arc<HttpMcpClient>>,
    recovery: Arc<Mutex<RecoveryContext>>,
    timeout: Duration,
    /// 路由缓存；高频 `route()`/`tools()` 调用不重复重建。
    routes_cache: RoutesCache,
}

/// 路由缓存：构建时刻 + 当时的完整路由表。
type RoutesCache = Arc<Mutex<Option<(Instant, Vec<McpToolRoute>)>>>;

impl McpToolClient {
    pub(crate) fn new(
        supervisors: Vec<Arc<Mutex<McpSupervisor>>>,
        http: Vec<Arc<HttpMcpClient>>,
        recovery: Arc<Mutex<RecoveryContext>>,
        timeout: Duration,
    ) -> Self {
        Self {
            supervisors,
            http,
            recovery,
            timeout,
            routes_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// 路由缓存：5 秒内复用；`invalidate` 可强制重建。
    pub fn routes(&self) -> Vec<McpToolRoute> {
        let now = Instant::now();
        if let Ok(cache) = self.routes_cache.lock()
            && let Some((cached_at, routes)) = cache.as_ref()
            && now.duration_since(*cached_at) < Duration::from_secs(5)
        {
            return routes.clone();
        }
        let routes = self.build_routes();
        if let Ok(mut cache) = self.routes_cache.lock() {
            *cache = Some((now, routes.clone()));
        }
        routes
    }

    /// 强制下次 `routes()` 重建；MCP `list_changed` 或 HTTP `refresh` 后调用。
    pub fn invalidate(&self) {
        if let Ok(mut cache) = self.routes_cache.lock() {
            *cache = None;
        }
    }

    fn build_routes(&self) -> Vec<McpToolRoute> {
        let mut used = std::collections::BTreeSet::new();
        let mut routes = Vec::new();
        for (index, supervisor) in self.supervisors.iter().enumerate() {
            let Ok(supervisor) = supervisor.lock() else {
                continue;
            };
            let server = supervisor.name().to_owned();
            let mut tools: Vec<_> = supervisor.tools().iter().collect();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            for tool in tools {
                let exposed_name = unique_exposed_name(&server, &tool.name, &mut used);
                let description = tool
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("MCP tool {}.{}", server, tool.name));
                let input_schema = if tool.input_schema.is_null() {
                    serde_json::json!({"type": "object"})
                } else {
                    tool.input_schema.clone()
                };
                routes.push(McpToolRoute {
                    exposed_name,
                    server: server.clone(),
                    tool: tool.name.clone(),
                    description,
                    input_schema,
                    // `readOnlyHint` 是 MCP 服务端自报，不可作为授权依据。
                    // 仅保留在 route 上供展示/诊断（`trit-host tools`），绝不进入 ToolSpec.read_only。
                    declared_read_only: tool
                        .annotations
                        .as_ref()
                        .is_some_and(|annotations| annotations.read_only_hint),
                    supervisor: index,
                    http: None,
                });
            }
        }
        for (index, client) in self.http.iter().enumerate() {
            let server = client.name().to_owned();
            let Ok(mut tools) = client.tools() else {
                continue;
            };
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            for tool in tools {
                let exposed_name = unique_exposed_name(&server, &tool.name, &mut used);
                let description = tool
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("MCP tool {}.{}", server, tool.name));
                let input_schema = if tool.input_schema.is_null() {
                    serde_json::json!({"type": "object"})
                } else {
                    tool.input_schema.clone()
                };
                routes.push(McpToolRoute {
                    exposed_name,
                    server: server.clone(),
                    tool: tool.name.clone(),
                    description,
                    input_schema,
                    declared_read_only: tool
                        .annotations
                        .as_ref()
                        .is_some_and(|annotations| annotations.read_only_hint),
                    supervisor: 0,
                    http: Some(index),
                });
            }
        }
        routes
    }

    pub fn is_empty(&self) -> bool {
        self.routes().is_empty()
    }

    fn route(&self, exposed_name: &str) -> Option<McpToolRoute> {
        self.routes()
            .into_iter()
            .find(|route| route.exposed_name == exposed_name)
    }

    pub fn call(&self, exposed_name: &str, arguments: Value) -> Result<Value, AgentError> {
        let route = self
            .route(exposed_name)
            .ok_or_else(|| AgentError::Tool(format!("unknown MCP tool: {exposed_name}")))?;
        if let Some(index) = route.http {
            let client = self
                .http
                .get(index)
                .ok_or_else(|| AgentError::Tool("HTTP MCP client is missing".into()))?;
            return client.call(&route.tool, arguments).map_err(|error| {
                AgentError::Tool(format!("MCP {}/{}: {error}", route.server, route.tool))
            });
        }
        let recovery = self
            .recovery
            .lock()
            .map_err(|_| AgentError::Tool("MCP recovery lock is poisoned".into()))?
            .clone();
        let supervisor = self
            .supervisors
            .get(route.supervisor)
            .ok_or_else(|| AgentError::Tool("MCP supervisor is missing".into()))?;
        let mut supervisor = supervisor
            .lock()
            .map_err(|_| AgentError::Tool("MCP supervisor lock is poisoned".into()))?;
        supervisor
            .call_tool(&recovery, &route.tool, arguments, self.timeout)
            .map_err(|error| {
                AgentError::Tool(format!("MCP {}/{}: {error}", route.server, route.tool))
            })
    }
}

impl ToolCatalog for McpToolClient {
    fn tools(&self) -> Vec<ToolSpec> {
        self.routes()
            .into_iter()
            .map(|route| ToolSpec {
                name: route.exposed_name,
                description: format!(
                    "{} (MCP {} / {})",
                    route.description, route.server, route.tool
                ),
                // MCP 的 readOnlyHint 不进入 ToolSpec.read_only（授权元数据）。
                read_only: false,
                input_schema: Some(route.input_schema),
            })
            .collect()
    }
}

pub struct McpToolExecutor<T> {
    client: Arc<McpToolClient>,
    inner: T,
}

impl<T: ToolExecutor> McpToolExecutor<T> {
    pub fn new(client: Arc<McpToolClient>, inner: T) -> Self {
        Self { client, inner }
    }
}

impl<T: ToolExecutor> ToolExecutor for McpToolExecutor<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if request.tool.starts_with("mcp__") {
            let arguments = request
                .arguments
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            return match self.client.call(&request.tool, arguments) {
                Ok(result) => Ok(mcp_outcome(request, true, false, summarize(&result))),
                Err(error) => {
                    let message = error.to_string();
                    let in_doubt = message.to_ascii_lowercase().contains("timeout");
                    Ok(mcp_outcome(request, false, in_doubt, message))
                }
            };
        }
        self.inner.execute(request)
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

fn mcp_outcome(request: &ToolRequest, passed: bool, in_doubt: bool, output: String) -> ToolOutcome {
    ToolOutcome {
        call_id: request.call_id.clone(),
        tool: request.tool.clone(),
        command: None,
        passed,
        assertions: 0,
        written_paths: Vec::new(),
        in_doubt,
        output,
    }
}

fn summarize(result: &Value) -> String {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| result.to_string());
    text.chars().take(8000).collect()
}

fn sanitize_component(part: &str) -> String {
    let mut sanitized: String = part
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    sanitized
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn unique_exposed_name(
    server: &str,
    tool: &str,
    used: &mut std::collections::BTreeSet<String>,
) -> String {
    const LIMIT: usize = 64;
    let base = format!(
        "mcp__{}__{}",
        sanitize_component(server),
        sanitize_component(tool)
    );
    let mut candidate = truncate_chars(&base, LIMIT);
    let mut counter = 2usize;
    while !used.insert(candidate.clone()) {
        let suffix = format!("__{counter}");
        let prefix_len = LIMIT.saturating_sub(suffix.chars().count());
        candidate = format!("{}{}", truncate_chars(&base, prefix_len), suffix);
        counter = counter.saturating_add(1);
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_names_are_sanitized_and_unique() {
        let mut used = std::collections::BTreeSet::new();
        let first = unique_exposed_name("my server", "echo/tool", &mut used);
        assert_eq!(first, "mcp__my_server__echo_tool");
        let second = unique_exposed_name("my server", "echo/tool", &mut used);
        assert_eq!(second, "mcp__my_server__echo_tool__2");
        assert!(first.len() <= 64);
    }
}
