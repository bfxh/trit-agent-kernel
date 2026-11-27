//! 技术栈中立性契约：编程语言/框架/包管理器跟项目走，不跟模型偏好。

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn json(rel: &str) -> serde_json::Value {
    serde_json::from_str(&read(rel)).unwrap_or_else(|error| panic!("parse {rel}: {error}"))
}

fn strings(value: &serde_json::Value, what: &str) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{what} 必须是数组"))
        .iter()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

#[test]
fn policy_is_well_formed() {
    let policy = json("docs/agent-tech-stack-policy.json");
    assert_eq!(policy["schema"], "trit.agent-tech-stack-policy/v1");
    assert_eq!(
        policy["principle"], "stack-follows-project-not-model-preference",
        "原则必须是：技术栈跟项目走，不跟模型偏好"
    );
    let forbidden = strings(&policy["forbidden"], "forbidden");
    for rule in [
        "introduce-new-language-without-user-authorization",
        "introduce-new-framework-or-package-manager-without-reason",
        "rewrite-existing-code-into-model-preferred-language",
    ] {
        assert!(
            forbidden.iter().any(|item| item == rule),
            "forbidden 必须包含 {rule}"
        );
    }
    let detect = strings(&policy["detect"], "detect");
    for marker in ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"] {
        assert!(
            detect.iter().any(|item| item == marker),
            "detect 必须包含 {marker}"
        );
    }
    let applies = strings(&policy["applies_to"], "applies_to");
    for who in ["main-agent", "subagent", "external-cli-subagent"] {
        assert!(
            applies.iter().any(|item| item == who),
            "applies_to 必须覆盖 {who}"
        );
    }
    assert_eq!(
        policy["subagent_contract"]["response"], "StackReport{used,added}",
        "子智能体必须自报技术栈"
    );
    assert!(
        policy["exception"]
            .as_str()
            .is_some_and(|text| text.contains("explicit_override")),
        "例外必须是 brief 里的显式授权"
    );
    assert_eq!(
        policy["natural_language"]["policy"], "docs/agent-language-policy.json",
        "自然语言与编程语言必须分开管理"
    );
    assert_eq!(
        policy["enforcement"]["contract_test"],
        "crates/trit-host/tests/tech_stack_policy.rs"
    );
    assert_eq!(
        policy["enforcement"]["human_doc"],
        "docs/TECH-STACK-POLICY.md"
    );
}

#[test]
fn manifest_binds_the_tech_stack_policy() {
    let manifest = json("docs/agent-model-manifest.json");
    let binding = &manifest["tech_stack_policy"];
    assert_eq!(binding["contract"], "docs/agent-tech-stack-policy.json");
    assert_eq!(binding["schema"], "trit.agent-tech-stack-policy/v1");
    assert_eq!(binding["applies_to_subagents"], true);
    assert!(
        repo_root()
            .join("docs/agent-tech-stack-policy.json")
            .is_file()
    );
    assert!(repo_root().join("docs/TECH-STACK-POLICY.md").is_file());
}

#[test]
fn human_doc_states_the_rule_and_separates_languages() {
    let text = read("docs/TECH-STACK-POLICY.md");
    assert!(text.contains("项目现状"), "文档必须写明跟项目现状");
    assert!(text.contains("编程语言"), "文档必须点名编程语言");
    assert!(text.contains("子智能体"), "文档必须覆盖子智能体");
    assert!(text.contains("不在设置里放开"), "编程语言不能在设置里放开");
    assert!(
        text.contains("cargo test -p trit-host --test tech_stack_policy"),
        "文档必须给出可执行检查"
    );
}

#[test]
fn subagent_stack_validation_rejects_unauthorized_usage() {
    let empty = trit_orch::StackPolicy::default();
    // 用了项目里不存在、也未授权的技术：拒绝（防"用新语言但不报新增"）。
    let used_unknown = trit_orch::StackReport {
        used: vec!["python".to_owned()],
        added: Vec::new(),
    };
    let error = trit_orch::validate_stack(&used_unknown, &empty).unwrap_err();
    assert!(
        error.to_string().contains("outside the project stack"),
        "{error}"
    );

    // 项目现有栈内：通过。
    let rust_project = trit_orch::StackPolicy {
        existing: vec!["rust".to_owned()],
        allowed_additions: Vec::new(),
    };
    let used_rust = trit_orch::StackReport {
        used: vec!["rust".to_owned()],
        added: Vec::new(),
    };
    trit_orch::validate_stack(&used_rust, &rust_project).unwrap();

    // 新增未授权：拒绝。
    let added_python = trit_orch::StackReport {
        used: vec!["rust".to_owned()],
        added: vec!["python".to_owned()],
    };
    let error = trit_orch::validate_stack(&added_python, &rust_project).unwrap_err();
    assert!(
        error.to_string().contains("without authorization"),
        "{error}"
    );

    // 显式授权后才放行。
    let authorized = trit_orch::StackPolicy {
        existing: vec!["rust".to_owned()],
        allowed_additions: vec!["python".to_owned()],
    };
    trit_orch::validate_stack(&added_python, &authorized).unwrap();
}

#[test]
fn python_writes_are_rejected_outside_test_paths() {
    let root = std::env::temp_dir().join(format!(
        "trit-tech-stack-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut config = trit_host::tools::WorkspaceConfig::new(&root);
    config.allow_write = true;
    let mut tools = trit_host::tools::LocalTools::new(config).unwrap();

    // 生产路径：写不进去（硬拒绝，不是扣分）。
    let error = trit_host::ToolExecutor::execute(
        &mut tools,
        &trit_host::ToolRequest::write("w1", "write_file", "src/a.py", "print(1)"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("test paths"), "{error}");
    assert!(!root.join("src/a.py").exists(), "被拒绝的写入不应落盘");

    // 测试路径：允许。
    trit_host::ToolExecutor::execute(
        &mut tools,
        &trit_host::ToolRequest::write("w2", "write_file", "tests/test_a.py", "print(1)"),
    )
    .unwrap();
    assert!(root.join("tests/test_a.py").is_file());

    // 其它语言不受影响。
    trit_host::ToolExecutor::execute(
        &mut tools,
        &trit_host::ToolRequest::write("w3", "write_file", "src/lib.rs", "pub fn f() {}\n"),
    )
    .unwrap();

    let _ = std::fs::remove_dir_all(&root);
}
