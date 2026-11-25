//! `docs/examples/gate-chain` 的端到端回归：示例必须能直接跑通，报告字段保持稳定。
//!
//! 示例文件用 `include_str!` 编进测试，改示例就会改测试输入；不需要运行时去猜相对路径。

#[allow(dead_code)]
mod cli_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const REPO: &str = include_str!("../../../docs/examples/gate-chain/workspace/.agent/repo.json");
const HANDOFF: &str =
    include_str!("../../../docs/examples/gate-chain/workspace/.agent/handoff/h-001.json");
const ALIGNMENT: &str =
    include_str!("../../../docs/examples/gate-chain/workspace/.agent/alignment.json");
const REPORT: &str = include_str!("../../../docs/examples/gate-chain/workspace/src/report.md");
const CHECK: &str =
    include_str!("../../../docs/examples/gate-chain/workspace/tests/report_check.md");

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "trit-gate-chain-example-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn materialize(root: &Path) {
    write(root, ".agent/repo.json", REPO);
    write(root, ".agent/handoff/h-001.json", HANDOFF);
    write(root, ".agent/alignment.json", ALIGNMENT);
    write(root, "src/report.md", REPORT);
    write(root, "tests/report_check.md", CHECK);
}

fn run_example(root: &Path) -> (i32, Value) {
    cli_support::run(
        "gate-chain",
        &json!({
            "root": root.to_string_lossy(),
            "expected": "bfxh/trit-agent-kernel",
            "task_kind": "agent",
            "packet": ".agent/handoff/h-001.json",
            "receiver_state_hash": "H",
            "manifest": ".agent/alignment.json"
        }),
    )
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn example_workspace_passes_the_chain() {
    let root = root("pass");
    materialize(&root);
    let (code, value) = run_example(&root);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["status"], "passed");
    assert_eq!(value["counts"]["passed"], 3);
    // alignment/2 的两个目标都被需求父链覆盖。
    assert_eq!(value["steps"][2]["detail"]["coverage"]["goals_covered"], 2);
    assert_eq!(value["steps"][2]["detail"]["status"], "aligned");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn example_report_shape_is_stable() {
    let root = root("shape");
    materialize(&root);
    let (_, value) = run_example(&root);

    let expected: Vec<String> = ["counts", "exit", "root", "schema", "status", "steps"]
        .iter()
        .map(|key| (*key).to_owned())
        .collect();
    assert_eq!(keys(&value), expected, "{value}");
    assert_eq!(value["schema"], "gate-chain/1");

    let steps = value["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 3);
    let ids: Vec<&str> = steps
        .iter()
        .map(|step| step["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["repo", "delivery", "alignment"]);
    for step in steps {
        let step_keys = keys(step);
        for required in ["gate", "id", "reason", "state"] {
            assert!(
                step_keys.contains(&required.to_owned()),
                "missing {required}: {step_keys:?}"
            );
        }
        assert!(step_keys.len() <= 5, "unexpected step keys: {step_keys:?}");
    }
    let _ = fs::remove_dir_all(root);
}
