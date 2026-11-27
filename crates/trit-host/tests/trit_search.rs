//! `trit_search` 工具级回归：三值输出、过滤、失败关闭与性能统计。
//!
//! 与 `native_tools.rs` 的 `search_code` 用例同构，额外锁定三进制语义：
//! `T` = 确定匹配，`U` = 含通配未知位的匹配，`F` = 不匹配（不输出）。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use trit_host::tools::{LocalTools, WorkspaceConfig};
use trit_host::{AgentError, ToolExecutor, ToolRequest};

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-trit-search-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn make_tools(root: &Path) -> LocalTools {
    LocalTools::new(WorkspaceConfig::new(root)).unwrap()
}

fn request(tool: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        call_id: format!("{tool}-1"),
        tool: tool.into(),
        arguments: Some(arguments),
        ..ToolRequest::default()
    }
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// 结果行（`path:line:V text`）去掉末尾的统计摘要，便于精确计数。
fn result_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| line.contains(":T ") || line.contains(":U "))
        .collect()
}

#[test]
fn trit_search_marks_certain_and_unknown_matches() {
    let root = temp_workspace("verdicts");
    write(&root, "src/a.rs", "fn alpha() {}\nfn open(dir)\n");
    write(&root, "src/b.rs", "fn open(dir)\nfn xyz\n");
    let mut tools = make_tools(&root);

    let certain = tools
        .execute(&request("trit_search", json!({"query": "fn open"})))
        .unwrap();
    assert!(certain.passed);
    assert!(certain.output.contains("src/a.rs:2:T fn open"));
    assert!(certain.output.contains("src/b.rs:1:T fn open"));
    assert!(certain.output.contains("2 match(es), 0 unknown"));

    let unknown = tools
        .execute(&request("trit_search", json!({"query": "fn ?pen"})))
        .unwrap();
    assert!(unknown.passed);
    assert!(unknown.output.contains("src/a.rs:2:U fn open"));
    assert!(unknown.output.contains("src/b.rs:1:U fn open"));
    assert!(unknown.output.contains("2 match(es), 2 unknown"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_case_sensitivity_and_all_wildcards() {
    let root = temp_workspace("case");
    write(&root, "lower.txt", "needle in text\n");
    write(&root, "upper.txt", "Needle in text\n");
    let mut tools = make_tools(&root);

    let sensitive = tools
        .execute(&request(
            "trit_search",
            json!({"query": "needle", "case_sensitive": true}),
        ))
        .unwrap();
    assert!(sensitive.output.contains("lower.txt:1:T"));
    assert!(!sensitive.output.contains("upper.txt"));

    let insensitive = tools
        .execute(&request(
            "trit_search",
            json!({"query": "NEEDLE", "case_sensitive": false}),
        ))
        .unwrap();
    assert!(insensitive.output.contains("lower.txt:1:T"));
    assert!(insensitive.output.contains("upper.txt:1:T"));

    // 纯通配没有确定字节：不会命中短行，命中长行时一律是 Unknown。
    let wildcard = tools
        .execute(&request("trit_search", json!({"query": "???"})))
        .unwrap();
    assert!(wildcard.output.contains("lower.txt:1:U"));
    assert!(wildcard.output.contains("upper.txt:1:U"));
    assert!(!wildcard.output.contains(":T "));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_honors_glob_filter_path_scope_and_limits() {
    let root = temp_workspace("scope");
    write(&root, "src/a.rs", "needle rs\n");
    write(&root, "src/nested/c.rs", "needle nested\n");
    write(&root, "src/b.py", "needle py\n");
    write(&root, "other/d.rs", "needle other\n");
    let mut tools = make_tools(&root);

    let filtered = tools
        .execute(&request(
            "trit_search",
            json!({"query": "needle", "glob": "**/*.rs"}),
        ))
        .unwrap();
    assert!(filtered.output.contains("src/a.rs"));
    assert!(filtered.output.contains("src/nested/c.rs"));
    assert!(!filtered.output.contains("src/b.py"));

    let scoped = tools
        .execute(&request(
            "trit_search",
            json!({"query": "needle", "path": "other"}),
        ))
        .unwrap();
    assert!(scoped.output.contains("other/d.rs"));
    assert!(!scoped.output.contains("src/a.rs"));

    let capped = tools
        .execute(&request(
            "trit_search",
            json!({"query": "needle", "max_results": 2}),
        ))
        .unwrap();
    assert_eq!(result_lines(&capped.output).len(), 2);
    assert!(capped.output.contains("2 match(es)"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_fails_closed_on_bad_inputs() {
    let root = temp_workspace("invalid");
    write(&root, "a.txt", "needle\n");
    let mut tools = make_tools(&root);

    let error = tools
        .execute(&request("trit_search", json!({})))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("query")));

    let too_long = "a".repeat(4097);
    let error = tools
        .execute(&request("trit_search", json!({"query": too_long})))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("pattern too long")));

    let error = tools
        .execute(&request(
            "trit_search",
            json!({"query": "needle", "glob": "["}),
        ))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("glob")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_skips_hidden_binary_ignored_forbidden_and_oversize() {
    let root = temp_workspace("guards");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(&root, ".gitignore", "ignored.txt\n");
    write(&root, "src/a.rs", "fn alpha() {}\n// needle here\n");
    write(&root, "small.txt", "needle small\n");
    write(&root, "ignored.txt", "needle ignored\n");
    write(&root, ".hidden/secret.txt", "needle hidden\n");
    write(&root, "private/secret.txt", "needle private\n");
    write(&root, "big.txt", &format!("needle {}\n", "x".repeat(100)));
    std::fs::write(root.join("binary.bin"), b"needle\0binary").unwrap();

    let mut config = WorkspaceConfig::new(&root);
    config.forbid = vec![PathBuf::from("private")];
    config.max_search_file_bytes = 32;
    let mut tools = LocalTools::new(config).unwrap();
    let outcome = tools
        .execute(&request("trit_search", json!({"query": "needle"})))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("src/a.rs:2:T"));
    assert!(outcome.output.contains("small.txt:1:T"));
    assert!(!outcome.output.contains("ignored.txt"));
    assert!(!outcome.output.contains(".hidden/secret.txt"));
    assert!(!outcome.output.contains("private/secret.txt"));
    assert!(!outcome.output.contains("big.txt"));
    assert!(!outcome.output.contains("binary.bin"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_reports_scan_metrics_in_agent_visible_output() {
    let root = temp_workspace("metrics");
    write(&root, "a.txt", "needle one\nneedle two\n");
    write(&root, "b.txt", "nothing\n");
    let mut tools = make_tools(&root);

    let outcome = tools
        .execute(&request("trit_search", json!({"query": "needle"})))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("2 match(es), 0 unknown"));
    assert!(
        outcome.output.contains("scanned 2 file(s)"),
        "output must expose scan metrics: {}",
        outcome.output
    );
    assert!(
        outcome.output.contains("byte(s)"),
        "output must expose byte metrics: {}",
        outcome.output
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_reports_no_matches_with_metrics() {
    let root = temp_workspace("none");
    write(&root, "a.txt", "alpha\n");
    let mut tools = make_tools(&root);

    let outcome = tools
        .execute(&request("trit_search", json!({"query": "missing-token"})))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("no matches"));
    assert!(outcome.output.contains("scanned 1 file(s)"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn trit_search_parallel_files_are_ordered_and_complete() {
    let root = temp_workspace("parallel");
    for index in 0..64 {
        write(
            &root,
            &format!("src/f{index:03}.rs"),
            &format!("fn open {index}\n"),
        );
    }
    let mut tools = make_tools(&root);

    let outcome = tools
        .execute(&request(
            "trit_search",
            json!({"query": "fn open", "max_results": 100}),
        ))
        .unwrap();
    let lines = result_lines(&outcome.output);
    assert_eq!(lines.len(), 64);
    let paths: Vec<&str> = lines
        .iter()
        .map(|line| line.split(':').next().unwrap())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "parallel results must stay path-ordered");
    assert!(outcome.output.contains("scanned 64 file(s)"));
    let _ = std::fs::remove_dir_all(&root);
}
