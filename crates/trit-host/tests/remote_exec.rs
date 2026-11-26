//! `remote-exec`：真实执行端点，用 `subagent-run` 跑写文件任务。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use trit_host::{
    RemotePlanConfig, run_remote_plan, run_remote_subagent, run_remote_subagent_reply_scoped,
};
use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, Plan, RemoteEndpoint, RemoteNode,
    SubagentProfile, SubagentRequest, Task, TaskSpec,
};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("trit-remote-exec-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn script(dir: &Path) -> PathBuf {
    let path = dir.join("proposals.json");
    let proposals = serde_json::json!([{
        "tool_calls": [{
            "call_id": "w1",
            "tool": "write_file",
            "read_only": false,
            "paths": ["a.txt"],
            "content": "hello-remote"
        }],
        "completion": {
            "criteria": [{"id": "c1", "kind": {"kind": "diff", "paths": ["a.txt"]}}]
        },
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "cache_hit_tokens": 8,
            "cache_miss_tokens": 2,
            "estimated": false
        }
    }]);
    std::fs::write(&path, proposals.to_string()).unwrap();
    path
}

fn task(id: &str) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        max_tokens: 1000,
        ..TaskSpec::default()
    }
}

fn request_for(spec: &TaskSpec) -> SubagentRequest {
    let capsule = ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "objective".into(),
            value: "write a.txt".into(),
        }],
        &["objective"],
        64,
    )
    .unwrap();
    let profile = SubagentProfile {
        name: format!("{}@remote", spec.task.id),
        access: AccessMode::ReadWrite,
        write_roots: vec!["a.txt".into()],
        tools: vec!["write_file".into(), "read_file".into()],
        max_tokens: 1000,
    };
    SubagentRequest::new(spec, capsule, profile)
}

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

fn spawn_exec(workspace: &Path, script: &Path, once: bool) -> TestNode {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trit-host"));
    command
        .arg("remote-exec")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--workspace")
        .arg(workspace)
        .arg("--script")
        .arg(script)
        .arg("--exec-timeout-secs")
        .arg("10")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if once {
        command.arg("--once");
    }
    let mut child = command.spawn().unwrap();
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

fn spawn_task_exec(root: &Path, script: &Path, once: bool) -> TestNode {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trit-host"));
    command
        .arg("remote-exec")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--workspace-root")
        .arg(root)
        .arg("--script")
        .arg(script)
        .arg("--exec-timeout-secs")
        .arg("10")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if once {
        command.arg("--once");
    }
    let mut child = command.spawn().unwrap();
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

#[test]
fn remote_exec_isolates_workspace_per_task() {
    let root = temp_dir("root");
    let scripts = temp_dir("scripts-task");
    let base = script(&scripts);
    std::fs::copy(&base, scripts.join("t1.json")).unwrap();
    let node = spawn_task_exec(&root, &scripts.join("{task}.json"), true);
    let endpoint = RemoteEndpoint {
        url: format!("{}/run", node.base),
        auth_token: None,
        timeout_secs: 30,
    };
    let result = run_remote_subagent(&endpoint, &request_for(&task("t1"))).unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    assert_eq!(
        std::fs::read_to_string(root.join("t1").join("a.txt")).unwrap(),
        "hello-remote"
    );

    let mut node = node;
    assert!(node.child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn per_task_workspace_separates_plan_scopes() {
    let root = temp_dir("root-scope");
    let scripts = temp_dir("scripts-scope");
    let base = script(&scripts);
    std::fs::copy(&base, scripts.join("t1.json")).unwrap();
    let mut node = spawn_task_exec(&root, &scripts.join("{task}.json"), false);
    let endpoint = RemoteEndpoint {
        url: format!("{}/run", node.base),
        auth_token: None,
        timeout_secs: 30,
    };
    for scope in ["plan-a", "plan-b"] {
        let reply =
            run_remote_subagent_reply_scoped(&endpoint, &request_for(&task("t1")), Some(scope))
                .unwrap();
        assert!(reply.result.reusable(), "{:?}", reply.result.status);
    }
    assert_eq!(
        std::fs::read_to_string(root.join("plan-a").join("t1").join("a.txt")).unwrap(),
        "hello-remote"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("plan-b").join("t1").join("a.txt")).unwrap(),
        "hello-remote"
    );

    node.child.kill().unwrap();
    assert!(node.child.wait().is_ok());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn remote_exec_rejects_unsafe_task_id_in_per_task_mode() {
    let root = temp_dir("root-unsafe");
    let scripts = temp_dir("scripts-unsafe");
    let node = spawn_task_exec(&root, &scripts.join("{task}.json"), true);
    let endpoint = RemoteEndpoint {
        url: format!("{}/run", node.base),
        auth_token: None,
        timeout_secs: 30,
    };
    let result = run_remote_subagent(&endpoint, &request_for(&task("../escape"))).unwrap();
    assert!(
        matches!(result.status, trit_orch::DelegationStatus::Failed { .. }),
        "{:?}",
        result.status
    );

    let mut node = node;
    assert!(node.child.wait().unwrap().success());
    assert!(!root.parent().unwrap().join("escape").exists());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn remote_exec_runs_subagent_and_writes_file() {
    let workspace = temp_dir("ws");
    let scripts = temp_dir("script");
    let node = spawn_exec(&workspace, &script(&scripts), true);
    let endpoint = RemoteEndpoint {
        url: format!("{}/run", node.base),
        auth_token: None,
        timeout_secs: 30,
    };
    let result = run_remote_subagent(&endpoint, &request_for(&task("a"))).unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "hello-remote"
    );

    let mut node = node;
    assert!(node.child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn remote_plan_dispatches_to_a_real_executor() {
    let workspace = temp_dir("ws-plan");
    let scripts = temp_dir("script-plan");
    let node = spawn_exec(&workspace, &script(&scripts), false);
    let plan = Plan::new(1, vec![task("a")]).unwrap();
    let report = run_remote_plan(
        &plan,
        vec![RemoteNode::new(
            "exec-node",
            RemoteEndpoint {
                url: format!("{}/run", node.base),
                auth_token: None,
                timeout_secs: 30,
            },
        )],
        RemotePlanConfig::new(1),
        CancelToken::new(),
        request_for,
    )
    .unwrap();

    assert_eq!(report.report.completed, ["a"], "{:?}", report.report);
    assert_eq!(report.node_of("a"), Some("exec-node"));
    assert_eq!(report.attempts_of("a").len(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "hello-remote"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}
