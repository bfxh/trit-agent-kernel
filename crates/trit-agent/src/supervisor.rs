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

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
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

fn default_roots_watch_debounce_ms() -> u64 {
    1_000
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
    pub roots: Vec<McpRoot>,
    /// 监视这些目录的直接子目录，自动生成 roots 并发送 roots/list_changed。
    #[serde(default)]
    pub roots_watch: Vec<std::path::PathBuf>,
    #[serde(default = "default_roots_watch_debounce_ms")]
    pub roots_watch_debounce_ms: u64,
    #[serde(default)]
    pub log_level: Option<McpLogLevel>,
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
            roots: Vec::new(),
            roots_watch: Vec::new(),
            roots_watch_debounce_ms: default_roots_watch_debounce_ms(),
            log_level: None,
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
#[serde(default, rename_all = "camelCase")]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpResourceRead {
    pub contents: Vec<McpResourceContent>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpPromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpPrompt {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<McpPromptArgument>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRoot {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpLogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

/// 服务端发起的 sampling/createMessage；返回完整的 assistant message。
pub trait SamplingHandler: Send + Sync {
    fn create_message(&self, request: serde_json::Value) -> Result<serde_json::Value, String>;
}

impl<F> SamplingHandler for F
where
    F: Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync,
{
    fn create_message(&self, request: serde_json::Value) -> Result<serde_json::Value, String> {
        self(request)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpNotification {
    ToolsListChanged,
    PromptsListChanged,
    ResourcesListChanged,
    ResourceUpdated {
        uri: String,
    },
    LoggingMessage {
        level: String,
        logger: Option<String>,
        data: serde_json::Value,
    },
    Other {
        method: String,
    },
}

#[derive(Default)]
struct McpNotificationState {
    tools_changed: AtomicBool,
    prompts_changed: AtomicBool,
    resources_changed: AtomicBool,
    contents_changed: AtomicBool,
    queue: Mutex<Vec<McpNotification>>,
}

impl McpNotificationState {
    fn record(&self, method: &str, params: &serde_json::Value) {
        let notification = match method {
            "notifications/tools/list_changed" => {
                self.tools_changed.store(true, Ordering::Relaxed);
                McpNotification::ToolsListChanged
            }
            "notifications/prompts/list_changed" => {
                self.prompts_changed.store(true, Ordering::Relaxed);
                McpNotification::PromptsListChanged
            }
            "notifications/resources/list_changed" => {
                self.resources_changed.store(true, Ordering::Relaxed);
                McpNotification::ResourcesListChanged
            }
            "notifications/resources/updated" => {
                self.contents_changed.store(true, Ordering::Relaxed);
                McpNotification::ResourceUpdated {
                    uri: params
                        .get("uri")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }
            }
            "notifications/message" => McpNotification::LoggingMessage {
                level: params
                    .get("level")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("info")
                    .to_owned(),
                logger: params
                    .get("logger")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                data: params
                    .get("data")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            },
            other => McpNotification::Other {
                method: other.to_owned(),
            },
        };
        if let Ok(mut queue) = self.queue.lock() {
            if queue.len() >= 1024 {
                queue.remove(0);
            }
            queue.push(notification);
        }
    }

    fn drain(&self) -> Vec<McpNotification> {
        self.queue
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }

    fn clear_list_flags(&self) {
        self.tools_changed.store(false, Ordering::Relaxed);
        self.prompts_changed.store(false, Ordering::Relaxed);
        self.resources_changed.store(false, Ordering::Relaxed);
        self.contents_changed.store(false, Ordering::Relaxed);
    }

    fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::Relaxed)
    }

    fn take_prompts_changed(&self) -> bool {
        self.prompts_changed.swap(false, Ordering::Relaxed)
    }

    fn take_resources_changed(&self) -> bool {
        self.resources_changed.swap(false, Ordering::Relaxed)
    }

    fn take_contents_changed(&self) -> bool {
        self.contents_changed.swap(false, Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct McpContextSnapshot {
    pub server: String,
    pub roots: Vec<McpRoot>,
    pub tools: Vec<McpTool>,
    pub resources: Vec<McpResource>,
    pub resource_contents: std::collections::BTreeMap<String, String>,
    pub prompts: Vec<McpPrompt>,
}

type McpContextRegistry = Arc<Mutex<std::collections::BTreeMap<String, McpContextSnapshot>>>;

#[derive(Clone, Default)]
struct McpFeatures {
    sampling: Option<Arc<dyn SamplingHandler>>,
    roots: Arc<Mutex<Vec<McpRoot>>>,
    server: String,
    context_registry: Arc<Mutex<std::collections::BTreeMap<String, McpContextSnapshot>>>,
}

impl McpFeatures {
    fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            ..Self::default()
        }
    }

    fn capabilities(&self) -> serde_json::Value {
        let mut capabilities = serde_json::json!({
            "roots": {"listChanged": true}
        });
        if self.sampling.is_some() {
            capabilities["sampling"] = serde_json::json!({});
        }
        capabilities
    }
}

fn server_request_response(
    features: &McpFeatures,
    request: &serde_json::Value,
) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let params = request
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let result = match method {
        "roots/list" => {
            let roots = features
                .roots
                .lock()
                .map(|roots| roots.clone())
                .unwrap_or_default();
            Ok(serde_json::json!({"roots": roots}))
        }
        "sampling/createMessage" => match &features.sampling {
            Some(handler) => {
                let mut params = params;
                inject_sampling_context(features, &mut params);
                handler
                    .create_message(params)
                    .map_err(|message| (-32000, message))
            }
            None => Err((-32601, "sampling/createMessage is not supported".to_owned())),
        },
        "ping" => Ok(serde_json::json!({})),
        other => Err((-32601, format!("server request method not found: {other}"))),
    };
    match result {
        Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    }
}

fn inject_sampling_context(features: &McpFeatures, params: &mut serde_json::Value) {
    let include = params
        .get("includeContext")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("none");
    if include != "thisServer" && include != "allServers" {
        return;
    }
    let Ok(registry) = features.context_registry.lock() else {
        return;
    };
    let mut snapshots: Vec<&McpContextSnapshot> = registry.values().collect();
    if include == "thisServer" {
        snapshots.retain(|snapshot| snapshot.server == features.server);
    } else {
        snapshots.sort_by(|left, right| left.server.cmp(&right.server));
    }
    if snapshots.is_empty() {
        return;
    }
    let mut context = String::from("MCP context available to this request:\n");
    for snapshot in snapshots {
        context.push_str(&format!("[{}]\n", snapshot.server));
        if !snapshot.roots.is_empty() {
            let roots: Vec<&str> = snapshot
                .roots
                .iter()
                .map(|root| root.uri.as_str())
                .collect();
            context.push_str(&format!("roots: {}\n", roots.join(", ")));
        }
        if !snapshot.tools.is_empty() {
            let tools: Vec<&str> = snapshot
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            context.push_str(&format!("tools: {}\n", tools.join(", ")));
        }
        if !snapshot.resources.is_empty() {
            let resources: Vec<String> = snapshot
                .resources
                .iter()
                .map(|resource| format!("{} ({})", resource.uri, resource.name))
                .collect();
            context.push_str(&format!("resources: {}\n", resources.join(", ")));
        }
        if !snapshot.resource_contents.is_empty() {
            context.push_str("resource contents:\n");
            for (uri, text) in &snapshot.resource_contents {
                context.push_str(&format!("--- {uri}\n{text}\n"));
            }
        }
        if !snapshot.prompts.is_empty() {
            let prompts: Vec<&str> = snapshot
                .prompts
                .iter()
                .map(|prompt| prompt.name.as_str())
                .collect();
            context.push_str(&format!("prompts: {}\n", prompts.join(", ")));
        }
    }
    const MAX_CONTEXT_CHARS: usize = 6000;
    if context.chars().count() > MAX_CONTEXT_CHARS {
        context = context.chars().take(MAX_CONTEXT_CHARS).collect();
        context.push_str("...");
    }
    let system = params
        .get("systemPrompt")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let combined = if system.is_empty() {
        context
    } else {
        format!("{system}\n\n{context}")
    };
    params["systemPrompt"] = serde_json::Value::String(combined);
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
    #[error("roots watch: {0}")]
    Watch(String),
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
    stdin: Arc<Mutex<ChildStdin>>,
    responses: Receiver<String>,
    next_id: u64,
    handshake: McpHandshake,
    tools: Vec<McpTool>,
    resources: Vec<McpResource>,
    resource_templates: Vec<McpResourceTemplate>,
    prompts: Vec<McpPrompt>,
}

impl McpConnection {
    fn open(
        spec: &ProcessSpec,
        timeout: Duration,
        features: McpFeatures,
        notifications: Arc<McpNotificationState>,
    ) -> Result<Self, SupervisorError> {
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
        let stdin = Arc::new(Mutex::new(stdin));
        let (response_sender, responses) = mpsc::channel();
        let (request_sender, requests) = mpsc::channel::<serde_json::Value>();
        let reader_notifications = Arc::clone(&notifications);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                    && let Some(method) = value.get("method").and_then(serde_json::Value::as_str)
                {
                    if value.get("id").is_some() {
                        if request_sender.send(value).is_err() {
                            break;
                        }
                    } else {
                        let params = value
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        reader_notifications.record(method, &params);
                    }
                    continue;
                }
                if response_sender.send(line).is_err() {
                    break;
                }
            }
        });
        let dispatcher_stdin = Arc::clone(&stdin);
        let dispatcher_features = features.clone();
        std::thread::spawn(move || {
            while let Ok(request) = requests.recv() {
                let response = server_request_response(&dispatcher_features, &request);
                let Ok(line) = serde_json::to_string(&response) else {
                    continue;
                };
                let Ok(mut stdin) = dispatcher_stdin.lock() else {
                    break;
                };
                if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });
        let mut connection = Self {
            name: spec.name.clone(),
            child: WaitedChild::spawn(child),
            stdin,
            responses,
            next_id: 1,
            handshake: McpHandshake::default(),
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
        };
        let handshake = connection.negotiate(timeout, &features)?;
        connection.handshake = handshake;
        if connection.handshake.capabilities.tools.is_some() {
            connection.tools = connection.discover_tools(timeout)?;
        }
        if connection.handshake.capabilities.resources.is_some() {
            let (resources, templates) = connection.discover_resources(timeout)?;
            connection.resources = resources;
            connection.resource_templates = templates;
        }
        if connection.handshake.capabilities.prompts.is_some() {
            connection.prompts = connection.discover_prompts(timeout)?;
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
        let mut stdin = self.stdin.lock().map_err(|_| SupervisorError::Mcp {
            name: self.name.clone(),
            message: "stdin lock is poisoned".into(),
        })?;
        writeln!(stdin, "{line}").map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("write: {error}"),
        })?;
        stdin.flush().map_err(|error| SupervisorError::Mcp {
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
            let line = match self.responses.recv_timeout(timeout) {
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

    fn negotiate(
        &mut self,
        timeout: Duration,
        features: &McpFeatures,
    ) -> Result<McpHandshake, SupervisorError> {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": features.capabilities(),
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
        let values = self.paginate("tools/list", "tools", timeout)?;
        self.decode("tools/list", values)
    }

    fn discover_resources(
        &mut self,
        timeout: Duration,
    ) -> Result<(Vec<McpResource>, Vec<McpResourceTemplate>), SupervisorError> {
        let values = self.paginate("resources/list", "resources", timeout)?;
        let resources = self.decode("resources/list", values)?;
        let values = self.paginate("resources/templates/list", "resourceTemplates", timeout)?;
        let templates = self.decode("resources/templates/list", values)?;
        Ok((resources, templates))
    }

    fn discover_prompts(&mut self, timeout: Duration) -> Result<Vec<McpPrompt>, SupervisorError> {
        let values = self.paginate("prompts/list", "prompts", timeout)?;
        self.decode("prompts/list", values)
    }

    fn paginate(
        &mut self,
        method: &str,
        field: &str,
        timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, SupervisorError> {
        let mut values = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            let params = match cursor.as_ref() {
                Some(cursor) => serde_json::json!({"cursor": cursor}),
                None => serde_json::json!({}),
            };
            let result = self.request(method, params, timeout)?;
            let page = result
                .get(field)
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            values.extend(page);
            cursor = result
                .get("nextCursor")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let Some(next) = cursor.as_ref() else {
                return Ok(values);
            };
            if !seen.insert(next.clone()) {
                return Err(SupervisorError::Mcp {
                    name: self.name.clone(),
                    message: format!("{method} repeated cursor {next}"),
                });
            }
        }
        Err(SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("{method} exceeded 256 pages"),
        })
    }

    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        values: Vec<serde_json::Value>,
    ) -> Result<Vec<T>, SupervisorError> {
        values
            .into_iter()
            .map(|value| {
                serde_json::from_value(value).map_err(|error| SupervisorError::Mcp {
                    name: self.name.clone(),
                    message: format!("{method} item: {error}"),
                })
            })
            .collect()
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

    fn read_resource(
        &mut self,
        uri: &str,
        timeout: Duration,
    ) -> Result<McpResourceRead, SupervisorError> {
        let result = self.request("resources/read", serde_json::json!({"uri": uri}), timeout)?;
        serde_json::from_value(result).map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("resources/read result: {error}"),
        })
    }

    fn subscribe_resource(
        &mut self,
        method: &str,
        uri: &str,
        timeout: Duration,
    ) -> Result<(), SupervisorError> {
        self.request(method, serde_json::json!({"uri": uri}), timeout)?;
        Ok(())
    }

    fn get_prompt(
        &mut self,
        name: &str,
        arguments: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, SupervisorError> {
        let mut params = serde_json::json!({"name": name});
        if let Some(arguments) = arguments {
            params["arguments"] = arguments;
        }
        self.request("prompts/get", params, timeout)
    }

    fn notify_roots_changed(&mut self) -> Result<(), SupervisorError> {
        self.notify("notifications/roots/list_changed", serde_json::json!({}))
    }

    fn set_log_level(
        &mut self,
        level: McpLogLevel,
        timeout: Duration,
    ) -> Result<(), SupervisorError> {
        self.request(
            "logging/setLevel",
            serde_json::json!({"level": level}),
            timeout,
        )?;
        Ok(())
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
    resources: Vec<McpResource>,
    resource_templates: Vec<McpResourceTemplate>,
    resource_contents: std::collections::BTreeMap<String, String>,
    prompts: Vec<McpPrompt>,
    notifications: Arc<McpNotificationState>,
    features: McpFeatures,
    log_level: Option<McpLogLevel>,
    launched: bool,
    restarts: usize,
    stopped: bool,
    timeout: Duration,
}

impl McpSupervisor {
    pub fn new(spec: ProcessSpec, policy: RestartPolicy) -> Self {
        let server = spec.name.clone();
        Self {
            spec,
            policy,
            connection: None,
            handshake: None,
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            resource_contents: std::collections::BTreeMap::new(),
            prompts: Vec::new(),
            notifications: Arc::new(McpNotificationState::default()),
            features: McpFeatures::new(server),
            log_level: None,
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

    /// 声明 roots；握手时广告 `roots.listChanged=true`。
    pub fn with_roots(mut self, roots: Vec<McpRoot>) -> Self {
        self.features.roots = Arc::new(Mutex::new(roots));
        self
    }

    /// 期望的服务端日志级别；每次重连握手后自动重新应用。
    pub fn with_log_level(mut self, level: McpLogLevel) -> Self {
        self.log_level = Some(level);
        self
    }

    fn with_context_registry(mut self, registry: McpContextRegistry) -> Self {
        self.features.context_registry = registry;
        self
    }

    fn sync_context(&self) {
        let snapshot = McpContextSnapshot {
            server: self.spec.name.clone(),
            roots: self.roots(),
            tools: self.tools.clone(),
            resources: self.resources.clone(),
            resource_contents: self.resource_contents.clone(),
            prompts: self.prompts.clone(),
        };
        if let Ok(mut registry) = self.features.context_registry.lock() {
            registry.insert(self.spec.name.clone(), snapshot);
        }
    }

    fn clear_context(&self) {
        if let Ok(mut registry) = self.features.context_registry.lock() {
            registry.remove(&self.spec.name);
        }
    }

    /// 预取文本资源正文；失败或超预算的资源跳过，不影响连接。
    fn load_resource_contents(&mut self) {
        const MAX_RESOURCES: usize = 8;
        const MAX_CHARS: usize = 2000;
        self.resource_contents.clear();
        let resources: Vec<String> = self
            .resources
            .iter()
            .filter(|resource| {
                resource.mime_type.as_deref().is_none_or(|mime| {
                    mime.starts_with("text/")
                        || mime.contains("json")
                        || mime.contains("xml")
                        || mime.contains("yaml")
                })
            })
            .take(MAX_RESOURCES)
            .map(|resource| resource.uri.clone())
            .collect();
        let timeout = self.timeout.min(Duration::from_secs(2));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        for uri in resources {
            if std::time::Instant::now() >= deadline {
                break;
            }
            let Ok(read) = self
                .connected()
                .and_then(|connection| connection.read_resource(&uri, timeout))
            else {
                continue;
            };
            let Some(text) = read.contents.into_iter().find_map(|content| content.text) else {
                continue;
            };
            let text = if text.chars().count() > MAX_CHARS {
                text.chars().take(MAX_CHARS).collect()
            } else {
                text
            };
            self.resource_contents.insert(uri, text);
        }
    }

    /// 声明 sampling handler；握手时会向服务端广告 sampling capability。
    pub fn with_sampling_handler<H>(mut self, handler: H) -> Self
    where
        H: SamplingHandler + 'static,
    {
        self.features.sampling = Some(Arc::new(handler));
        self
    }

    pub fn with_sampling_arc(mut self, handler: Arc<dyn SamplingHandler>) -> Self {
        self.features.sampling = Some(handler);
        self
    }

    pub fn restarts(&self) -> usize {
        self.restarts
    }

    pub fn roots(&self) -> Vec<McpRoot> {
        self.features
            .roots
            .lock()
            .map(|roots| roots.clone())
            .unwrap_or_default()
    }

    pub fn log_level(&self) -> Option<McpLogLevel> {
        self.log_level
    }

    /// 更新 roots 并通知服务端重新 `roots/list`；断线时只保存，下次握手生效。
    pub fn set_roots(&mut self, roots: Vec<McpRoot>) -> Result<(), SupervisorError> {
        if self.stopped {
            return Err(SupervisorError::Stopped);
        }
        *self
            .features
            .roots
            .lock()
            .map_err(|_| SupervisorError::Mcp {
                name: self.spec.name.clone(),
                message: "roots lock is poisoned".into(),
            })? = roots;
        if self
            .connection
            .as_mut()
            .is_some_and(McpConnection::is_alive)
        {
            self.connected()?.notify_roots_changed()?;
        }
        self.sync_context();
        Ok(())
    }

    /// 设置服务端日志级别；服务端未广告 logging capability 时 fail closed。
    pub fn set_log_level(
        &mut self,
        recovery: &RecoveryContext,
        level: McpLogLevel,
    ) -> Result<(), SupervisorError> {
        self.ensure_connected(recovery)?;
        if self
            .handshake
            .as_ref()
            .and_then(|handshake| handshake.capabilities.logging.as_ref())
            .is_none()
        {
            return Err(SupervisorError::Mcp {
                name: self.spec.name.clone(),
                message: "server does not advertise logging capability".into(),
            });
        }
        let timeout = self.timeout;
        self.connected()?.set_log_level(level, timeout)?;
        self.log_level = Some(level);
        Ok(())
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
            self.clear_context();
            return Ok(McpStatus::GaveUp);
        }
        self.connection = None;
        if was_connected {
            std::thread::sleep(self.policy.backoff(self.restarts));
            self.restarts += 1;
        }
        self.notifications.clear_list_flags();
        let mut connection = McpConnection::open(
            &self.spec,
            self.timeout,
            self.features.clone(),
            Arc::clone(&self.notifications),
        )?;
        if let Some(level) = self.log_level
            && connection.handshake.capabilities.logging.is_some()
        {
            connection.set_log_level(level, self.timeout)?;
        }
        connection.send_recovery(recovery)?;
        self.handshake = Some(connection.handshake.clone());
        self.tools = connection.tools.clone();
        self.resources = connection.resources.clone();
        self.resource_templates = connection.resource_templates.clone();
        self.prompts = connection.prompts.clone();
        self.connection = Some(connection);
        self.launched = true;
        self.load_resource_contents();
        self.sync_context();
        if was_connected {
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

    pub fn resources(&self) -> &[McpResource] {
        &self.resources
    }

    pub fn resource_templates(&self) -> &[McpResourceTemplate] {
        &self.resource_templates
    }

    pub fn prompts(&self) -> &[McpPrompt] {
        &self.prompts
    }

    /// 取走已收到的服务端通知；list_changed 标志由 refresh_pending 单独消费。
    pub fn drain_notifications(&self) -> Vec<McpNotification> {
        self.notifications.drain()
    }

    fn connected(&mut self) -> Result<&mut McpConnection, SupervisorError> {
        self.connection
            .as_mut()
            .ok_or_else(|| SupervisorError::Mcp {
                name: self.spec.name.clone(),
                message: "not connected".into(),
            })
    }

    /// 重新拉取 tools/list；连接已断开时先按恢复包重连。
    pub fn refresh_tools(
        &mut self,
        recovery: &RecoveryContext,
    ) -> Result<&[McpTool], SupervisorError> {
        self.ensure_connected(recovery)?;
        let timeout = self.timeout;
        let tools = self.connected()?.discover_tools(timeout)?;
        self.notifications.take_tools_changed();
        if let Some(connection) = self.connection.as_mut() {
            connection.tools = tools.clone();
        }
        self.tools = tools;
        self.sync_context();
        Ok(&self.tools)
    }

    pub fn refresh_resources(
        &mut self,
        recovery: &RecoveryContext,
    ) -> Result<&[McpResource], SupervisorError> {
        self.ensure_connected(recovery)?;
        let timeout = self.timeout;
        let (resources, templates) = self.connected()?.discover_resources(timeout)?;
        self.notifications.take_resources_changed();
        if let Some(connection) = self.connection.as_mut() {
            connection.resources = resources.clone();
            connection.resource_templates = templates.clone();
        }
        self.resources = resources;
        self.resource_templates = templates;
        self.load_resource_contents();
        self.sync_context();
        Ok(&self.resources)
    }

    pub fn refresh_prompts(
        &mut self,
        recovery: &RecoveryContext,
    ) -> Result<&[McpPrompt], SupervisorError> {
        self.ensure_connected(recovery)?;
        let timeout = self.timeout;
        let prompts = self.connected()?.discover_prompts(timeout)?;
        self.notifications.take_prompts_changed();
        if let Some(connection) = self.connection.as_mut() {
            connection.prompts = prompts.clone();
        }
        self.prompts = prompts;
        self.sync_context();
        Ok(&self.prompts)
    }

    /// 自动处理 list_changed：只刷新服务端声明过 capability 的集合。
    pub fn refresh_pending(&mut self, recovery: &RecoveryContext) -> Result<(), SupervisorError> {
        self.ensure_connected(recovery)?;
        let Some(connection) = self.connection.as_ref() else {
            return Ok(());
        };
        let capabilities = &connection.handshake.capabilities;
        let refresh_tools = self.notifications.take_tools_changed() && capabilities.tools.is_some();
        let refresh_resources =
            self.notifications.take_resources_changed() && capabilities.resources.is_some();
        let refresh_prompts =
            self.notifications.take_prompts_changed() && capabilities.prompts.is_some();
        let contents_changed = self.notifications.take_contents_changed();
        let timeout = self.timeout;
        if refresh_tools {
            let tools = self.connected()?.discover_tools(timeout)?;
            if let Some(connection) = self.connection.as_mut() {
                connection.tools = tools.clone();
            }
            self.tools = tools;
        }
        if refresh_resources {
            let (resources, templates) = self.connected()?.discover_resources(timeout)?;
            if let Some(connection) = self.connection.as_mut() {
                connection.resources = resources.clone();
                connection.resource_templates = templates.clone();
            }
            self.resources = resources;
            self.resource_templates = templates;
            self.load_resource_contents();
        }
        if refresh_prompts {
            let prompts = self.connected()?.discover_prompts(timeout)?;
            if let Some(connection) = self.connection.as_mut() {
                connection.prompts = prompts.clone();
            }
            self.prompts = prompts;
        }
        if contents_changed && !refresh_resources {
            self.load_resource_contents();
        }
        self.sync_context();
        Ok(())
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
        self.connected()?.call_tool(name, arguments, timeout)
    }

    pub fn read_resource(
        &mut self,
        recovery: &RecoveryContext,
        uri: &str,
        timeout: Duration,
    ) -> Result<McpResourceRead, SupervisorError> {
        self.ensure_connected(recovery)?;
        self.connected()?.read_resource(uri, timeout)
    }

    pub fn subscribe_resource(
        &mut self,
        recovery: &RecoveryContext,
        uri: &str,
    ) -> Result<(), SupervisorError> {
        self.ensure_connected(recovery)?;
        let timeout = self.timeout;
        self.connected()?
            .subscribe_resource("resources/subscribe", uri, timeout)
    }

    pub fn unsubscribe_resource(
        &mut self,
        recovery: &RecoveryContext,
        uri: &str,
    ) -> Result<(), SupervisorError> {
        self.ensure_connected(recovery)?;
        let timeout = self.timeout;
        self.connected()?
            .subscribe_resource("resources/unsubscribe", uri, timeout)
    }

    pub fn get_prompt(
        &mut self,
        recovery: &RecoveryContext,
        name: &str,
        arguments: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, SupervisorError> {
        self.ensure_connected(recovery)?;
        self.connected()?.get_prompt(name, arguments, timeout)
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
        self.clear_context();
    }

    pub fn stop(&mut self) {
        self.stopped = true;
        self.clear_context();
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
            if let Ok(mut supervisor) = supervisor.lock()
                && supervisor.ensure_connected(&recovery).is_ok()
            {
                let _ = supervisor.refresh_pending(&recovery);
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

fn path_to_root_uri(path: &std::path::Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_string_lossy().replace('\\', "/");
    format!("file:///{}", text.trim_start_matches('/'))
}

fn scan_watch_roots(paths: &[std::path::PathBuf]) -> Vec<McpRoot> {
    let mut roots = std::collections::BTreeMap::new();
    for base in paths {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_dir = entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            if !is_dir {
                continue;
            }
            let path = entry.path();
            let uri = path_to_root_uri(&path);
            roots.entry(uri.clone()).or_insert(McpRoot {
                uri,
                name: entry.file_name().to_str().map(str::to_owned),
            });
        }
    }
    roots.into_values().collect()
}

fn merge_roots(static_roots: Vec<McpRoot>, watched: Vec<McpRoot>) -> Vec<McpRoot> {
    let mut roots = std::collections::BTreeMap::new();
    for root in static_roots.into_iter().chain(watched) {
        roots.entry(root.uri.clone()).or_insert(root);
    }
    roots.into_values().collect()
}

fn watch_anchor(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut candidate = absolute.parent()?.to_path_buf();
    loop {
        if candidate.is_dir() {
            return Some(candidate);
        }
        candidate = candidate.parent()?.to_path_buf();
    }
}

struct RootsWatchHandle {
    stop: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
    handle: Option<JoinHandle<()>>,
}

impl RootsWatchHandle {
    fn start(
        paths: Vec<std::path::PathBuf>,
        static_roots: Vec<McpRoot>,
        debounce: Duration,
        initial: Vec<McpRoot>,
        supervisors: Vec<Arc<Mutex<McpSupervisor>>>,
    ) -> Result<Self, SupervisorError> {
        Self::start_with(paths, static_roots, debounce, initial, move |roots| {
            for supervisor in &supervisors {
                if let Ok(mut supervisor) = supervisor.lock() {
                    let _ = supervisor.set_roots(roots.clone());
                }
            }
        })
    }

    fn start_with<F>(
        paths: Vec<std::path::PathBuf>,
        static_roots: Vec<McpRoot>,
        debounce: Duration,
        initial: Vec<McpRoot>,
        mut on_change: F,
    ) -> Result<Self, SupervisorError>
    where
        F: FnMut(Vec<McpRoot>) + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let _ = sender.send(event);
            })
            .map_err(|error| SupervisorError::Watch(error.to_string()))?;
        let mut anchors = std::collections::BTreeSet::new();
        for path in &paths {
            if let Some(anchor) = watch_anchor(path) {
                anchors.insert(anchor);
            }
        }
        for anchor in anchors {
            watcher
                .watch(&anchor, RecursiveMode::Recursive)
                .map_err(|error| {
                    SupervisorError::Watch(format!("{}: {error}", anchor.display()))
                })?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut last = initial;
            while let Ok(event) = receiver.recv() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                if event.is_err() {
                    continue;
                }
                std::thread::sleep(debounce);
                while receiver.try_recv().is_ok() {}
                let roots = merge_roots(static_roots.clone(), scan_watch_roots(&paths));
                if roots != last {
                    last = roots.clone();
                    on_change(roots);
                }
            }
        });
        Ok(Self {
            stop,
            watcher: Some(watcher),
            handle: Some(handle),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.watcher.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RootsWatchHandle {
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
    roots_watch: Option<RootsWatchHandle>,
}

impl SupervisorRuntime {
    pub fn start(
        config: &SupervisorConfig,
        recovery: RecoveryContext,
    ) -> Result<Self, SupervisorError> {
        Self::start_with_sampling(config, recovery, None)
    }

    pub fn start_with_sampling(
        config: &SupervisorConfig,
        recovery: RecoveryContext,
        sampling: Option<Arc<dyn SamplingHandler>>,
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
            roots_watch: None,
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
        let context_registry: McpContextRegistry =
            Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let initial_roots =
            merge_roots(config.roots.clone(), scan_watch_roots(&config.roots_watch));
        for spec in &config.mcp {
            let supervisor = McpSupervisor::new(spec.clone(), config.restart)
                .with_timeout(timeout)
                .with_roots(initial_roots.clone())
                .with_context_registry(Arc::clone(&context_registry));
            let supervisor = match config.log_level {
                Some(level) => supervisor.with_log_level(level),
                None => supervisor,
            };
            let supervisor = match sampling.as_ref() {
                Some(handler) => supervisor.with_sampling_arc(Arc::clone(handler)),
                None => supervisor,
            };
            let supervisor = Arc::new(Mutex::new(supervisor));
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
        if !config.roots_watch.is_empty() && !runtime.mcps.is_empty() {
            runtime.roots_watch = Some(RootsWatchHandle::start(
                config.roots_watch.clone(),
                config.roots.clone(),
                Duration::from_millis(config.roots_watch_debounce_ms.clamp(50, 1_000)),
                initial_roots,
                runtime.mcps.clone(),
            )?);
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
        if let Some(mut watch) = self.roots_watch.take() {
            watch.stop();
        }
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
    fn roots_watch_detects_directory_changes() {
        let base = std::env::temp_dir().join(format!(
            "trit-roots-watch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("ws1")).unwrap();
        let initial = scan_watch_roots(std::slice::from_ref(&base));
        assert_eq!(initial.len(), 1);
        assert!(initial[0].uri.starts_with("file:///"));
        assert!(!initial[0].uri.contains("?/"));
        assert!(initial[0].uri.ends_with("ws1"));

        let (sender, updates) = mpsc::channel();
        let mut handle = RootsWatchHandle::start_with(
            vec![base.clone()],
            Vec::new(),
            Duration::from_millis(20),
            initial,
            move |roots| {
                let _ = sender.send(roots);
            },
        )
        .unwrap();

        std::fs::create_dir_all(base.join("ws2")).unwrap();
        let roots = updates.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(roots.iter().any(|root| root.uri.ends_with("ws2")));

        std::fs::remove_dir_all(base.join("ws2")).unwrap();
        let roots = updates.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!roots.iter().any(|root| root.uri.ends_with("ws2")));

        handle.stop();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sampling_context_injection_respects_scope() {
        let features = McpFeatures::new("server-a");
        {
            let mut registry = features.context_registry.lock().unwrap();
            registry.insert(
                "server-a".into(),
                McpContextSnapshot {
                    server: "server-a".into(),
                    roots: vec![McpRoot {
                        uri: "file:///a".into(),
                        name: None,
                    }],
                    tools: vec![McpTool {
                        name: "tool-a".into(),
                        description: None,
                        input_schema: serde_json::json!({}),
                    }],
                    resources: vec![McpResource {
                        uri: "file:///a/doc".into(),
                        name: "doc-a".into(),
                        ..McpResource::default()
                    }],
                    resource_contents: std::collections::BTreeMap::from([(
                        "file:///a/doc".to_owned(),
                        "body-text".to_owned(),
                    )]),
                    prompts: vec![McpPrompt {
                        name: "prompt-a".into(),
                        ..McpPrompt::default()
                    }],
                },
            );
            registry.insert(
                "server-b".into(),
                McpContextSnapshot {
                    server: "server-b".into(),
                    tools: vec![McpTool {
                        name: "tool-b".into(),
                        description: None,
                        input_schema: serde_json::json!({}),
                    }],
                    ..McpContextSnapshot::default()
                },
            );
        }

        let mut params = serde_json::json!({
            "includeContext": "thisServer",
            "systemPrompt": "base"
        });
        inject_sampling_context(&features, &mut params);
        let system = params["systemPrompt"].as_str().unwrap();
        assert!(system.starts_with("base\n\n"));
        assert!(system.contains("tool-a"));
        assert!(!system.contains("tool-b"));

        let mut params = serde_json::json!({"includeContext": "allServers"});
        inject_sampling_context(&features, &mut params);
        let system = params["systemPrompt"].as_str().unwrap();
        assert!(system.contains("tool-a"));
        assert!(system.contains("tool-b"));
        assert!(system.contains("file:///a/doc"));
        assert!(system.contains("body-text"));

        let mut params = serde_json::json!({"includeContext": "none"});
        inject_sampling_context(&features, &mut params);
        assert!(params.get("systemPrompt").is_none());
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

    #[test]
    fn mcp_supervisor_rejects_log_level_without_capability() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-logging-{}-{:?}",
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
        );
        let recovery = RecoveryContext::default();
        supervisor.ensure_connected(&recovery).unwrap();
        let error = supervisor
            .set_log_level(&recovery, McpLogLevel::Debug)
            .unwrap_err();
        assert!(
            matches!(&error, SupervisorError::Mcp { message, .. } if message.contains("logging")),
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
$greeted = $false
$notified = $false
$resampled = $false
while ($null -ne ($line = [Console]::In.ReadLine())) {
  [IO.File]::AppendAllText($log, $line + "`n")
  $id = 0
  if ($line -match '"id":([0-9]+)') { $id = $Matches[1] }
  if ($line -like '*"method":"initialize"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":true},"resources":{"subscribe":true,"listChanged":true},"prompts":{"listChanged":true},"logging":{},"completions":{}},"serverInfo":{"name":"tool-stub","version":"1.2"}}}')
  } elseif ($line -like '*"method":"notifications/initialized"*') {
    if (-not $greeted) {
      Write-Output '{"jsonrpc":"2.0","id":900,"method":"roots/list","params":{}}'
      Write-Output '{"jsonrpc":"2.0","id":901,"method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"sample"}}],"maxTokens":16}}'
      $greeted = $true
    }
  } elseif ($line -like '*"method":"notifications/roots/list_changed"*') {
    Write-Output '{"jsonrpc":"2.0","id":902,"method":"roots/list","params":{}}'
  } elseif ($line -like '*"method":"logging/setLevel"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{}}')
  } elseif ($line -like '*"method":"tools/list"*') {
    if ($line -like '*"cursor"*') {
      Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"echo2","inputSchema":{"type":"object"}}]}}')
    } else {
      Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"page2"}}')
    }
    if (-not $notified) {
      Write-Output '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}'
      Write-Output '{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}'
      Write-Output '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}'
      Write-Output '{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"file:///doc.txt"}}'
      Write-Output '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","logger":"stub","data":{"message":"hi"}}}'
      $notified = $true
    }
  } elseif ($line -like '*"method":"tools/call"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"ok"}]}}')
    if (-not $resampled) {
      Write-Output '{"jsonrpc":"2.0","id":903,"method":"sampling/createMessage","params":{"messages":[{"role":"user","content":"context?"}],"maxTokens":16,"includeContext":"allServers"}}'
      $resampled = $true
    }
  } elseif ($line -like '*"method":"resources/templates/list"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"resourceTemplates":[{"uriTemplate":"file:///{path}","name":"file-template","description":"File","mimeType":"text/plain"}]}}')
  } elseif ($line -like '*"method":"resources/list"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"resources":[{"uri":"file:///doc.txt","name":"doc","description":"Doc","mimeType":"text/plain","size":5}]}}')
  } elseif ($line -like '*"method":"resources/read"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"contents":[{"uri":"file:///doc.txt","mimeType":"text/plain","text":"hello"}]}}')
  } elseif ($line -like '*"method":"resources/subscribe"*' -or $line -like '*"method":"resources/unsubscribe"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{}}')
  } elseif ($line -like '*"method":"prompts/list"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"prompts":[{"name":"summarize","description":"Summarize","arguments":[{"name":"topic","description":"Topic","required":true}]}]}}')
  } elseif ($line -like '*"method":"prompts/get"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"description":"Prompt","messages":[{"role":"user","content":{"type":"text","text":"prompt-ok"}}]}}')
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
greeted=0
notified=0
resampled=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":true},"resources":{"subscribe":true,"listChanged":true},"prompts":{"listChanged":true},"logging":{},"completions":{}},"serverInfo":{"name":"tool-stub","version":"1.2"}}}' ;;
    *'"method":"notifications/initialized"'*)
      if [ "$greeted" -eq 0 ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":900,"method":"roots/list","params":{}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":901,"method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"sample"}}],"maxTokens":16}}'
        greeted=1
      fi ;;
    *'"method":"notifications/roots/list_changed"'*) printf '%s\n' '{"jsonrpc":"2.0","id":902,"method":"roots/list","params":{}}' ;;
    *'"method":"logging/setLevel"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}' ;;
    *'"method":"tools/list"'*)
      case "$line" in
        *'"cursor"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"echo2","inputSchema":{"type":"object"}}]}}' ;;
        *) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"page2"}}' ;;
      esac
      if [ "$notified" -eq 0 ]; then
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}'
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}'
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}'
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"file:///doc.txt"}}'
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","logger":"stub","data":{"message":"hi"}}}'
        notified=1
      fi ;;
    *'"method":"tools/call"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"ok"}]}}'
      if [ "$resampled" -eq 0 ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":903,"method":"sampling/createMessage","params":{"messages":[{"role":"user","content":"context?"}],"maxTokens":16,"includeContext":"allServers"}}'
        resampled=1
      fi ;;
    *'"method":"resources/templates/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"resourceTemplates":[{"uriTemplate":"file:///{path}","name":"file-template","description":"File","mimeType":"text/plain"}]}}' ;;
    *'"method":"resources/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"resources":[{"uri":"file:///doc.txt","name":"doc","description":"Doc","mimeType":"text/plain","size":5}]}}' ;;
    *'"method":"resources/read"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"contents":[{"uri":"file:///doc.txt","mimeType":"text/plain","text":"hello"}]}}' ;;
    *'"method":"resources/subscribe"'*|*'"method":"resources/unsubscribe"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}' ;;
    *'"method":"prompts/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"prompts":[{"name":"summarize","description":"Summarize","arguments":[{"name":"topic","description":"Topic","required":true}]}]}}' ;;
    *'"method":"prompts/get"'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"description":"Prompt","messages":[{"role":"user","content":{"type":"text","text":"prompt-ok"}}]}}' ;;
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
        let sampled_requests = Arc::new(Mutex::new(Vec::new()));
        let sampled = Arc::clone(&sampled_requests);
        let mut supervisor = McpSupervisor::new(
            mcp_tool_stub_spec(&dir, &log),
            RestartPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        )
        .with_roots(vec![McpRoot {
            uri: "file:///workspace".into(),
            name: Some("workspace".into()),
        }])
        .with_sampling_handler(move |request: serde_json::Value| {
            assert_eq!(request["maxTokens"].as_u64(), Some(16));
            sampled.lock().unwrap().push(request.clone());
            Ok(serde_json::json!({
                "role": "assistant",
                "content": {"type": "text", "text": "sampled"},
                "model": "test-model"
            }))
        });
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
        let mut saw_context = false;
        for _ in 0..3000 {
            saw_context = sampled_requests.lock().unwrap().iter().any(|request| {
                request["includeContext"] == "allServers"
                    && request["systemPrompt"].as_str().is_some_and(|system| {
                        system.contains("file:///doc.txt")
                            && system.contains("echo")
                            && system.contains("hello")
                    })
            });
            if saw_context {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            saw_context,
            "allServers sampling should receive MCP tool/resource context"
        );

        let resources = supervisor.resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "file:///doc.txt");
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(supervisor.resource_templates().len(), 1);
        assert_eq!(
            supervisor.resource_templates()[0].uri_template,
            "file:///{path}"
        );
        let prompts = supervisor.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "summarize");
        assert_eq!(prompts[0].arguments[0].name, "topic");
        assert!(prompts[0].arguments[0].required);

        let read = supervisor
            .read_resource(&recovery, "file:///doc.txt", Duration::from_secs(30))
            .unwrap();
        assert_eq!(read.contents[0].text.as_deref(), Some("hello"));
        supervisor
            .subscribe_resource(&recovery, "file:///doc.txt")
            .unwrap();
        supervisor
            .unsubscribe_resource(&recovery, "file:///doc.txt")
            .unwrap();
        let prompt = supervisor
            .get_prompt(
                &recovery,
                "summarize",
                Some(serde_json::json!({"topic": "rust"})),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(prompt["messages"][0]["content"]["text"], "prompt-ok");

        let mut notifications = Vec::new();
        for _ in 0..3000 {
            notifications.extend(supervisor.drain_notifications());
            if notifications.len() >= 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(notifications.contains(&McpNotification::ToolsListChanged));
        assert!(notifications.contains(&McpNotification::PromptsListChanged));
        assert!(notifications.contains(&McpNotification::ResourcesListChanged));
        assert!(notifications.contains(&McpNotification::ResourceUpdated {
            uri: "file:///doc.txt".into()
        }));
        assert!(notifications.iter().any(|notification| matches!(
            notification,
            McpNotification::LoggingMessage { level, logger, data }
                if level == "info" && logger.as_deref() == Some("stub") && data["message"] == "hi"
        )));
        supervisor.refresh_pending(&recovery).unwrap();
        assert_eq!(supervisor.tools().len(), 2);
        assert_eq!(supervisor.resources().len(), 1);
        assert_eq!(supervisor.prompts().len(), 1);

        let mut log_text = String::new();
        for _ in 0..3000 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.contains("file:///workspace") && log_text.contains("\"sampled\"") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(log_text.contains("file:///workspace"));
        assert!(log_text.contains("\"text\":\"sampled\""));
        assert!(log_text.contains("\"sampling\":{}"));
        assert!(log_text.contains("\"roots\":{\"listChanged\":true}"));

        supervisor
            .set_log_level(&recovery, McpLogLevel::Debug)
            .unwrap();
        assert_eq!(supervisor.log_level(), Some(McpLogLevel::Debug));
        supervisor
            .set_roots(vec![McpRoot {
                uri: "file:///new-root".into(),
                name: Some("new".into()),
            }])
            .unwrap();
        assert_eq!(supervisor.roots()[0].uri, "file:///new-root");
        for _ in 0..3000 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.contains("file:///new-root")
                && log_text.contains("\"method\":\"notifications/roots/list_changed\"")
                && log_text.contains("\"method\":\"logging/setLevel\"")
                && log_text.contains("\"level\":\"debug\"")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(log_text.contains("file:///new-root"));
        assert!(log_text.contains("\"method\":\"notifications/roots/list_changed\""));
        assert!(log_text.contains("\"method\":\"logging/setLevel\""));
        assert!(log_text.contains("\"level\":\"debug\""));

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
        for _ in 0..3000 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            if log_text.matches("\"method\":\"logging/setLevel\"").count() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            log_text.matches("\"method\":\"logging/setLevel\"").count() >= 2,
            "desired log level should be reapplied after reconnect"
        );

        let refreshed = supervisor.refresh_tools(&recovery).unwrap();
        assert_eq!(refreshed.len(), 2);
        supervisor.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supervisor_loop_auto_refreshes_list_changed() {
        let _guard = serialize_mcp_test();
        let dir = std::env::temp_dir().join(format!(
            "trit-mcp-auto-refresh-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mcp-tool-stub.log");
        let supervisor = Arc::new(Mutex::new(
            McpSupervisor::new(
                mcp_tool_stub_spec(&dir, &log),
                RestartPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
            )
            .with_timeout(Duration::from_secs(30)),
        ));
        let recovery = Arc::new(Mutex::new(RecoveryContext::default()));
        let mut loop_handle = SupervisorLoop::spawn_mcp_shared(
            Arc::clone(&supervisor),
            recovery,
            Duration::from_millis(20),
        );
        let mut log_text = String::new();
        for _ in 0..3000 {
            log_text = std::fs::read_to_string(&log).unwrap_or_default();
            let resources_lists = log_text.matches("\"method\":\"resources/list\"").count();
            let prompts_lists = log_text.matches("\"method\":\"prompts/list\"").count();
            if resources_lists >= 2 && prompts_lists >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            log_text.matches("\"method\":\"resources/list\"").count() >= 2,
            "list_changed should trigger another resources/list"
        );
        assert!(
            log_text.matches("\"method\":\"prompts/list\"").count() >= 2,
            "list_changed should trigger another prompts/list"
        );
        loop_handle.stop();
        supervisor.lock().unwrap().stop();
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
                "roots": [{"uri": "file:///workspace", "name": "workspace"}],
                "roots_watch": ["/tmp/workspaces"],
                "roots_watch_debounce_ms": 250,
                "log_level": "warning",
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
        assert_eq!(config.roots.len(), 1);
        assert_eq!(config.roots[0].uri, "file:///workspace");
        assert_eq!(config.roots_watch.len(), 1);
        assert_eq!(config.roots_watch_debounce_ms, 250);
        assert_eq!(config.log_level, Some(McpLogLevel::Warning));
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
            roots: Vec::new(),
            roots_watch: Vec::new(),
            roots_watch_debounce_ms: 1_000,
            log_level: Some(McpLogLevel::Info),
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
