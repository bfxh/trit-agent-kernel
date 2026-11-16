//! 网关与 MCP 进程 supervisor。
//!
//! 这是真实进程管理：启动子进程、检测退出、退避重启；MCP 额外做 stdio JSON-RPC
//! 握手/重连，并在重连后把 session 状态层、钩子契约和 cache ledger 作为恢复包发回。
//! 不做端口级零停机承诺，只保证自动恢复路径和可审计的重启次数。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context_layers::CacheLedger;
use crate::port_bridge::{PortBridge, PortBridgeSpec};
use crate::session_migration::HookDescriptor;
use crate::state_layer::StateLayer;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub name: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub current_dir: Option<PathBuf>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let program = program.into();
        Self {
            name: program.display().to_string(),
            program,
            args: Vec::new(),
            env: Vec::new(),
            current_dir: None,
        }
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn with_current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestartPolicy {
    /// 0 表示不自动重启；首次启动不算重启次数。
    pub max_attempts: usize,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 50,
            max_backoff_ms: 1_000,
        }
    }
}

impl RestartPolicy {
    fn backoff(&self, attempt: usize) -> Duration {
        let exponent = attempt.min(20) as u32;
        let base = self.initial_backoff_ms.saturating_mul(1u64 << exponent);
        Duration::from_millis(base.min(self.max_backoff_ms))
    }
}

fn default_interval_ms() -> u64 {
    500
}

fn default_mcp_timeout_ms() -> u64 {
    30_000
}

/// 宿主/CLI 的 supervisor 声明；可直接从 JSON 解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorConfig {
    #[serde(default)]
    pub gateway: Vec<ProcessSpec>,
    #[serde(default)]
    pub mcp: Vec<ProcessSpec>,
    #[serde(default)]
    pub bridges: Vec<PortBridgeSpec>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    #[serde(default = "default_mcp_timeout_ms")]
    pub mcp_timeout_ms: u64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            gateway: Vec::new(),
            mcp: Vec::new(),
            bridges: Vec::new(),
            restart: RestartPolicy::default(),
            interval_ms: default_interval_ms(),
            mcp_timeout_ms: default_mcp_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorStatus {
    Started,
    AlreadyRunning,
    Restarted,
    GaveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpStatus {
    Connected,
    AlreadyConnected,
    Reconnected,
    GaveUp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpToolsCapability {
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpResourcesCapability {
    pub subscribe: bool,
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpPromptsCapability {
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpCapabilities {
    pub tools: Option<McpToolsCapability>,
    pub resources: Option<McpResourcesCapability>,
    pub prompts: Option<McpPromptsCapability>,
    pub logging: Option<serde_json::Value>,
    pub experimental: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpHandshake {
    pub protocol_version: String,
    pub capabilities: McpCapabilities,
    pub server_info: McpServerInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryContext {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub state: Option<StateLayer>,
    #[serde(default)]
    pub hooks: Vec<HookDescriptor>,
    #[serde(default)]
    pub cache: Option<CacheLedger>,
}

impl RecoveryContext {
    /// 状态层校验失败时拒绝恢复；没有状态层不算失败。
    pub fn validate(&self) -> Result<(), SupervisorError> {
        if let Some(state) = &self.state
            && state.validation.verdict == trit_orch::Ternary::False
        {
            return Err(SupervisorError::RecoveryRejected(
                "state layer validation failed".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SupervisorError {
    #[error("process {name}: {message}")]
    Process { name: String, message: String },
    #[error("MCP {name}: {message}")]
    Mcp { name: String, message: String },
    #[error("port bridge {name}: {message}")]
    Port { name: String, message: String },
    #[error("recovery rejected: {0}")]
    RecoveryRejected(String),
    #[error("supervisor stopped")]
    Stopped,
}

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// 子进程退出由阻塞在 OS wait 上的线程捕获，而不是调用方轮询 `try_wait`。
struct WaitedChild {
    pid: u32,
    exit: Receiver<std::io::Result<ExitStatus>>,
    monitor: Option<JoinHandle<()>>,
    terminated: bool,
}

impl WaitedChild {
    fn spawn(mut child: Child) -> Self {
        let pid = child.id();
        let (sender, exit) = mpsc::channel();
        let monitor = std::thread::spawn(move || {
            let status = child.wait();
            let _ = sender.send(status);
        });
        Self {
            pid,
            exit,
            monitor: Some(monitor),
            terminated: false,
        }
    }

    fn is_running(&mut self) -> bool {
        if self.terminated {
            return false;
        }
        match self.exit.try_recv() {
            Err(mpsc::TryRecvError::Empty) => true,
            _ => {
                self.terminated = true;
                false
            }
        }
    }

    fn terminate(&mut self) {
        if self.terminated {
            if let Some(monitor) = self.monitor.take() {
                let _ = monitor.join();
            }
            return;
        }
        if self.is_running() {
            kill_process(self.pid);
            match self.exit.recv_timeout(Duration::from_secs(3)) {
                Ok(_) | Err(RecvTimeoutError::Disconnected) => self.terminated = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if self.terminated || !self.is_running() {
            self.terminated = true;
            if let Some(monitor) = self.monitor.take() {
                let _ = monitor.join();
            }
        } else {
            // 超时未退出：放弃 join，避免阻塞调用方。
            let _ = self.monitor.take();
        }
    }
}

impl Drop for WaitedChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub struct ProcessSupervisor {
    spec: ProcessSpec,
    policy: RestartPolicy,
    child: Option<WaitedChild>,
    launched: bool,
    restarts: usize,
    stopped: bool,
}

impl ProcessSupervisor {
    pub fn new(spec: ProcessSpec, policy: RestartPolicy) -> Self {
        Self {
            spec,
            policy,
            child: None,
            launched: false,
            restarts: 0,
            stopped: false,
        }
    }

    pub fn spec(&self) -> &ProcessSpec {
        &self.spec
    }

    pub fn restarts(&self) -> usize {
        self.restarts
    }

    pub fn is_running(&mut self) -> bool {
        self.child.as_mut().is_some_and(WaitedChild::is_running)
    }

    pub fn ensure_running(&mut self) -> Result<SupervisorStatus, SupervisorError> {
        if self.stopped {
            return Err(SupervisorError::Stopped);
        }
        if self.is_running() {
            return Ok(SupervisorStatus::AlreadyRunning);
        }
        let was_started = self.launched;
        if was_started && self.restarts >= self.policy.max_attempts {
            self.child = None;
            return Ok(SupervisorStatus::GaveUp);
        }
        if was_started {
            std::thread::sleep(self.policy.backoff(self.restarts));
        }
        let mut command = Command::new(&self.spec.program);
        command.args(&self.spec.args);
        for (key, value) in &self.spec.env {
            command.env(key, value);
        }
        if let Some(dir) = &self.spec.current_dir {
            command.current_dir(dir);
        }
        let child = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| SupervisorError::Process {
                name: self.spec.name.clone(),
                message: error.to_string(),
            })?;
        self.child = Some(WaitedChild::spawn(child));
        self.launched = true;
        if was_started {
            self.restarts += 1;
            Ok(SupervisorStatus::Restarted)
        } else {
            Ok(SupervisorStatus::Started)
        }
    }

    pub fn stop(&mut self) {
        self.stopped = true;
        if let Some(mut child) = self.child.take() {
            child.terminate();
        }
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

struct McpConnection {
    name: String,
    child: WaitedChild,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    handshake: McpHandshake,
    tools: Vec<McpTool>,
}

impl McpConnection {
    fn open(spec: &ProcessSpec, timeout: Duration) -> Result<Self, SupervisorError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        if let Some(dir) = &spec.current_dir {
            command.current_dir(dir);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| SupervisorError::Mcp {
                name: spec.name.clone(),
                message: format!("spawn: {error}"),
            })?;
        let stdin = child.stdin.take().ok_or_else(|| SupervisorError::Mcp {
            name: spec.name.clone(),
            message: "missing stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| SupervisorError::Mcp {
            name: spec.name.clone(),
            message: "missing stdout".into(),
        })?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut connection = Self {
            name: spec.name.clone(),
            child: WaitedChild::spawn(child),
            stdin,
            lines,
            next_id: 1,
            handshake: McpHandshake::default(),
            tools: Vec::new(),
        };
        let handshake = connection.negotiate(timeout)?;
        connection.handshake = handshake;
        if connection.handshake.capabilities.tools.is_some() {
            connection.tools = connection.discover_tools(timeout)?;
        }
        Ok(connection)
    }

    fn is_alive(&mut self) -> bool {
        self.child.is_running()
    }

    fn write_json(&mut self, value: &serde_json::Value) -> Result<(), SupervisorError> {
        let line = serde_json::to_string(value).map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("encode: {error}"),
        })?;
        writeln!(self.stdin, "{line}").map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("write: {error}"),
        })?;
        self.stdin.flush().map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("flush: {error}"),
        })
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), SupervisorError> {
        self.write_json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, SupervisorError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        loop {
            let line = match self.lines.recv_timeout(timeout) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(SupervisorError::Mcp {
                        name: self.name.clone(),
                        message: format!("timeout waiting for {method} id {id}"),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(SupervisorError::Mcp {
                        name: self.name.clone(),
                        message: "stdout closed".into(),
                    });
                }
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(SupervisorError::Mcp {
                    name: self.name.clone(),
                    message: format!("{method}: {error}"),
                });
            }
            return Ok(value
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    fn negotiate(&mut self, timeout: Duration) -> Result<McpHandshake, SupervisorError> {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "trit-agent", "version": "0.1.0"}
            }),
            timeout,
        )?;
        let mut handshake = serde_json::from_value::<McpHandshake>(result).map_err(|error| {
            SupervisorError::Mcp {
                name: self.name.clone(),
                message: format!("initialize result: {error}"),
            }
        })?;
        if handshake.protocol_version.is_empty() {
            handshake.protocol_version = MCP_PROTOCOL_VERSION.into();
        } else if !MCP_SUPPORTED_VERSIONS.contains(&handshake.protocol_version.as_str()) {
            return Err(SupervisorError::Mcp {
                name: self.name.clone(),
                message: format!(
                    "unsupported protocol version {}",
                    handshake.protocol_version
                ),
            });
        }
        self.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(handshake)
    }

    fn discover_tools(&mut self, timeout: Duration) -> Result<Vec<McpTool>, SupervisorError> {
        #[derive(serde::Deserialize)]
        struct ToolsPage {
            #[serde(default)]
            tools: Vec<McpTool>,
            #[serde(default, rename = "nextCursor")]
            next_cursor: Option<String>,
        }

        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            let params = match cursor.as_ref() {
                Some(cursor) => serde_json::json!({"cursor": cursor}),
                None => serde_json::json!({}),
            };
            let result = self.request("tools/list", params, timeout)?;
            let page: ToolsPage =
                serde_json::from_value(result).map_err(|error| SupervisorError::Mcp {
                    name: self.name.clone(),
                    message: format!("tools/list result: {error}"),
                })?;
            tools.extend(page.tools);
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(tools);
            };
            if !seen.insert(next.clone()) {
                return Err(SupervisorError::Mcp {
                    name: self.name.clone(),
                    message: format!("tools/list repeated cursor {next}"),
                });
            }
        }
        Err(SupervisorError::Mcp {
            name: self.name.clone(),
            message: "tools/list exceeded 256 pages".into(),
        })
    }

    fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, SupervisorError> {
        self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
            timeout,
        )
    }

    fn send_recovery(&mut self, recovery: &RecoveryContext) -> Result<(), SupervisorError> {
        let params = serde_json::to_value(recovery).map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("recovery encode: {error}"),
        })?;
        self.notify("notifications/trit/recovered", params)
    }

    fn stop(&mut self) {
        self.child.terminate();
    }
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct McpSupervisor {
    spec: ProcessSpec,
    policy: RestartPolicy,
    connection: Option<McpConnection>,
    handshake: Option<McpHandshake>,
    tools: Vec<McpTool>,
    launched: bool,
    restarts: usize,
    stopped: bool,
    timeout: Duration,
}

impl McpSupervisor {
    pub fn new(spec: ProcessSpec, policy: RestartPolicy) -> Self {
        Self {
            spec,
            policy,
            connection: None,
            handshake: None,
            tools: Vec::new(),
            launched: false,
            restarts: 0,
            stopped: false,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn restarts(&self) -> usize {
        self.restarts
    }

    pub fn ensure_connected(
        &mut self,
        recovery: &RecoveryContext,
    ) -> Result<McpStatus, SupervisorError> {
        if self.stopped {
            return Err(SupervisorError::Stopped);
        }
        if self
            .connection
            .as_mut()
            .is_some_and(McpConnection::is_alive)
        {
            return Ok(McpStatus::AlreadyConnected);
        }
        recovery.validate()?;
        let was_connected = self.launched;
        if was_connected && self.restarts >= self.policy.max_attempts {
            self.connection = None;
            return Ok(McpStatus::GaveUp);
        }
        self.connection = None;
        if was_connected {
            std::thread::sleep(self.policy.backoff(self.restarts));
        }
        let mut connection = McpConnection::open(&self.spec, self.timeout)?;
        connection.send_recovery(recovery)?;
        self.handshake = Some(connection.handshake.clone());
        self.tools = connection.tools.clone();
        self.connection = Some(connection);
        self.launched = true;
        if was_connected {
            self.restarts += 1;
            Ok(McpStatus::Reconnected)
        } else {
            Ok(McpStatus::Connected)
        }
    }

    pub fn handshake(&self) -> Option<&McpHandshake> {
        self.handshake.as_ref()
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// 重新拉取 tools/list；连接已断开时先按恢复包重连。
    pub fn refresh_tools(
        &mut self,
        recovery: &RecoveryContext,
    ) -> Result<&[McpTool], SupervisorError> {
        self.ensure_connected(recovery)?;
        let timeout = self.timeout;
        let connection = self.connection.as_mut().ok_or(SupervisorError::Stopped)?;
        let tools = connection.discover_tools(timeout)?;
        connection.tools = tools.clone();
        self.tools = tools;
        Ok(&self.tools)
    }

    /// 调用 MCP 工具；断线时先重连并重新握手/发现工具。
    pub fn call_tool(
        &mut self,
        recovery: &RecoveryContext,
        name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, SupervisorError> {
        self.ensure_connected(recovery)?;
        let connection = self.connection.as_mut().ok_or(SupervisorError::Stopped)?;
        connection.call_tool(name, arguments, timeout)
    }

    /// 模拟外部崩溃：杀掉子进程但保留“曾经连接过”的事实，下一次 ensure 会重启。
    pub fn kill_connection(&mut self) {
        if let Some(connection) = self.connection.as_mut() {
            connection.stop();
        }
    }

    /// 显式断开并重置重连计数；下一次 ensure 会走首次连接路径。
    pub fn drop_connection(&mut self) {
        self.connection = None;
        self.launched = false;
        self.restarts = 0;
    }

    pub fn stop(&mut self) {
        self.stopped = true;
        if let Some(mut connection) = self.connection.take() {
            connection.stop();
        }
    }
}

impl Drop for McpSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 网关进程直接复用进程 supervisor；具体协议由网关自己负责。
pub type GatewaySupervisor = ProcessSupervisor;

/// 后台 supervisor：按 interval 自动检查并重启/重连，直到 stop 被调用。
pub struct SupervisorLoop {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SupervisorLoop {
    pub fn spawn_process(supervisor: Arc<Mutex<ProcessSupervisor>>, interval: Duration) -> Self {
        Self::spawn(interval, move || {
            if let Ok(mut supervisor) = supervisor.lock() {
                let _ = supervisor.ensure_running();
            }
        })
    }

    pub fn spawn_mcp(
        supervisor: Arc<Mutex<McpSupervisor>>,
        recovery: RecoveryContext,
        interval: Duration,
    ) -> Self {
        Self::spawn_mcp_shared(supervisor, Arc::new(Mutex::new(recovery)), interval)
    }

    /// 恢复包可在运行中更新；下一次重连使用最新快照。
    pub fn spawn_mcp_shared(
        supervisor: Arc<Mutex<McpSupervisor>>,
        recovery: Arc<Mutex<RecoveryContext>>,
        interval: Duration,
    ) -> Self {
        Self::spawn(interval, move || {
            let recovery = match recovery.lock() {
                Ok(recovery) => recovery.clone(),
                Err(_) => return,
            };
            if let Ok(mut supervisor) = supervisor.lock() {
                let _ = supervisor.ensure_connected(&recovery);
            }
        })
    }

    fn spawn(interval: Duration, mut tick: impl FnMut() + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                tick();
                std::thread::sleep(interval);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SupervisorLoop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 一组 gateway/MCP sidecar：启动一次，后台持续重启/重连，drop 时全部停止。
pub struct SupervisorRuntime {
    bridges: Vec<PortBridge>,
    gateways: Vec<Arc<Mutex<ProcessSupervisor>>>,
    mcps: Vec<Arc<Mutex<McpSupervisor>>>,
    recovery: Arc<Mutex<RecoveryContext>>,
    loops: Vec<SupervisorLoop>,
}

impl SupervisorRuntime {
    pub fn start(
        config: &SupervisorConfig,
        recovery: RecoveryContext,
    ) -> Result<Self, SupervisorError> {
        recovery.validate()?;
        let interval = Duration::from_millis(config.interval_ms.max(1));
        let timeout = Duration::from_millis(config.mcp_timeout_ms.max(1));
        let mut runtime = Self {
            bridges: Vec::new(),
            gateways: Vec::new(),
            mcps: Vec::new(),
            recovery: Arc::new(Mutex::new(recovery)),
            loops: Vec::new(),
        };
        for spec in &config.bridges {
            let bridge = PortBridge::start(spec).map_err(|error| SupervisorError::Port {
                name: spec.name.clone(),
                message: error.to_string(),
            })?;
            runtime.bridges.push(bridge);
        }
        for spec in &config.gateway {
            let supervisor = Arc::new(Mutex::new(ProcessSupervisor::new(
                spec.clone(),
                config.restart,
            )));
            supervisor
                .lock()
                .map_err(|_| SupervisorError::Process {
                    name: spec.name.clone(),
                    message: "supervisor lock is poisoned".into(),
                })?
                .ensure_running()?;
            runtime.loops.push(SupervisorLoop::spawn_process(
                Arc::clone(&supervisor),
                interval,
            ));
            runtime.gateways.push(supervisor);
        }
        for spec in &config.mcp {
            let supervisor = Arc::new(Mutex::new(
                McpSupervisor::new(spec.clone(), config.restart).with_timeout(timeout),
            ));
            let initial = runtime
                .recovery
                .lock()
                .map_err(|_| SupervisorError::Mcp {
                    name: spec.name.clone(),
                    message: "recovery lock is poisoned".into(),
                })?
                .clone();
            supervisor
                .lock()
                .map_err(|_| SupervisorError::Mcp {
                    name: spec.name.clone(),
                    message: "supervisor lock is poisoned".into(),
                })?
                .ensure_connected(&initial)?;
            runtime.loops.push(SupervisorLoop::spawn_mcp_shared(
                Arc::clone(&supervisor),
                Arc::clone(&runtime.recovery),
                interval,
            ));
            runtime.mcps.push(supervisor);
        }
        Ok(runtime)
    }

    pub fn gateway_count(&self) -> usize {
        self.gateways.len()
    }

    pub fn mcp_count(&self) -> usize {
        self.mcps.len()
    }

    pub fn bridge_count(&self) -> usize {
        self.bridges.len()
    }

    /// 更新下一次重连使用的恢复包；状态层校验失败时拒绝。
    pub fn refresh_recovery(&self, recovery: RecoveryContext) -> Result<(), SupervisorError> {
        recovery.validate()?;
        *self.recovery.lock().map_err(|_| SupervisorError::Mcp {
            name: "runtime".into(),
            message: "recovery lock is poisoned".into(),
        })? = recovery;
        Ok(())
    }

    pub fn stop(&mut self) {
        while let Some(mut loop_handle) = self.loops.pop() {
            loop_handle.stop();
        }
        for supervisor in &self.gateways {
            if let Ok(mut supervisor) = supervisor.lock() {
                supervisor.stop();
            }
        }
        for supervisor in &self.mcps {
            if let Ok(mut supervisor) = supervisor.lock() {
                supervisor.stop();
            }
        }
        while let Some(mut bridge) = self.bridges.pop() {
            bridge.stop();
        }
    }
}

impl Drop for SupervisorRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit_spec() -> ProcessSpec {
        if cfg!(windows) {
            ProcessSpec::new("cmd").with_args(["/C", "exit 0"])
        } else {
            ProcessSpec::new("sh").with_args(["-c", "exit 0"])
        }
    }

    fn wait_exit(supervisor: &mut ProcessSupervisor) {
        for _ in 0..100 {
            if !supervisor.is_running() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("child did not exit");
    }

    #[test]
    fn waited_child_blocks_on_os_wait_and_can_be_terminated() {
        let child = if cfg!(windows) {
            Command::new("powershell")
                .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        } else {
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut waited = WaitedChild::spawn(child);
        assert!(waited.is_running());
        waited.terminate();
        assert!(!waited.is_running());
    }

    #[test]
    fn process_supervisor_restarts_and_stops_at_the_limit() {
        let mut supervisor = ProcessSupervisor::new(
            exit_spec(),
            RestartPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        );
        assert_eq!(
            supervisor.ensure_running().unwrap(),
            SupervisorStatus::Started
        );
        wait_exit(&mut supervisor);
        assert_eq!(
            supervisor.ensure_running().unwrap(),
            SupervisorStatus::Restarted
        );
        wait_exit(&mut supervisor);
        assert_eq!(
            supervisor.ensure_running().unwrap(),
            SupervisorStatus::GaveUp
        );
        assert_eq!(
            supervisor.ensure_running().unwrap(),
            SupervisorStatus::GaveUp,
            "restart budget exhaustion must be permanent"
        );
        supervisor.stop();
    }

    #[test]
    fn recovery_rejects_invalid_state() {
        let mut state = StateLayer::default();
        state.validation.verdict = trit_orch::Ternary::False;
        let recovery = RecoveryContext {
            state: Some(state),
            ..RecoveryContext::default()
        };
        assert!(matches!(
            recovery.validate(),
            Err(SupervisorError::RecoveryRejected(_))
        ));
    }

    #[test]
    fn mcp_supervisor_rejects_unsupported_protocol_version() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-protocol-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-stub.log");
        let spec = mcp_stub_spec(&dir, &log).with_env("TRIT_MCP_STUB_PROTOCOL", "9999-01-01");
        let mut supervisor = McpSupervisor::new(
            spec,
            RestartPolicy {
                max_attempts: 0,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        );
        let error = supervisor
            .ensure_connected(&RecoveryContext::default())
            .unwrap_err();
        assert!(
            matches!(&error, SupervisorError::Mcp { message, .. } if message.contains("unsupported protocol version")),
            "{error}"
        );
        supervisor.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MCP stub 会启动真实 PowerShell/Node 进程，串行化避免 CI 冷启动互相挤压。
    fn serialize_mcp_test() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mcp_stub_spec(dir: &std::path::Path, log: &std::path::Path) -> ProcessSpec {
        if cfg!(windows) {
            let script = dir.join("mcp-stub.ps1");
            std::fs::write(
                &script,
                r#"$log = $env:TRIT_MCP_STUB_LOG
$proto = $env:TRIT_MCP_STUB_PROTOCOL
if (-not $proto) { $proto = '2024-11-05' }
Write-Output ('{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"' + $proto + '","capabilities":{},"serverInfo":{"name":"stub","version":"0"}}}')
while ($null -ne ($line = [Console]::In.ReadLine())) {
  [IO.File]::AppendAllText($log, $line + "`n")
}
"#,
            )
            .unwrap();
            ProcessSpec::new("powershell")
                .with_args([
                    "-NoProfile".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-File".to_string(),
                    script.display().to_string(),
                ])
                .with_env("TRIT_MCP_STUB_LOG", log.display().to_string())
        } else {
            let script = dir.join("mcp-stub.sh");
            std::fs::write(
                &script,
                r#"#!/bin/sh
log="$TRIT_MCP_STUB_LOG"
proto="$TRIT_MCP_STUB_PROTOCOL"
[ -z "$proto" ] && proto="2024-11-05"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"'"$proto"'","capabilities":{},"serverInfo":{"name":"stub","version":"0"}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> "$log"; done
"#,
            )
            .unwrap();
            ProcessSpec::new("sh")
                .with_args([script.display().to_string()])
                .with_env("TRIT_MCP_STUB_LOG", log.display().to_string())
        }
    }

    fn mcp_tool_stub_spec(dir: &std::path::Path, log: &std::path::Path) -> ProcessSpec {
        if cfg!(windows) {
            let script = dir.join("mcp-tool-stub.ps1");
            std::fs::write(
                &script,
                r#"$log = $env:TRIT_MCP_STUB_LOG
while ($null -ne ($line = [Console]::In.ReadLine())) {
  [IO.File]::AppendAllText($log, $line + "`n")
  $id = 0
  if ($line -match '"id":([0-9]+)') { $id = $Matches[1] }
  if ($line -like '*"method":"initialize"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":true},"resources":{"subscribe":true,"listChanged":false},"prompts":{"listChanged":true},"logging":{},"completions":{}},"serverInfo":{"name":"tool-stub","version":"1.2"}}}')
  } elseif ($line -like '*"method":"tools/list"*') {
    if ($line -like '*"cursor"*') {
      Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"echo2","inputSchema":{"type":"object"}}]}}')
    } else {
      Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"page2"}}')
    }
  } elseif ($line -like '*"method":"tools/call"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"ok"}]}}')
  }
}
"#,
            )
            .unwrap();
            ProcessSpec::new("powershell")
                .with_args([
                    "-NoProfile".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-File".to_string(),
                    script.display().to_string(),
                ])
                .with_env("TRIT_MCP_STUB_LOG", log.display().to_string())
        } else {
            let script = dir.join("mcp-tool-stub.sh");
            std::fs::write(
                &script,
                r#"#!/bin/sh
log="$TRIT_MCP_STUB_LOG"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":true},"resources":{"subscribe":true,"listChanged":false},"prompts":{"listChanged":true},"logging":{},"completions":{}},"serverInfo":{"name":"tool-stub","version":"1.2"}}}' ;;
    *'"method":"tools/list"'*)
      case "$line" in
        *'"cursor"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"echo2","inputSchema":{"type":"object"}}]}}' ;;
        *) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"page2"}}' ;;
      esac ;;
    *'"method":"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"ok"}]}}' ;;
  esac
done
"#,
            )
            .unwrap();
            ProcessSpec::new("sh")
                .with_args([script.display().to_string()])
                .with_env("TRIT_MCP_STUB_LOG", log.display().to_string())
        }
    }

    #[test]
    fn mcp_supervisor_negotiates_capabilities_and_calls_tools() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-tools-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-tool-stub.log");
        let mut supervisor = McpSupervisor::new(
            mcp_tool_stub_spec(&dir, &log),
            RestartPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        );
        let recovery = RecoveryContext::default();
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::Connected
        );

        let handshake = supervisor.handshake().expect("handshake");
        assert_eq!(handshake.protocol_version, "2024-11-05");
        assert_eq!(handshake.server_info.name, "tool-stub");
        assert_eq!(handshake.server_info.version, "1.2");
        assert!(
            handshake
                .capabilities
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed)
        );
        assert!(
            handshake
                .capabilities
                .resources
                .as_ref()
                .is_some_and(|resources| resources.subscribe)
        );
        assert!(
            handshake
                .capabilities
                .prompts
                .as_ref()
                .is_some_and(|prompts| prompts.list_changed)
        );
        assert!(handshake.capabilities.logging.is_some());
        assert!(handshake.capabilities.extra.contains_key("completions"));

        let tools = supervisor.tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("Echo"));
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(tools[1].name, "echo2");

        let result = supervisor
            .call_tool(
                &recovery,
                "echo",
                serde_json::json!({"text": "hi"}),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(result["content"][0]["text"], "ok");

        supervisor.kill_connection();
        let result = supervisor
            .call_tool(
                &recovery,
                "echo2",
                serde_json::json!({}),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(result["content"][0]["text"], "ok");
        assert_eq!(supervisor.restarts(), 1);

        let refreshed = supervisor.refresh_tools(&recovery).unwrap();
        assert_eq!(refreshed.len(), 2);
        supervisor.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_supervisor_handshakes_reconnects_and_sends_recovery() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-supervisor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-stub.log");
        let spec = mcp_stub_spec(&dir, &log);
        let mut supervisor = McpSupervisor::new(
            spec,
            RestartPolicy {
                max_attempts: 2,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        )
        .with_timeout(Duration::from_secs(30));
        let recovery = RecoveryContext {
            session_id: Some("session-1".into()),
            state: Some(StateLayer::default()),
            hooks: vec![HookDescriptor {
                id: "pre-tool".into(),
                event: "pre_tool".into(),
                contract_hash: "h1".into(),
                enabled: true,
            }],
            cache: Some(CacheLedger::default()),
        };
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::Connected
        );
        supervisor.kill_connection();
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::Reconnected
        );
        assert_eq!(supervisor.restarts(), 1);
        let mut log_text = String::new();
        for _ in 0..100 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.contains("notifications/trit/recovered") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(log_text.contains("notifications/trit/recovered"));
        assert!(log_text.contains("\"state\""));
        assert!(log_text.contains("\"hooks\""));
        assert!(log_text.contains("\"cache\""));
        supervisor.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_supervisor_gives_up_until_explicit_reset() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-giveup-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-stub.log");
        let mut supervisor = McpSupervisor::new(
            mcp_stub_spec(&dir, &log),
            RestartPolicy {
                max_attempts: 0,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        )
        .with_timeout(Duration::from_secs(30));
        let recovery = RecoveryContext::default();
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::Connected
        );
        supervisor.kill_connection();
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::GaveUp
        );
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::GaveUp,
            "restart budget exhaustion must be permanent"
        );
        supervisor.drop_connection();
        assert_eq!(
            supervisor.ensure_connected(&recovery).unwrap(),
            McpStatus::Connected
        );
        supervisor.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supervisor_loop_reconnects_with_latest_recovery_snapshot() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-latest-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-stub.log");
        let supervisor = Arc::new(Mutex::new(
            McpSupervisor::new(
                mcp_stub_spec(&dir, &log),
                RestartPolicy {
                    max_attempts: 3,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
            )
            .with_timeout(Duration::from_secs(30)),
        ));
        let recovery = Arc::new(Mutex::new(RecoveryContext {
            session_id: Some("session-old".into()),
            ..RecoveryContext::default()
        }));
        let mut loop_handle = SupervisorLoop::spawn_mcp_shared(
            Arc::clone(&supervisor),
            Arc::clone(&recovery),
            Duration::from_millis(20),
        );
        let mut log_text = String::new();
        for _ in 0..3000 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.contains("session-old") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(log_text.contains("session-old"));

        *recovery.lock().unwrap() = RecoveryContext {
            session_id: Some("session-new".into()),
            ..RecoveryContext::default()
        };
        supervisor.lock().unwrap().kill_connection();
        for _ in 0..3000 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.contains("session-new") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(log_text.contains("session-new"));
        loop_handle.stop();
        supervisor.lock().unwrap().stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supervisor_loop_restarts_without_explicit_ensure() {
        let supervisor = Arc::new(Mutex::new(ProcessSupervisor::new(
            exit_spec(),
            RestartPolicy {
                max_attempts: 2,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        )));
        let mut loop_handle =
            SupervisorLoop::spawn_process(Arc::clone(&supervisor), Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(300));
        loop_handle.stop();
        assert!(supervisor.lock().unwrap().restarts() >= 1);
    }

    #[test]
    fn gateway_alias_uses_process_supervisor() {
        let mut gateway: GatewaySupervisor = ProcessSupervisor::new(
            exit_spec(),
            RestartPolicy {
                max_attempts: 0,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        );
        assert_eq!(gateway.ensure_running().unwrap(), SupervisorStatus::Started);
        gateway.stop();
    }

    #[test]
    fn supervisor_config_parses_partial_json() {
        let config: SupervisorConfig = serde_json::from_str(
            r#"{
                "gateway": [{"name": "gw", "program": "gateway-bin"}],
                "bridges": [{"name": "front", "listen": "127.0.0.1:0", "target": "127.0.0.1:9"}],
                "restart": {"max_attempts": 1}
            }"#,
        )
        .unwrap();
        assert_eq!(config.gateway.len(), 1);
        assert_eq!(config.gateway[0].name, "gw");
        assert!(config.mcp.is_empty());
        assert_eq!(config.bridges.len(), 1);
        assert_eq!(config.bridges[0].connect_timeout_ms, 30_000);
        assert_eq!(config.bridges[0].retry_ms, 50);
        assert_eq!(config.restart.max_attempts, 1);
        assert_eq!(config.restart.max_backoff_ms, 1_000);
        assert_eq!(config.interval_ms, 500);
        assert_eq!(config.mcp_timeout_ms, 30_000);
    }

    #[test]
    fn supervisor_runtime_starts_sidecars_and_refreshes_recovery() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-supervisor-runtime-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-stub.log");
        let config = SupervisorConfig {
            gateway: vec![exit_spec()],
            mcp: vec![mcp_stub_spec(&dir, &log)],
            bridges: vec![PortBridgeSpec::new("front", "127.0.0.1:0", "127.0.0.1:9")],
            restart: RestartPolicy {
                max_attempts: 2,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            interval_ms: 10,
            mcp_timeout_ms: 30_000,
        };
        let mut runtime = SupervisorRuntime::start(
            &config,
            RecoveryContext {
                session_id: Some("session-1".into()),
                state: Some(StateLayer::default()),
                ..RecoveryContext::default()
            },
        )
        .unwrap();
        assert_eq!(runtime.gateway_count(), 1);
        assert_eq!(runtime.mcp_count(), 1);
        assert_eq!(runtime.bridge_count(), 1);

        let mut log_text = String::new();
        for _ in 0..100 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.contains("session-1") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(log_text.contains("session-1"));

        runtime
            .refresh_recovery(RecoveryContext {
                session_id: Some("session-2".into()),
                ..RecoveryContext::default()
            })
            .unwrap();
        let mut invalid = RecoveryContext::default();
        let mut state = StateLayer::default();
        state.validation.verdict = trit_orch::Ternary::False;
        invalid.state = Some(state);
        assert!(runtime.refresh_recovery(invalid).is_err());

        runtime.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
