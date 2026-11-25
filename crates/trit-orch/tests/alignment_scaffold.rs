//! `alignment-scaffold` 的契约：只生成故意留空的 v2 骨架，不自动 aligned。

mod cli_support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use trit_orch::alignment::{AlignmentOptions, ScaffoldOptions, check, scaffold};

fn root(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "trit-alignment-scaffold-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base
}

fn options(goal: &str, requirements: &[&str]) -> ScaffoldOptions {
    ScaffoldOptions {
        goal: goal.to_owned(),
        requirements: requirements.iter().map(|text| (*text).to_owned()).collect(),
        force: false,
    }
}

#[test]
fn scaffold_writes_incomplete_v2_and_gate_still_refuses() {
    let base = root("shape");
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    let outcome = scaffold(
        &root,
        Path::new("alignment.json"),
        &options("对齐骨架", &["需求一", "需求二"]),
    )
    .unwrap();
    assert_eq!(outcome.manifest, "alignment.json");
    assert_eq!(outcome.goals, 1);
    assert_eq!(outcome.requirements, 2);

    let document: Value =
        serde_json::from_str(&fs::read_to_string(root.join("alignment.json")).unwrap()).unwrap();
    assert_eq!(document["schema"], "alignment/2");
    assert_eq!(document["goals"].as_array().unwrap().len(), 1);
    assert_eq!(document["requirements"].as_array().unwrap().len(), 2);
    for key in ["artifacts", "evidence", "non_goals", "allow_unlinked"] {
        assert!(
            document[key].as_array().unwrap().is_empty(),
            "{key} must start empty"
        );
    }
    for (index, requirement) in document["requirements"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(requirement["id"], format!("R{}", index + 1));
        assert_eq!(requirement["priority"], "must");
        assert_eq!(requirement["parent"], "G1");
        for key in ["artifacts", "evidence", "changes"] {
            assert!(
                requirement[key].as_array().unwrap().is_empty(),
                "{key} must start empty"
            );
        }
    }

    let report = check(
        &root,
        Path::new("alignment.json"),
        &AlignmentOptions {
            changed: Some(vec!["src/a.py".to_owned()]),
            ..Default::default()
        },
    );
    assert_eq!(report.exit, 1, "{report:?}");
    assert_eq!(report.status, "unaligned");
    let codes: Vec<&str> = report
        .failures
        .iter()
        .map(|failure| failure.code.as_str())
        .collect();
    assert!(codes.contains(&"missing-artifact"), "{codes:?}");
    assert!(codes.contains(&"missing-evidence"), "{codes:?}");
    assert!(codes.contains(&"unlinked-change"), "{codes:?}");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn scaffold_requires_force_to_overwrite_and_rejects_bad_inputs() {
    let base = root("guard");
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    let manifest = Path::new("alignment.json");

    scaffold(&root, manifest, &options("第一版", &["需求一"])).unwrap();
    let before = fs::read(root.join("alignment.json")).unwrap();
    let error = scaffold(&root, manifest, &options("第二版", &["需求二"])).unwrap_err();
    assert_eq!(error.kind(), "alignment_scaffold_rejected");
    assert!(error.to_string().contains("已存在"), "{error}");
    assert_eq!(fs::read(root.join("alignment.json")).unwrap(), before);

    let mut force = options("第二版", &["需求二"]);
    force.force = true;
    let outcome = scaffold(&root, manifest, &force).unwrap();
    assert_eq!(outcome.requirements, 1);
    let replaced: Value =
        serde_json::from_str(&fs::read_to_string(root.join("alignment.json")).unwrap()).unwrap();
    assert_eq!(replaced["goals"][0]["text"], "第二版");

    let error = scaffold(&root, manifest, &options(" ", &["需求"])).unwrap_err();
    assert_eq!(error.kind(), "alignment_scaffold_rejected");
    let error = scaffold(&root, manifest, &options("目标", &[])).unwrap_err();
    assert_eq!(error.kind(), "alignment_scaffold_rejected");

    let escape = base.join("outside.json");
    let error = scaffold(
        &root,
        Path::new("../outside.json"),
        &options("逃逸", &["需求"]),
    )
    .unwrap_err();
    assert_eq!(error.kind(), "alignment_scaffold_rejected");
    assert!(!escape.exists());

    fs::create_dir_all(root.join("directory")).unwrap();
    let error = scaffold(&root, Path::new("directory"), &options("目录", &["需求"])).unwrap_err();
    assert_eq!(error.kind(), "alignment_scaffold_rejected");

    let absolute_outside = base.join("absolute-outside.json");
    let error = scaffold(&root, &absolute_outside, &options("绝对路径", &["需求"])).unwrap_err();
    assert_eq!(error.kind(), "alignment_scaffold_rejected");
    assert!(!absolute_outside.exists());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn cli_scaffold_reports_success_and_usage_errors_as_exit_two() {
    let base = root("cli");
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    let input = json!({
        "root": root.to_string_lossy(),
        "manifest": "align.json",
        "goal": "CLI 骨架",
        "requirements": ["需求一", "需求二"],
    });
    let (code, value) = cli_support::run("alignment-scaffold", &input);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["ok"], true);
    assert_eq!(value["manifest"], "align.json");
    assert_eq!(value["goals"], 1);
    assert_eq!(value["requirements"], 2);
    assert!(root.join("align.json").is_file());

    let (code, value) = cli_support::run("alignment-scaffold", &input);
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["kind"], "alignment_scaffold_rejected");
    assert!(
        value["error"].as_str().unwrap().contains("已存在"),
        "{value}"
    );

    let (code, value) = cli_support::run(
        "alignment-scaffold",
        &json!({
            "root": root.to_string_lossy(),
            "manifest": "empty.json",
            "goal": "空需求",
            "requirements": [],
        }),
    );
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["kind"], "alignment_scaffold_rejected");
    assert!(!root.join("empty.json").exists());
    let _ = fs::remove_dir_all(base);
}
