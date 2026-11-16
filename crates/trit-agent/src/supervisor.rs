//! 网关与 MCP 进程 supervisor。
//!
//! 这是真实进程管理：启动子进程、检测退出、退避重启；MCP 额外做 stdio JSON-RPC
//! 握手/重连，并在重连后把 session 状态层、钩子契约和 cache ledger 作为恢复包发回。
//! 不做端口级零停机承诺，只保证自动恢复路径和可审计的重启次数。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context_layers::CacheLedger;
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
    #[error("recovery rejected: {0}")]
    RecoveryRejected(String),
    #[error("supervisor stopped")]
    Stopped,
}

pub struct ProcessSupervisor {
    spec: ProcessSpec,
    policy: RestartPolicy,
    child: Option<Child>,
    restarts: usize,
    stopped: bool,
}

impl ProcessSupervisor {
    pub fn new(spec: ProcessSpec, policy: RestartPolicy) -> Self {
        Self {
            spec,
            policy,
            child: None,
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
        self.child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_none()
            && self.child.is_some()
    }

    pub fn ensure_running(&mut self) -> Result<SupervisorStatus, SupervisorError> {
        if self.stopped {
            return Err(SupervisorError::Stopped);
        }
        if self.is_running() {
            return Ok(SupervisorStatus::AlreadyRunning);
        }
        let was_started = self.child.is_some();
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
        self.child = Some(child);
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
            let _ = child.kill();
            let _ = child.wait();
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
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
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
            child,
            stdin,
            lines,
            next_id: 1,
        };
        connection.handshake(timeout)?;
        Ok(connection)
    }

    fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
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

    fn handshake(&mut self, timeout: Duration) -> Result<(), SupervisorError> {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "trit-agent", "version": "0.1.0"}
            }),
            timeout,
        )?;
        self.notify("notifications/initialized", serde_json::json!({}))
    }

    fn send_recovery(&mut self, recovery: &RecoveryContext) -> Result<(), SupervisorError> {
        let params = serde_json::to_value(recovery).map_err(|error| SupervisorError::Mcp {
            name: self.name.clone(),
            message: format!("recovery encode: {error}"),
        })?;
        self.notify("notifications/trit/recovered", params)
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
            restarts: 0,
            stopped: false,
            timeout: Duration::from_secs(5),
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
        let was_connected = self.connection.is_some();
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
        self.connection = Some(connection);
        if was_connected {
            self.restarts += 1;
            Ok(McpStatus::Reconnected)
        } else {
            Ok(McpStatus::Connected)
        }
    }

    /// 模拟外部崩溃：杀掉子进程但保留“曾经连接过”的事实，下一次 ensure 会重启。
    pub fn kill_connection(&mut self) {
        if let Some(connection) = self.connection.as_mut() {
            connection.stop();
        }
    }

    /// 显式断开（不保留重连计数），下一次 ensure 会走首次连接路径。
    pub fn drop_connection(&mut self) {
        self.connection = None;
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
        Self::spawn(interval, move || {
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

    fn mcp_stub_spec(dir: &std::path::Path, log: &std::path::Path) -> ProcessSpec {
        if cfg!(windows) {
            let script = dir.join("mcp-stub.ps1");
            std::fs::write(
                &script,
                r#"$log = $env:TRIT_MCP_STUB_LOG
Write-Output '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"stub","version":"0"}}}'
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
                "#!/bin/sh\nlog=\"$TRIT_MCP_STUB_LOG\"\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"stub\",\"version\":\"0\"}}}'\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$log\"; done\n",
            )
            .unwrap();
            ProcessSpec::new("sh")
                .with_args([script.display().to_string()])
                .with_env("TRIT_MCP_STUB_LOG", log.display().to_string())
        }
    }

    #[test]
    fn mcp_supervisor_handshakes_reconnects_and_sends_recovery() {
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
        .with_timeout(Duration::from_secs(5));
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
}
