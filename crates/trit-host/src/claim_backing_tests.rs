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
    let request = ClaimRequest {
        criteria: vec![CriterionRequest {
            id: id.to_owned(),
            kind,
        }],
    };
    back_claim(&request, "task", receipts)
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
