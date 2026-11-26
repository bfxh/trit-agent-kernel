//! 完成声明背书：fail-closed 的负例覆盖。
//!
//! 五类必须不通过：零断言、工具失败、存疑（超时/断流）、伪 diff（空路径）、manual 自证。

use super::*;
use trit_reasonix::CriterionRequest;

fn shell(call: &str, command: &str, passed: bool, assertions: u32, in_doubt: bool) -> ToolOutcome {
    ToolOutcome {
        call_id: call.to_owned(),
        tool: "shell".to_owned(),
        command: Some(command.to_owned()),
        passed,
        assertions,
        written_paths: Vec::new(),
        in_doubt,
        output: String::new(),
    }
}

fn write(call: &str, path: &str, in_doubt: bool) -> ToolOutcome {
    ToolOutcome {
        call_id: call.to_owned(),
        tool: "write_file".to_owned(),
        command: None,
        passed: !in_doubt,
        assertions: 0,
        written_paths: vec![path.to_owned()],
        in_doubt,
        output: String::new(),
    }
}

fn judged(id: &str, kind: CriterionKind, receipts: &[ToolOutcome]) -> CriterionClaim {
    judged_scoped(id, kind, receipts, None)
}

fn judged_scoped(
    id: &str,
    kind: CriterionKind,
    receipts: &[ToolOutcome],
    scope: Option<&crate::permissions::WriteClaim>,
) -> CriterionClaim {
    let request = ClaimRequest {
        criteria: vec![CriterionRequest {
            id: id.to_owned(),
            kind,
        }],
    };
    back_claim(&request, "task", receipts, scope)
        .criteria
        .into_iter()
        .next()
        .expect("criterion")
}

#[test]
fn verification_without_receipt_stays_unknown() {
    let claim = judged(
        "c1",
        CriterionKind::Verification {
            command: "cargo test".to_owned(),
        },
        &[],
    );
    assert!(!claim.backed);
    assert!(!claim.passed);
}

#[test]
fn zero_assertion_run_cannot_back_a_claim() {
    let receipts = vec![shell("a", "cargo test", true, 0, false)];
    let claim = judged(
        "c1",
        CriterionKind::Verification {
            command: "cargo test".to_owned(),
        },
        &receipts,
    );
    assert!(!claim.backed, "零断言不能算背书");
    assert!(!claim.passed);
}

#[test]
fn passed_run_with_assertions_backs_a_claim() {
    let receipts = vec![shell("a", "cargo test", true, 12, false)];
    let claim = judged(
        "c1",
        CriterionKind::Verification {
            command: "cargo test".to_owned(),
        },
        &receipts,
    );
    assert!(claim.backed);
    assert!(claim.passed);
}

#[test]
fn a_single_failed_run_refutes_even_after_a_pass() {
    let receipts = vec![
        shell("a", "cargo test", true, 3, false),
        shell("b", "cargo test", false, 0, false),
    ];
    let claim = judged(
        "c1",
        CriterionKind::Verification {
            command: "cargo test".to_owned(),
        },
        &receipts,
    );
    assert!(claim.backed, "命令确实跑过");
    assert!(!claim.passed, "存在失败执行时不能算通过");
}

#[test]
fn in_doubt_run_stays_unknown() {
    let receipts = vec![shell("a", "cargo test", true, 5, true)];
    let claim = judged(
        "c1",
        CriterionKind::Verification {
            command: "cargo test".to_owned(),
        },
        &receipts,
    );
    assert!(!claim.backed);
    assert!(!claim.passed);
}

#[test]
fn manual_never_backs_itself() {
    let claim = judged("c1", CriterionKind::Manual, &[]);
    assert!(!claim.backed);
    assert!(!claim.passed);
}

#[test]
fn empty_diff_paths_cannot_back_a_claim() {
    let claim = judged("c1", CriterionKind::Diff { paths: Vec::new() }, &[]);
    assert!(!claim.backed, "空路径清单不能自证");
    assert!(!claim.passed);
}

#[test]
fn written_paths_back_a_diff_claim() {
    let receipts = vec![
        write("a", "src/lib.rs", false),
        write("b", "src/b.rs", false),
    ];
    let claim = judged(
        "c1",
        CriterionKind::Diff {
            paths: vec!["src/lib.rs".to_owned(), "src/b.rs".to_owned()],
        },
        &receipts,
    );
    assert!(claim.backed);
    assert!(claim.passed);
}

#[test]
fn underreported_write_refutes_a_passing_diff_claim() {
    let receipts = vec![write("a", "src/a.rs", false), write("b", "src/b.rs", false)];
    let claim = judged(
        "c1",
        CriterionKind::Diff {
            paths: vec!["src/a.rs".to_owned()],
        },
        &receipts,
    );
    assert!(claim.backed, "声明的文件确实写了");
    assert!(
        !claim.passed,
        "还改了 src/b.rs 却只声明 src/a.rs，完成声明漏报"
    );
}

#[test]
fn declaring_every_write_across_diff_criteria_still_passes() {
    let receipts = vec![write("a", "src/a.rs", false), write("b", "src/b.rs", false)];
    let request = ClaimRequest {
        criteria: vec![
            CriterionRequest {
                id: "c1".to_owned(),
                kind: CriterionKind::Diff {
                    paths: vec!["src/a.rs".to_owned()],
                },
            },
            CriterionRequest {
                id: "c2".to_owned(),
                kind: CriterionKind::Diff {
                    paths: vec!["src/b.rs".to_owned()],
                },
            },
        ],
    };
    let claim = back_claim(&request, "task", &receipts, None);
    assert_eq!(claim.criteria.len(), 2);
    assert!(claim.criteria.iter().all(|criterion| criterion.passed));
}

#[test]
fn verification_claim_is_unaffected_by_writes_outside_diff_scope() {
    let receipts = vec![
        shell("a", "cargo test", true, 5, false),
        write("b", "src/a.rs", false),
    ];
    let claim = judged(
        "c1",
        CriterionKind::Verification {
            command: "cargo test".to_owned(),
        },
        &receipts,
    );
    assert!(claim.backed);
    assert!(claim.passed, "没有 diff criterion 时不做漏报判定");
}

#[test]
fn move_receipts_are_not_treated_as_unreported() {
    let receipts = vec![ToolOutcome {
        call_id: "m1".to_owned(),
        tool: "move_path".to_owned(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()],
        in_doubt: false,
        output: String::new(),
    }];
    let claim = judged(
        "c1",
        CriterionKind::Diff {
            paths: vec!["src/b.rs".to_owned()],
        },
        &receipts,
    );
    assert!(claim.backed);
    assert!(
        claim.passed,
        "move 的变更面是双向的，声明目标即可，不按漏报处理"
    );
}

#[test]
fn in_doubt_write_keeps_diff_unknown() {
    let receipts = vec![write("a", "src/lib.rs", true)];
    let claim = judged(
        "c1",
        CriterionKind::Diff {
            paths: vec!["src/lib.rs".to_owned()],
        },
        &receipts,
    );
    assert!(!claim.backed);
    assert!(!claim.passed);
}

fn scope(paths: &[&str]) -> crate::permissions::WriteClaim {
    crate::permissions::WriteClaim::new(
        env!("CARGO_MANIFEST_DIR"),
        paths
            .iter()
            .map(|path| std::path::PathBuf::from(*path))
            .collect(),
    )
    .expect("write claim")
}

#[test]
fn diff_inside_write_scope_still_passes() {
    let write_scope = scope(&["src"]);
    let receipts = vec![write("a", "src/lib.rs", false)];
    let claim = judged_scoped(
        "c1",
        CriterionKind::Diff {
            paths: vec!["src/lib.rs".to_owned()],
        },
        &receipts,
        Some(&write_scope),
    );
    assert!(claim.backed);
    assert!(claim.passed);
}

#[test]
fn diff_declared_outside_write_scope_is_refuted() {
    let write_scope = scope(&["src"]);
    let receipts = vec![write("a", "docs/x.md", false)];
    let claim = judged_scoped(
        "c1",
        CriterionKind::Diff {
            paths: vec!["docs/x.md".to_owned()],
        },
        &receipts,
        Some(&write_scope),
    );
    assert!(claim.backed, "文件确实被写过，不是证据不足");
    assert!(!claim.passed, "声明路径越出授权范围时不能算通过");
}

#[test]
fn out_of_scope_write_refutes_even_when_declared_paths_are_inside() {
    let write_scope = scope(&["src"]);
    let receipts = vec![
        write("a", "src/lib.rs", false),
        write("b", "docs/x.md", false),
    ];
    let claim = judged_scoped(
        "c1",
        CriterionKind::Diff {
            paths: vec!["src/lib.rs".to_owned()],
        },
        &receipts,
        Some(&write_scope),
    );
    assert!(claim.backed);
    assert!(
        !claim.passed,
        "回执里出现越界写入时，即使声明路径在范围内也不能通过"
    );
}
