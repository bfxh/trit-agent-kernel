//! 远程子 agent：真实 loopback HTTP 往返，以及“没有合法响应就没有完成”的全部
//! 失败路径（鉴权、非 2xx、重定向、空体、截断、超限、引用不匹配、传输失败）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use trit_host::{
    FRAME_CONTENT_TYPE, IDEMPOTENCY_HEADER, MAX_RESPONSE_BYTES, RemoteServer, RemoteServerConfig,
    run_remote_subagent, run_remote_subagent_reply_cancellable,
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
