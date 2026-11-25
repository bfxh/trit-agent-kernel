//! `trit-host remote-plan`：用真实 `remote-echo` 节点跑一次跨机演练，校验 CLI 输出。

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use trit_orch::{RemoteEndpoint, RemoteNode, Task, TaskSpec};

struct TestNode {
    id: String,
    base: String,
    child: Child,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_node(id: &str, args: &[&str]) -> TestNode {
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
    TestNode {
        id: id.into(),
        base: format!("http://{}", value["bind"].as_str().unwrap()),
        child,
    }
}

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

fn node_json(node: &TestNode, capacity: usize) -> serde_json::Value {
    let remote = RemoteNode::new(
        node.id.clone(),
        RemoteEndpoint {
            url: format!("{}/run", node.base),
            auth_token: None,
            timeout_secs: 5,
        },
    )
    .with_max_in_flight(capacity);
    serde_json::to_value(remote).unwrap()
}

fn input_file(name: &str, value: &serde_json::Value) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "trit-remote-plan-{}-{name}.json",
        std::process::id()
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    path
}

#[test]
fn remote_plan_cli_dispatches_across_nodes() {
    let node_a = spawn_node("node-a", &[]);
    let node_b = spawn_node("node-b", &[]);
    let input = serde_json::json!({
        "revision": 1,
        "workers": 2,
        "failure_threshold": 1,
        "retry_limit": 0,
        "tasks": [task("t1"), task("t2"), task("t3")],
        "nodes": [node_json(&node_a, 1), node_json(&node_b, 1)],
    });
    let path = input_file("dispatch", &input);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "remote-plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(report["ok"], true);
    assert_eq!(report["completed"].as_array().unwrap().len(), 3);
    assert!(report["failed"].is_null(), "{report}");
    assert!(report["deferred"].as_array().unwrap().is_empty());
    assert_eq!(report["assignments"].as_object().unwrap().len(), 3);
    for task in ["t1", "t2", "t3"] {
        let attempts = report["attempts"][task].as_array().unwrap();
        assert_eq!(attempts.len(), 1, "{task}: {attempts:?}");
    }

    let attempts: u64 = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["attempts"].as_u64().unwrap())
        .sum();
    assert_eq!(attempts, 3);
    let used: HashSet<&str> = report["assignments"]
        .as_object()
        .unwrap()
        .values()
        .map(|node| node.as_str().unwrap())
        .collect();
    assert_eq!(used, HashSet::from(["node-a", "node-b"]));
}

#[test]
fn remote_plan_cli_persists_node_health() {
    let good = spawn_node("node-b", &[]);
    let dead = serde_json::to_value(
        RemoteNode::new(
            "node-a",
            RemoteEndpoint {
                url: "http://127.0.0.1:1/run".into(),
                auth_token: None,
                timeout_secs: 1,
            },
        )
        .with_max_in_flight(1),
    )
    .unwrap();
    let health = std::env::temp_dir().join(format!(
        "trit-remote-plan-health-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&health);
    let input = serde_json::json!({
        "revision": 1,
        "workers": 2,
        "failure_threshold": 1,
        "retry_limit": 0,
        "tasks": [task("t1"), task("t2")],
        "nodes": [dead, node_json(&good, 1)],
    });
    let path = input_file("health", &input);

    let first = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "first remote-plan failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["completed"].as_array().unwrap().len(), 1, "{report}");
    assert!(!report["failed"].is_null(), "{report}");

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&health).unwrap()).unwrap();
    let bad_health = saved
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "node-a")
        .unwrap();
    assert_eq!(bad_health["healthy"], false, "{saved}");

    // 第二次运行：node-a 已被隔离，两个任务都应落到 node-b。
    let second = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&health);
    assert!(
        second.status.success(),
        "second remote-plan failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["completed"].as_array().unwrap().len(), 2, "{report}");
    assert!(report["failed"].is_null(), "{report}");
    for task in ["t1", "t2"] {
        assert_eq!(report["assignments"][task], "node-b", "{report}");
    }
    let bad_node = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "node-a")
        .unwrap();
    assert_eq!(bad_node["healthy"], false, "{report}");
}

#[test]
fn remote_plan_cli_rejects_duplicate_node_ids() {
    let node = spawn_node("dup", &[]);
    let input = serde_json::json!({
        "revision": 1,
        "tasks": [task("t1")],
        "nodes": [node_json(&node, 1), node_json(&node, 1)],
    });
    let path = input_file("duplicate", &input);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("duplicate remote node id"), "{stderr}");
}
