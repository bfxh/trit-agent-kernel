//! `trit-host remote-plan`：用真实 `remote-echo` 节点跑一次跨机演练，校验 CLI 输出。

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
        "scope": "plan-main",
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
fn remote_plan_cli_probe_readmits_a_recovered_node() {
    let node_a = spawn_node("node-a", &[]);
    let node_b = spawn_node("node-b", &[]);
    let health = std::env::temp_dir().join(format!(
        "trit-remote-plan-probe-ok-{}.json",
        std::process::id()
    ));
    let snapshots = serde_json::json!([
        {"id": "node-a", "healthy": false, "in_flight": 0, "attempts": 0, "failures": 1},
        {"id": "node-b", "healthy": true, "in_flight": 0, "attempts": 0, "failures": 0},
    ]);
    std::fs::write(&health, serde_json::to_vec(&snapshots).unwrap()).unwrap();
    let input = serde_json::json!({
        "revision": 1,
        "workers": 2,
        "failure_threshold": 1,
        "tasks": [task("t1"), task("t2")],
        "nodes": [node_json(&node_a, 1), node_json(&node_b, 1)],
    });
    let path = input_file("probe-ok", &input);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .arg("--probe")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&health);
    assert!(
        output.status.success(),
        "remote-plan --probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["probes"][0]["node"], "node-a", "{report}");
    assert_eq!(report["probes"][0]["ok"], true, "{report}");
    assert_eq!(report["probes"][0]["readmitted"], true, "{report}");

    let nodes: std::collections::BTreeMap<&str, &serde_json::Value> = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect();
    assert_eq!(nodes["node-a"]["healthy"], true, "{report}");
    let used: HashSet<&str> = report["assignments"]
        .as_object()
        .unwrap()
        .values()
        .map(|node| node.as_str().unwrap())
        .collect();
    assert_eq!(used, HashSet::from(["node-a", "node-b"]), "{report}");
}

#[test]
fn remote_plan_cli_periodic_probe_readmits_isolated_node() {
    let slow = spawn_node("node-a", &["--sleep-ms", "1000"]);
    let recovered = spawn_node("node-b", &[]);
    let health = std::env::temp_dir().join(format!(
        "trit-remote-plan-probe-periodic-{}.json",
        std::process::id()
    ));
    let snapshots = serde_json::json!([
        {"id": "node-a", "healthy": true, "in_flight": 0, "attempts": 0, "failures": 0},
        {"id": "node-b", "healthy": false, "in_flight": 0, "attempts": 0, "failures": 1},
    ]);
    std::fs::write(&health, serde_json::to_vec(&snapshots).unwrap()).unwrap();
    let input = serde_json::json!({
        "revision": 1,
        "workers": 2,
        "failure_threshold": 1,
        "tasks": [task("t1"), task("t2")],
        "nodes": [node_json(&slow, 1), node_json(&recovered, 1)],
    });
    let path = input_file("probe-periodic", &input);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .arg("--probe-interval-ms")
        .arg("100")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&health);
    assert!(
        output.status.success(),
        "remote-plan --probe-interval-ms failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let nodes: std::collections::BTreeMap<&str, &serde_json::Value> = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect();
    assert_eq!(nodes["node-b"]["healthy"], true, "{report}");
    assert!(
        report["probes"].as_array().unwrap().iter().any(|probe| {
            probe["node"] == "node-b"
                && probe["phase"] == "periodic"
                && probe["ok"] == true
                && probe["readmitted"] == true
        }),
        "periodic probe should readmit node-b: {report}"
    );
    let used: HashSet<&str> = report["assignments"]
        .as_object()
        .unwrap()
        .values()
        .map(|node| node.as_str().unwrap())
        .collect();
    assert!(
        used.contains("node-b"),
        "the recovered node must take the queued task: {report}"
    );
}

#[test]
fn remote_plan_flushes_health_while_running() {
    let slow = spawn_node("node-a", &["--sleep-ms", "1500"]);
    let health = std::env::temp_dir().join(format!(
        "trit-remote-plan-flush-{}.json",
        std::process::id()
    ));
    let snapshots = serde_json::json!([
        {"id": "node-a", "healthy": true, "in_flight": 0, "attempts": 0, "failures": 0},
    ]);
    // 紧凑写入（无换行）；后台写回走 pretty JSON，因此出现换行即说明文件被
    // 健康写回线程重写过，而不是只在计划结束后才落盘。
    std::fs::write(&health, serde_json::to_vec(&snapshots).unwrap()).unwrap();
    let input = serde_json::json!({
        "revision": 1,
        "workers": 1,
        "failure_threshold": 1,
        "tasks": [task("t1")],
        "nodes": [node_json(&slow, 1)],
    });
    let path = input_file("health-flush", &input);

    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .arg("--health-flush-ms")
        .arg("100")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut observed_while_running = false;
    loop {
        let rewritten = std::fs::read_to_string(&health)
            .ok()
            .is_some_and(|text| text.contains('\n'));
        if rewritten && child.try_wait().unwrap().is_none() {
            observed_while_running = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "plan did not flush health while still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&health);
    assert!(
        observed_while_running,
        "health flush must persist state before the plan exits: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "remote-plan --health-flush-ms failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn remote_plan_cli_failed_probe_keeps_the_node_isolated() {
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
        "trit-remote-plan-probe-fail-{}.json",
        std::process::id()
    ));
    let snapshots = serde_json::json!([
        {"id": "node-a", "healthy": false, "in_flight": 0, "attempts": 0, "failures": 1},
        {"id": "node-b", "healthy": true, "in_flight": 0, "attempts": 0, "failures": 0},
    ]);
    std::fs::write(&health, serde_json::to_vec(&snapshots).unwrap()).unwrap();
    let input = serde_json::json!({
        "revision": 1,
        "workers": 2,
        "failure_threshold": 1,
        "tasks": [task("t1"), task("t2")],
        "nodes": [dead, node_json(&good, 1)],
    });
    let path = input_file("probe-fail", &input);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .arg("--probe")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&health);
    assert!(
        output.status.success(),
        "remote-plan --probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["probes"][0]["node"], "node-a", "{report}");
    assert_eq!(report["probes"][0]["ok"], false, "{report}");
    assert_eq!(report["probes"][0]["readmitted"], false, "{report}");
    assert!(
        report["probes"][0]["error"]
            .as_str()
            .unwrap()
            .contains("transport"),
        "{report}"
    );
    let bad_node = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "node-a")
        .unwrap();
    assert_eq!(bad_node["healthy"], false, "{report}");
    for task in ["t1", "t2"] {
        assert_eq!(report["assignments"][task], "node-b", "{report}");
    }
}

#[test]
fn remote_plan_cli_persists_node_health() {
    let good = spawn_node("node-b", &[]);
    // 用一个会拖过 1s 超时的节点代替立即拒绝连接：否则 node-a 的失败可能
    // 赶在调度器把另一个任务派到 node-b 之前发生，测试结果依赖线程时序。
    let slow = spawn_node("node-a", &["--sleep-ms", "2000"]);
    let dead = serde_json::to_value(
        RemoteNode::new(
            "node-a",
            RemoteEndpoint {
                url: format!("{}/run", slow.base),
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
fn remote_plan_preserves_health_entries_outside_node_table() {
    let node = spawn_node("node-a", &[]);
    let health = std::env::temp_dir().join(format!(
        "trit-remote-plan-health-unknown-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&health);
    let snapshots = serde_json::json!([
        {"id": "ghost", "healthy": false, "in_flight": 0, "attempts": 7, "failures": 4}
    ]);
    std::fs::write(&health, serde_json::to_vec(&snapshots).unwrap()).unwrap();

    let input = serde_json::json!({
        "revision": 1,
        "workers": 1,
        "failure_threshold": 1,
        "retry_limit": 0,
        "tasks": [task("t1")],
        "nodes": [node_json(&node, 1)],
    });
    let path = input_file("health-unknown", &input);
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .arg("--health")
        .arg(&health)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "remote-plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 本次演练的节点表只含 node-a；ghost 的隔离状态不能被整份覆盖抹掉。
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&health).unwrap()).unwrap();
    let ghost = saved
        .as_array()
        .unwrap()
        .iter()
        .find(|snapshot| snapshot["id"] == "ghost")
        .expect("ghost health entry must survive the plan");
    assert_eq!(ghost["healthy"], false, "{saved}");
    assert_eq!(ghost["attempts"], 7, "{saved}");
    assert_eq!(ghost["failures"], 4, "{saved}");
    let _ = std::fs::remove_file(&health);
}

#[test]
fn remote_plan_cli_requires_health_and_positive_interval_for_periodic_probe() {
    let missing_health = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--probe-interval-ms")
        .arg("100")
        .output()
        .unwrap();
    assert_eq!(missing_health.status.code(), Some(2), "{missing_health:?}");
    let stderr = String::from_utf8_lossy(&missing_health.stderr);
    assert!(
        stderr.contains("--probe-interval-ms requires --health"),
        "{stderr}"
    );

    let zero = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--health")
        .arg("unused-health.json")
        .arg("--probe-interval-ms")
        .arg("0")
        .output()
        .unwrap();
    assert_eq!(zero.status.code(), Some(2), "{zero:?}");
    let stderr = String::from_utf8_lossy(&zero.stderr);
    assert!(
        stderr.contains("--probe-interval-ms must be positive"),
        "{stderr}"
    );
}

#[test]
fn remote_plan_cli_requires_health_for_health_flush() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--health-flush-ms")
        .arg("100")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--health-flush-ms requires --health"),
        "{stderr}"
    );
}

#[test]
fn remote_plan_cli_validates_probe_concurrency() {
    let orphan = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--probe-concurrency")
        .arg("4")
        .output()
        .unwrap();
    assert_eq!(orphan.status.code(), Some(2), "{orphan:?}");
    let stderr = String::from_utf8_lossy(&orphan.stderr);
    assert!(
        stderr.contains("--probe-concurrency requires --probe or --probe-interval-ms"),
        "{stderr}"
    );

    for (value, expected) in [("0", "must be positive"), ("65", "must be <= 64")] {
        let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
            .arg("remote-plan")
            .arg("--health")
            .arg("unused-health.json")
            .arg("--probe")
            .arg("--probe-concurrency")
            .arg(value)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{value}: {stderr}");
    }
}

#[test]
fn remote_plan_cli_requires_health_for_probe() {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--probe")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--probe requires --health"), "{stderr}");
}

#[test]
fn remote_plan_cli_rejects_unsafe_scope() {
    let node = spawn_node("node-a", &[]);
    let input = serde_json::json!({
        "revision": 1,
        "scope": "../escape",
        "tasks": [task("t1")],
        "nodes": [node_json(&node, 1)],
    });
    let path = input_file("unsafe-scope", &input);

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .arg("remote-plan")
        .arg("--input")
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a safe namespace"), "{stderr}");
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
