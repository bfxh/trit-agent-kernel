//! 独立节点健康守护：一次扫描只探已隔离节点，成功写回，失败保持隔离。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use trit_host::probe_remote_health_file;
use trit_orch::{NodeSnapshot, RemoteEndpoint, RemoteNode};

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
    }
}

#[test]
fn monitor_readmits_isolated_node_and_persists_snapshot() {
    let dir = temp_dir("readmit");
    let server = spawn_node();
    let health = dir.join("health.json");
    write_health(&health, &[isolated("n1")]);

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
fn monitor_drops_health_entries_outside_the_node_table() {
    let dir = temp_dir("stale");
    let health = dir.join("health.json");
    write_health(&health, &[isolated("n1"), isolated("ghost")]);

    let report =
        probe_remote_health_file(&[node("n1", "http://127.0.0.1:1/run".into())], &health).unwrap();
    assert_eq!(report.ignored, vec!["ghost".to_owned()]);
    let stored = read_health(&health);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, "n1");

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
