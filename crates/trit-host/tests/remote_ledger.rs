//! 共享幂等账本：claim/renew/complete 协议，以及两台远端共享账本时的单次执行。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use trit_host::remote_ledger::{self, Claim};
use trit_host::{
    LedgerServer, LedgerServerConfig, RemotePlanConfig, RemoteServer, RemoteServerConfig,
    run_remote_plan_with_table, run_remote_subagent, run_remote_subagent_reply,
};
use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, DelegationStatus, NodeTable, Plan,
    RemoteEndpoint, RemoteNode, SubagentProfile, SubagentRequest, SubagentResponse, Task, TaskSpec,
    decode_frame, encode_frame,
};

fn task(id: &str) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        max_tokens: 100,
        ..TaskSpec::default()
    }
}

fn request() -> SubagentRequest {
    request_for(&task("t1"))
}

fn request_for(task: &TaskSpec) -> SubagentRequest {
    let capsule = ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "goal".into(),
            value: "remote".into(),
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
    SubagentRequest::new(task, capsule, profile)
}

fn endpoint(url: &str, token: Option<&str>, timeout_secs: u64) -> RemoteEndpoint {
    RemoteEndpoint {
        url: url.into(),
        auth_token: token.map(Into::into),
        timeout_secs,
    }
}

fn state_file(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "trit-ledger-state-{}-{tag}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn request_status(address: &str, method: &str, path: &str, authorization: Option<&str>) -> String {
    let auth = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: test\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.lines().next().unwrap_or_default().to_string()
}

#[test]
fn state_file_is_single_writer() {
    let path = state_file("single-writer");
    let lock_path = std::path::PathBuf::from(format!("{}.lock", path.display()));
    let _ = std::fs::remove_file(&lock_path);

    let first =
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)).unwrap();
    let error =
        match LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)) {
            Ok(_) => panic!("a second ledger must not share the same state file"),
            Err(error) => error,
        };
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse, "{error}");
    assert!(error.to_string().contains(".lock"), "{error}");

    // 进程退出（这里是 drop）后锁必须释放，重启才能接回同一份日志。
    drop(first);
    let restarted =
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)).unwrap();
    drop(restarted);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&lock_path);
}

#[test]
fn ledger_health_is_read_only_and_requires_bearer() {
    let server =
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_token("secret")).unwrap();
    let address = server.addr().unwrap().to_string();
    let handle = thread::spawn(move || {
        for _ in 0..6 {
            server.serve_once().unwrap();
        }
    });

    assert_eq!(
        request_status(&address, "GET", "/health", Some("Bearer secret")),
        "HTTP/1.1 200 OK"
    );
    for header in [None, Some("Bearer secre"), Some("Bearer secretx")] {
        assert_eq!(
            request_status(&address, "GET", "/health", header),
            "HTTP/1.1 401 Unauthorized",
            "health must use the same bearer as the ledger"
        );
    }
    assert_eq!(
        request_status(&address, "GET", "/claim", Some("Bearer secret")),
        "HTTP/1.1 405 Method Not Allowed"
    );
    assert_eq!(
        request_status(&address, "POST", "/health", Some("Bearer secret")),
        "HTTP/1.1 405 Method Not Allowed"
    );
    handle.join().unwrap();
}

#[test]
fn ledger_limits_connections_with_503() {
    let ledger = Arc::new(
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_max_connections(1)).unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());

    // 第一条连接不发请求，只占住唯一的槽位。
    let first = TcpStream::connect(address).unwrap();
    thread::sleep(Duration::from_millis(150));

    // 超额连接在读取协议前就被拒绝，避免慢客户端拖住账本线程。
    let second = TcpStream::connect(address).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(second);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(
        status.starts_with("HTTP/1.1 503"),
        "excess ledger connection must be rejected with 503, got {status}"
    );

    drop(first);
    drop(ledger);
}

#[test]
fn ledger_bounds_request_read_time_with_408() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-ledger")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--once")
        .arg("--request-read-timeout-ms")
        .arg("400")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let address = value["bind"].as_str().unwrap();

    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let writer = thread::spawn(move || {
        writer
            .write_all(b"POST /claim HTTP/1.1\r\nHost: test\r\nX-Trit-Idempotency: a@1\r\nX-Slow: ")
            .unwrap();
        for _ in 0..5 {
            thread::sleep(Duration::from_millis(150));
            if writer.write_all(b"a").is_err() {
                break;
            }
        }
    });
    let mut response = Vec::new();
    // macOS 可能在服务端超时关闭连接时把已排队的 408 变成 RST；已读到的字节仍然有效。
    match std::io::Read::read_to_end(&mut stream, &mut response) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        Err(error) => panic!("ledger response read failed: {error}"),
    }
    let response = String::from_utf8(response).unwrap();
    let _ = writer.join();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        response.starts_with("HTTP/1.1 408"),
        "slow ledger request must hit the total read deadline, got {response}"
    );
}

#[test]
fn ledger_claim_complete_and_pending_takeover() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_token("t")
                .with_pending_ttl(Duration::from_millis(120)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), Some("t"), 5);

    let Claim::New { lease: lease_one } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap()
    else {
        panic!("expected first claim to win");
    };
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending
    );

    // 占位过期后可被接管，旧持有者的令牌同时失效。
    thread::sleep(Duration::from_millis(160));
    let Claim::New { lease: lease_two } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap()
    else {
        panic!("expected expired lease to be taken over");
    };

    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"done".to_vec(),
        tokens: 0,
    })
    .unwrap();
    assert!(
        remote_ledger::renew(&ledger_endpoint, "t1@0", &lease_one).is_err(),
        "a stale owner must not renew"
    );
    assert!(
        remote_ledger::complete(&ledger_endpoint, "t1@0", &lease_one, &frame).is_err(),
        "a stale owner must not write the completion"
    );
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease_two, &frame).unwrap();
    assert!(
        remote_ledger::complete(&ledger_endpoint, "t1@0", &lease_two, &frame).is_ok(),
        "the recorded owner may repeat its completion idempotently"
    );
    assert!(
        remote_ledger::complete(&ledger_endpoint, "t1@0", &lease_one, &frame).is_err(),
        "a stale lease must not be told that its completion won"
    );
    assert!(
        remote_ledger::renew(&ledger_endpoint, "t1@0", &lease_two).is_err(),
        "a completed key must not be renewed"
    );
    match remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() {
        Claim::Done(body) => {
            let mut buffer = body;
            let response: SubagentResponse = decode_frame(&mut buffer).unwrap().unwrap();
            assert_eq!(response.task, "t1");
            assert!(matches!(response.status, DelegationStatus::Completed));
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(ledger.completions(), 1);

    // 错误的 token 一律拒绝。
    let unauthorized = endpoint(&format!("http://{address}"), Some("wrong"), 5);
    assert!(remote_ledger::claim(&unauthorized, "t2@0").is_err());
}

#[test]
fn two_nodes_share_ledger_and_execute_once() {
    let ledger = Arc::new(LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0")).unwrap());
    let ledger_address = ledger.addr().unwrap();
    let ledger_url = format!("http://{ledger_address}");
    let running_ledger = Arc::clone(&ledger);
    thread::spawn(move || running_ledger.serve_forever().unwrap());

    let node_a = Arc::new(
        RemoteServer::bind(
            RemoteServerConfig::new("127.0.0.1:0")
                .with_sleep_ms(500)
                .with_ledger(endpoint(&ledger_url, None, 5)),
        )
        .unwrap(),
    );
    let node_b = Arc::new(
        RemoteServer::bind(
            RemoteServerConfig::new("127.0.0.1:0")
                .with_sleep_ms(500)
                .with_ledger(endpoint(&ledger_url, None, 5)),
        )
        .unwrap(),
    );
    let url_a = format!("http://{}/run", node_a.addr().unwrap());
    let url_b = format!("http://{}/run", node_b.addr().unwrap());

    let server_a = Arc::clone(&node_a);
    let server_b = Arc::clone(&node_b);
    let handle_a = thread::spawn(move || server_a.serve_once().unwrap());
    let handle_b = thread::spawn(move || server_b.serve_once().unwrap());

    let barrier = Arc::new(Barrier::new(2));
    let barrier_a = Arc::clone(&barrier);
    let barrier_b = Arc::clone(&barrier);
    let handle_client_a = thread::spawn(move || {
        barrier_a.wait();
        run_remote_subagent(&endpoint(&url_a, None, 30), &request()).unwrap()
    });
    let handle_client_b = thread::spawn(move || {
        barrier_b.wait();
        run_remote_subagent(&endpoint(&url_b, None, 30), &request()).unwrap()
    });

    let result_a = handle_client_a.join().unwrap();
    let result_b = handle_client_b.join().unwrap();
    assert!(result_a.reusable(), "{:?}", result_a.status);
    assert!(result_b.reusable(), "{:?}", result_b.status);
    assert_eq!(result_a.digest, result_b.digest);
    // 两次 complete 调用才说明执行了两次；共享账本下必须只有一次。
    assert_eq!(ledger.completions(), 1, "duplicate remote execution");
    handle_a.join().unwrap();
    handle_b.join().unwrap();
}

#[test]
fn completed_records_survive_restart() {
    let path = state_file("restart");
    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"persisted".to_vec(),
        tokens: 0,
    })
    .unwrap();

    let ledger = Arc::new(
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)).unwrap(),
    );
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let running = Arc::clone(&ledger);
    let handle = thread::spawn(move || {
        running.serve_once().unwrap();
        running.serve_once().unwrap();
        running.serve_once().unwrap();
    });
    let Claim::New { lease } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() else {
        panic!("expected empty ledger to grant the claim");
    };
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease, &frame).unwrap();
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease, &frame).unwrap();
    handle.join().unwrap();
    drop(ledger);

    // 重启后从追加日志恢复：同一 key 直接回放，不再执行。
    let restarted = Arc::new(
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)).unwrap(),
    );
    let address = restarted.addr().unwrap();
    let restarted_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&restarted);
    let handle = thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });
    match remote_ledger::claim(&restarted_endpoint, "t1@0").unwrap() {
        Claim::Done(body) => {
            let mut buffer = body;
            let response: SubagentResponse = decode_frame(&mut buffer).unwrap().unwrap();
            assert_eq!(response.content, b"persisted");
        }
        other => panic!("expected persisted Done, got {other:?}"),
    }
    assert!(
        remote_ledger::complete(&restarted_endpoint, "t1@0", &lease, &frame).is_ok(),
        "the persisted owner may repeat its completion"
    );
    assert!(
        remote_ledger::complete(&restarted_endpoint, "t1@0", "0000000000000000", &frame).is_err(),
        "another lease must not be told that it wrote the persisted frame"
    );
    handle.join().unwrap();
    assert_eq!(restarted.completions(), 0);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("t1@0"), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("\"frame\""))
            .count(),
        1,
        "duplicate complete must not append a second done record: {text}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn pending_renewal_records_do_not_evict_done_on_restart() {
    let path = state_file("renew-window");
    let config = LedgerServerConfig::new("127.0.0.1:0")
        .with_pending_ttl(Duration::from_secs(5))
        .with_max_entries(2)
        .with_state_path(&path);
    let ledger = Arc::new(LedgerServer::bind(config).unwrap());
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"retained".to_vec(),
        tokens: 0,
    })
    .unwrap();
    let server = Arc::clone(&ledger);
    let handle = thread::spawn(move || {
        for _ in 0..3 {
            server.serve_once().unwrap();
        }
    });
    let Claim::New { lease: lease_one } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap()
    else {
        panic!("expected t1 claim");
    };
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease_one, &frame).unwrap();
    let Claim::New { lease: lease_two } = remote_ledger::claim(&ledger_endpoint, "t2@0").unwrap()
    else {
        panic!("expected t2 claim");
    };
    handle.join().unwrap();
    drop(ledger);

    // 10 条续租记录会把 t1 的完成帧挤出“最后 N 条”窗口；回放必须按 key 状态去重。
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_pending_ttl(Duration::from_secs(5))
                .with_max_entries(2)
                .with_state_path(&path),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&ledger);
    let handle = thread::spawn(move || {
        for _ in 0..10 {
            server.serve_once().unwrap();
        }
    });
    for _ in 0..10 {
        remote_ledger::renew(&ledger_endpoint, "t2@0", &lease_two).unwrap();
    }
    handle.join().unwrap();
    drop(ledger);

    let restarted = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_pending_ttl(Duration::from_secs(5))
                .with_max_entries(2)
                .with_state_path(&path),
        )
        .unwrap(),
    );
    let address = restarted.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&restarted);
    let handle = thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });
    assert!(
        matches!(
            remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
            Claim::Done(_)
        ),
        "done records must survive a long renewal log"
    );
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t2@0").unwrap(),
        Claim::Pending,
        "the active renewed lease must also survive"
    );
    handle.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn capacity_evictions_replay_in_the_same_key_order() {
    let path = state_file("capacity-order");
    let config = || {
        LedgerServerConfig::new("127.0.0.1:0")
            .with_pending_ttl(Duration::from_secs(5))
            .with_max_entries(2)
            .with_state_path(&path)
    };
    let ledger = Arc::new(LedgerServer::bind(config()).unwrap());
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&ledger);
    let handle = thread::spawn(move || {
        for _ in 0..6 {
            server.serve_once().unwrap();
        }
    });
    for task in ["y", "z", "a"] {
        let key = format!("{task}@0");
        let Claim::New { lease } = remote_ledger::claim(&ledger_endpoint, &key).unwrap() else {
            panic!("expected {key} claim");
        };
        let frame = encode_frame(&SubagentResponse {
            task: task.into(),
            epoch: 0,
            status: DelegationStatus::Completed,
            content: task.as_bytes().to_vec(),
            tokens: 0,
        })
        .unwrap();
        remote_ledger::complete(&ledger_endpoint, &key, &lease, &frame).unwrap();
    }
    handle.join().unwrap();
    drop(ledger);

    let restarted = Arc::new(LedgerServer::bind(config()).unwrap());
    let address = restarted.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&restarted);
    let handle = thread::spawn(move || {
        for _ in 0..3 {
            server.serve_once().unwrap();
        }
    });
    // 运行时先淘汰 y，再插入 a，最终是 z+a；回放不能因为 a 的 key 更小就反过来淘汰 a。
    assert!(matches!(
        remote_ledger::claim(&ledger_endpoint, "a@0").unwrap(),
        Claim::Done(_)
    ));
    assert!(matches!(
        remote_ledger::claim(&ledger_endpoint, "y@0").unwrap(),
        Claim::New { .. }
    ));
    assert!(matches!(
        remote_ledger::claim(&ledger_endpoint, "z@0").unwrap(),
        Claim::Done(_)
    ));
    handle.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn legacy_done_only_state_still_loads() {
    let path = state_file("legacy");
    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"legacy".to_vec(),
        tokens: 0,
    })
    .unwrap();
    let hex: String = frame.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::write(&path, format!("{{\"key\":\"t1@0\",\"frame\":\"{hex}\"}}\n")).unwrap();

    let ledger = Arc::new(
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)).unwrap(),
    );
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&ledger);
    let handle = thread::spawn(move || server.serve_once().unwrap());
    match remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() {
        Claim::Done(body) => {
            let mut buffer = body;
            let response: SubagentResponse = decode_frame(&mut buffer).unwrap().unwrap();
            assert_eq!(response.content, b"legacy");
        }
        other => panic!("expected legacy Done, got {other:?}"),
    }
    handle.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn pending_lease_survives_restart_and_expires() {
    let path = state_file("pending");
    let ttl = Duration::from_secs(1);
    let ledger = LedgerServer::bind(
        LedgerServerConfig::new("127.0.0.1:0")
            .with_pending_ttl(ttl)
            .with_state_path(&path),
    )
    .unwrap();
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let running = Arc::new(ledger);
    let server = Arc::clone(&running);
    let handle = thread::spawn(move || server.serve_once().unwrap());
    let Claim::New { lease: lease_one } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap()
    else {
        panic!("expected first claim to win");
    };
    handle.join().unwrap();
    drop(running);

    // 占位和令牌已落盘，重启后不能被其他节点立刻接管，续租也仍然可用。
    assert!(
        std::fs::read_to_string(&path).unwrap().contains(&lease_one),
        "pending lease must be durable"
    );
    let restarted = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_pending_ttl(ttl)
                .with_state_path(&path),
        )
        .unwrap(),
    );
    let address = restarted.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&restarted);
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            server.serve_once().unwrap();
        }
    });
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending,
        "a durable lease must still block takeover after restart"
    );
    remote_ledger::renew(&ledger_endpoint, "t1@0", &lease_one).unwrap();
    handle.join().unwrap();
    drop(restarted);

    // 原始 TTL 已过，但续租记录也落盘了：再次重启仍保持 Pending。
    thread::sleep(Duration::from_millis(600));
    let renewed = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_pending_ttl(ttl)
                .with_state_path(&path),
        )
        .unwrap(),
    );
    let address = renewed.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"after-restart".to_vec(),
        tokens: 0,
    })
    .unwrap();
    let server = Arc::clone(&renewed);
    let handle = thread::spawn(move || {
        for _ in 0..3 {
            server.serve_once().unwrap();
        }
    });
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending,
        "a renewed lease must survive restart past the original TTL"
    );
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease_one, &frame).unwrap();
    let Claim::New { lease: lease_two } = remote_ledger::claim(&ledger_endpoint, "t2@0").unwrap()
    else {
        panic!("expected t2 claim to win");
    };
    handle.join().unwrap();
    drop(renewed);

    let frame_two = encode_frame(&SubagentResponse {
        task: "t2".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"stale".to_vec(),
        tokens: 0,
    })
    .unwrap();

    // 超过 TTL 再重启：过期占位被丢弃，旧令牌不能写完成。
    thread::sleep(Duration::from_millis(1100));
    let expired = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_pending_ttl(ttl)
                .with_state_path(&path),
        )
        .unwrap(),
    );
    let address = expired.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&expired);
    let handle = thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });
    assert!(
        matches!(
            remote_ledger::claim(&ledger_endpoint, "t2@0").unwrap(),
            Claim::New { .. }
        ),
        "an expired lease must be claimable again"
    );
    assert!(
        remote_ledger::complete(&ledger_endpoint, "t2@0", &lease_two, &frame_two).is_err(),
        "the stale owner must not complete after expiry"
    );
    handle.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn scoped_ledger_keys_isolate_plans() {
    let ledger = Arc::new(LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0")).unwrap());
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"scoped".to_vec(),
        tokens: 0,
    })
    .unwrap();

    let Claim::New { lease } = remote_ledger::claim(&ledger_endpoint, "plan-a|t1@0").unwrap()
    else {
        panic!("expected scoped claim to win");
    };
    remote_ledger::complete(&ledger_endpoint, "plan-a|t1@0", &lease, &frame).unwrap();
    assert!(matches!(
        remote_ledger::claim(&ledger_endpoint, "plan-a|t1@0").unwrap(),
        Claim::Done(_)
    ));
    assert!(
        matches!(
            remote_ledger::claim(&ledger_endpoint, "plan-b|t1@0").unwrap(),
            Claim::New { .. }
        ),
        "same task in another scope must not see plan-a's completion"
    );
    assert!(
        remote_ledger::complete(&ledger_endpoint, "plan-a|t2@0", &lease, &frame).is_err(),
        "complete must still verify the task@epoch suffix"
    );
}

#[test]
fn state_file_respects_byte_budget_and_compacts() {
    let path = state_file("bounded");
    let config = LedgerServerConfig::new("127.0.0.1:0")
        .with_max_state_bytes(1024)
        .with_state_path(&path);
    let ledger = Arc::new(LedgerServer::bind(config).unwrap());
    let address = ledger.addr().unwrap();
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let running = Arc::clone(&ledger);
    let handle = thread::spawn(move || {
        for _ in 0..16 {
            running.serve_once().unwrap();
        }
    });
    for epoch in 0..8 {
        let key = format!("plan|t1@{epoch}");
        let Claim::New { lease } = remote_ledger::claim(&ledger_endpoint, &key).unwrap() else {
            panic!("expected fresh claim for {key}");
        };
        let frame = encode_frame(&SubagentResponse {
            task: "t1".into(),
            epoch,
            status: DelegationStatus::Completed,
            content: vec![b'x'; 80],
            tokens: 0,
        })
        .unwrap();
        remote_ledger::complete(&ledger_endpoint, &key, &lease, &frame).unwrap();
    }
    handle.join().unwrap();
    drop(ledger);

    let physical = std::fs::metadata(&path).unwrap().len();
    assert!(
        physical <= 1900,
        "compaction must bound the physical log near the cap, got {physical} bytes"
    );

    // 重启触发压缩并回放：最旧键已被字节上限淘汰，最新键仍可回放。
    let restarted = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_max_state_bytes(1024)
                .with_state_path(&path),
        )
        .unwrap(),
    );
    let address = restarted.addr().unwrap();
    let restarted_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&restarted);
    let handle = thread::spawn(move || {
        server.serve_once().unwrap();
        server.serve_once().unwrap();
    });
    assert!(matches!(
        remote_ledger::claim(&restarted_endpoint, "plan|t1@7").unwrap(),
        Claim::Done(_)
    ));
    assert!(
        matches!(
            remote_ledger::claim(&restarted_endpoint, "plan|t1@0").unwrap(),
            Claim::New { .. }
        ),
        "the byte cap must have evicted the oldest record"
    );
    handle.join().unwrap();
    assert_eq!(restarted.completions(), 0);
    let physical = std::fs::metadata(&path).unwrap().len();
    assert!(
        physical <= 1024,
        "startup compaction must bring the log under the cap, got {physical} bytes"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn renew_extends_pending_lease() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_millis(100)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    let Claim::New { lease } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() else {
        panic!("expected empty ledger to grant the claim");
    };
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(60));
        remote_ledger::renew(&ledger_endpoint, "t1@0", &lease).unwrap();
    }
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending,
        "renewed lease must still be owned"
    );
    thread::sleep(Duration::from_millis(150));
    assert!(
        matches!(
            remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
            Claim::New { .. }
        ),
        "lease must expire after renewals stop"
    );
    assert!(
        remote_ledger::renew(&ledger_endpoint, "never-claimed", &lease).is_err(),
        "unknown keys are not renewable"
    );
}

#[test]
fn slow_remote_execution_heartbeat_prevents_lease_takeover() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_millis(150)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    let node = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(500)
            .with_ledger(ledger_endpoint.clone())
            .with_ledger_renew_interval(Duration::from_millis(40)),
    )
    .unwrap();
    let url = format!("http://{}/run", node.addr().unwrap());
    let server = thread::spawn(move || node.serve_once().unwrap());
    let endpoint_for_call = endpoint(&url, None, 30);
    let request_for_call = request();
    let client = thread::spawn(move || run_remote_subagent(&endpoint_for_call, &request_for_call));

    // 执行总耗时 500ms，占位 TTL 只有 150ms；心跳必须让 lease 一直有效。
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending,
        "a live execution must keep its ledger lease"
    );
    let result = client.join().unwrap().unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    server.join().unwrap();
    assert!(matches!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Done(_)
    ));
    assert_eq!(ledger.completions(), 1);
}

#[test]
fn lease_loss_aborts_in_flight_execution() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_millis(100)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    let node = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(3000)
            .with_ledger(ledger_endpoint.clone())
            .with_ledger_renew_interval(Duration::from_millis(800)),
    )
    .unwrap();
    let url = format!("http://{}/run", node.addr().unwrap());
    let server = thread::spawn(move || node.serve_once().unwrap());
    let endpoint_for_call = endpoint(&url, None, 30);
    let request_for_call = request();
    let started = Instant::now();
    let client =
        thread::spawn(move || run_remote_subagent_reply(&endpoint_for_call, &request_for_call));

    // 等节点拿到占位且其短 TTL 已过，再声称接管；下一次 800ms 心跳会拿到 403。
    thread::sleep(Duration::from_millis(500));
    loop {
        match remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() {
            Claim::New { .. } => break,
            Claim::Pending => thread::sleep(Duration::from_millis(25)),
            Claim::Done(_) => panic!("unexpected completion before takeover"),
        }
    }

    let reply = client.join().unwrap().unwrap();
    assert!(
        reply.lease_lost,
        "server must mark a lost lease so the scheduler does not blame the node"
    );
    assert!(
        matches!(reply.result.status, DelegationStatus::Aborted),
        "a lost lease must abort in-flight work, got {:?}",
        reply.result.status
    );
    assert!(!reply.result.reusable());
    assert!(
        started.elapsed() < Duration::from_millis(2800),
        "execution must abort before the 3s sleep completes, took {:?}",
        started.elapsed()
    );
    server.join().unwrap();
    assert_eq!(
        ledger.completions(),
        0,
        "the stale owner must not write a completion"
    );
}

#[test]
fn stale_completion_is_aborted_instead_of_returned() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_millis(50)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    let node = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(300)
            .with_ledger(ledger_endpoint.clone())
            // 心跳晚于执行结束：让“登记完成时才发现租约丢失”的路径必然发生。
            .with_ledger_renew_interval(Duration::from_secs(30)),
    )
    .unwrap();
    let url = format!("http://{}/run", node.addr().unwrap());
    let server = thread::spawn(move || node.serve_once().unwrap());
    let client =
        thread::spawn(move || run_remote_subagent_reply(&endpoint(&url, None, 10), &request()));

    // node 已开始 300ms 睡眠；TTL 过期后外部持有者接管并写入自己的完成帧。
    thread::sleep(Duration::from_millis(200));
    let lease = loop {
        match remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() {
            Claim::New { lease } => break lease,
            Claim::Pending => thread::sleep(Duration::from_millis(25)),
            Claim::Done(_) => panic!("unexpected completion before takeover"),
        }
    };
    let winner = SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"winner".to_vec(),
        tokens: 0,
    };
    let frame = encode_frame(&winner).unwrap();
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease, &frame).unwrap();

    let reply = client.join().unwrap().unwrap();
    assert!(
        reply.lease_lost,
        "a completion rejected by the ledger must be reported as lease loss"
    );
    assert!(
        matches!(reply.result.status, DelegationStatus::Aborted),
        "the stale owner must not return its own Completed frame, got {:?}",
        reply.result.status
    );
    assert!(reply.content.is_empty());
    assert_eq!(ledger.completions(), 1, "only the new owner's frame wins");
    server.join().unwrap();
}

#[test]
fn deadline_does_not_register_a_completion() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_secs(5)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    let node = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(300)
            .with_exec_timeout(Duration::from_millis(100))
            .with_ledger(ledger_endpoint.clone()),
    )
    .unwrap();
    let url = format!("http://{}/run", node.addr().unwrap());
    let server = thread::spawn(move || node.serve_once().unwrap());

    let reply = run_remote_subagent_reply(&endpoint(&url, None, 10), &request()).unwrap();
    assert!(matches!(reply.result.status, DelegationStatus::Aborted));
    assert!(reply.deadline_exceeded);
    server.join().unwrap();
    assert_eq!(
        ledger.completions(),
        0,
        "a timed-out execution must not become the authoritative completion"
    );
    // 占位仍在，直到 TTL 过期；调用方不会看到伪造的 Done。
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending
    );
}

#[test]
fn deadline_covers_ledger_wait_and_keeps_the_timeout_flag() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_secs(5)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    // 另一个持有者先占位：本节点会停在 claim 等待，直到自己的 deadline 到点。
    assert!(matches!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::New { .. }
    ));

    let node = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_exec_timeout(Duration::from_millis(100))
            .with_ledger(ledger_endpoint.clone()),
    )
    .unwrap();
    let url = format!("http://{}/run", node.addr().unwrap());
    let server = thread::spawn(move || node.serve_once().unwrap());

    let reply = run_remote_subagent_reply(&endpoint(&url, None, 10), &request()).unwrap();
    assert!(matches!(reply.result.status, DelegationStatus::Aborted));
    assert!(
        reply.deadline_exceeded,
        "a deadline hit while waiting for the ledger must stay distinguishable"
    );
    server.join().unwrap();
    assert_eq!(ledger.completions(), 0);
}

#[test]
fn superseded_node_stays_healthy_and_retry_reads_completion() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0").with_pending_ttl(Duration::from_millis(100)),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), None, 5);

    let node_a = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_sleep_ms(3000)
            .with_ledger(ledger_endpoint.clone())
            .with_ledger_renew_interval(Duration::from_millis(800)),
    )
    .unwrap();
    let node_b = RemoteServer::bind(
        RemoteServerConfig::new("127.0.0.1:0")
            .with_ledger(ledger_endpoint.clone())
            .with_ledger_renew_interval(Duration::from_millis(800)),
    )
    .unwrap();
    let url_a = format!("http://{}/run", node_a.addr().unwrap());
    let url_b = format!("http://{}/run", node_b.addr().unwrap());
    let server_a = thread::spawn(move || node_a.serve_once().unwrap());
    let server_b = thread::spawn(move || node_b.serve_once().unwrap());

    let table = Arc::new(
        NodeTable::new(vec![
            RemoteNode::new("node-a", endpoint(&url_a, None, 30)),
            RemoteNode::new("node-b", endpoint(&url_b, None, 30)),
        ])
        .unwrap(),
    );
    let plan = Plan::new(1, vec![task("t1")]).unwrap();
    let config = RemotePlanConfig::new(1)
        .with_failure_threshold(1)
        .with_retry_limit(1);
    let table_for_run = Arc::clone(&table);
    let started = Instant::now();
    let runner = thread::spawn(move || {
        run_remote_plan_with_table(
            &plan,
            table_for_run,
            config,
            CancelToken::new(),
            request_for,
        )
    });

    // node-a 先拿到占位并开始 3s 睡眠；外部持有者在 TTL 过期后接管并立刻完成。
    thread::sleep(Duration::from_millis(500));
    let lease = loop {
        match remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() {
            Claim::New { lease } => break lease,
            Claim::Pending => thread::sleep(Duration::from_millis(25)),
            Claim::Done(_) => panic!("unexpected completion before takeover"),
        }
    };
    let external = SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"external-completion".to_vec(),
        tokens: 7,
    };
    let frame = encode_frame(&external).unwrap();
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease, &frame).unwrap();

    let report = runner.join().unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "node-a must stop at its next heartbeat instead of sleeping 3s, took {:?}",
        started.elapsed()
    );
    assert_eq!(report.report.completed, vec!["t1".to_string()]);
    assert!(report.report.failed.is_none());

    let attempts = &report.attempts["t1"];
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].node, "node-a");
    assert!(matches!(attempts[0].status, DelegationStatus::Aborted));
    assert_eq!(attempts[1].node, "node-b");
    assert!(matches!(attempts[1].status, DelegationStatus::Completed));

    let node_a = report
        .nodes
        .iter()
        .find(|node| node.id == "node-a")
        .unwrap();
    assert!(node_a.healthy, "lease takeover is not a node failure");
    assert_eq!(node_a.failures, 0);
    assert_eq!(node_a.attempts, 1);
    let node_b = report
        .nodes
        .iter()
        .find(|node| node.id == "node-b")
        .unwrap();
    assert!(node_b.healthy);
    assert_eq!(node_b.attempts, 1);

    server_a.join().unwrap();
    server_b.join().unwrap();
    assert_eq!(
        ledger.completions(),
        1,
        "only the new owner writes the completion"
    );
}

#[test]
fn unreachable_ledger_fails_closed_without_executing() {
    let dead_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let node = RemoteServer::bind(RemoteServerConfig::new("127.0.0.1:0").with_ledger(endpoint(
        &format!("http://127.0.0.1:{dead_port}"),
        None,
        1,
    )))
    .unwrap();
    let url = format!("http://{}/run", node.addr().unwrap());
    let server = thread::spawn(move || node.serve_once().unwrap());

    let result = run_remote_subagent(&endpoint(&url, None, 30), &request()).unwrap();
    assert!(
        matches!(&result.status, DelegationStatus::Failed { message } if message.contains("503")),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
    server.join().unwrap();
}

#[test]
fn token_budget_blocks_new_claims_after_spend() {
    let ledger = Arc::new(
        LedgerServer::bind(
            LedgerServerConfig::new("127.0.0.1:0")
                .with_token("t")
                .with_token_budget(10),
        )
        .unwrap(),
    );
    let address = ledger.addr().unwrap();
    let running = Arc::clone(&ledger);
    thread::spawn(move || running.serve_forever().unwrap());
    let ledger_endpoint = endpoint(&format!("http://{address}"), Some("t"), 5);

    let Claim::New { lease } = remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap() else {
        panic!("expected first claim to win");
    };
    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"done".to_vec(),
        tokens: 10,
    })
    .unwrap();
    remote_ledger::complete(&ledger_endpoint, "t1@0", &lease, &frame).unwrap();

    // 预算已用尽：新键 claim 直接 503，客户端报错。
    let error = remote_ledger::claim(&ledger_endpoint, "t2@0").unwrap_err();
    assert!(error.contains("503"), "{error}");
}
