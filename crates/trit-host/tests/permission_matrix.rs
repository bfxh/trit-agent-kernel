//! 权限策略与 write claim 的决策矩阵。
//!
//! 这三条约束不能靠"读起来对"来保证：
//! 1. `deny` 永远优先于 `allow` 与模式回退；
//! 2. `bash` 永远不享受 `read_only` 快路径（模型自报不算证明）；
//! 3. claim 存在时，提取不到写入目标的 shell 命令 fail closed。

use std::path::PathBuf;

use trit_host::ToolRequest;
use trit_host::permissions::{
    PermissionDecision, PermissionMode, PermissionPolicy, PermissionRule, WriteClaim,
};

fn read(path: &str) -> ToolRequest {
    ToolRequest::read("call-read", path)
}

fn write(path: &str) -> ToolRequest {
    ToolRequest::write("call-write", "write_file", path, "content")
}

fn shell(command: &str) -> ToolRequest {
    ToolRequest::shell("call-bash", command)
}

fn rule(tool: &str, subject: Option<&str>, decision: PermissionDecision) -> PermissionRule {
    PermissionRule {
        tool: tool.into(),
        subject: subject.map(str::to_owned),
        decision,
    }
}

#[test]
fn ask_mode_asks_for_writes_and_shell_but_allows_reads() {
    let policy = PermissionPolicy::new(PermissionMode::Ask);
    assert_eq!(
        policy.decide(&read("src/lib.rs")),
        PermissionDecision::Allow
    );
    assert_eq!(policy.decide(&write("src/lib.rs")), PermissionDecision::Ask);
    assert_eq!(policy.decide(&shell("cargo test")), PermissionDecision::Ask);
}

#[test]
fn auto_and_yolo_allow_ordinary_writes() {
    for mode in [PermissionMode::Auto, PermissionMode::Yolo] {
        let policy = PermissionPolicy::new(mode);
        assert_eq!(
            policy.decide(&write("src/lib.rs")),
            PermissionDecision::Allow,
            "{mode:?}"
        );
        assert_eq!(
            policy.decide(&shell("cargo test")),
            PermissionDecision::Allow,
            "{mode:?}"
        );
    }
}

#[test]
fn explicit_deny_beats_mode_and_allow_rules() {
    for mode in [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Yolo,
    ] {
        let policy = PermissionPolicy::new(mode)
            .with_rule(rule("bash", None, PermissionDecision::Allow))
            .with_rule(rule("bash", None, PermissionDecision::Deny));
        assert_eq!(
            policy.decide(&shell("rm -rf /")),
            PermissionDecision::Deny,
            "{mode:?}"
        );
    }
}

#[test]
fn ask_rule_beats_allow_rule_and_mode() {
    let policy = PermissionPolicy::new(PermissionMode::Yolo)
        .with_rule(rule("write_file", None, PermissionDecision::Allow))
        .with_rule(rule(
            "write_file",
            Some("src/locked.rs"),
            PermissionDecision::Ask,
        ));
    assert_eq!(
        policy.decide(&write("src/locked.rs")),
        PermissionDecision::Ask
    );
    // 同工具、不同 subject：allow 规则仍然生效。
    assert_eq!(
        policy.decide(&write("src/open.rs")),
        PermissionDecision::Allow
    );
}

#[test]
fn wildcard_tool_rule_matches_every_tool() {
    let policy = PermissionPolicy::new(PermissionMode::Ask).with_rule(rule(
        "*",
        None,
        PermissionDecision::Deny,
    ));
    for request in [
        read("a.rs"),
        write("b.rs"),
        shell("ls"),
        ToolRequest::context_read("c", "C1"),
    ] {
        assert_eq!(
            policy.decide(&request),
            PermissionDecision::Deny,
            "{}",
            request.tool
        );
    }
}

#[test]
fn subject_matches_command_for_bash_and_first_path_for_files() {
    let policy = PermissionPolicy::new(PermissionMode::Ask)
        .with_rule(rule("bash", Some("cargo test"), PermissionDecision::Allow))
        .with_rule(rule(
            "write_file",
            Some("README.md"),
            PermissionDecision::Allow,
        ));

    assert_eq!(
        policy.decide(&shell("cargo test")),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.decide(&shell("cargo test --all")),
        PermissionDecision::Ask
    );
    assert_eq!(
        policy.decide(&write("README.md")),
        PermissionDecision::Allow
    );
    assert_eq!(policy.decide(&write("src/lib.rs")), PermissionDecision::Ask);
}

#[test]
fn bash_never_uses_the_readonly_fast_path() {
    let mut request = shell("ls");
    request.read_only = true; // 模型自报
    let policy = PermissionPolicy::new(PermissionMode::Ask);
    assert_eq!(policy.decide(&request), PermissionDecision::Ask);

    // 但显式 allow 规则可以放行（那是用户的决定，不是模型自报）。
    let policy = policy.with_rule(rule("bash", None, PermissionDecision::Allow));
    assert_eq!(policy.decide(&request), PermissionDecision::Allow);
}

#[test]
fn readonly_non_bash_tools_skip_approval_even_in_ask_mode() {
    let policy = PermissionPolicy::new(PermissionMode::Ask);
    for request in [
        ToolRequest::read("c1", "a.rs"),
        ToolRequest {
            call_id: "c2".into(),
            tool: "search_code".into(),
            read_only: true,
            ..ToolRequest::default()
        },
        ToolRequest::context_read("c3", "C1"),
    ] {
        assert_eq!(
            policy.decide(&request),
            PermissionDecision::Allow,
            "{}",
            request.tool
        );
    }
}

#[test]
fn claim_allows_literal_directory_and_globs() {
    let root = temp_root("claim-allow");
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join("tests/unit")).unwrap();
    let claim = WriteClaim::new(
        &root,
        vec![
            PathBuf::from("src"),
            PathBuf::from("tests/**/*.rs"),
            PathBuf::from("README.md"),
        ],
    )
    .unwrap();

    assert!(claim.allows_path("src/lib.rs"));
    assert!(claim.allows_path("src/nested/deep/mod.rs"));
    assert!(claim.allows_path("README.md"));
    assert!(claim.allows_path("tests/unit/case.rs"));
    assert!(!claim.allows_path("docs/guide.md"));
    assert!(!claim.allows_path(""));
    assert_eq!(claim.pattern_count(), 1);

    cleanup(&root);
}

#[test]
fn claim_rejects_escaping_paths() {
    let root = temp_root("claim-escape");
    let error = WriteClaim::new(&root, vec![PathBuf::from("../outside.txt")])
        .expect_err("越出 root 的声明必须失败");
    assert!(error.to_string().contains("escapes root"), "{error}");

    let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();
    assert!(!claim.allows_path("../outside.txt"));
    assert!(!claim.allows_path("src/../../outside.txt"));
    cleanup(&root);
}

#[test]
fn claim_shell_requests_require_extractable_targets() {
    let root = temp_root("claim-shell");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();

    // 无写入目标：fail closed。
    assert!(!claim.allows_request(&shell("cargo build")));
    // 目标在 claim 内：放行。
    assert!(claim.allows_request(&shell("cargo test > src/log.txt")));
    // 目标在 claim 外：拒绝。
    assert!(!claim.allows_request(&shell("cargo test > /tmp/log.txt")));
    // 多个目标必须全部在 claim 内。
    assert!(!claim.allows_request(&shell("cp src/a.rs src/b.rs > outside.txt")));
    cleanup(&root);
}

#[test]
fn claim_move_requires_both_ends_covered() {
    let root = temp_root("claim-move");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();

    let mut both = ToolRequest {
        call_id: "m1".into(),
        tool: "move_path".into(),
        paths: vec!["src/a.rs".into(), "src/b.rs".into()],
        ..ToolRequest::default()
    };
    assert!(claim.allows_request(&both));

    both.paths = vec!["src/a.rs".into(), "outside/b.rs".into()];
    assert!(!claim.allows_request(&both), "目标越界必须拒绝");

    // 只有一端：结构不合法，直接拒绝。
    both.paths = vec!["src/a.rs".into()];
    assert!(!claim.allows_request(&both));
    cleanup(&root);
}

#[test]
fn claim_write_requires_every_path_covered() {
    let root = temp_root("claim-write");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();

    let mut request = write("src/ok.rs");
    assert!(claim.allows_request(&request));

    request.paths = vec!["src/ok.rs".into(), "docs/nope.md".into()];
    assert!(!claim.allows_request(&request));

    // 空路径：没有可校验的目标，拒绝。
    request.paths.clear();
    assert!(!claim.allows_request(&request));
    cleanup(&root);
}

#[test]
fn claim_readonly_requests_pass_without_paths() {
    let root = temp_root("claim-read");
    let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();
    // 只读请求不改变工作区，claim 不约束它（bash 除外，上面单独覆盖）。
    assert!(claim.allows_request(&read("anywhere.md")));
    cleanup(&root);
}

fn temp_root(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-perm-test-{tag}-{}-{:?}",
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
