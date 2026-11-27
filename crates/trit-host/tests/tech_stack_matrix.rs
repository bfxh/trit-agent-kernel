//! tech-stack 策略的路径矩阵：test-only 语言判定、审计保序、shell 目标提取。
//!
//! 规则本身很短，容易在"看起来像测试目录"的写法上出错（`src/tests.rs`、
//! `mytests/x.py`、`src/tests/` 与 `tests/` 的区别）。这里把这些边界钉死。

use std::path::PathBuf;

use trit_host::ToolRequest;
use trit_host::permissions::WriteClaim;
use trit_host::tech_stack::{is_test_path, shell_write_targets, validate_write_path, violations};

fn shell(command: &str) -> ToolRequest {
    ToolRequest::shell("call-bash", command)
}

fn temp_root(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-stack-test-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_dir_all(path);
}

#[test]
fn test_path_detection_matches_directory_conventions() {
    for path in [
        "tests/test_x.py",
        "tests/unit/test_x.py",
        "pkg/tests/test_x.py",
        "test/test_x.py",
        "spec/flow/test_x.py",
        "conftest.py",
        "pkg/conftest.py",
        "pkg/test_api.py",
        "pkg/api_test.py",
        "TESTS/Test_X.PY",
    ] {
        assert!(is_test_path(path), "{path} 应算测试路径");
    }

    for path in [
        "src/lib.rs",
        "src/tests.rs",     // 文件名含 tests，但不是目录
        "mytests/x.py",     // 不是 tests 目录
        "src/testing/x.py", // testing 不是 tests
        "tests.json",
        "src/contest.py",
        "app/tests_helpers.py", // 文件名不是 test_ 前缀
    ] {
        assert!(!is_test_path(path), "{path} 不应算测试路径");
    }
}

#[test]
fn write_validation_only_constrains_test_only_languages() {
    // 生产路径的 Python 一律拒绝。
    for path in [
        "src/main.py",
        "scripts/deploy.py",
        "tools/gen.py",
        "src/deep/path/module.py",
        "__init__.py",
        "src/MAIN.PY",
    ] {
        let error = validate_write_path(path).expect_err(&format!("{path} 必须被拒绝"));
        assert!(error.contains("test paths"), "{path}: {error}");
    }

    // 测试路径的 Python 允许。
    for path in [
        "tests/a.py",
        "tests\\a.py",
        "pkg/tests/deep/a.py",
        "conftest.py",
        "test_a.py",
        "a_test.py",
    ] {
        assert!(validate_write_path(path).is_ok(), "{path} 应允许");
    }

    // 非 test-only 语言不受限制。
    for path in [
        "src/lib.rs",
        "src/main.go",
        "app/index.ts",
        "docs/readme.md",
        "Dockerfile",
    ] {
        assert!(validate_write_path(path).is_ok(), "{path} 应允许");
    }
}

#[test]
fn violations_preserves_input_order_and_deduplicates_nothing() {
    let paths: Vec<String> = [
        "src/a.py",
        "tests/ok.py",
        "scripts/b.py",
        "src/a.py",
        "src/lib.rs",
        "tools/c.PY",
    ]
    .iter()
    .map(|path| (*path).to_owned())
    .collect();
    let found = violations(&paths);
    assert_eq!(
        found,
        vec![
            "src/a.py".to_owned(),
            "scripts/b.py".to_owned(),
            "src/a.py".to_owned(),
            "tools/c.PY".to_owned(),
        ]
    );
    assert!(violations(&[]).is_empty());
}

#[test]
fn shell_write_targets_cover_redirects_and_tee() {
    for (command, expected) in [
        ("echo x > src/a.py", "src/a.py"),
        ("echo x >> src/a.py", "src/a.py"),
        ("echo x 2> src/a.py", "src/a.py"),
        ("echo x >src/a.py", "src/a.py"),
        ("echo x > > src/a.py", "src/a.py"),
        ("cat x | tee src/a.py", "src/a.py"),
    ] {
        let targets = shell_write_targets(command);
        assert!(
            targets.contains(&expected.to_owned()),
            "{command} -> {targets:?}"
        );
    }
}

#[test]
fn shell_write_targets_ignore_fd_duplication_and_readonly_commands() {
    // `2>&1` / `>&2` 是 fd 复制，不是写文件。
    for command in [
        "cargo test 2>&1",
        "echo hi >&2",
        "cargo build > /dev/null 2>&1",
        "cat src/a.rs | grep needle 2>&1",
    ] {
        let targets = shell_write_targets(command);
        assert!(
            targets.iter().all(|target| !target.starts_with('&')),
            "{command} -> {targets:?}"
        );
    }

    for command in [
        "ls -la",
        "git status --short",
        "cargo test --lib",
        "find . -name '*.rs'",
    ] {
        assert!(
            shell_write_targets(command).is_empty(),
            "{command} 不应提取到写入目标"
        );
    }
}

#[test]
fn shell_write_targets_take_cp_and_mv_destination() {
    assert_eq!(
        shell_write_targets("cp src/a.rs src/b.rs"),
        vec!["src/b.rs".to_owned()]
    );
    assert_eq!(
        shell_write_targets("mv src/a.rs src/b.rs"),
        vec!["src/b.rs".to_owned()]
    );
    // tee 后面带选项时不把选项当目标。
    assert!(shell_write_targets("cat x | tee -a src/log.txt").is_empty());
}

#[test]
fn shell_write_targets_are_heuristic_around_quotes_and_eval() {
    // 启发式会把引号留在目标里（`src/a.py'`），或干脆提取不到目标。
    // 这不是安全边界：调用方（claim）必须对"提取结果不干净"的情况 fail closed。
    for command in ["eval 'echo x > src/a.py'", "sh -c \"echo x > src/a.py\""] {
        let targets = shell_write_targets(command);
        assert!(
            targets.iter().all(|target| target != "src/a.py"),
            "{command} 不应提取出干净路径：{targets:?}"
        );
    }

    // 无写入目标的命令（脚本内部写文件看不出来）。
    for command in ["bash scripts/run.sh", "python3 -c 'print(1)'"] {
        assert!(
            shell_write_targets(command).is_empty(),
            "{command} 应提取不到目标"
        );
    }
}

#[test]
fn claim_fails_closed_when_shell_targets_cannot_be_extracted() {
    let root = temp_root("claim-heuristic");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();

    // 完全没有可提取目标的命令：claim 必须拒绝（不能默认"没写文件"）。
    for command in [
        "bash scripts/run.sh",
        "python3 -c 'print(1)'",
        "cargo test --lib",
    ] {
        assert!(
            !claim.allows_request(&shell(command)),
            "{command} 必须被 claim 拒绝"
        );
    }

    // 引号污染的目标仍然落到 `src/` 前缀下：真实写入确实在 claim 内，放行是对的。
    // 这条断言的作用是钉住"引号不会让 claim 误判到别处"，而不是要求拒绝。
    for command in ["eval 'echo x > src/a.py'", "sh -c \"echo x > src/a.py\""] {
        assert!(
            claim.allows_request(&shell(command)),
            "{command} 的目标仍应在 claim 内"
        );
    }
    cleanup(&root);
}
