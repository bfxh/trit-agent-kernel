//! 跨机派发：真实双节点 loopback、失败隔离、全失败与预取消。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use trit_host::{RemotePlanConfig, run_remote_plan};
use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, DelegationStatus, Dependency, Plan,
    RemoteEndpoint, RemoteNode, SubagentProfile, SubagentRequest, Task, TaskSpec, Ternary,
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
