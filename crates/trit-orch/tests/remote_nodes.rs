//! 跨机节点表：校验、容量、最轻负载、等待/取消与故障隔离。

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use trit_orch::{
    CancelToken, Error, NodeOutcome, NodeSnapshot, NodeTable, RemoteEndpoint, RemoteNode,
};

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
fn abandoned_lease_releases_without_health_penalty() {
    let table = NodeTable::new(vec![node("a", 1)]).unwrap();
    let cancel = CancelToken::new();
    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::Abandoned);
    let snapshot = table.snapshot();
    assert!(snapshot[0].healthy);
    assert_eq!(snapshot[0].attempts, 0);
    assert_eq!(snapshot[0].failures, 0);

    // 容量已归还，可以再次准入。
    let lease = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(lease.node().id, "a");
    lease.finish(NodeOutcome::Succeeded);
}

#[test]
fn superseded_resets_failures_without_quarantining() {
    let table = NodeTable::new(vec![node("a", 1)])
        .unwrap()
        .with_failure_threshold(2);
    let cancel = CancelToken::new();

    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    assert!(table.snapshot()[0].healthy);

    // 租约被接管证明节点仍能服务：计入尝试，但不算失败，并清掉连续失败计数。
    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::Superseded);
    let snapshot = table.snapshot();
    assert!(snapshot[0].healthy);
    assert_eq!(snapshot[0].attempts, 2);
    assert_eq!(snapshot[0].failures, 1);

    // 前一次 NodeFailed 的连续计数已被 Superseded 清零，这次不会触发隔离。
    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    assert!(table.snapshot()[0].healthy);

    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    assert!(!table.snapshot()[0].healthy);
}

#[test]
fn over_budget_counts_an_attempt_but_keeps_the_node_healthy() {
    let table = NodeTable::new(vec![node("a", 1)])
        .unwrap()
        .with_failure_threshold(2);
    let cancel = CancelToken::new();

    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    assert!(table.snapshot()[0].healthy);

    // 服务端明确回了超时，说明节点仍能服务：本事计入失败数，但清零连续失败。
    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::OverBudget);
    let snapshot = table.snapshot();
    assert!(snapshot[0].healthy);
    assert_eq!(snapshot[0].attempts, 2);
    assert_eq!(snapshot[0].failures, 2);

    // 连续失败计数已清零，一次 NodeFailed 不足以隔离。
    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    assert!(table.snapshot()[0].healthy);

    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    assert!(!table.snapshot()[0].healthy);
}

#[test]
fn readmit_revives_only_known_isolated_nodes() {
    let table = NodeTable::new(vec![node("a", 1)])
        .unwrap()
        .with_failure_threshold(1);
    let cancel = CancelToken::new();

    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);
    let before = &table.snapshot()[0];
    assert!(!before.healthy);
    assert_eq!(before.attempts, 1);
    assert_eq!(before.failures, 1);

    assert!(table.readmit("a"));
    let after = &table.snapshot()[0];
    assert!(after.healthy);
    assert_eq!(after.attempts, 1, "恢复不能抹掉历史尝试数");
    assert_eq!(after.failures, 1, "恢复不能抹掉历史失败数");

    // 已恢复节点、未知节点都不做隐式放行。
    assert!(!table.readmit("a"));
    assert!(!table.readmit("missing"));
    let lease = table.acquire(&cancel).unwrap().unwrap();
    lease.finish(NodeOutcome::Succeeded);
}

#[test]
fn snapshots_restore_quarantine_and_counts() {
    let table = NodeTable::new(vec![node("a", 1), node("b", 1)]).unwrap();
    let cancel = CancelToken::new();
    let lease = table.acquire(&cancel).unwrap().unwrap();
    assert_eq!(lease.node().id, "a");
    lease.finish(NodeOutcome::NodeFailed);
    let snapshots = table.snapshot();
    assert!(!snapshots[0].healthy);
    assert_eq!(snapshots[0].failures, 1);

    let restored = NodeTable::new(vec![node("a", 1), node("b", 1)])
        .unwrap()
        .with_failure_threshold(1)
        .with_snapshots(&snapshots);
    assert_eq!(restored.healthy_nodes(), 1);
    let lease = restored.acquire(&cancel).unwrap().unwrap();
    assert_eq!(lease.node().id, "b");
    lease.finish(NodeOutcome::Succeeded);

    // 健康文件里已删除的节点会被忽略，不影响现有节点。
    let ignored = NodeSnapshot {
        id: "ghost".into(),
        healthy: false,
        in_flight: 0,
        attempts: 9,
        failures: 9,
    };
    let table = NodeTable::new(vec![node("a", 1), node("b", 1)])
        .unwrap()
        .with_snapshots(&[ignored]);
    assert_eq!(table.healthy_nodes(), 2);
    assert_eq!(table.snapshot()[0].attempts, 0);
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
fn acquire_excluding_skips_failed_node() {
    let table = NodeTable::new(vec![node("a", 1), node("b", 1)]).unwrap();
    let cancel = CancelToken::new();
    let mut excluded = std::collections::BTreeSet::new();

    let first = table
        .acquire_excluding(&cancel, &excluded)
        .unwrap()
        .unwrap();
    assert_eq!(first.node().id, "a");
    first.finish(NodeOutcome::NodeFailed);
    excluded.insert("a".into());

    // 重试必须换节点，而不是回到刚失败的 a。
    let retry = table
        .acquire_excluding(&cancel, &excluded)
        .unwrap()
        .unwrap();
    assert_eq!(retry.node().id, "b");
    retry.finish(NodeOutcome::Succeeded);

    excluded.insert("b".into());
    let error = table.acquire_excluding(&cancel, &excluded).unwrap_err();
    assert_eq!(error.kind(), "remote_nodes");
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
