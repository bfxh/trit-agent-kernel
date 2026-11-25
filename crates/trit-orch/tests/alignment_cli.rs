//! `trit-orch alignment` 的进程级回归：aligned=0 / unaligned=1 / unknown=3。

mod cli_support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

fn root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "trit-alignment-cli-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn manifest() -> serde_json::Value {
    json!({
        "schema": "alignment/1",
        "goal": {"id": "G1", "text": "ship it"},
        "requirements": [{
            "id": "R1",
            "text": "a",
            "priority": "must",
            "parent": "G1",
            "artifacts": ["A1"],
            "evidence": ["E1"],
            "changes": ["src/a.py"]
        }],
        "artifacts": [{"id": "A1", "path": "src/a.py"}],
        "evidence": [{"id": "E1", "path": "tests/a.py", "locator": "text:test_a"}]
    })
}

#[test]
fn cli_reports_aligned_and_unaligned() {
    let root = root("check");
    write(&root, "src/a.py", "print(1)\n");
    write(&root, "tests/a.py", "def test_a(): pass\n");
    fs::write(
        root.join("alignment.json"),
        serde_json::to_vec(&manifest()).unwrap(),
    )
    .unwrap();

    let (code, value) = cli_support::run(
        "alignment",
        &json!({"root": root.to_string_lossy(), "manifest": "alignment.json"}),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["status"], "aligned");

    let (code, value) = cli_support::run(
        "alignment",
        &json!({
            "root": root.to_string_lossy(),
            "manifest": "alignment.json",
            "changed": ["other.py"],
            "changed_known": true
        }),
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["status"], "unaligned");
    assert!(
        value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["code"] == "unlinked-change")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cli_missing_manifest_is_unknown_exit_three() {
    let root = root("missing");
    let (code, value) = cli_support::run(
        "alignment",
        &json!({"root": root.to_string_lossy(), "manifest": ".agent/alignment.json"}),
    );
    assert_eq!(code, 3, "{value}");
    assert_eq!(value["status"], "unknown");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cli_manifest_outside_root_is_unknown_exit_three() {
    let root = root("outside");
    let outside = root.parent().unwrap().join(format!(
        "{}-outside.json",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&outside, serde_json::to_vec(&manifest()).unwrap()).unwrap();
    let (code, value) = cli_support::run(
        "alignment",
        &json!({
            "root": root.to_string_lossy(),
            "manifest": outside.to_string_lossy()
        }),
    );
    assert_eq!(code, 3, "{value}");
    assert_eq!(value["status"], "unknown");
    assert!(
        value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["code"] == "manifest-outside-root"),
        "{value}"
    );
    let _ = fs::remove_file(outside);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cli_goal_cycle_is_unaligned_exit_one() {
    let root = root("goal-cycle");
    write(&root, "src/a.py", "print(1)\n");
    write(&root, "tests/a.py", "def test_a(): pass\n");
    let cycle = json!({
        "schema": "alignment/2",
        "goals": [
            {"id": "G1", "text": "root", "parent": "G2"},
            {"id": "G2", "text": "child", "parent": "G1"}
        ],
        "requirements": [{
            "id": "R1",
            "text": "a",
            "priority": "must",
            "parent": "G1",
            "artifacts": ["A1"],
            "evidence": ["E1"]
        }],
        "artifacts": [{"id": "A1", "path": "src/a.py"}],
        "evidence": [{"id": "E1", "path": "tests/a.py"}]
    });
    fs::write(root.join("cycle.json"), serde_json::to_vec(&cycle).unwrap()).unwrap();
    let (code, value) = cli_support::run(
        "alignment",
        &json!({"root": root.to_string_lossy(), "manifest": "cycle.json"}),
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["status"], "unaligned");
    assert!(
        value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["code"] == "goal-cycle"),
        "{value}"
    );
    let _ = fs::remove_dir_all(root);
}
