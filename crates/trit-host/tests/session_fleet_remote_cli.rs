//! `trit-host session-fleet-remote` 的 dry-run 回归：不连网，只列任务与节点。

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::{SessionEvent, SessionLog};
use trit_orch::{DelegationStatus, SubagentRequest, SubagentResponse, decode_frame, encode_frame};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-session-fleet-remote-cli-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn open_session(path: &Path) {
    let mut log = SessionLog::open(path).unwrap();
    log.append(SessionEvent::TurnStarted {
        task: "remote work".into(),
        objective: "objective".into(),
        max_steps: 2,
    })
    .unwrap();
}

#[test]
fn dry_run_lists_tasks_and_nodes_without_network() {
    let dir = temp_dir("dry");
    open_session(&dir.join("session.jsonl"));
    let nodes = dir.join("nodes.json");
    fs::write(
        &nodes,
        r#"[{"id":"node-1","max_in_flight":1,"endpoint":{"url":"http://127.0.0.1:9","auth_token":null,"timeout_secs":1}}]"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-fleet-remote",
            "--root",
            dir.to_str().unwrap(),
            "--nodes",
            nodes.to_str().unwrap(),
            "--budget-tokens",
            "100",
            "--compact",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(report["nodes"][0], "node-1");
    assert_eq!(report["budget_tokens"], 100);
    assert!(report["completed"].as_array().unwrap().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn missing_nodes_fails_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(["session-fleet-remote"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires --nodes"), "{stderr}");
}

struct EchoNode {
    child: std::process::Child,
}

impl Drop for EchoNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_echo_node() -> (EchoNode, trit_host::RemoteNode) {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-echo")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let base = format!("http://{}", value["bind"].as_str().unwrap());
    let node = trit_host::RemoteNode::new(
        "node-echo",
        trit_orch::RemoteEndpoint {
            url: format!("{base}/run"),
            auth_token: None,
            timeout_secs: 5,
        },
    );
    (EchoNode { child }, node)
}

#[test]
fn apply_reaches_a_real_echo_node() {
    let (_child, node) = spawn_echo_node();
    let fleet = trit_host::SessionFleetReport {
        schema: trit_host::SESSION_FLEET_SCHEMA,
        workers: 1,
        tasks: vec![trit_host::FleetTask {
            id: "task-1".into(),
            task: "do work".into(),
            deps: Vec::new(),
            workspace: None,
            parent: None,
            max_tokens: 0,
        }],
        layers: vec![vec!["task-1".into()]],
        skipped: Vec::new(),
        cyclic: Vec::new(),
    };

    let outcome = trit_host::run_remote_fleet(
        &fleet,
        vec![node],
        &trit_host::RemoteFleetOptions {
            workers: 1,
            retry_limit: 0,
            health_probe: false,
            dry_run: false,
            scope: None,
        },
    )
    .unwrap();
    assert_eq!(outcome.completed, vec!["task-1".to_owned()]);
    assert_eq!(
        outcome.assignments.get("task-1").map(String::as_str),
        Some("node-echo")
    );
    assert!(outcome.failed.is_none());
}

/// 记录请求头的最小远端：收到一帧就回 Completed，用于断言幂等键。
struct CaptureNode {
    headers: Arc<Mutex<Vec<String>>>,
    node: trit_host::RemoteNode,
    _handle: std::thread::JoinHandle<()>,
}

fn spawn_header_capture_node() -> CaptureNode {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let headers = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&headers);
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0usize;
        let mut idempotency = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("x-trit-idempotency:") {
                idempotency = Some(line.split_once(':').unwrap().1.trim().to_owned());
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        captured
            .lock()
            .unwrap()
            .push(idempotency.expect("missing idempotency header"));
        let request: SubagentRequest = decode_frame(&mut body).unwrap().unwrap();
        let frame = encode_frame(&SubagentResponse {
            task: request.task.clone(),
            epoch: request.epoch,
            status: DelegationStatus::Completed,
            content: b"captured".to_vec(),
            tokens: 0,
            model: Some("gpt-4o".into()),
            usage: Some(trit_orch::SubagentUsage {
                fresh: 1_000_000,
                cached: 0,
                output: 1_000_000,
                estimated: false,
            }),
            dissent: None,
        })
        .unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-trit-frame\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            frame.len()
        )
        .unwrap();
        stream.write_all(&frame).unwrap();
        stream.flush().unwrap();
    });
    let node = trit_host::RemoteNode::new(
        "node-capture",
        trit_orch::RemoteEndpoint {
            url: format!("http://{address}/run"),
            auth_token: None,
            timeout_secs: 5,
        },
    );
    CaptureNode {
        headers,
        node,
        _handle: handle,
    }
}

#[test]
fn advance_sends_the_scoped_idempotency_key() {
    let dir = temp_dir("scope");
    open_session(&dir.join("session.jsonl"));
    let capture = spawn_header_capture_node();
    let nodes = dir.join("nodes.json");
    fs::write(
        &nodes,
        serde_json::to_string(std::slice::from_ref(&capture.node)).unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "session-advance",
            "--root",
            dir.to_str().unwrap(),
            "--nodes",
            nodes.to_str().unwrap(),
            "--scope",
            "plan-a",
            "--apply",
            "--compact",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["remote"]["completed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task == "session.run"),
        "{}",
        report["remote"]
    );
    let headers = capture.headers.lock().unwrap();
    assert_eq!(
        headers.as_slice(),
        ["plan-a|session.run@0"],
        "the remote must receive the scoped idempotency key"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn remote_usage_is_priced_into_the_outcome() {
    let capture = spawn_header_capture_node();
    let fleet = trit_host::SessionFleetReport {
        schema: trit_host::SESSION_FLEET_SCHEMA,
        workers: 1,
        tasks: vec![trit_host::FleetTask {
            id: "task-1".into(),
            task: "do work".into(),
            deps: Vec::new(),
            workspace: None,
            parent: None,
            max_tokens: 0,
        }],
        layers: vec![vec!["task-1".into()]],
        skipped: Vec::new(),
        cyclic: Vec::new(),
    };

    let outcome = trit_host::run_remote_fleet(
        &fleet,
        vec![capture.node.clone()],
        &trit_host::RemoteFleetOptions {
            workers: 1,
            retry_limit: 0,
            health_probe: false,
            dry_run: false,
            scope: None,
        },
    )
    .unwrap();
    assert_eq!(outcome.completed, vec!["task-1".to_owned()]);
    assert_eq!(outcome.usage.fresh, 1_000_000);
    assert_eq!(outcome.usage.output, 1_000_000);
    let cost = outcome.cost_usd.expect("remote usage must be priced");
    // gpt-4o: $2.50 / Mtok 输入 + $10.00 / Mtok 输出。
    assert!((cost - 12.5).abs() < 1e-9, "{cost}");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
}
