//! 语言策略与模型清单的机器可判契约。
//!
//! 规则：产物语言由**项目文档**决定，模型偏好 / 厂商标识 / 训练语料都不是依据；
//! 接入任何模型（现在或将来）都必须显式声明遵守该策略。
//!
//! 相关文件：
//! - `docs/agent-language-policy.json`（机器契约）
//! - `docs/LANGUAGE-POLICY.md`（人类面说明）
//! - `docs/agent-model-manifest.json`（智能体 / 模型 / 插件清单）

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
fn language_policy_is_well_formed() {
    let policy = json("docs/agent-language-policy.json");
    assert_eq!(policy["schema"], "trit.agent-language-policy/v1");
    assert_eq!(
        policy["principle"], "artifact-locale-follows-project-documentation",
        "策略必须写明：产物语言跟随项目文档"
    );
    assert!(
        policy["rule"]
            .as_str()
            .is_some_and(|rule| rule.contains("项目文档")),
        "rule 必须点明项目文档是依据"
    );
    let allowed = strings(&policy["allowed_locales"], "allowed_locales");
    assert!(allowed.contains(&"zh-CN".to_owned()) && allowed.contains(&"en-US".to_owned()));
    let default = policy["default_locale"].as_str().expect("default_locale");
    assert!(
        allowed.iter().any(|locale| locale == default),
        "默认语言 {default} 必须在允许集合内"
    );
    for key in [
        "docs",
        "code_comments",
        "commit_messages",
        "cli_help",
        "cli_errors",
        "test_assertion_messages",
        "ui_copy",
        "log_messages",
        "agent_notices",
    ] {
        let locale = policy["artifacts"][key]
            .as_str()
            .unwrap_or_else(|| panic!("artifacts.{key} 缺失"));
        assert!(
            allowed.iter().any(|allowed| allowed == locale),
            "artifacts.{key} 的语言 {locale} 不在允许集合"
        );
    }
    let never = strings(&policy["never"], "never");
    for rule in [
        "switch-language-because-of-model-preference",
        "switch-language-because-of-model-vendor",
        "translate-identifiers-paths-commands-or-protocol-names",
    ] {
        assert!(
            never.iter().any(|item| item == rule),
            "never 必须包含 {rule}"
        );
    }
    assert!(
        !strings(&policy["untranslated"], "untranslated").is_empty(),
        "必须声明不翻译的类别"
    );
    assert!(policy["fallback_when_project_silent"].as_str().is_some());
    assert_eq!(
        policy["enforcement"]["contract_test"],
        "crates/trit-host/tests/agent_policy.rs"
    );
    assert_eq!(
        policy["enforcement"]["human_doc"],
        "docs/LANGUAGE-POLICY.md"
    );
    assert_eq!(
        policy["enforcement"]["manifest_binding"],
        "docs/agent-model-manifest.json#language_policy"
    );
}

#[test]
fn manifest_binds_the_language_policy_and_declares_neutrality() {
    let manifest = json("docs/agent-model-manifest.json");
    assert_eq!(manifest["schema"], "trit.agent-model-manifest/v1");
    let policy = &manifest["language_policy"];
    assert_eq!(policy["contract"], "docs/agent-language-policy.json");
    assert_eq!(policy["schema"], "trit.agent-language-policy/v1");
    assert_eq!(policy["model_preference_is_not_a_source"], true);
    assert!(
        repo_root()
            .join("docs/agent-language-policy.json")
            .is_file(),
        "manifest 指向的语言策略文件必须存在"
    );
    assert!(
        repo_root().join("docs/LANGUAGE-POLICY.md").is_file(),
        "人类面说明必须存在"
    );

    let neutrality = &manifest["model_neutrality"];
    assert!(
        neutrality["applies_to"]
            .as_str()
            .is_some_and(|scope| scope.contains("future")),
        "模型无关性必须覆盖将来的模型"
    );
    let forbidden = strings(&neutrality["forbidden"], "model_neutrality.forbidden");
    for rule in [
        "language-preference",
        "project-preference",
        "vendor-specific-output-shape",
    ] {
        assert!(
            forbidden.iter().any(|item| item == rule),
            "model_neutrality.forbidden 必须包含 {rule}"
        );
    }
    let requires = strings(
        &neutrality["adding_a_model_requires"],
        "model_neutrality.adding_a_model_requires",
    );
    assert!(
        requires
            .iter()
            .any(|item| item == "language-policy-acknowledgement"),
        "接入新模型必须确认遵守语言策略"
    );
}

#[test]
fn every_manifest_entry_acknowledges_the_language_policy() {
    let manifest = json("docs/agent-model-manifest.json");
    let mut seen = 0usize;
    for group in ["agents", "models", "plugins"] {
        let entries = manifest[group]
            .as_array()
            .unwrap_or_else(|| panic!("{group} 必须是数组"));
        assert!(!entries.is_empty(), "{group} 不能为空");
        for entry in entries {
            let id = entry["id"].as_str().unwrap_or("<missing id>");
            for field in [
                "id",
                "role",
                "owner",
                "status",
                "modalities",
                "capabilities",
                "must_not",
            ] {
                assert!(entry.get(field).is_some(), "{id} 缺少必需字段 {field}");
            }
            assert_eq!(
                entry["language_behavior"], "follow-project-language-policy",
                "{id} 必须显式声明遵守项目语言策略"
            );
            seen += 1;
        }
    }
    assert!(
        seen >= 4,
        "清单条目数为 {seen}，应覆盖 agent / model / plugin"
    );
}

#[test]
fn language_policy_doc_states_the_rule() {
    let text = read("docs/LANGUAGE-POLICY.md");
    assert!(text.contains("项目文档"), "文档必须写明语言由项目文档决定");
    assert!(text.contains("偏好"), "文档必须写明模型偏好不是依据");
    assert!(text.contains("模型无关"), "文档必须写明策略与模型无关");
    assert!(
        text.contains("cargo test -p trit-host --test agent_policy"),
        "文档必须给出可执行的检查命令"
    );
}
