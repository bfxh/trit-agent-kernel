//! 跨机派发：真实双节点 loopback、失败隔离、有界重试与幂等键复用。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use trit_host::{RemotePlanConfig, RemoteProbePhase, run_remote_plan, run_remote_plan_with_table};
use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, DelegationStatus, Dependency,
    NodeSnapshot, NodeTable, Plan, RemoteEndpoint, RemoteNode, SubagentProfile, SubagentRequest,
    SubagentResponse, Task, TaskSpec, Ternary, decode_frame, encode_frame,
};

struct TestNode {
    id: String,
    base: String,
    max_in_flight: usize,
    timeout_secs: u64,
    child: Child,
}

impl TestNode {
    fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    fn remote(&self) -> RemoteNode {
        RemoteNode::new(
            self.id.clone(),
            RemoteEndpoint {
                url: format!("{}/run", self.base),
                auth_token: None,
                timeout_secs: self.timeout_secs,
            },
        )
        .with_max_in_flight(self.max_in_flight)
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 起一个真实 `trit-host remote-echo`；stdout 首行是实际端口。
fn spawn_node(id: &str, capacity: usize, args: &[&str]) -> TestNode {
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
    let base = format!("http://{}", value["bind"].as_str().unwrap());
    TestNode {
        id: id.into(),
        base,
        max_in_flight: capacity,
        timeout_secs: 5,
        child,
    }
}

/// 起一个只接受一次 `/health`、延迟后回 200 的节点，用于测探测并发。
fn delayed_health_node(id: &str, delay: Duration) -> (RemoteNode, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        std::thread::sleep(delay);
        let _ =
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.flush();
    });
    let node = RemoteNode::new(
        id,
        RemoteEndpoint {
            url: format!("{base}/run"),
            auth_token: None,
            timeout_secs: 5,
        },
    );
    (node, handle)
}

fn task(id: &str, deps: &[&str]) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps: deps
                .iter()
                .map(|dependency| Dependency {
                    task: (*dependency).into(),
                    required: Ternary::True,
                })
                .collect(),
        },
        epoch: 0,
        ..TaskSpec::default()
    }
}

fn plan(tasks: Vec<TaskSpec>) -> Plan {
    Plan::new(1, tasks).unwrap()
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
        name: format!("{}@remote", task.task.id),
        access: AccessMode::ReadOnly,
        write_roots: Vec::new(),
        tools: vec!["read_file".into()],
        max_tokens: 100,
    };
    SubagentRequest::new(task, capsule, profile)
}

#[test]
fn library_preflight_readmits_an_isolated_node() {
    let node = spawn_node("recovered", 1, &[]);
    let table = NodeTable::new(vec![node.remote()])
        .unwrap()
        .with_snapshots(&[NodeSnapshot {
            id: "recovered".into(),
            healthy: false,
            in_flight: 0,
            attempts: 0,
            failures: 1,
            updated_at_ms: 0,
        }]);

    let report = run_remote_plan_with_table(
        &plan(vec![task("t1", &[])]),
        Arc::new(table),
        RemotePlanConfig::new(1).with_health_probe_on_start(),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.report.completed.len(), 1, "{:?}", report.report);
    assert!(report.reusable("t1"), "{:?}", report.result("t1"));
    assert_eq!(report.probes.len(), 1, "{:?}", report.probes);
    let probe = &report.probes[0];
    assert_eq!(probe.node, "recovered");
    assert_eq!(probe.phase, RemoteProbePhase::Preflight);
    assert!(probe.ok);
    assert!(probe.readmitted);
    assert!(probe.error.is_none(), "{probe:?}");
    assert!(report.nodes[0].healthy, "{:?}", report.nodes);
}

#[test]
fn library_stale_healthy_snapshot_requires_a_probe() {
    let node = spawn_node("stale", 1, &[]);
    let stale = NodeSnapshot {
        id: "stale".into(),
        healthy: true,
        in_flight: 0,
        attempts: 3,
        failures: 1,
        updated_at_ms: 0,
    };

    let table = NodeTable::new(vec![node.remote()])
        .unwrap()
        .with_snapshots(std::slice::from_ref(&stale));
    let report = run_remote_plan_with_table(
        &plan(vec![task("t1", &[])]),
        Arc::new(table),
        RemotePlanConfig::new(1).with_health_max_age(Duration::from_millis(1_000)),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.stale_nodes, vec!["stale"], "{:?}", report.nodes);
    assert!(!report.nodes[0].healthy, "{:?}", report.nodes);
    assert_eq!(report.nodes[0].attempts, 3, "新鲜度隔离保留累计尝试数");
    assert_eq!(report.nodes[0].failures, 1, "新鲜度隔离保留累计失败数");
    assert!(report.report.completed.is_empty(), "{:?}", report.report);
    assert!(report.probes.is_empty(), "未配探测时不应自行发请求");

    // 同一份超龄快照在 preflight 探测成功后重新放行并完成任务。
    let table = NodeTable::new(vec![node.remote()])
        .unwrap()
        .with_snapshots(&[stale]);
    let report = run_remote_plan_with_table(
        &plan(vec![task("t1", &[])]),
        Arc::new(table),
        RemotePlanConfig::new(1)
            .with_health_max_age(Duration::from_millis(1_000))
            .with_health_probe_on_start(),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.stale_nodes, vec!["stale"], "{:?}", report.nodes);
    assert_eq!(report.report.completed.len(), 1, "{:?}", report.report);
    assert!(report.reusable("t1"), "{:?}", report.result("t1"));
    assert_eq!(report.probes.len(), 1, "{:?}", report.probes);
    assert!(report.probes[0].readmitted, "{:?}", report.probes);
    assert!(report.nodes[0].healthy, "{:?}", report.nodes);
}

#[test]
fn library_preflight_probes_isolated_nodes_with_bounded_concurrency() {
    let worker = spawn_node("a-worker", 1, &[]);
    let delay = Duration::from_millis(250);
    let (mut nodes, mut handles): (Vec<RemoteNode>, Vec<JoinHandle<()>>) = (0..4)
        .map(|index| delayed_health_node(&format!("n{index}"), delay))
        .unzip();
    nodes.push(worker.remote());
    let snapshots: Vec<NodeSnapshot> = nodes
        .iter()
        .filter(|node| node.id != "a-worker")
        .map(|node| NodeSnapshot {
            id: node.id.clone(),
            healthy: false,
            in_flight: 0,
            attempts: 2,
            failures: 1,
            updated_at_ms: 0,
        })
        .collect();
    let table = NodeTable::new(nodes).unwrap().with_snapshots(&snapshots);

    let started = Instant::now();
    let report = run_remote_plan_with_table(
        &plan(vec![task("t1", &[])]),
        Arc::new(table),
        RemotePlanConfig::new(1)
            .with_health_probe_on_start()
            .with_health_probe_concurrency(4),
        CancelToken::new(),
        request_for,
    )
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(report.report.completed.len(), 1, "{:?}", report.report);
    assert_eq!(report.probes.len(), 4, "{:?}", report.probes);
    let event_nodes: Vec<&str> = report
        .probes
        .iter()
        .map(|probe| probe.node.as_str())
        .collect();
    assert_eq!(
        event_nodes,
        ["n0", "n1", "n2", "n3"],
        "并发 preflight 的事件仍按节点 id 升序"
    );
    assert!(
        report
            .probes
            .iter()
            .all(|probe| probe.ok && probe.readmitted),
        "{:?}",
        report.probes
    );
    assert!(
        elapsed < Duration::from_millis(700),
        "4 个 250ms 探测串行需 1s；并发应明显更快，实际 {elapsed:?}"
    );
    for handle in handles.drain(..) {
        handle.join().unwrap();
    }
}

#[test]
fn library_preflight_readmits_a_recovered_node_before_the_slowest_probe() {
    let (fast, fast_handle) = delayed_health_node("fast", Duration::from_millis(50));
    let (slow, slow_handle) = delayed_health_node("slow", Duration::from_millis(600));
    let snapshot = |id: &str| NodeSnapshot {
        id: id.into(),
        healthy: false,
        in_flight: 0,
        attempts: 1,
        failures: 1,
        updated_at_ms: 0,
    };
    let table = Arc::new(
        NodeTable::new(vec![fast, slow])
            .unwrap()
            .with_snapshots(&[snapshot("fast"), snapshot("slow")]),
    );

    let started = Instant::now();
    let observed = Arc::new(Mutex::new(None::<u64>));
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let table = Arc::clone(&table);
        let observed = Arc::clone(&observed);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let snapshots = table.snapshot();
                if let Some(fast) = snapshots.iter().find(|snapshot| snapshot.id == "fast")
                    && fast.healthy
                {
                    let mut slot = observed.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(started.elapsed().as_millis() as u64);
                    }
                }
                if snapshots.iter().all(|snapshot| snapshot.healthy) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let outcome = run_remote_plan_with_table(
        &plan(vec![]),
        Arc::clone(&table),
        RemotePlanConfig::new(2)
            .with_health_probe_on_start()
            .with_health_probe_concurrency(2),
        CancelToken::new(),
        request_for,
    );
    stop.store(true, Ordering::Relaxed);
    poller.join().unwrap();
    let report = outcome.unwrap();

    let fast_observed = observed
        .lock()
        .unwrap()
        .expect("poll should observe the fast node recover");
    assert!(
        fast_observed < 300,
        "fast node should be readmitted at ~50ms, but was observed at {fast_observed}ms; \
         slow probe must not hold the whole scan"
    );
    assert_eq!(report.probes.len(), 2, "{:?}", report.probes);
    let event_nodes: Vec<&str> = report
        .probes
        .iter()
        .map(|probe| probe.node.as_str())
        .collect();
    assert_eq!(event_nodes, ["fast", "slow"], "事件仍按 id 升序");
    assert!(
        report
            .probes
            .iter()
            .all(|probe| probe.ok && probe.readmitted),
        "{:?}",
        report.probes
    );
    fast_handle.join().unwrap();
    slow_handle.join().unwrap();
}

#[test]
fn plan_spreads_across_two_nodes() {
    let node_a = spawn_node("node-a", 1, &[]);
    let node_b = spawn_node("node-b", 1, &[]);
    let plan = plan(vec![
        task("t1", &[]),
        task("t2", &[]),
        task("t3", &[]),
        task("t4", &[]),
    ]);

    let report = run_remote_plan(
        &plan,
        vec![node_a.remote(), node_b.remote()],
        RemotePlanConfig::new(2),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.report.completed.len(), 4, "{:?}", report.report);
    assert!(report.report.failed.is_none());
    assert!(report.report.deferred.is_empty());
    for task in ["t1", "t2", "t3", "t4"] {
        assert!(report.reusable(task), "{task}: {:?}", report.result(task));
    }

    assert_eq!(report.assignments.len(), 4);
    let used: std::collections::BTreeSet<&str> =
        report.assignments.values().map(String::as_str).collect();
    assert_eq!(used, ["node-a", "node-b"].into_iter().collect());

    let nodes: BTreeMap<&str, &trit_orch::NodeSnapshot> = report
        .nodes
        .iter()
        .map(|snapshot| (snapshot.id.as_str(), snapshot))
        .collect();
    assert!(nodes["node-a"].healthy && nodes["node-b"].healthy);
    assert_eq!(nodes["node-a"].attempts + nodes["node-b"].attempts, 4);
    assert_eq!(nodes["node-a"].failures, 0);
}

#[test]
fn unresponsive_node_is_quarantined_and_plan_stops() {
    // node-a 5s 不响应、客户端 1s 超时 → Aborted（节点级失败，被隔离）；
    // node-b 1100ms 回包，它上面的任务在隔离之后才完成；剩余任务不再启动。
    let node_a = spawn_node("node-a", 1, &["--sleep-ms", "5000"]).with_timeout(1);
    let node_b = spawn_node("node-b", 1, &["--sleep-ms", "1100"]);
    let plan = plan(vec![
        task("t1", &[]),
        task("t2", &[]),
        task("t3", &[]),
        task("t4", &[]),
    ]);

    let report = run_remote_plan(
        &plan,
        vec![node_a.remote(), node_b.remote()],
        RemotePlanConfig::new(2),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.report.completed.len(), 1, "{:?}", report.report);
    assert!(
        report
            .report
            .failed
            .as_ref()
            .unwrap()
            .message
            .contains("node-a")
    );
    assert_eq!(report.report.deferred.len(), 2, "{:?}", report.report);

    let completed = &report.report.completed[0];
    assert!(report.reusable(completed));
    assert_eq!(report.node_of(completed), Some("node-b"));

    let failure = report.report.failed.as_ref().unwrap();
    assert_eq!(report.node_of(&failure.task), Some("node-a"));
    // 远端没有合法响应：精确结局是 Aborted，不会被汇总改写成 Failed。
    assert!(
        matches!(
            report.result(&failure.task).map(|result| &result.status),
            Some(DelegationStatus::Aborted)
        ),
        "{:?}",
        report.result(&failure.task)
    );
    for task in &report.report.deferred {
        assert!(matches!(
            report.result(task).map(|result| &result.status),
            Some(DelegationStatus::Deferred)
        ));
    }

    let nodes: BTreeMap<&str, &trit_orch::NodeSnapshot> = report
        .nodes
        .iter()
        .map(|snapshot| (snapshot.id.as_str(), snapshot))
        .collect();
    assert!(!nodes["node-a"].healthy);
    assert!(nodes["node-b"].healthy);
}

#[test]
fn remote_task_failures_never_report_completion() {
    let node = spawn_node("only", 1, &["--fail"]);
    let plan = plan(vec![task("t1", &[]), task("t2", &[])]);

    let report = run_remote_plan(
        &plan,
        vec![node.remote()],
        RemotePlanConfig::new(1),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert!(report.report.completed.is_empty());
    assert!(report.report.failed.is_some());
    assert!(!report.reusable("t1") && !report.reusable("t2"));
    // 远端回了 Failed：任务失败，但节点仍然健康（不是节点级故障）。
    assert!(report.nodes[0].healthy);
    assert_eq!(report.nodes[0].attempts, 1);
    assert_eq!(report.nodes[0].failures, 1);
}

#[test]
fn pre_cancelled_plan_dispatches_nothing() {
    let node = spawn_node("node", 1, &[]);
    let plan = plan(vec![task("t1", &[]), task("t2", &[])]);
    let cancel = CancelToken::new();
    cancel.cancel();

    let report = run_remote_plan(
        &plan,
        vec![node.remote()],
        RemotePlanConfig::new(1),
        cancel,
        request_for,
    )
    .unwrap();

    assert!(report.report.completed.is_empty());
    assert!(report.assignments.is_empty());
    assert_eq!(report.nodes[0].attempts, 0);
    assert!(report.report.cancelled.len() == 2);
    for task in ["t1", "t2"] {
        assert!(matches!(
            report.result(task).map(|result| &result.status),
            Some(DelegationStatus::Aborted)
        ));
    }
}

#[test]
fn retry_switches_node_and_reuses_idempotency_key() {
    // 同一个端点被登记成两个节点 id：第一次连接直接断开（Aborted），第二次回 Completed。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let keys: Arc<Mutex<Vec<String>>> = Arc::default();
    let handle: JoinHandle<()> = {
        let keys = Arc::clone(&keys);
        std::thread::spawn(move || {
            for attempt in 0..2 {
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
                let request: SubagentRequest = decode_frame(&mut buffer).unwrap().unwrap();
                keys.lock().unwrap().push(idempotency.unwrap_or_default());

                if attempt == 0 {
                    // 不回任何 HTTP 响应就断开：客户端只能判 Aborted。
                    continue;
                }
                let response = SubagentResponse {
                    task: request.task.clone(),
                    epoch: request.epoch,
                    status: DelegationStatus::Completed,
                    content: b"retried".to_vec(),
                    tokens: 0,
                    model: None,
                    usage: None,
                    dissent: None,
                };
                let frame = encode_frame(&response).unwrap();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    trit_host::FRAME_CONTENT_TYPE,
                    frame.len()
                );
                let _ = stream.write_all(&frame);
                let _ = stream.flush();
            }
        })
    };

    let endpoint = RemoteEndpoint {
        url: format!("http://{address}/run"),
        auth_token: None,
        timeout_secs: 5,
    };
    let plan = plan(vec![task("t1", &[])]);
    let report = run_remote_plan(
        &plan,
        vec![
            RemoteNode::new("node-a", endpoint.clone()),
            RemoteNode::new("node-b", endpoint),
        ],
        RemotePlanConfig::new(1).with_retry_limit(1),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.report.completed, ["t1"], "{:?}", report.report);
    assert!(report.reusable("t1"));
    assert_eq!(report.node_of("t1"), Some("node-b"), "重试必须换节点");
    assert_eq!(report.attempts_of("t1").len(), 2);
    assert_eq!(report.attempts_of("t1")[0].node, "node-a");
    assert!(matches!(
        report.attempts_of("t1")[0].status,
        DelegationStatus::Aborted
    ));
    assert_eq!(report.attempts_of("t1")[1].node, "node-b");
    assert!(matches!(
        report.attempts_of("t1")[1].status,
        DelegationStatus::Completed
    ));

    let keys = keys.lock().unwrap().clone();
    assert_eq!(keys, ["t1@0", "t1@0"], "两次尝试必须复用同一个幂等键");
    let _ = handle.join();
}

#[test]
fn deadline_miss_is_retried_without_quarantining_the_node() {
    // node-a 自己配了 250ms 请求上限；它仍能回合法 Aborted，只是本次超预算。
    let slow = spawn_node(
        "node-a",
        1,
        &["--sleep-ms", "2000", "--request-timeout-ms", "250"],
    );
    let fast = spawn_node("node-b", 1, &[]);
    let plan = plan(vec![task("t1", &[])]);

    let report = run_remote_plan(
        &plan,
        vec![slow.remote(), fast.remote()],
        RemotePlanConfig::new(1).with_retry_limit(1),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.report.completed, ["t1"], "{:?}", report.report);
    assert_eq!(report.node_of("t1"), Some("node-b"), "超预算必须换节点");
    assert_eq!(report.attempts_of("t1").len(), 2);
    assert!(matches!(
        report.attempts_of("t1")[0].status,
        DelegationStatus::Aborted
    ));
    let nodes: BTreeMap<&str, &trit_orch::NodeSnapshot> = report
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    assert!(
        nodes["node-a"].healthy,
        "a deadline reply proves the node is reachable: {:?}",
        report.nodes
    );
    assert_eq!(nodes["node-a"].failures, 1);
    assert_eq!(nodes["node-a"].attempts, 1);
    assert!(nodes["node-b"].healthy);
}

#[test]
fn retry_limit_bounds_attempts() {
    // 两个节点都不回包：retry_limit=1 只允许一次换节点重试。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        }
    });

    let endpoint = RemoteEndpoint {
        url: format!("http://{address}/run"),
        auth_token: None,
        timeout_secs: 1,
    };
    let plan = plan(vec![task("t1", &[])]);
    let report = run_remote_plan(
        &plan,
        vec![
            RemoteNode::new("node-a", endpoint.clone()),
            RemoteNode::new("node-b", endpoint),
        ],
        RemotePlanConfig::new(1).with_retry_limit(1),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert!(report.report.completed.is_empty());
    assert_eq!(report.attempts_of("t1").len(), 2, "{:?}", report.report);
    assert!(
        report
            .attempts_of("t1")
            .iter()
            .all(|attempt| matches!(attempt.status, DelegationStatus::Aborted))
    );
    assert!(report.report.failed.is_some());
    assert!(report.nodes.iter().all(|node| !node.healthy));
}

#[test]
fn task_failure_is_not_retried() {
    // 远端明确回 Failed：任务级失败，换节点重试也不会改变结果，按设计不重试。
    let node = spawn_node("only", 1, &["--fail"]);
    let plan = plan(vec![task("t1", &[])]);
    let report = run_remote_plan(
        &plan,
        vec![node.remote()],
        RemotePlanConfig::new(1).with_retry_limit(3),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.attempts_of("t1").len(), 1, "{:?}", report.attempts);
    assert!(matches!(
        report.attempts_of("t1")[0].status,
        DelegationStatus::Failed { .. }
    ));
}

#[test]
fn user_cancel_aborts_in_flight_without_quarantining_node() {
    let node = spawn_node("slow", 1, &["--sleep-ms", "3000"]);
    let remote = node.remote();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let plan = plan(vec![task("t1", &[]), task("t2", &[])]);
    let started = Instant::now();
    let handle = std::thread::spawn(move || {
        run_remote_plan(
            &plan,
            vec![remote],
            RemotePlanConfig::new(1),
            token,
            request_for,
        )
    });

    std::thread::sleep(Duration::from_millis(150));
    cancel.cancel();
    let report = handle.join().unwrap().unwrap();

    assert!(
        matches!(
            &report.result("t1").unwrap().status,
            DelegationStatus::Aborted
        ),
        "{:?}",
        report.result("t1")
    );
    assert!(report.report.completed.is_empty(), "{:?}", report.report);
    // 用户取消不记账：容量归还、节点仍健康、没有产生 attempts。
    assert_eq!(report.nodes[0].attempts, 0, "{:?}", report.nodes);
    assert!(report.nodes[0].healthy, "{:?}", report.nodes);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancel must not wait for the 3s sleep: {:?}",
        started.elapsed()
    );
}

#[test]
fn invalid_node_table_is_rejected_before_dispatch() {
    let endpoint = RemoteEndpoint {
        url: "http://127.0.0.1:9/run".into(),
        auth_token: None,
        timeout_secs: 5,
    };
    let duplicate = vec![
        RemoteNode::new("dup", endpoint.clone()),
        RemoteNode::new("dup", endpoint),
    ];
    let plan = plan(vec![task("t1", &[])]);

    let error = run_remote_plan(
        &plan,
        duplicate,
        RemotePlanConfig::new(1),
        CancelToken::new(),
        request_for,
    )
    .unwrap_err();

    assert!(
        matches!(&error, trit_host::AgentError::Kernel(inner) if inner.kind() == "remote_nodes"),
        "{error:?}"
    );
}
