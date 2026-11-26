//! 远程子 agent：真实 loopback HTTP 往返，以及“没有合法响应就没有完成”的全部
//! 失败路径（鉴权、非 2xx、重定向、空体、截断、超限、引用不匹配、传输失败）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use trit_host::{
    FRAME_CONTENT_TYPE, IDEMPOTENCY_HEADER, MAX_RESPONSE_BYTES, RemoteServer, RemoteServerConfig,
    probe_remote_health, run_remote_subagent, run_remote_subagent_reply,
    run_remote_subagent_reply_cancellable, run_remote_subagent_reply_scoped,
};
use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, DelegationStatus, RemoteEndpoint,
    SubagentProfile, SubagentRequest, SubagentResponse, Task, TaskSpec, decode_frame, encode_frame,
};

fn task(id: &str, epoch: u64) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        epoch,
        ..TaskSpec::default()
    }
}

fn request() -> SubagentRequest {
    let capsule = ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "goal".into(),
            value: "ship".into(),
        }],
        &["goal"],
        64,
    )
    .unwrap();
    let profile = SubagentProfile {
        name: "child".into(),
        access: AccessMode::ReadOnly,
        write_roots: Vec::new(),
        tools: vec!["read_file".into()],
        max_tokens: 100,
    };
    SubagentRequest::new(&task("a", 3), capsule, profile)
}

fn endpoint(url: &str, token: Option<&str>, timeout_secs: u64) -> RemoteEndpoint {
    RemoteEndpoint {
        url: url.into(),
        auth_token: token.map(Into::into),
        timeout_secs,
    }
}

fn response(request: &SubagentRequest, status: DelegationStatus) -> SubagentResponse {
    SubagentResponse {
        task: request.task.clone(),
        epoch: request.epoch,
        status,
        content: b"remote output".to_vec(),
        tokens: 7,
    }
}

type Captured = (Vec<u8>, Option<String>);

/// 一次性 loopback 服务端：收一个请求，按闭包回包并关闭。
fn serve_once(
    reply: impl FnOnce(&[u8], Option<&str>) -> (u16, Vec<u8>) + Send + 'static,
) -> (String, JoinHandle<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut content_length = 0usize;
        let mut authorization = None;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            let Some((name, value)) = trimmed.split_once(':') else {
                continue;
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap(),
                "authorization" => authorization = Some(value.trim().to_string()),
                _ => {}
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();

        let (code, reply_body) = reply(&body, authorization.as_deref());
        let _ = write!(
            stream,
            "HTTP/1.1 {code} Reply\r\nContent-Type: {FRAME_CONTENT_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply_body.len()
        );
        let _ = stream.write_all(&reply_body);
        let _ = stream.flush();
        (body, authorization)
    });
    (format!("http://{address}/run"), handle)
}

/// 用 `trit-host remote-echo` 起一个真实端点；stdout 首行给出实际端口。
fn spawn_echo(args: &[&str]) -> (String, Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-echo")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let bind = value["bind"].as_str().unwrap().to_string();
    (format!("http://{bind}/run"), child)
}

fn cancel(url: &str, key: &str) {
    let origin = url.strip_suffix("/run").unwrap_or(url);
    let address = origin.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST /cancel HTTP/1.1\r\nHost: test\r\n{IDEMPOTENCY_HEADER}: {key}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 204"),
        "cancel should be accepted, got {response}"
    );
}

fn health_status(address: &str, authorization: Option<&str>) -> String {
    let auth = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: test\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.lines().next().unwrap_or_default().to_string()
}

#[test]
fn bearer_token_requires_an_exact_header() {
    let server =
        RemoteServer::bind(RemoteServerConfig::new("127.0.0.1:0").with_token("secret")).unwrap();
    let address = server.addr().unwrap().to_string();
    let handle = std::thread::spawn(move || server.serve_connections(5).unwrap());

    assert_eq!(
        health_status(&address, Some("Bearer secret")),
        "HTTP/1.1 200 OK"
    );
    for header in [
        Some("Bearer secre"),
        Some("Bearer secretx"),
        Some("bearer secret"),
        None,
    ] {
        let status = health_status(&address, header);
        assert!(
            status.starts_with("HTTP/1.1 401"),
            "near-match header {header:?} must be rejected, got {status}"
        );
    }
    handle.join().unwrap();
}

#[test]
fn remote_round_trip_over_loopback() {
    let request = request();
    let (url, handle) = serve_once(|body, auth| {
        assert!(
            auth.is_none(),
            "no token configured, no Authorization expected"
        );
        let mut buffer = body.to_vec();
        let received: SubagentRequest = decode_frame(&mut buffer).unwrap().unwrap();
        assert!(buffer.is_empty(), "request body must be exactly one frame");
        assert_eq!(received.task, "a");
        assert_eq!(received.epoch, 3);
        (
            200,
            encode_frame(&response(&received, DelegationStatus::Completed)).unwrap(),
        )
    });

    let result = run_remote_subagent(&endpoint(&url, None, 30), &request).unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    assert_eq!(result.task, "a");
    assert_eq!(result.epoch, 3);
    assert!(result.reference().starts_with("a@3#"));
    assert_eq!(result.tokens, 7);

    let (body, _) = handle.join().unwrap();
    let mut buffer = body;
    let posted: SubagentRequest = decode_frame(&mut buffer).unwrap().unwrap();
    assert_eq!(posted, request);
}

#[test]
fn bearer_token_is_sent_and_required() {
    let request = request();
    let (url, handle) = serve_once(|body, auth| {
        assert_eq!(auth, Some("Bearer s3cret"));
        let mut buffer = body.to_vec();
        let received: SubagentRequest = decode_frame(&mut buffer).unwrap().unwrap();
        (
            200,
            encode_frame(&response(&received, DelegationStatus::Completed)).unwrap(),
        )
    });
    let result = run_remote_subagent(&endpoint(&url, Some("s3cret"), 30), &request).unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    handle.join().unwrap();

    // 错误的 token 由服务端拒绝：401 → Failed，绝不 Completed。
    let (url, handle) = serve_once(|_, auth| {
        let ok = auth == Some("Bearer right");
        (if ok { 200 } else { 401 }, Vec::new())
    });
    let result = run_remote_subagent(&endpoint(&url, Some("wrong"), 30), &request).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("401")),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
    handle.join().unwrap();
}

#[test]
fn non_2xx_is_failed_not_completed() {
    let (url, handle) = serve_once(|_, _| (500, b"boom".to_vec()));
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("500")),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
    handle.join().unwrap();
}

#[test]
fn redirect_is_not_followed() {
    // 3xx 一律不跳转：避免把 Authorization 引到另一个 host。
    let (url, handle) = serve_once(|_, _| (302, Vec::new()));
    let result = run_remote_subagent(&endpoint(&url, Some("token"), 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("302")),
        "{:?}",
        result.status
    );
    handle.join().unwrap();
}

#[test]
fn empty_body_is_failed() {
    let (url, handle) = serve_once(|_, _| (200, Vec::new()));
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("no response frame")),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
    handle.join().unwrap();
}

#[test]
fn truncated_frame_is_failed() {
    let (url, handle) = serve_once(|_, _| {
        let mut body = 100u32.to_be_bytes().to_vec();
        body.extend_from_slice(b"{\"task\"");
        (200, body)
    });
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("truncated")),
        "{:?}",
        result.status
    );
    handle.join().unwrap();
}

#[test]
fn oversized_response_is_failed() {
    // 超过 MAX_RESPONSE_BYTES 的体不解码，也不整读进内存放大。
    let (url, handle) = serve_once(|_, _| (200, vec![0u8; MAX_RESPONSE_BYTES + 1]));
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("exceeds")),
        "{:?}",
        result.status
    );
    handle.join().unwrap();
}

#[test]
fn malformed_and_mismatched_responses_are_protocol_errors() {
    // 不是帧：没有长度前缀。
    let (url, handle) = serve_once(|_, _| (200, b"not-a-frame".to_vec()));
    let error = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap_err();
    assert!(
        matches!(&error, trit_host::AgentError::Kernel(inner) if inner.kind() == "subagent_protocol"),
        "{error:?}"
    );
    handle.join().unwrap();

    // 合法帧但回收了别的 epoch：引用校验拒绝。
    let (url, handle) = serve_once(|_, _| {
        let mut response = response(&request(), DelegationStatus::Completed);
        response.epoch = 9;
        (200, encode_frame(&response).unwrap())
    });
    let error = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap_err();
    assert!(
        matches!(&error, trit_host::AgentError::Kernel(inner) if inner.kind() == "subagent_protocol"),
        "{error:?}"
    );
    handle.join().unwrap();
}

#[test]
fn truncated_body_is_aborted() {
    // 200 头声明 100 字节，只写 10 字节就断开：传输失败，按 Aborted。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut content_length = 0usize;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {FRAME_CONTENT_TYPE}\r\nContent-Length: 100\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(b"0123456789");
        let _ = stream.flush();
    });

    let result = run_remote_subagent(
        &endpoint(&format!("http://{addr}/run"), None, 5),
        &request(),
    )
    .unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Aborted),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
    let _ = handle.join();
}

#[test]
fn transport_failure_is_aborted_not_completed() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let result = run_remote_subagent(
        &endpoint(&format!("http://127.0.0.1:{port}/run"), None, 5),
        &request(),
    )
    .unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Aborted),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
}

#[test]
fn plaintext_remote_is_rejected_without_transport() {
    // 未连网就先失败：明文 http 指向非 loopback 一律拒绝。
    let error = run_remote_subagent(
        &endpoint("http://agents.example.com/run", Some("token"), 30),
        &request(),
    )
    .unwrap_err();
    assert!(
        matches!(&error, trit_host::AgentError::Kernel(inner) if inner.kind() == "subagent_protocol"),
        "{error:?}"
    );
}

#[test]
fn remote_echo_bin_serves_a_real_endpoint() {
    let (url, mut child) = spawn_echo(&["--once", "--token", "t"]);
    let result = run_remote_subagent(&endpoint(&url, Some("t"), 30), &request()).unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    assert!(result.reference().starts_with("a@3#"));
    assert!(child.wait().unwrap().success());
}

#[test]
fn remote_echo_fail_and_silent_modes_never_complete() {
    let (url, mut child) = spawn_echo(&["--once", "--fail"]);
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "{:?}",
        result.status
    );
    child.wait().unwrap();

    let (url, mut child) = spawn_echo(&["--once", "--silent"]);
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("no response frame")),
        "{:?}",
        result.status
    );
    child.wait().unwrap();
}

#[test]
fn remote_timeout_is_aborted() {
    let (url, mut child) = spawn_echo(&["--once", "--sleep-ms", "2000"]);
    let result = run_remote_subagent(&endpoint(&url, None, 1), &request()).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Aborted),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
    let _ = child.wait();
}

#[test]
fn request_deadline_aborts_cli_slow_request() {
    let (url, mut child) = spawn_echo(&[
        "--once",
        "--sleep-ms",
        "2000",
        "--request-timeout-ms",
        "250",
    ]);
    let started = Instant::now();
    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Aborted),
        "the CLI watchdog must abort a request past its deadline, got {:?}",
        result.status
    );
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "watchdog must fire before the 2s sleep completes, took {:?}",
        started.elapsed()
    );
    let _ = child.wait();
}

#[test]
fn health_probe_is_authenticated_and_does_not_execute() {
    let server = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_token("t")
            .with_sleep_ms(2000),
    )
    .unwrap();
    let url = format!("http://{}/run", server.addr().unwrap());
    let handle = std::thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });

    let started = Instant::now();
    probe_remote_health(&endpoint(&url, Some("t"), 5)).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "health probe must not enter the 2s sleep path: {:?}",
        started.elapsed()
    );

    let error = probe_remote_health(&endpoint(&url, None, 5)).unwrap_err();
    assert!(error.contains("401"), "{error}");
    handle.join().unwrap();
}

#[test]
fn idempotency_header_carries_task_at_epoch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut content_length = 0usize;
        let mut idempotency = None;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            let Some((name, value)) = trimmed.split_once(':') else {
                continue;
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap(),
                "x-trit-idempotency" => idempotency = Some(value.trim().to_string()),
                _ => {}
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        let mut buffer = body;
        let received: SubagentRequest = decode_frame(&mut buffer).unwrap().unwrap();
        let frame = encode_frame(&response(&received, DelegationStatus::Completed)).unwrap();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {FRAME_CONTENT_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            frame.len()
        );
        let _ = stream.write_all(&frame);
        let _ = stream.flush();
        idempotency
    });

    let result = run_remote_subagent(
        &endpoint(&format!("http://{address}/run"), None, 5),
        &request(),
    )
    .unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    assert_eq!(handle.join().unwrap().as_deref(), Some("a@3"));
}

#[test]
fn echo_server_dedupes_matching_idempotency_key() {
    // 同一个 key 的第二次请求必须直接回缓存，不再执行（也就跳过 sleep）。
    let server =
        RemoteServer::bind(RemoteServerConfig::new("127.0.0.1:0").with_sleep_ms(500)).unwrap();
    let url = format!("http://{}/run", server.addr().unwrap());
    let handle = std::thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });

    let started = Instant::now();
    let first = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    let first_elapsed = started.elapsed();
    assert!(first.reusable(), "{:?}", first.status);
    assert!(
        first_elapsed >= Duration::from_millis(450),
        "first request should execute and sleep: {first_elapsed:?}"
    );

    let started = Instant::now();
    let second = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    let second_elapsed = started.elapsed();
    assert!(second.reusable(), "{:?}", second.status);
    assert_eq!(second.reference(), first.reference());
    assert!(
        second_elapsed < Duration::from_millis(300),
        "dedupe hit must skip execution: {second_elapsed:?}"
    );
    handle.join().unwrap();
}

#[test]
fn execution_deadline_aborts_slow_request() {
    let server = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(2000)
            .with_exec_timeout(Duration::from_millis(250)),
    )
    .unwrap();
    let url = format!("http://{}/run", server.addr().unwrap());
    let handle = std::thread::spawn(move || server.serve_once().unwrap());

    let started = Instant::now();
    let reply = run_remote_subagent_reply(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(reply.result.status, DelegationStatus::Aborted),
        "a request past its deadline must abort, got {:?}",
        reply.result.status
    );
    assert!(
        reply.deadline_exceeded,
        "the server must flag the abort as a deadline miss"
    );
    assert!(!reply.result.reusable());
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "deadline must fire before the 2s sleep completes, took {:?}",
        started.elapsed()
    );
    handle.join().unwrap();
}

#[test]
fn cache_byte_budget_evicts_the_oldest_response() {
    // 预算只够一条：第二条写入必须淘汰第一条，第三次请求才会重新执行。
    let first = request();
    let mut second = first.clone();
    second.epoch = 4;
    let expected_first = SubagentResponse {
        task: first.task.clone(),
        epoch: first.epoch,
        status: DelegationStatus::Completed,
        content: format!("remote echo {}@{}", first.task, first.epoch).into_bytes(),
        tokens: 0,
    };
    let frame_bytes = encode_frame(&expected_first).unwrap().len();
    let one_entry = format!("{}@{}", first.task, first.epoch).len() + frame_bytes;
    let server = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(250)
            .with_max_cache_bytes(one_entry + 1),
    )
    .unwrap();
    let url = format!("http://{}/run", server.addr().unwrap());
    let handle = std::thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });

    for (label, request) in [
        ("first", &first),
        ("second", &second),
        ("first-again", &first),
    ] {
        let started = Instant::now();
        let result = run_remote_subagent(&endpoint(&url, None, 30), request).unwrap();
        assert!(result.reusable(), "{label}: {:?}", result.status);
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "{label} must execute instead of hitting an over-budget cache: {:?}",
            started.elapsed()
        );
    }
    handle.join().unwrap();
}

#[test]
fn concurrent_duplicate_requests_execute_once_without_a_ledger() {
    let server = Arc::new(
        RemoteServer::bind(RemoteServerConfig::new("127.0.0.1:0").with_sleep_ms(250)).unwrap(),
    );
    let url = format!("http://{}/run", server.addr().unwrap());
    let worker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || server.serve_connections(2).unwrap())
    };
    let request = request();
    let first = {
        let endpoint = endpoint(&url, None, 30);
        let request = request.clone();
        std::thread::spawn(move || run_remote_subagent(&endpoint, &request))
    };
    let second = {
        let endpoint = endpoint(&url, None, 30);
        let request = request.clone();
        std::thread::spawn(move || run_remote_subagent(&endpoint, &request))
    };

    for (label, worker) in [("first", first), ("second", second)] {
        let response = worker.join().unwrap().unwrap();
        assert!(
            matches!(&response.status, DelegationStatus::Completed),
            "{label}: {:?}",
            response.status
        );
    }
    worker.join().unwrap();
    assert_eq!(
        server.executed_requests(),
        1,
        "the second duplicate must wait and replay the cached response"
    );
}

#[test]
fn oversized_header_line_is_rejected_with_431() {
    let (url, mut child) = spawn_echo(&["--once"]);
    let origin = url.strip_suffix("/run").unwrap();
    let address = origin.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST /run HTTP/1.1\r\nHost: test\r\nX-Long: {}\r\n",
        "a".repeat(9000)
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 431"),
        "oversized header must be rejected, got {response}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn too_many_headers_are_rejected_with_431() {
    let (url, mut child) = spawn_echo(&["--once"]);
    let origin = url.strip_suffix("/run").unwrap();
    let address = origin.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    let mut request = String::from("POST /run HTTP/1.1\r\nHost: test\r\n");
    for index in 0..65 {
        request.push_str(&format!("X-Header-{index}: value\r\n"));
    }
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 431"),
        "too many headers must be rejected, got {response}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn oversized_header_block_is_rejected_with_431() {
    let (url, mut child) = spawn_echo(&["--once"]);
    let origin = url.strip_suffix("/run").unwrap();
    let address = origin.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    let mut request = String::from("POST /run HTTP/1.1\r\nHost: test\r\n");
    let filler = "b".repeat(8000);
    for index in 0..9 {
        request.push_str(&format!("X-Header-{index}: {filler}\r\n"));
    }
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 431"),
        "oversized header block must be rejected, got {response}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn request_read_deadline_aborts_slow_header_trickle() {
    let (url, mut child) = spawn_echo(&["--once", "--request-read-timeout-ms", "400"]);
    let origin = url.strip_suffix("/run").unwrap();
    let address = origin.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        writer
            .write_all(b"POST /run HTTP/1.1\r\nHost: test\r\nX-Slow: ")
            .unwrap();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(150));
            if writer.write_all(b"a").is_err() {
                break;
            }
        }
    });
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let _ = writer.join();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        response.starts_with("HTTP/1.1 408"),
        "slow headers must hit the total read deadline, got {response}"
    );
}

#[test]
fn request_read_deadline_aborts_slow_body_upload() {
    let (url, mut child) = spawn_echo(&["--once", "--request-read-timeout-ms", "400"]);
    let origin = url.strip_suffix("/run").unwrap();
    let address = origin.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        writer
            .write_all(b"POST /run HTTP/1.1\r\nHost: test\r\nContent-Length: 10\r\n\r\n")
            .unwrap();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(150));
            if writer.write_all(b"a").is_err() {
                break;
            }
        }
    });
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let _ = writer.join();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        response.starts_with("HTTP/1.1 408"),
        "slow body upload must hit the total read deadline, got {response}"
    );
}

#[test]
fn echo_server_rejects_mismatched_idempotency_key() {
    // 头里的 key 与帧里的 task@epoch 不一致：400，不解帧执行。
    let server = RemoteServer::bind(RemoteServerConfig::new("127.0.0.1:0")).unwrap();
    let address = server.addr().unwrap();
    let handle = std::thread::spawn(move || {
        let _ = server.serve_once();
    });

    let frame = encode_frame(&request()).unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    let _ = write!(
        stream,
        "POST /run HTTP/1.1\r\nHost: test\r\n{IDEMPOTENCY_HEADER}: b@3\r\nContent-Type: {FRAME_CONTENT_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        frame.len()
    );
    let _ = stream.write_all(&frame);
    let _ = stream.flush();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let _ = reader.read_line(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "unexpected response: {response}"
    );
    handle.join().unwrap();
}

#[test]
fn scoped_idempotency_isolates_plans_and_dedupes_within_scope() {
    // 不同计划即使 task@epoch 相同，也必须各自执行；同计划的重复请求才命中缓存。
    let server =
        RemoteServer::bind(RemoteServerConfig::new("127.0.0.1:0").with_sleep_ms(500)).unwrap();
    let url = format!("http://{}/run", server.addr().unwrap());
    let handle = std::thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });
    let endpoint = endpoint(&url, None, 30);
    let request = request();

    let started = Instant::now();
    let first = run_remote_subagent_reply_scoped(&endpoint, &request, Some("plan-a")).unwrap();
    assert!(first.result.reusable(), "{:?}", first.result.status);
    assert!(
        started.elapsed() >= Duration::from_millis(450),
        "first plan-a request should execute"
    );

    let started = Instant::now();
    let other = run_remote_subagent_reply_scoped(&endpoint, &request, Some("plan-b")).unwrap();
    assert!(other.result.reusable(), "{:?}", other.result.status);
    assert!(
        started.elapsed() >= Duration::from_millis(450),
        "same task in another scope must not reuse plan-a's cached response"
    );

    let started = Instant::now();
    let repeat = run_remote_subagent_reply_scoped(&endpoint, &request, Some("plan-a")).unwrap();
    assert!(repeat.result.reusable(), "{:?}", repeat.result.status);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "same scope must hit the cached response"
    );
    assert_eq!(repeat.result.reference(), first.result.reference());
    handle.join().unwrap();
}

#[test]
fn unsafe_idempotency_scope_is_rejected_before_network() {
    let endpoint = endpoint("http://127.0.0.1:9/run", None, 1);
    let error =
        run_remote_subagent_reply_scoped(&endpoint, &request(), Some("../escape")).unwrap_err();
    assert!(
        error.to_string().contains("not a safe namespace"),
        "unexpected error: {error}"
    );
}

#[test]
fn connection_limit_rejects_excess_with_503() {
    let server = Arc::new(
        RemoteServer::bind(
            RemoteServerConfig::new("127.0.0.1:0")
                .with_sleep_ms(3000)
                .with_max_connections(1),
        )
        .unwrap(),
    );
    let address = server.addr().unwrap();
    let running = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = running.serve_forever();
    });

    let frame = encode_frame(&request()).unwrap();
    let mut first = TcpStream::connect(address).unwrap();
    let _ = write!(
        first,
        "POST /run HTTP/1.1\r\nHost: test\r\nContent-Type: {FRAME_CONTENT_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        frame.len()
    );
    let _ = first.write_all(&frame);
    let _ = first.flush();
    // 等第一个连接被 accept 并占住唯一槽位。
    std::thread::sleep(Duration::from_millis(150));

    // 连接上限在读取请求前判定；不发请求也能观察 503，且避免未读数据触发 RST。
    let second = TcpStream::connect(address).unwrap();
    let mut reader = BufReader::new(second);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(
        status.starts_with("HTTP/1.1 503"),
        "excess connection must be rejected with 503, got {status}"
    );

    drop(first);
    drop(server);
}

#[test]
fn cancel_reaches_all_concurrent_requests_with_the_same_key() {
    let (url, mut child) = spawn_echo(&["--sleep-ms", "1000"]);
    let endpoint = endpoint(&url, None, 10);
    let request = request();
    let first = {
        let endpoint = endpoint.clone();
        let request = request.clone();
        std::thread::spawn(move || run_remote_subagent(&endpoint, &request))
    };
    let second = {
        let endpoint = endpoint.clone();
        let request = request.clone();
        std::thread::spawn(move || run_remote_subagent(&endpoint, &request))
    };

    std::thread::sleep(Duration::from_millis(250));
    cancel(&url, "a@3");
    // 两个请求的登记有先后；再取消一次，确保晚到者也被覆盖。
    std::thread::sleep(Duration::from_millis(100));
    cancel(&url, "a@3");

    for (label, worker) in [("first", first), ("second", second)] {
        let response = worker.join().unwrap().unwrap();
        assert!(
            matches!(&response.status, DelegationStatus::Aborted),
            "{label} must be aborted, got {:?}",
            response.status
        );
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn cancellation_reaches_slow_server_and_returns_aborted() {
    let (url, mut child) = spawn_echo(&["--sleep-ms", "3000"]);
    let endpoint = endpoint(&url, None, 10);
    let request = request();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let started = Instant::now();
    let worker = std::thread::spawn(move || {
        run_remote_subagent_reply_cancellable(&endpoint, &request, &token)
    });

    std::thread::sleep(Duration::from_millis(150));
    cancel.cancel();
    let reply = worker.join().unwrap().unwrap();
    assert!(
        matches!(&reply.result.status, DelegationStatus::Aborted),
        "expected Aborted, got {:?}",
        reply.result.status
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancel must not wait for the 3s sleep: {:?}",
        started.elapsed()
    );
    let _ = child.kill();
    let _ = child.wait();
}
