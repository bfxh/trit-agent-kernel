//! 共享幂等账本：claim/complete 协议，以及两台远端共享账本时的单次执行。

use std::net::TcpListener;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use trit_host::remote_ledger::{self, Claim};
use trit_host::{
    LedgerServer, LedgerServerConfig, RemoteServer, RemoteServerConfig, run_remote_subagent,
};
use trit_orch::{
    AccessMode, CapsuleEntry, ContextCapsule, DelegationStatus, RemoteEndpoint, SubagentProfile,
    SubagentRequest, SubagentResponse, Task, TaskSpec, decode_frame, encode_frame,
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
    SubagentRequest::new(&task("t1"), capsule, profile)
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

    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::New
    );
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::Pending
    );

    // 占位过期后可被接管。
    thread::sleep(Duration::from_millis(160));
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::New
    );

    let frame = encode_frame(&SubagentResponse {
        task: "t1".into(),
        epoch: 0,
        status: DelegationStatus::Completed,
        content: b"done".to_vec(),
        tokens: 0,
    })
    .unwrap();
    remote_ledger::complete(&ledger_endpoint, "t1@0", &frame).unwrap();
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
    });
    assert_eq!(
        remote_ledger::claim(&ledger_endpoint, "t1@0").unwrap(),
        Claim::New
    );
    remote_ledger::complete(&ledger_endpoint, "t1@0", &frame).unwrap();
    handle.join().unwrap();
    drop(ledger);

    // 重启后从追加日志恢复：同一 key 直接回放，不再执行。
    let restarted = Arc::new(
        LedgerServer::bind(LedgerServerConfig::new("127.0.0.1:0").with_state_path(&path)).unwrap(),
    );
    let address = restarted.addr().unwrap();
    let restarted_endpoint = endpoint(&format!("http://{address}"), None, 5);
    let server = Arc::clone(&restarted);
    let handle = thread::spawn(move || server.serve_once().unwrap());
    match remote_ledger::claim(&restarted_endpoint, "t1@0").unwrap() {
        Claim::Done(body) => {
            let mut buffer = body;
            let response: SubagentResponse = decode_frame(&mut buffer).unwrap().unwrap();
            assert_eq!(response.content, b"persisted");
        }
        other => panic!("expected persisted Done, got {other:?}"),
    }
    handle.join().unwrap();
    assert_eq!(restarted.completions(), 0);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("t1@0"), "{text}");
    let _ = std::fs::remove_file(&path);
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
