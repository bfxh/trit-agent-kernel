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
    script_for(dir, "a.txt")
}

fn script_for(dir: &Path, rel: &str) -> PathBuf {
    let path = dir.join("proposals.json");
    let proposals = serde_json::json!([{
        "tool_calls": [{
            "call_id": "w1",
            "tool": "write_file",
            "read_only": false,
            "paths": [rel],
            "content": "hello"
        }],
        "completion": {
            "criteria": [{"id": "c1", "kind": {"kind": "diff", "paths": [rel]}}]
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

fn request_with_roots(roots: Vec<String>) -> SubagentRequest {
    request_with_tools(vec!["write_file".into(), "read_file".into()], roots)
}

fn request_with_tools(tools: Vec<String>, roots: Vec<String>) -> SubagentRequest {
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
            value: "write a file".into(),
        }],
        &["objective"],
        64,
    )
    .unwrap();
    let profile = SubagentProfile {
        name: "child".into(),
        access: AccessMode::ReadWrite,
        write_roots: roots,
        tools,
        max_tokens: 1000,
    };
    SubagentRequest::new(&task, capsule, profile)
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
fn subagent_run_rejects_writes_outside_profile_roots() {
    let workspace = temp_dir("ws-roots");
    let scripts = temp_dir("script-roots");
    let result = run_subagent(
        &config(&workspace, &script_for(&scripts, "docs/escape.txt")),
        &request_with_roots(vec!["src".into()]),
    )
    .unwrap();

    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "profile 只允许 src，写 docs 必须失败: {:?}",
        result.status
    );
    assert!(
        !workspace.join("docs/escape.txt").exists(),
        "越界写入不能落盘"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn subagent_run_allows_writes_inside_profile_roots() {
    let workspace = temp_dir("ws-in-roots");
    let scripts = temp_dir("script-in-roots");
    let result = run_subagent(
        &config(&workspace, &script_for(&scripts, "src/ok.txt")),
        &request_with_roots(vec!["src".into()]),
    )
    .unwrap();

    assert!(
        matches!(result.status, DelegationStatus::Completed),
        "src 内的写入应通过: {:?}",
        result.status
    );
    assert!(workspace.join("src/ok.txt").is_file());

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&scripts);
}

#[test]
fn subagent_run_rejects_tools_outside_profile_whitelist() {
    let workspace = temp_dir("ws-tools");
    let scripts = temp_dir("script-tools");
    let result = run_subagent(
        &config(&workspace, &script_for(&scripts, "a.txt")),
        &request_with_tools(vec!["read_file".into()], vec!["a.txt".into()]),
    )
    .unwrap();

    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "profile 只允许 read_file，写文件必须失败: {:?}",
        result.status
    );
    assert!(!workspace.join("a.txt").exists(), "被拒绝的工具不能落盘");

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
