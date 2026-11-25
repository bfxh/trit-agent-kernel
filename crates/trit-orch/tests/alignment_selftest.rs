//! Python `alignment_gate.py self-test` 核心契约的 Rust 等价。
//!
//! 这里不试图逐条复制终端文案；重点是同一组输入仍得到 aligned / unaligned /
//! unknown，并且关键失败码不丢。Rust 版对未知字段统一 fail-closed 为
//! `manifest-unreadable`，与 Python 的逐字段诊断路径不同。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use trit_orch::alignment::{AlignmentOptions, AlignmentReport, check};

fn base(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "trit-alignment-selftest-{tag}-{}-{}",
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

fn fixtures(root: &Path) {
    for (rel, text) in [
        ("src/a.py", "def f():\n    return 1\n"),
        (
            "tests/test_a.py",
            "from src.a import f\n\ndef test_a():\n    assert f() == 1\n",
        ),
        ("vendor/a.py", "x = 1\n"),
        ("src/extra.py", "x = 2\n"),
    ] {
        write(root, rel, text);
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn v1() -> Value {
    json!({
        "schema": "alignment/1",
        "goal": {"id": "G1", "text": "把输入整理成可验收的产物"},
        "requirements": [
            {
                "id": "R1",
                "text": "产物包含目标字段",
                "priority": "must",
                "parent": "G1",
                "artifacts": ["A1"],
                "evidence": ["E1"],
                "changes": ["src/a.py"],
            },
            {
                "id": "R2",
                "text": "证据能定位到测试",
                "priority": "should",
                "parent": "G1",
                "artifacts": ["A1"],
                "evidence": ["E1"],
            },
        ],
        "artifacts": [{"id": "A1", "path": "src/a.py"}],
        "evidence": [{"id": "E1", "path": "tests/test_a.py", "locator": "text:test_a"}],
        "non_goals": [{"id": "N1", "text": "不碰 vendor", "paths": ["vendor/**"]}],
        "allow_unlinked": ["Cargo.lock"],
    })
}

fn v2() -> Value {
    let mut doc = v1();
    doc.as_object_mut().unwrap().remove("goal");
    doc["schema"] = json!("alignment/2");
    doc["goals"] = json!([{"id": "G1", "text": "把输入整理成可验收的产物"}]);
    doc
}

fn v3() -> Value {
    let mut doc = v2();
    doc["schema"] = json!("alignment/3");
    doc["evidence"][0] = json!({
        "id": "E1",
        "path": "tests/test_a.py",
        "locator": "text:test_a",
        "kind": "run",
        "command": "python -m pytest tests/test_a.py",
    });
    doc
}

fn check_doc(root: &Path, doc: &Value, options: &AlignmentOptions) -> AlignmentReport {
    fs::write(root.join("manifest.json"), serde_json::to_vec(doc).unwrap()).unwrap();
    check(root, Path::new("manifest.json"), options)
}

fn check_text(root: &Path, rel: &str, text: &str) -> AlignmentReport {
    write(root, rel, text);
    check(root, Path::new(rel), &AlignmentOptions::default())
}

fn expect_aligned(report: &AlignmentReport) {
    assert_eq!(report.status, "aligned", "{report:?}");
    assert_eq!(report.exit, 0, "{report:?}");
}

fn expect_code(report: &AlignmentReport, code: &str) {
    assert!(
        report.failures.iter().any(|failure| failure.code == code),
        "missing {code}: {report:?}"
    );
}

fn expect_unknown(report: &AlignmentReport, code: &str) {
    assert_eq!(report.status, "unknown", "{report:?}");
    assert_eq!(report.exit, 3, "{report:?}");
    expect_code(report, code);
}

#[test]
fn selftest_goal_tree_and_schemas() {
    let root = base("tree");
    fixtures(&root);

    let report = check_doc(&root, &v1(), &AlignmentOptions::default());
    expect_aligned(&report);
    assert_eq!(report.score, 1.0, "{report:?}");
    assert_eq!(report.must_score, 1.0, "{report:?}");
    assert_eq!(report.coverage.goals_covered, 1, "{report:?}");
    assert_eq!(report.coverage.requirements_full, 2, "{report:?}");
    assert_eq!(report.counts.goals, 1, "{report:?}");

    let report = check_doc(&root, &v2(), &AlignmentOptions::default());
    expect_aligned(&report);
    assert_eq!(report.schema, "alignment/2", "{report:?}");

    let mut child = v2();
    child["goals"].as_array_mut().unwrap().push(json!({
        "id": "G2", "text": "子目标", "parent": "G1"
    }));
    child["requirements"][0]["parent"] = json!("G2");
    child["requirements"][1]["parent"] = json!("R1");
    let report = check_doc(&root, &child, &AlignmentOptions::default());
    expect_aligned(&report);
    assert_eq!(report.counts.goals, 2, "{report:?}");
    assert_eq!(report.coverage.goals_covered, 2, "{report:?}");

    let mut uncovered = v2();
    uncovered["goals"].as_array_mut().unwrap().push(json!({
        "id": "G2", "text": "空挂的子目标", "parent": "G1"
    }));
    let report = check_doc(&root, &uncovered, &AlignmentOptions::default());
    expect_code(&report, "goal-uncovered");
    assert_eq!(report.coverage.goals_covered, 1, "{report:?}");
    let report = check_doc(
        &root,
        &uncovered,
        &AlignmentOptions {
            strict: false,
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "goal-uncovered");

    let mut cycle = v2();
    cycle["goals"].as_array_mut().unwrap().push(json!({
        "id": "G2", "text": "子目标", "parent": "G1"
    }));
    cycle["goals"][0]["parent"] = json!("G2");
    let report = check_doc(&root, &cycle, &AlignmentOptions::default());
    expect_code(&report, "goal-cycle");

    let mut roots = v2();
    roots["goals"].as_array_mut().unwrap().push(json!({
        "id": "G2", "text": "第二个根", "parent": null
    }));
    let report = check_doc(&root, &roots, &AlignmentOptions::default());
    expect_code(&report, "goal-root");

    let mut unknown_parent = v2();
    unknown_parent["goals"][0]["parent"] = json!("G9");
    let report = check_doc(&root, &unknown_parent, &AlignmentOptions::default());
    expect_code(&report, "unknown-goal-parent");

    let mut missing_goals = v2();
    missing_goals.as_object_mut().unwrap().remove("goals");
    let report = check_doc(&root, &missing_goals, &AlignmentOptions::default());
    expect_code(&report, "goals-missing");

    let mut mixed = v2();
    mixed["goal"] = json!({"id": "G1", "text": "混用 field"});
    let report = check_doc(&root, &mixed, &AlignmentOptions::default());
    expect_code(&report, "goal-field");

    let mut v1_with_goals = v1();
    v1_with_goals["goals"] = json!([{"id": "G1", "text": "混用 field"}]);
    let report = check_doc(&root, &v1_with_goals, &AlignmentOptions::default());
    expect_code(&report, "goals-field");

    let report = check_doc(&root, &v3(), &AlignmentOptions::default());
    expect_aligned(&report);
    assert_eq!(report.counts.provenance_verified, 0, "{report:?}");
    let report = check_doc(
        &root,
        &v3(),
        &AlignmentOptions {
            executed: Some(vec!["python -m pytest tests/test_a.py".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_aligned(&report);
    assert_eq!(report.counts.provenance_verified, 1, "{report:?}");
    assert_eq!(
        report.coverage.requirements_provenance_verified, 2,
        "{report:?}"
    );
    let report = check_doc(
        &root,
        &v3(),
        &AlignmentOptions {
            executed: Some(vec!["pytest -q".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "evidence-not-executed");
    let report = check_doc(
        &root,
        &v3(),
        &AlignmentOptions {
            executed: Some(vec!["python -m pytest tests/test_a.py -q".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "evidence-not-executed");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn selftest_provenance_and_length_rules() {
    let root = base("provenance");
    fixtures(&root);

    let mut static_evidence = v3();
    static_evidence["evidence"][0]["kind"] = json!("file");
    static_evidence["evidence"][0]
        .as_object_mut()
        .unwrap()
        .remove("command");
    let report = check_doc(&root, &static_evidence, &AlignmentOptions::default());
    expect_code(&report, "missing-provenance");

    let mut no_path = v3();
    no_path["evidence"][0]["command"] = json!("python -m pytest");
    let report = check_doc(&root, &no_path, &AlignmentOptions::default());
    expect_code(&report, "evidence-command-path");

    let mut empty_command = v3();
    empty_command["evidence"][0]["command"] = json!("");
    let report = check_doc(&root, &empty_command, &AlignmentOptions::default());
    expect_code(&report, "evidence-command");

    let mut bad_kind = v3();
    bad_kind["evidence"][0]["kind"] = json!("shell");
    let report = check_doc(&root, &bad_kind, &AlignmentOptions::default());
    expect_code(&report, "evidence-kind");

    let mut unknown_field = v3();
    unknown_field["evidence"][0]["receipt"] = json!("run.json");
    let report = check_doc(&root, &unknown_field, &AlignmentOptions::default());
    expect_unknown(&report, "manifest-unreadable");

    let mut optional = v3();
    optional["requirements"] = json!([{
        "id": "R1",
        "text": "只做可选",
        "priority": "may",
        "parent": "G1",
        "artifacts": ["A1"],
        "evidence": ["E1"],
    }]);
    optional["evidence"][0]["kind"] = json!("file");
    optional["evidence"][0]
        .as_object_mut()
        .unwrap()
        .remove("command");
    let report = check_doc(&root, &optional, &AlignmentOptions::default());
    expect_aligned(&report);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.code == "optional-no-provenance"),
        "{report:?}"
    );

    let mut v2_with_kind = v2();
    v2_with_kind["evidence"][0]["kind"] = json!("run");
    let report = check_doc(&root, &v2_with_kind, &AlignmentOptions::default());
    expect_code(&report, "unknown-field");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn selftest_field_and_path_failures() {
    let root = base("fields");
    fixtures(&root);

    let mut missing_goal = v1();
    missing_goal.as_object_mut().unwrap().remove("goal");
    let report = check_doc(&root, &missing_goal, &AlignmentOptions::default());
    expect_code(&report, "goal-missing");

    let mut empty_goal = v1();
    empty_goal["goal"]["text"] = json!("");
    let report = check_doc(&root, &empty_goal, &AlignmentOptions::default());
    expect_code(&report, "goal-invalid");

    let mut duplicate = v1();
    duplicate["requirements"][1]["id"] = json!("R1");
    let report = check_doc(&root, &duplicate, &AlignmentOptions::default());
    expect_code(&report, "duplicate-id");

    let mut unknown_parent = v1();
    unknown_parent["requirements"][0]["parent"] = json!("G9");
    let report = check_doc(&root, &unknown_parent, &AlignmentOptions::default());
    expect_code(&report, "unknown-parent");

    let mut parent_cycle = v1();
    parent_cycle["requirements"][0]["parent"] = json!("R2");
    parent_cycle["requirements"][1]["parent"] = json!("R1");
    let report = check_doc(&root, &parent_cycle, &AlignmentOptions::default());
    expect_code(&report, "parent-cycle");

    let mut no_requirements = v1();
    no_requirements["requirements"] = json!([]);
    let report = check_doc(&root, &no_requirements, &AlignmentOptions::default());
    expect_code(&report, "no-requirements");

    let mut bad_artifact_ref = v1();
    bad_artifact_ref["requirements"][0]["artifacts"] = json!(["A9"]);
    let report = check_doc(&root, &bad_artifact_ref, &AlignmentOptions::default());
    expect_code(&report, "missing-artifact-ref");

    let mut bad_evidence_ref = v1();
    bad_evidence_ref["requirements"][0]["evidence"] = json!(["E9"]);
    let report = check_doc(&root, &bad_evidence_ref, &AlignmentOptions::default());
    expect_code(&report, "missing-evidence-ref");

    let mut no_artifact = v1();
    no_artifact["requirements"][0]["artifacts"] = json!([]);
    let report = check_doc(&root, &no_artifact, &AlignmentOptions::default());
    expect_code(&report, "missing-artifact");

    let mut no_evidence = v1();
    no_evidence["requirements"][0]["evidence"] = json!([]);
    let report = check_doc(&root, &no_evidence, &AlignmentOptions::default());
    expect_code(&report, "missing-evidence");

    let mut should_without_evidence = v1();
    should_without_evidence["requirements"][1]["evidence"] = json!([]);
    let report = check_doc(
        &root,
        &should_without_evidence,
        &AlignmentOptions::default(),
    );
    expect_code(&report, "missing-evidence");
    let report = check_doc(
        &root,
        &should_without_evidence,
        &AlignmentOptions {
            strict: false,
            ..AlignmentOptions::default()
        },
    );
    expect_aligned(&report);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.code == "missing-evidence"),
        "{report:?}"
    );

    let mut optional_no_artifact = v1();
    optional_no_artifact["requirements"][0]["priority"] = json!("may");
    optional_no_artifact["requirements"][0]["artifacts"] = json!([]);
    let report = check_doc(&root, &optional_no_artifact, &AlignmentOptions::default());
    expect_aligned(&report);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.code == "optional-no-artifact"),
        "{report:?}"
    );

    let mut bad_priority = v1();
    bad_priority["requirements"][0]["priority"] = json!("required");
    let report = check_doc(&root, &bad_priority, &AlignmentOptions::default());
    expect_code(&report, "priority-invalid");

    let mut empty_text = v1();
    empty_text["requirements"][0]["text"] = json!("");
    let report = check_doc(&root, &empty_text, &AlignmentOptions::default());
    expect_code(&report, "requirement-text");

    let mut bad_changes = v1();
    bad_changes["requirements"][0]["changes"] = json!("src/a.py");
    let report = check_doc(&root, &bad_changes, &AlignmentOptions::default());
    expect_unknown(&report, "manifest-unreadable");

    let mut bad_sha = v1();
    bad_sha["artifacts"][0]["sha256"] = json!("xyz");
    let report = check_doc(&root, &bad_sha, &AlignmentOptions::default());
    expect_code(&report, "sha-invalid");

    let mut missing_artifact = v1();
    missing_artifact["artifacts"][0]["path"] = json!("src/missing.py");
    let report = check_doc(&root, &missing_artifact, &AlignmentOptions::default());
    expect_code(&report, "artifact-missing");

    let mut missing_evidence = v1();
    missing_evidence["evidence"][0]["path"] = json!("tests/missing.py");
    let report = check_doc(&root, &missing_evidence, &AlignmentOptions::default());
    expect_code(&report, "evidence-missing");

    let mut bad_hash = v1();
    bad_hash["artifacts"][0]["sha256"] = json!("0".repeat(64));
    let report = check_doc(&root, &bad_hash, &AlignmentOptions::default());
    expect_code(&report, "hash-mismatch");

    let good_sha = "5b76d0962c09ab4ee309fac65fad3568c97abdec983b405146ae3e86a235e352";
    let mut good_hash = v1();
    good_hash["artifacts"][0]["sha256"] = json!(good_sha.to_uppercase());
    let report = check_doc(&root, &good_hash, &AlignmentOptions::default());
    expect_aligned(&report);

    let mut line_locator = v1();
    line_locator["evidence"][0]["locator"] = json!("L1-L4");
    let report = check_doc(&root, &line_locator, &AlignmentOptions::default());
    expect_aligned(&report);

    let mut range = v1();
    range["evidence"][0]["locator"] = json!("L99");
    let report = check_doc(&root, &range, &AlignmentOptions::default());
    expect_code(&report, "locator-out-of-range");

    let mut missing_text = v1();
    missing_text["evidence"][0]["locator"] = json!("text:not-here");
    let report = check_doc(&root, &missing_text, &AlignmentOptions::default());
    expect_code(&report, "locator-missing");

    let mut bad_locator = v1();
    bad_locator["evidence"][0]["locator"] = json!("## section");
    let report = check_doc(&root, &bad_locator, &AlignmentOptions::default());
    expect_code(&report, "locator-invalid");

    let mut escape = v1();
    escape["artifacts"][0]["path"] = json!("../escape.py");
    let report = check_doc(&root, &escape, &AlignmentOptions::default());
    expect_code(&report, "bad-path");

    let mut absolute = v1();
    absolute["artifacts"][0]["path"] = json!("C:/abs.py");
    let report = check_doc(&root, &absolute, &AlignmentOptions::default());
    expect_code(&report, "bad-path");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn selftest_changes_non_goals_and_manifest_errors() {
    let root = base("changes");
    fixtures(&root);

    let mut unlinked = v1();
    unlinked["requirements"][0]["changes"] = json!(["src/other.py"]);
    let report = check_doc(
        &root,
        &unlinked,
        &AlignmentOptions {
            changed: Some(vec!["misc/x.py".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "unlinked-change");

    let mut glob = v1();
    glob["requirements"][0]["changes"] = json!(["lib/**"]);
    let report = check_doc(
        &root,
        &glob,
        &AlignmentOptions {
            changed: Some(vec!["lib/x.py".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_aligned(&report);

    let mut allow_unlinked = v1();
    allow_unlinked["requirements"][0]["changes"] = json!([]);
    allow_unlinked["allow_unlinked"] = json!(["generated/**"]);
    let report = check_doc(
        &root,
        &allow_unlinked,
        &AlignmentOptions {
            changed: Some(vec!["generated/x.py".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_aligned(&report);

    let mut non_goal_change = v1();
    non_goal_change["non_goals"][0]["paths"] = json!(["src/**"]);
    let report = check_doc(
        &root,
        &non_goal_change,
        &AlignmentOptions {
            changed: Some(vec!["src/a.py".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "non-goal-change");

    let mut declared = v1();
    let report = check_doc(
        &root,
        &declared,
        &AlignmentOptions {
            changed: Some(vec!["src/a.py".to_owned()]),
            changed_known: true,
            ..AlignmentOptions::default()
        },
    );
    expect_aligned(&report);
    assert_eq!(
        report.coverage.requirements_changes_realized, 1,
        "{report:?}"
    );

    declared["requirements"][0]["changes"] = json!(["src/old.py"]);
    let report = check_doc(
        &root,
        &declared,
        &AlignmentOptions {
            changed: Some(vec!["src/a.py".to_owned()]),
            changed_known: true,
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "declared-change-missing");

    let report = check_doc(
        &root,
        &v1(),
        &AlignmentOptions {
            changed: Some(vec![]),
            changed_known: true,
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "declared-change-missing");
    assert_eq!(report.counts.changed_known, 1, "{report:?}");
    assert_eq!(report.counts.changed, 0, "{report:?}");

    let mut empty_declared = v1();
    empty_declared["requirements"][0]["changes"] = json!(["src/a.py"]);
    let report = check_doc(
        &root,
        &empty_declared,
        &AlignmentOptions {
            changed: Some(vec![]),
            changed_known: true,
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "declared-change-missing");

    let mut multi_declared = v1();
    multi_declared["requirements"][0]["changes"] = json!(["src/a.py", "src/old.py"]);
    let report = check_doc(
        &root,
        &multi_declared,
        &AlignmentOptions {
            changed: Some(vec!["src/a.py".to_owned()]),
            changed_known: true,
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "declared-change-missing");

    let mut should_declared = v1();
    should_declared["requirements"][0]["changes"] = json!([]);
    should_declared["requirements"][1]["changes"] = json!(["src/old.py"]);
    let report = check_doc(
        &root,
        &should_declared,
        &AlignmentOptions {
            changed: Some(vec![]),
            changed_known: true,
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "declared-change-missing");
    let report = check_doc(
        &root,
        &should_declared,
        &AlignmentOptions {
            changed: Some(vec![]),
            changed_known: true,
            strict: false,
            ..AlignmentOptions::default()
        },
    );
    expect_aligned(&report);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.code == "declared-change-missing"),
        "{report:?}"
    );

    let bad_change = v1();
    let report = check_doc(
        &root,
        &bad_change,
        &AlignmentOptions {
            changed: Some(vec!["../escape.py".to_owned()]),
            ..AlignmentOptions::default()
        },
    );
    expect_code(&report, "bad-change");

    let mut non_goal_paths = v1();
    non_goal_paths["non_goals"][0]["paths"] = json!([]);
    let report = check_doc(&root, &non_goal_paths, &AlignmentOptions::default());
    expect_code(&report, "non-goal-paths");

    let mut non_goal_artifact = v1();
    non_goal_artifact["non_goals"][0]["artifacts"] = json!(["A1"]);
    let report = check_doc(&root, &non_goal_artifact, &AlignmentOptions::default());
    expect_code(&report, "non-goal-artifact");

    let mut non_goal_evidence = v1();
    non_goal_evidence["non_goals"][0]["evidence"] = json!(["E1"]);
    let report = check_doc(&root, &non_goal_evidence, &AlignmentOptions::default());
    expect_code(&report, "non-goal-evidence");

    let mut non_goal_artifact_ref = v1();
    non_goal_artifact_ref["non_goals"][0]["artifacts"] = json!(["A9"]);
    let report = check_doc(&root, &non_goal_artifact_ref, &AlignmentOptions::default());
    expect_code(&report, "non-goal-artifacts-ref");

    let mut non_goal_evidence_ref = v1();
    non_goal_evidence_ref["non_goals"][0]["evidence"] = json!(["E9"]);
    let report = check_doc(&root, &non_goal_evidence_ref, &AlignmentOptions::default());
    expect_code(&report, "non-goal-evidence-ref");

    let mut unreferenced = v1();
    unreferenced["artifacts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "A2", "path": "src/extra.py"}));
    unreferenced["non_goals"][0]["artifacts"] = json!(["A2"]);
    let report = check_doc(&root, &unreferenced, &AlignmentOptions::default());
    expect_aligned(&report);

    let mut non_goal_artifact_path = v1();
    non_goal_artifact_path["artifacts"][0]["path"] = json!("vendor/a.py");
    let report = check_doc(&root, &non_goal_artifact_path, &AlignmentOptions::default());
    expect_code(&report, "non-goal-artifact-path");

    let mut scope_literal = v1();
    scope_literal["requirements"][0]["changes"] = json!(["vendor/x.py"]);
    let report = check_doc(&root, &scope_literal, &AlignmentOptions::default());
    expect_code(&report, "non-goal-scope");

    let mut scope_glob = v1();
    scope_glob["requirements"][0]["changes"] = json!(["vendor/**"]);
    let report = check_doc(&root, &scope_glob, &AlignmentOptions::default());
    expect_code(&report, "non-goal-scope");

    let mut scope_literal_path = v1();
    scope_literal_path["non_goals"][0]["paths"] = json!(["vendor/a.py"]);
    scope_literal_path["requirements"][0]["changes"] = json!(["vendor/**"]);
    let report = check_doc(&root, &scope_literal_path, &AlignmentOptions::default());
    expect_code(&report, "non-goal-scope");

    let mut allow_scope = v1();
    allow_scope["allow_unlinked"] = json!(["vendor/**"]);
    let report = check_doc(&root, &allow_scope, &AlignmentOptions::default());
    expect_code(&report, "non-goal-scope");

    let mut allow_literal = v1();
    allow_literal["allow_unlinked"] = json!(["vendor/x.py"]);
    let report = check_doc(&root, &allow_literal, &AlignmentOptions::default());
    expect_code(&report, "non-goal-scope");

    let mut contradiction = v1();
    contradiction["requirements"][0]["contradicts"] = json!(["R2"]);
    let report = check_doc(&root, &contradiction, &AlignmentOptions::default());
    expect_code(&report, "contradiction");

    let mut unknown_contradiction = v1();
    unknown_contradiction["requirements"][0]["contradicts"] = json!(["R9"]);
    let report = check_doc(&root, &unknown_contradiction, &AlignmentOptions::default());
    expect_code(&report, "unknown-contradiction");

    let mut self_contradiction = v1();
    self_contradiction["requirements"][0]["contradicts"] = json!(["R1"]);
    let report = check_doc(&root, &self_contradiction, &AlignmentOptions::default());
    expect_code(&report, "self-contradiction");

    let report = check_text(&root, "bad.json", "{");
    expect_unknown(&report, "manifest-unreadable");
    let report = check_text(
        &root,
        "bad-schema.json",
        r#"{"schema":"alignment/9","requirements":[]}"#,
    );
    expect_unknown(&report, "schema-mismatch");
    let report = check(
        &root,
        Path::new("missing.json"),
        &AlignmentOptions::default(),
    );
    expect_unknown(&report, "manifest-missing");

    let outside = root.parent().unwrap().join("outside.json");
    fs::write(&outside, serde_json::to_vec(&v1()).unwrap()).unwrap();
    let report = check(&root, &outside, &AlignmentOptions::default());
    expect_unknown(&report, "manifest-outside-root");
    let report = check(
        &root,
        Path::new("../outside.json"),
        &AlignmentOptions::default(),
    );
    expect_unknown(&report, "manifest-outside-root");

    let first = check_doc(&root, &v1(), &AlignmentOptions::default());
    let second = check_doc(&root, &v1(), &AlignmentOptions::default());
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "same input must produce deterministic report"
    );

    let _ = fs::remove_dir_all(root);
}
