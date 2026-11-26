//! 独立节点健康守护：一次扫描只探已隔离节点，成功写回，失败保持隔离。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use trit_host::{
    RemoteHealthScope, probe_remote_health_file, probe_remote_health_file_scoped,
    probe_remote_health_file_with_concurrency, save_remote_health_file,
    spawn_remote_health_flusher,
};
use trit_orch::{CancelToken, NodeOutcome, NodeSnapshot, NodeTable, RemoteEndpoint, RemoteNode};

struct TestNode {
    base: String,
    child: Child,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("trit-remote-health-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn spawn_node() -> TestNode {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-echo")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--once")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    TestNode {
        base: format!("http://{}", value["bind"].as_str().unwrap()),
        child,
    }
}

fn node(id: &str, url: String) -> RemoteNode {
    RemoteNode::new(
        id,
        RemoteEndpoint {
            url,
            auth_token: None,
            timeout_secs: 2,
        },
    )
}

/// 只接受一次请求、延迟后回 200 的 `/health` 服务端；用于测并行探测。
fn delayed_health_server(delay: Duration) -> (String, JoinHandle<()>) {
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
    (base, handle)
}

fn write_health(path: &Path, snapshots: &[NodeSnapshot]) {
    std::fs::write(path, serde_json::to_string_pretty(snapshots).unwrap()).unwrap();
}

fn read_health(path: &Path) -> Vec<NodeSnapshot> {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn isolated(id: &str) -> NodeSnapshot {
    NodeSnapshot {
        id: id.into(),
        healthy: false,
        in_flight: 0,
        attempts: 3,
        failures: 2,
        updated_at_ms: 0,
    }
}

#[test]
fn save_merges_unmentioned_entries_and_replaces_known() {
    let dir = temp_dir("save-merge");
    let health = dir.join("health.json");
    write_health(&health, &[isolated("keep"), isolated("update")]);

    let mut updated = isolated("update");
    updated.healthy = true;
    updated.attempts = 4;
    updated.failures = 0;
    save_remote_health_file(&health, std::slice::from_ref(&updated)).unwrap();

    let stored = read_health(&health);
    assert_eq!(stored.len(), 2, "未提及的旧节点必须保留");
    let keep = stored
        .iter()
        .find(|snapshot| snapshot.id == "keep")
        .unwrap();
    assert!(!keep.healthy);
    assert_eq!(keep.attempts, 3);
    let update = stored
        .iter()
        .find(|snapshot| snapshot.id == "update")
        .unwrap();
    assert!(update.healthy);
    assert_eq!(update.attempts, 4);
    assert_eq!(update.failures, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_keeps_newer_existing_state_over_a_stale_writer() {
    let dir = temp_dir("save-merge-freshness");
    let health = dir.join("health.json");
    let mut fresh = isolated("a");
    fresh.healthy = true;
    fresh.updated_at_ms = 200;
    write_health(&health, std::slice::from_ref(&fresh));

    let mut stale = isolated("a");
    stale.updated_at_ms = 100;
    save_remote_health_file(&health, std::slice::from_ref(&stale)).unwrap();
    let stored = read_health(&health);
    assert!(stored[0].healthy, "旧写回不得覆盖更新鲜的放行状态");
    assert_eq!(stored[0].updated_at_ms, 200);

    let mut newer = isolated("a");
    newer.updated_at_ms = 300;
    save_remote_health_file(&health, std::slice::from_ref(&newer)).unwrap();
    let stored = read_health(&health);
    assert!(!stored[0].healthy, "更新鲜的写回必须生效");
    assert_eq!(stored[0].updated_at_ms, 300);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_health_writers_merge_instead_of_losing_entries() {
    let dir = temp_dir("save-merge-concurrent");
    let health = dir.join("health.json");
    let mut handles = Vec::new();
    for index in 0..8 {
        let path = health.clone();
        handles.push(std::thread::spawn(move || {
            let snapshot = isolated(&format!("n{index}"));
            save_remote_health_file(&path, std::slice::from_ref(&snapshot))
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let stored = read_health(&health);
    let mut ids: Vec<&str> = stored.iter().map(|snapshot| snapshot.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        ["n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7"],
        "并发写者必须串行合并，而不是互相覆盖"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn health_flusher_persists_changes_between_intervals() {
    let dir = temp_dir("flusher");
    let health = dir.join("health.json");
    let table =
        Arc::new(NodeTable::new(vec![node("n1", "http://127.0.0.1:1/run".into())]).unwrap());
    let lease = table.acquire(&CancelToken::new()).unwrap().unwrap();
    lease.finish(NodeOutcome::NodeFailed);

    let mut flusher =
        spawn_remote_health_flusher(Arc::clone(&table), &health, Duration::from_millis(20))
            .unwrap();

    let wait_until = |predicate: &dyn Fn(&[NodeSnapshot]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(stored) = std::fs::read_to_string(&health)
                && let Ok(snapshots) = serde_json::from_str::<Vec<NodeSnapshot>>(&stored)
                && predicate(&snapshots)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "health flusher did not persist expected snapshot: {}",
                std::fs::read_to_string(&health).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    wait_until(&|snapshots| {
        snapshots
            .iter()
            .any(|snapshot| snapshot.id == "n1" && !snapshot.healthy)
    });
    let stored = read_health(&health);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].in_flight, 0, "在途计数不得写进跨运行健康文件");

    assert!(table.readmit("n1"));
    wait_until(&|snapshots| {
        snapshots
            .iter()
            .any(|snapshot| snapshot.id == "n1" && snapshot.healthy)
    });

    flusher.stop_and_join();
    assert_eq!(flusher.last_error(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flusher_does_not_revert_a_newer_external_readmit() {
    let dir = temp_dir("flusher-freshness");
    let health = dir.join("health.json");
    let mut initial = isolated("n1");
    initial.updated_at_ms = 1;
    let table = Arc::new(
        NodeTable::new(vec![node("n1", "http://127.0.0.1:1/run".into())])
            .unwrap()
            .with_snapshots(std::slice::from_ref(&initial)),
    );
    let mut flusher =
        spawn_remote_health_flusher(Arc::clone(&table), &health, Duration::from_millis(20))
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(stored) = std::fs::read_to_string(&health)
            && let Ok(snapshots) = serde_json::from_str::<Vec<NodeSnapshot>>(&stored)
            && snapshots
                .iter()
                .any(|snapshot| snapshot.id == "n1" && !snapshot.healthy)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "health flusher did not persist the isolated snapshot"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut external = isolated("n1");
    external.healthy = true;
    external.updated_at_ms = u64::MAX / 2;
    save_remote_health_file(&health, std::slice::from_ref(&external)).unwrap();
    // 至少再跑几轮后台写回：旧快照时间戳更小，不得把外部放行改回隔离。
    std::thread::sleep(Duration::from_millis(100));
    flusher.stop_and_join();
    assert_eq!(flusher.last_error(), None);

    let stored = read_health(&health);
    assert!(stored[0].healthy, "旧快照不得覆盖更新的外部放行");
    assert_eq!(stored[0].updated_at_ms, external.updated_at_ms);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_probes_isolated_nodes_with_bounded_concurrency() {
    let dir = temp_dir("probe-concurrency");
    let health = dir.join("health.json");
    let delay = Duration::from_millis(250);
    let servers: Vec<(String, JoinHandle<()>)> =
        (0..4).map(|_| delayed_health_server(delay)).collect();
    let nodes: Vec<RemoteNode> = servers
        .iter()
        .enumerate()
        .map(|(index, (base, _))| node(&format!("n{index}"), format!("{base}/run")))
        .collect();
    let stored: Vec<NodeSnapshot> = nodes.iter().map(|node| isolated(&node.id)).collect();
    write_health(&health, &stored);

    let started = Instant::now();
    let report = probe_remote_health_file_with_concurrency(&nodes, &health, nodes.len()).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(report.checked, nodes.len());
    assert_eq!(report.readmitted, nodes.len());
    assert_eq!(report.still_isolated, 0);
    let event_nodes: Vec<&str> = report
        .events
        .iter()
        .map(|event| event.node.as_str())
        .collect();
    assert_eq!(
        event_nodes,
        ["n0", "n1", "n2", "n3"],
        "并发探测的事件仍按节点 id 升序"
    );
    assert!(
        elapsed < Duration::from_millis(700),
        "4 个 250ms 探测串行需 1s；并发应明显更快，实际 {elapsed:?}"
    );
    for (_, handle) in servers {
        handle.join().unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_readmits_isolated_node_and_persists_snapshot() {
    let dir = temp_dir("readmit");
    let server = spawn_node();
    let health = dir.join("health.json");
    let mut initial = isolated("n1");
    initial.updated_at_ms = 1;
    write_health(&health, std::slice::from_ref(&initial));

    let report =
        probe_remote_health_file(&[node("n1", format!("{}/run", server.base))], &health).unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.readmitted, 1);
    assert_eq!(report.still_isolated, 0);
    assert_eq!(report.events.len(), 1);
    assert!(report.events[0].ok);
    assert_eq!(report.events[0].node, "n1");

    let stored = read_health(&health);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].healthy);
    assert_eq!(stored[0].attempts, 3, "累计尝试数必须保留");
    assert_eq!(stored[0].failures, 2, "累计失败数必须保留");
    assert_eq!(stored[0].in_flight, 0);
    assert!(
        stored[0].updated_at_ms > initial.updated_at_ms,
        "成功探测必须推进节点更新时间"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_keeps_node_isolated_when_probe_fails() {
    let dir = temp_dir("still-isolated");
    let health = dir.join("health.json");
    write_health(&health, &[isolated("n1")]);

    let report =
        probe_remote_health_file(&[node("n1", "http://127.0.0.1:1/run".into())], &health).unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.readmitted, 0);
    assert_eq!(report.still_isolated, 1);
    assert!(!report.events[0].ok);
    assert!(report.events[0].error.is_some());

    let stored = read_health(&health);
    assert!(!stored[0].healthy);
    assert_eq!(stored[0].attempts, 3);
    assert_eq!(stored[0].failures, 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn probe_all_refreshes_healthy_nodes_without_counting_them_as_readmitted() {
    let dir = temp_dir("probe-all-refresh");
    let server = spawn_node();
    let health = dir.join("health.json");
    let initial = NodeSnapshot {
        id: "n1".into(),
        healthy: true,
        in_flight: 0,
        attempts: 1,
        failures: 0,
        updated_at_ms: 1,
    };
    write_health(&health, std::slice::from_ref(&initial));

    let report = probe_remote_health_file_scoped(
        &[node("n1", format!("{}/run", server.base))],
        &health,
        1,
        RemoteHealthScope::All,
    )
    .unwrap();
    assert_eq!(report.checked, 1, "健康节点也要真的探一次");
    assert_eq!(report.readmitted, 0, "本来健康的节点不算重新入列");
    assert_eq!(report.still_isolated, 0);
    assert!(report.events[0].ok);
    assert!(!report.events[0].readmitted, "事件同样不标记为重新入列");

    let stored = read_health(&health);
    assert!(stored[0].healthy);
    assert!(
        stored[0].updated_at_ms > initial.updated_at_ms,
        "巡检成功必须刷新观测时间"
    );
    assert_eq!(stored[0].attempts, 1, "累计尝试数必须保留");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn probe_all_isolates_healthy_node_that_stops_answering() {
    let dir = temp_dir("probe-all-isolate");
    let health = dir.join("health.json");
    let initial = NodeSnapshot {
        id: "n1".into(),
        healthy: true,
        in_flight: 0,
        attempts: 0,
        failures: 0,
        updated_at_ms: 1,
    };
    write_health(&health, std::slice::from_ref(&initial));

    let report = probe_remote_health_file_scoped(
        &[node("n1", "http://127.0.0.1:1/run".into())],
        &health,
        1,
        RemoteHealthScope::All,
    )
    .unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.readmitted, 0);
    assert_eq!(report.still_isolated, 1);
    assert!(!report.events[0].ok);

    let stored = read_health(&health);
    assert!(!stored[0].healthy, "主动巡检发现健康节点不可达时应就地隔离");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_preserves_health_entries_outside_the_node_table() {
    let dir = temp_dir("stale");
    let health = dir.join("health.json");
    write_health(&health, &[isolated("n1"), isolated("ghost")]);

    let report =
        probe_remote_health_file(&[node("n1", "http://127.0.0.1:1/run".into())], &health).unwrap();
    assert_eq!(report.ignored, vec!["ghost".to_owned()]);
    assert_eq!(report.nodes.len(), 2, "未知节点仍应写回");
    let stored = read_health(&health);
    assert_eq!(stored.len(), 2);
    assert!(
        stored
            .iter()
            .any(|snapshot| snapshot.id == "ghost" && !snapshot.healthy),
        "未知节点保留原快照，不因本轮未探测而被删除"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_initializes_missing_health_file_as_all_healthy() {
    let dir = temp_dir("missing");
    let health = dir.join("health.json");
    let report =
        probe_remote_health_file(&[node("n1", "http://127.0.0.1:1/run".into())], &health).unwrap();
    assert_eq!(report.checked, 0);
    assert!(report.events.is_empty());
    let stored = read_health(&health);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].healthy);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_reloads_nodes_and_holds_single_instance_lock() {
    let dir = temp_dir("reload");
    let first_server = spawn_node();
    let second_server = spawn_node();
    let nodes_path = dir.join("nodes.json");
    let health = dir.join("health.json");
    std::fs::write(
        &nodes_path,
        serde_json::to_string(&[node("a", format!("{}/run", first_server.base))]).unwrap(),
    )
    .unwrap();
    write_health(&health, &[isolated("a"), isolated("b")]);

    let mut monitor = ChildGuard {
        child: Command::new(env!("CARGO_BIN_EXE_trit-host"))
            .arg("remote-health")
            .arg("--nodes")
            .arg(&nodes_path)
            .arg("--health")
            .arg(&health)
            .arg("--interval-ms")
            .arg("500")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    };
    let stdout = monitor.child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let first: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(first["readmitted"], 1);
    assert_eq!(first["ignored"], serde_json::json!(["b"]));
    assert!(
        read_health(&health)
            .iter()
            .any(|snapshot| snapshot.id == "b" && !snapshot.healthy),
        "本轮未探测的节点仍保留原快照"
    );

    let busy = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-health")
        .arg("--nodes")
        .arg(&nodes_path)
        .arg("--health")
        .arg(&health)
        .arg("--once")
        .output()
        .unwrap();
    assert!(!busy.status.success(), "second monitor must fail closed");
    assert!(
        String::from_utf8_lossy(&busy.stderr).contains("another remote-health monitor"),
        "stderr: {}",
        String::from_utf8_lossy(&busy.stderr)
    );

    std::fs::write(
        &nodes_path,
        serde_json::to_string(&[node("b", format!("{}/run", second_server.base))]).unwrap(),
    )
    .unwrap();
    let mut next = String::new();
    reader.read_line(&mut next).unwrap();
    let second: serde_json::Value = serde_json::from_str(next.trim()).unwrap();
    assert_eq!(second["readmitted"], 1, "下一个节点应被热加载并探测");
    assert_eq!(second["events"][0]["node"], "b");
    assert!(
        read_health(&health)
            .iter()
            .any(|snapshot| snapshot.id == "b" && snapshot.healthy)
    );

    drop(monitor);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_health_once_cli_readmits_and_reports_json() {
    let dir = temp_dir("cli");
    let server = spawn_node();
    let nodes_path = dir.join("nodes.json");
    let health = dir.join("health.json");
    std::fs::write(
        &nodes_path,
        serde_json::to_string(&[node("n1", format!("{}/run", server.base))]).unwrap(),
    )
    .unwrap();
    write_health(&health, &[isolated("n1")]);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-health")
        .arg("--nodes")
        .arg(&nodes_path)
        .arg("--health")
        .arg(&health)
        .arg("--once")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["checked"], 1);
    assert_eq!(value["readmitted"], 1);
    assert_eq!(value["events"][0]["phase"], "monitor");
    assert!(read_health(&health)[0].healthy);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_health_cli_rejects_invalid_probe_concurrency() {
    for (value, expected) in [("0", "must be positive"), ("65", "must be <= 64")] {
        let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
            .arg("remote-health")
            .arg("--probe-concurrency")
            .arg(value)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{value}: {stderr}");
    }
}
