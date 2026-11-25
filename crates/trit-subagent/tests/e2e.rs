//! 独立子 agent 发布物的端到端回归：真实 spawn、权限映射、帧往返。

use std::path::{Path, PathBuf};

use trit_host::{SubagentProcessConfig, run_subagent};
use trit_orch::{
    AccessMode, CapsuleEntry, ContextCapsule, DelegationStatus, SubagentProfile, SubagentRequest,
    Task, TaskSpec,
};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("trit-subagent-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn request(access: AccessMode) -> SubagentRequest {
    let task = TaskSpec {
        task: Task {
            id: "a".into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        epoch: 0,
        ..TaskSpec::default()
    };
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
        name: "child".into(),
        access,
        write_roots: if access == AccessMode::ReadWrite {
            vec!["a.txt".into()]
        } else {
            Vec::new()
        },
        tools: vec!["write_file".into(), "read_file".into()],
        max_tokens: 1000,
    };
    SubagentRequest::new(&task, capsule, profile)
}

fn script(dir: &Path) -> PathBuf {
    let path = dir.join("proposals.json");
    let proposals = serde_json::json!([{
        "tool_calls": [{
            "call_id": "w1",
            "tool": "write_file",
            "read_only": false,
            "paths": ["a.txt"],
            "content": "hello"
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

fn config(workspace: &Path, script: &Path) -> SubagentProcessConfig {
    SubagentProcessConfig::new(env!("CARGO_BIN_EXE_trit-subagent")).with_args(vec![
        "--workspace".into(),
        workspace.display().to_string(),
        "--script".into(),
        script.display().to_string(),
    ])
}

#[test]
fn published_binary_executes_write_task() {
    let workspace = temp_dir("ws");
    let scripts = temp_dir("script");
    let result = run_subagent(
        &config(&workspace, &script(&scripts)),
        &request(AccessMode::ReadWrite),
    )
    .unwrap();

    assert!(result.reusable(), "{:?}", result.status);
    assert!(result.reference().starts_with("a@0#"));
    assert!(workspace.join("a.txt").is_file());

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn published_binary_enforces_read_only_profile() {
    let workspace = temp_dir("ws-ro");
    let scripts = temp_dir("script-ro");
    let result = run_subagent(
        &config(&workspace, &script(&scripts)),
        &request(AccessMode::ReadOnly),
    )
    .unwrap();

    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "{:?}",
        result.status
    );
    assert!(!workspace.join("a.txt").exists());

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn published_binary_reports_protocol_errors() {
    // 没有 --script：进程非零退出，宿主按 exit_status 映射为 Failed。
    let workspace = temp_dir("ws-bad");
    let config = SubagentProcessConfig::new(env!("CARGO_BIN_EXE_trit-subagent")).with_args(vec![
        "--workspace".into(),
        workspace.display().to_string(),
    ]);
    let result = run_subagent(&config, &request(AccessMode::ReadOnly)).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "{:?}",
        result.status
    );
    let _ = std::fs::remove_dir_all(&workspace);
}
