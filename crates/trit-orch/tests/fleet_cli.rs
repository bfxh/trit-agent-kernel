//! `trit-orch fleet-run` 的宿主契约：确定性演练、失败传播、取消归类与指标导出。

use std::io::Write;
use std::process::{Command, Stdio};

fn run(input: serde_json::Value) -> (i32, serde_json::Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-orch"))
        .arg("fleet-run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("fleet-run must print JSON");
    (output.status.code().unwrap_or(-1), value)
}

fn spec(id: &str, deps: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "task": {"id": id, "duration": 1, "deps": deps},
        "requirements": {"cpu": 0, "memory": 0},
        "max_tokens": 1000,
    })
}

fn dep(task: &str) -> serde_json::Value {
    serde_json::json!({"task": task, "required": "True"})
}

#[test]
fn fleet_run_reports_dependency_chain_references_and_metrics() {
    let (code, out) = run(serde_json::json!({
        "revision": 1,
        "workers": 2,
        "tasks": [
            spec("a", serde_json::json!([])),
            spec("b", serde_json::json!([dep("a")])),
        ],
    }));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], true);
    assert_eq!(out["completed"], serde_json::json!(["a", "b"]));
    assert!(out["failed"].is_null());
    assert!(out["deferred"].as_array().unwrap().is_empty());
    assert!(out["cancelled"].as_array().unwrap().is_empty());

    // 每个任务都有稳定引用 task@epoch#digest。
    assert!(out["references"]["a"].as_str().unwrap().starts_with("a@0#"));
    assert!(out["references"]["b"].as_str().unwrap().starts_with("b@0#"));

    // 指标同时以 JSON 与 Prometheus 文本导出。
    assert_eq!(out["metrics"]["started"], 2);
    assert_eq!(out["metrics"]["completed"], 2);
    let prometheus = out["metrics_prometheus"].as_str().unwrap();
    assert!(prometheus.contains("trit_schedule_started"), "{prometheus}");
}

#[test]
fn fleet_run_marks_simulated_failure_as_failed_and_defers_downstream() {
    let (code, out) = run(serde_json::json!({
        "revision": 1,
        "workers": 1,
        "fail": ["b"],
        "tasks": [
            spec("a", serde_json::json!([])),
            spec("b", serde_json::json!([dep("a")])),
            spec("c", serde_json::json!([dep("b")])),
        ],
    }));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], true);
    assert_eq!(out["completed"], serde_json::json!(["a"]));
    assert_eq!(out["failed"]["task"], "b");
    assert!(
        out["failed"]["message"]
            .as_str()
            .unwrap()
            .contains("simulated failure")
    );
    assert_eq!(out["deferred"], serde_json::json!(["c"]));
    // 未执行任务仍可引用，但状态不是完成。
    assert!(out["references"]["c"].as_str().is_some());
}

#[test]
fn fleet_run_cancel_after_marks_remaining_as_cancelled() {
    let (code, out) = run(serde_json::json!({
        "revision": 1,
        "workers": 1,
        "cancel_after": 1,
        "tasks": [
            spec("a", serde_json::json!([])),
            spec("b", serde_json::json!([dep("a")])),
        ],
    }));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["completed"], serde_json::json!(["a"]));
    assert_eq!(out["cancelled"], serde_json::json!(["b"]));
}

#[test]
fn fleet_run_rejects_zero_workers_with_machine_readable_error() {
    let (code, out) = run(serde_json::json!({
        "revision": 1,
        "workers": 0,
        "tasks": [spec("a", serde_json::json!([]))],
    }));
    assert_eq!(code, 1);
    assert_eq!(out["ok"], false);
    assert_eq!(out["kind"], "no_workers");
}

#[test]
fn unknown_subcommand_lists_fleet_run() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-orch"))
        .arg("definitely-not-a-command")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(
        value["error"].as_str().unwrap().contains("fleet-run"),
        "{}",
        value["error"]
    );
}
