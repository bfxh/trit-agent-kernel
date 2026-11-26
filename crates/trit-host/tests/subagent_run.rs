//! 可执行子 agent：`trit-host subagent-run` 的真实进程回归。
//!
//! 权限映射是核心不变量：`ReadWrite` 能写、`ReadOnly` 拒绝写；只有
//! `judgment.is_complete()` 为 true 才回 `Completed`。

use std::path::{Path, PathBuf};

use trit_host::{SubagentProcessConfig, run_subagent, run_subagent_reply};
use trit_orch::{
    AccessMode, CapsuleEntry, ContextCapsule, DelegationStatus, SubagentProfile, SubagentRequest,
    Task, TaskSpec,
};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-subagent-run-{tag}-{}",
        std::process::id()
    ));
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
    SubagentProcessConfig::new(env!("CARGO_BIN_EXE_trit-host")).with_args(vec![
        "subagent-run".into(),
        "--workspace".into(),
        workspace.display().to_string(),
        "--script".into(),
        script.display().to_string(),
    ])
}

#[test]
fn subagent_run_executes_with_write_profile() {
    let workspace = temp_dir("ws");
    let scripts = temp_dir("script");
    let reply = run_subagent_reply(
        &config(&workspace, &script(&scripts)),
        &request(AccessMode::ReadWrite),
    )
    .unwrap();
    let result = &reply.result;

    assert!(result.reusable(), "{:?}", result.status);
    assert_eq!(result.task, "a");
    assert!(result.reference().starts_with("a@0#"));
    let content = String::from_utf8_lossy(&reply.content);
    assert!(content.starts_with("# 任务：a"), "{content}");
    assert!(content.contains("## 工具回执"), "{content}");
    // 真实执行：workspace 里出现文件。
    assert!(workspace.join("a.txt").is_file());

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn subagent_run_read_only_profile_cannot_write() {
    let workspace = temp_dir("ws-ro");
    let scripts = temp_dir("script-ro");
    let result = run_subagent(
        &config(&workspace, &script(&scripts)),
        &request(AccessMode::ReadOnly),
    )
    .unwrap();

    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "read-only profile must not complete a write task: {:?}",
        result.status
    );
    assert!(!result.reusable());
    assert!(!workspace.join("a.txt").exists());

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}
