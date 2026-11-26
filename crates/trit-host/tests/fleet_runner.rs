//! fleet 执行器接线：真实 headless 轮次、每任务独立 session、失败传播。

use std::path::PathBuf;

use trit_host::tools::WorkspaceConfig;
use trit_host::{HeadlessFleetRunner, Proposal, ScriptedModel, ToolRequest, run_fleet};
use trit_orch::{
    CancelToken, Dependency, FleetConfig, Plan, Task, TaskGuidance, TaskSpec, Ternary,
};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-fleet-runner-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn spec(id: &str, deps: Vec<Dependency>, file: &str) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps,
        },
        guidance: TaskGuidance {
            objective: format!("write {file}"),
            ..TaskGuidance::default()
        },
        max_tokens: 10_000,
        ..TaskSpec::default()
    }
}

fn required(task: &str) -> Dependency {
    Dependency {
        task: task.into(),
        required: Ternary::True,
    }
}

fn proposal(id: &str, file: &str) -> Proposal {
    Proposal {
        tool_calls: vec![ToolRequest::write(
            format!("{id}-w"),
            "write_file",
            file,
            "hello",
        )],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "c1".into(),
                kind: CriterionKind::Diff {
                    paths: vec![file.into()],
                },
            }],
        }),
        usage: HostUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cache_hit_tokens: 8,
            cache_miss_tokens: 2,
            estimated: false,
        },
        ..Proposal::default()
    }
}

fn workspace(root: &std::path::Path) -> WorkspaceConfig {
    let mut config = WorkspaceConfig::new(root);
    config.allow_write = true;
    config
}

#[test]
fn fleet_runner_executes_real_headless_turns_with_isolated_sessions() {
    let workspace_dir = temp_dir("ws");
    let session_dir = temp_dir("sessions");
    let plan = Plan::new(
        1,
        vec![
            spec("a", vec![], "a.txt"),
            spec("b", vec![required("a")], "b.txt"),
        ],
    )
    .unwrap();

    let report = run_fleet(
        &plan,
        FleetConfig::new(2),
        CancelToken::new(),
        HeadlessFleetRunner::new(
            workspace(&workspace_dir),
            session_dir.as_path(),
            |task: &TaskSpec| {
                ScriptedModel::new(vec![proposal(
                    &task.task.id,
                    &format!("{}.txt", task.task.id),
                )])
            },
        ),
    )
    .unwrap();

    assert_eq!(report.completed, vec!["a", "b"]);
    // 真实执行：workspace 里出现了两个文件。
    assert!(workspace_dir.join("a.txt").is_file());
    assert!(workspace_dir.join("b.txt").is_file());
    // 每任务独立 session。
    assert!(session_dir.join("a.jsonl").is_file());
    assert!(session_dir.join("b.jsonl").is_file());
    // 稳定引用。
    assert!(report.references()["a"].starts_with("a@0#"));
    assert!(report.references()["b"].starts_with("b@0#"));
    assert!(report.reusable("a"));

    let _ = std::fs::remove_dir_all(&workspace_dir);
    let _ = std::fs::remove_dir_all(&session_dir);
}

#[test]
fn fleet_runner_maps_exhausted_model_to_failure_and_defers_downstream() {
    let workspace_dir = temp_dir("ws-fail");
    let session_dir = temp_dir("sessions-fail");
    let plan = Plan::new(
        1,
        vec![
            spec("a", vec![], "a.txt"),
            spec("b", vec![required("a")], "b.txt"),
        ],
    )
    .unwrap();

    let report = run_fleet(
        &plan,
        FleetConfig::new(1),
        CancelToken::new(),
        HeadlessFleetRunner::new(
            workspace(&workspace_dir),
            session_dir.as_path(),
            |_task: &TaskSpec| ScriptedModel::new(vec![]),
        ),
    )
    .unwrap();

    assert_eq!(report.failed.as_ref().unwrap().task, "a");
    assert_eq!(report.deferred, vec!["b"]);
    assert!(!report.reusable("a"));
    assert!(!report.reusable("b"));
    // 失败任务的 session 仍然落盘，便于复盘。
    assert!(session_dir.join("a.jsonl").is_file());
    assert!(!workspace_dir.join("b.txt").exists());

    let _ = std::fs::remove_dir_all(&workspace_dir);
    let _ = std::fs::remove_dir_all(&session_dir);
}
