//! 跨机节点表：校验、容量、最轻负载、等待/取消与故障隔离。

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use trit_orch::{CancelToken, Error, NodeOutcome, NodeTable, RemoteEndpoint, RemoteNode};

fn endpoint(url: &str) -> RemoteEndpoint {
    RemoteEndpoint {
        url: url.into(),
        auth_token: None,
        timeout_secs: 30,
    }
}

fn node(id: &str, capacity: usize) -> RemoteNode {
    RemoteNode::new(id, endpoint("http://127.0.0.1:9/run")).with_max_in_flight(capacity)
}

#[test]
fn table_validates_nodes() {
    assert_eq!(
        NodeTable::new(Vec::new()).unwrap_err().kind(),
        "remote_nodes"
    );

    assert_eq!(
        NodeTable::new(vec![node("  ", 1)]).unwrap_err().kind(),
        "remote_nodes"
    );

    assert_eq!(
        NodeTable::new(vec![node("a", 1), node("a", 1)])
            .unwrap_err()
            .kind(),
        "remote_nodes"
    );

    assert_eq!(
        NodeTable::new(vec![node("a", 0)]).unwrap_err().kind(),
        "remote_nodes"
    );

    // 明文 http 只允许 loopback：校验错误被包装成 remote_nodes。
    let remote_plaintext = RemoteNode::new("a", endpoint("http://agents.example.com/run"));
    assert_eq!(
        NodeTable::new(vec![remote_plaintext]).unwrap_err().kind(),
        "remote_nodes"
    );
}

#[test]
fn capacity_and_least_load() {
    let table = NodeTable::new(vec![node("a", 2), node("b", 1)]).unwrap();
    let cancel = CancelToken::new();

    // 负载比：a 0/2, b 0/1 → 平手按 id，取 a；随后 a 1/2 vs b 0/1 → b；再 a。
    let first = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(first.node().id, "a");
    let second = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(second.node().id, "b");
    let third = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(third.node().id, "a");

    let snapshots = table.snapshot();
    assert_eq!(snapshots[0].id, "a");
    assert_eq!(snapshots[0].in_flight, 2);
    assert_eq!(snapshots[1].id, "b");
    assert_eq!(snapshots[1].in_flight, 1);

    third.finish(NodeOutcome::Succeeded);
    second.finish(NodeOutcome::Succeeded);
    first.finish(NodeOutcome::Succeeded);
    assert!(table.snapshot().iter().all(|node| node.in_flight == 0));
    // a 承担两次、b 一次。
    assert_eq!(table.snapshot()[0].attempts, 2);
    assert_eq!(table.snapshot()[1].attempts, 1);
}

#[test]
fn full_table_waits_for_release() {
    let table = Arc::new(NodeTable::new(vec![node("a", 1)]).unwrap());
    let cancel = CancelToken::new();
    let lease = table.acquire(&cancel).unwrap().unwrap();

    let worker_table = Arc::clone(&table);
    let worker_cancel = cancel.clone();
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let lease = worker_table.acquire(&worker_cancel).unwrap().unwrap();
        sender.send(lease.node().id.clone()).unwrap();
        lease.finish(NodeOutcome::Succeeded);
    });

    std::thread::sleep(Duration::from_millis(40));
    lease.finish(NodeOutcome::Succeeded);
    assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "a");
    handle.join().unwrap();
}

#[test]
fn cancel_returns_none_even_while_waiting() {
    let table = Arc::new(NodeTable::new(vec![node("a", 1)]).unwrap());
    let cancel = CancelToken::new();
    let lease = table.acquire(&cancel).unwrap().unwrap();

    let worker_table = Arc::clone(&table);
    let worker_cancel = cancel.clone();
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        sender
            .send(worker_table.acquire(&worker_cancel).unwrap().is_none())
            .unwrap();
    });

    std::thread::sleep(Duration::from_millis(40));
    let started = Instant::now();
    cancel.cancel();
    assert!(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancel must wake the waiter: {:?}",
        started.elapsed()
    );
    // 等待者没有占用容量，持有者释放后表归零。
    lease.finish(NodeOutcome::Succeeded);
    handle.join().unwrap();
    assert_eq!(table.snapshot()[0].in_flight, 0);
    assert_eq!(table.snapshot()[0].attempts, 1);
}

#[test]
fn node_failure_isolates_but_task_failure_does_not() {
    let table = NodeTable::new(vec![node("a", 1), node("b", 1)]).unwrap();
    let cancel = CancelToken::new();

    let first = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(first.node().id, "a");
    first.finish(NodeOutcome::NodeFailed);

    let snapshots = table.snapshot();
    assert!(!snapshots[0].healthy, "node a must be quarantined");
    assert!(snapshots[1].healthy);

    let second = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(second.node().id, "b");
    second.finish(NodeOutcome::TaskFailed);

    let snapshots = table.snapshot();
    assert!(
        snapshots[1].healthy,
        "task failure must not quarantine a node"
    );
    assert_eq!(snapshots[1].failures, 1);

    let third = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(third.node().id, "b");
    third.finish(NodeOutcome::Succeeded);
    assert_eq!(table.healthy_nodes(), 1);

    // 最后一个健康节点也失败后，本轮没有可用容量。
    let last = table.acquire(&cancel).unwrap().unwrap();
    last.finish(NodeOutcome::NodeFailed);
    let error = table.acquire(&cancel).unwrap_err();
    assert_eq!(error.kind(), "remote_nodes");
    assert!(matches!(error, Error::RemoteNodes { .. }));
}

#[test]
fn failure_threshold_two_tolerates_one_failure() {
    let table = NodeTable::new(vec![node("a", 1)])
        .unwrap()
        .with_failure_threshold(2);
    let cancel = CancelToken::new();

    let first = table.acquire(&cancel).unwrap().unwrap();
    first.finish(NodeOutcome::NodeFailed);
    assert_eq!(table.healthy_nodes(), 1);

    let second = table.acquire(&cancel).unwrap().unwrap();
    second.finish(NodeOutcome::NodeFailed);
    assert_eq!(table.healthy_nodes(), 0);
}

#[test]
fn dropping_a_lease_releases_capacity_as_node_failure() {
    let table = NodeTable::new(vec![node("a", 1)]).unwrap();
    let cancel = CancelToken::new();

    let lease = table.acquire(&cancel).unwrap().unwrap();
    drop(lease);

    let snapshot = &table.snapshot()[0];
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.attempts, 1);
    assert_eq!(snapshot.failures, 1);
    assert!(!snapshot.healthy);
}

#[test]
fn snapshots_are_sorted_by_id() {
    let table = NodeTable::new(vec![node("zeta", 1), node("alpha", 1)]).unwrap();
    let ids: Vec<String> = table
        .snapshot()
        .into_iter()
        .map(|snapshot| snapshot.id)
        .collect();
    assert_eq!(ids, vec!["alpha", "zeta"]);
}

#[test]
fn remote_node_json_uses_documented_defaults() {
    let node: RemoteNode =
        serde_json::from_str(r#"{"id":"n1","endpoint":{"url":"https://agents.example.com/run"}}"#)
            .unwrap();
    assert_eq!(node.max_in_flight, 1);
    assert_eq!(node.endpoint.timeout_secs, 30);
    assert!(node.endpoint.auth_token.is_none());
    assert!(NodeTable::new(vec![node]).is_ok());
}
