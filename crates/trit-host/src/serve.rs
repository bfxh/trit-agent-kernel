//! Serve 的准入与路由决策（B7.8）。
//!
//! 这一层是**纯决策**，不打开 socket、不发响应；传输由宿主实现。它回答三个问题：
//!
//! 1. 配置是否安全：只允许 loopback 绑定；token 不得为空白。
//! 2. 请求是否可信：`Host` 必须是 loopback 或白名单 origin；`Origin` 必须同源或白名单；
//!    配置了 token 时必须带正确的 `Authorization: Bearer`。
//! 3. 路由是否已知：固定 golden 路由表；已知路径配错方法返回 405，未知路径 404。
//!
//! 错误用 [`ServeError`] 表达，`status` + `code` 是机器可判的契约。

use std::net::SocketAddr;

/// Serve 配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    /// 绑定地址；必须是 loopback。
    pub bind: SocketAddr,
    /// 可选 bearer token；配置后所有请求必须携带。
    pub auth_token: Option<String>,
    /// 允许的跨源 origin（完整 `scheme://host:port`）。
    pub allowed_origins: Vec<String>,
}

/// 从请求里摘出的关键头。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeHeaders {
    pub host: Option<String>,
    pub origin: Option<String>,
    pub authorization: Option<String>,
}

/// 机器可判的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ServeError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "bad_request", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }

    pub fn method_not_allowed(method: &str, path: &str) -> Self {
        Self::new(
            405,
            "method_not_allowed",
            format!("method {method} not allowed for {path}"),
        )
    }
}

/// golden 路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `GET /health`。
    Health,
    /// `GET /v1/session/status`。
    SessionStatus,
    /// `POST /v1/turn`。
    Turn,
    /// `GET /v1/events`（SSE）。
    Events,
}

impl ServeConfig {
    /// 配置校验：只允许 loopback；token 不得空白。
    pub fn validate(&self) -> Result<(), ServeError> {
        if !self.bind.ip().is_loopback() {
            return Err(ServeError::forbidden(format!(
                "serve bind must be loopback, got {}",
                self.bind.ip()
            )));
        }
        if self
            .auth_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            return Err(ServeError::forbidden("auth token is blank"));
        }
        Ok(())
    }
}

/// 请求准入：配置 → Host → Origin → Authorization。
///
/// 顺序固定，先便宜的检查；任何一步失败都直接返回对应错误码。
pub fn authorize(config: &ServeConfig, headers: &ServeHeaders) -> Result<(), ServeError> {
    config.validate()?;

    let host = headers
        .host
        .as_deref()
        .ok_or_else(|| ServeError::bad_request("missing Host header"))?;
    if !host_allowed(host, config) {
        return Err(ServeError::forbidden(format!("host not allowed: {host}")));
    }

    if let Some(origin) = headers
        .origin
        .as_deref()
        .filter(|origin| !origin_allowed(origin, host, config))
    {
        return Err(ServeError::forbidden(format!(
            "origin not allowed: {origin}"
        )));
    }

    if let Some(token) = &config.auth_token {
        let header = headers.authorization.as_deref().unwrap_or_default();
        if !bearer_matches(header, token) {
            return Err(ServeError::unauthorized("invalid or missing bearer token"));
        }
    }
    Ok(())
}

/// 路由决策：已知路径 + 方法 → [`Route`]；方法错误 405，未知路径 404。
pub fn route(method: &str, path: &str) -> Result<Route, ServeError> {
    let path = path.split('?').next().unwrap_or(path);
    let method = method.to_ascii_uppercase();
    match (method.as_str(), path) {
        ("GET", "/health") => Ok(Route::Health),
        ("GET", "/v1/session/status") => Ok(Route::SessionStatus),
        ("POST", "/v1/turn") => Ok(Route::Turn),
        ("GET", "/v1/events") => Ok(Route::Events),
        (method, known) if is_known_path(known) => {
            Err(ServeError::method_not_allowed(method, known))
        }
        (_, _) => Err(ServeError::not_found(format!("unknown path: {path}"))),
    }
}

fn is_known_path(path: &str) -> bool {
    matches!(
        path,
        "/health" | "/v1/session/status" | "/v1/turn" | "/v1/events"
    )
}

fn host_allowed(host: &str, config: &ServeConfig) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let name = strip_port(&host);
    if is_loopback_name(name) {
        return true;
    }
    config
        .allowed_origins
        .iter()
        .any(|origin| origin_host(origin) == name)
}

fn origin_allowed(origin: &str, host: &str, config: &ServeConfig) -> bool {
    let origin = origin.trim().to_ascii_lowercase();
    if origin.is_empty() || origin == "null" {
        return false;
    }
    if origin_host(&origin) == strip_port(&host.trim().to_ascii_lowercase()) {
        return true;
    }
    config.allowed_origins.iter().any(|allowed| {
        allowed
            .trim()
            .trim_end_matches('/')
            .eq_ignore_ascii_case(&origin)
    })
}

fn bearer_matches(header: &str, token: &str) -> bool {
    header
        .strip_prefix("Bearer ")
        .map(str::trim)
        .is_some_and(|value| value == token)
}

fn is_loopback_name(name: &str) -> bool {
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

/// 去掉端口；IPv6 方括号保留在 `[::1]` 里，转为 `::1`。
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    host.split(':').next().unwrap_or("")
}

/// 从 `scheme://host:port` 取 host。
fn origin_host(origin: &str) -> &str {
    let rest = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    strip_port(rest.split('/').next().unwrap_or(""))
}
