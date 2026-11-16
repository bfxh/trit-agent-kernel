use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use trit_agent::tools::{LocalTools, WorkspaceConfig};
use trit_agent::{AgentError, ToolExecutor, ToolRequest};

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-native-tools-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn make_tools(root: &Path, allow_shell: bool) -> LocalTools {
    let mut config = WorkspaceConfig::new(root);
    config.allow_shell = allow_shell;
    config.allow_unsandboxed_shell = allow_shell;
    LocalTools::new(config).unwrap()
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

#[test]
fn search_code_literal_skips_hidden_binary_and_ignored() {
    let root = temp_workspace("literal");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(&root, ".gitignore", "ignored.txt\n");
    write(&root, "src/a.rs", "fn alpha() {}\n// needle here\n");
    write(&root, "src/b.py", "needle in python\n");
    write(&root, "ignored.txt", "needle ignored\n");
    write(&root, ".hidden/secret.txt", "needle hidden\n");
    std::fs::write(root.join("binary.bin"), b"needle\0binary").unwrap();

    let mut tools = make_tools(&root, false);
    let outcome = tools
        .execute(&request("search_code", json!({"query": "needle"})))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("src/a.rs:2:"));
    assert!(outcome.output.contains("src/b.py:1:"));
    assert!(!outcome.output.contains("ignored.txt"));
    assert!(!outcome.output.contains(".hidden/secret.txt"));
    assert!(!outcome.output.contains("binary.bin"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_code_regex_and_case_sensitivity() {
    let root = temp_workspace("case");
    write(&root, "lower.txt", "needle in text\n");
    write(&root, "upper.txt", "Needle in text\n");
    let mut tools = make_tools(&root, false);

    let sensitive = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "case_sensitive": true}),
        ))
        .unwrap();
    assert!(sensitive.output.contains("lower.txt"));
    assert!(!sensitive.output.contains("upper.txt"));

    let insensitive = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "case_sensitive": false}),
        ))
        .unwrap();
    assert!(insensitive.output.contains("lower.txt"));
    assert!(insensitive.output.contains("upper.txt"));

    let regex = tools
        .execute(&request(
            "search_code",
            json!({"query": r"needle\s+in", "regex": true}),
        ))
        .unwrap();
    assert!(regex.output.contains("lower.txt:1:"));
    assert!(!regex.output.contains("upper.txt"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_code_honors_glob_filter_and_path_scope() {
    let root = temp_workspace("scope");
    write(&root, "src/a.rs", "needle rs\n");
    write(&root, "src/nested/c.rs", "needle nested\n");
    write(&root, "src/b.py", "needle py\n");
    write(&root, "other/d.rs", "needle other\n");
    let mut tools = make_tools(&root, false);

    let filtered = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "glob": "**/*.rs"}),
        ))
        .unwrap();
    assert!(filtered.output.contains("src/a.rs"));
    assert!(filtered.output.contains("src/nested/c.rs"));
    assert!(!filtered.output.contains("src/b.py"));

    let scoped = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "path": "other"}),
        ))
        .unwrap();
    assert!(scoped.output.contains("other/d.rs"));
    assert!(!scoped.output.contains("src/a.rs"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_code_caps_results_and_reports_no_matches() {
    let root = temp_workspace("limits");
    write(&root, "a.txt", "needle 1\nneedle 2\nneedle 3\n");
    write(&root, "b.txt", "needle 4\n");
    let mut tools = make_tools(&root, false);

    let capped = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "max_results": 2}),
        ))
        .unwrap();
    assert_eq!(capped.output.lines().count(), 2);

    let none = tools
        .execute(&request("search_code", json!({"query": "missing-token"})))
        .unwrap();
    assert!(none.passed);
    assert!(none.output.contains("no matches"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_code_invalid_inputs_fail_closed() {
    let root = temp_workspace("invalid");
    write(&root, "a.txt", "needle\n");
    let mut tools = make_tools(&root, false);

    let error = tools
        .execute(&request("search_code", json!({})))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("query")));

    let error = tools
        .execute(&request(
            "search_code",
            json!({"query": "(", "regex": true}),
        ))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("regex")));

    let error = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "glob": "["}),
        ))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("glob")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_code_respects_forbidden_paths_and_file_size_limit() {
    let root = temp_workspace("guards");
    write(&root, "private/secret.txt", "needle private\n");
    write(&root, "big.txt", &format!("needle {}\n", "x".repeat(100)));
    write(&root, "small.txt", "needle small\n");

    let mut config = WorkspaceConfig::new(&root);
    config.forbid = vec![PathBuf::from("private")];
    config.max_search_file_bytes = 32;
    let mut tools = LocalTools::new(config).unwrap();
    let outcome = tools
        .execute(&request("search_code", json!({"query": "needle"})))
        .unwrap();
    assert!(!outcome.output.contains("private/secret.txt"));
    assert!(!outcome.output.contains("big.txt"));
    assert!(outcome.output.contains("small.txt"));
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn search_code_rejects_symlinked_base_and_skips_symlinked_files() {
    use std::os::unix::fs::symlink;

    let root = temp_workspace("symlink");
    let outside = temp_workspace("symlink-outside");
    write(&outside, "secret.txt", "needle outside\n");
    write(&root, "real.txt", "needle real\n");
    symlink(&outside, root.join("linkdir")).unwrap();
    symlink(outside.join("secret.txt"), root.join("linkfile.txt")).unwrap();
    let mut tools = make_tools(&root, false);

    let error = tools
        .execute(&request(
            "search_code",
            json!({"query": "needle", "path": "linkdir"}),
        ))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("symlink")));

    let outcome = tools
        .execute(&request("search_code", json!({"query": "needle"})))
        .unwrap();
    assert!(outcome.output.contains("real.txt"));
    assert!(!outcome.output.contains("linkfile.txt"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn glob_lists_files_with_filters_and_limits() {
    let root = temp_workspace("glob");
    write(&root, "src/a.rs", "a");
    write(&root, "src/nested/c.rs", "c");
    write(&root, "src/b.py", "b");
    write(&root, "README.md", "readme");
    let mut tools = make_tools(&root, false);

    let outcome = tools
        .execute(&request("glob", json!({"pattern": "**/*.rs"})))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("src/a.rs"));
    assert!(outcome.output.contains("src/nested/c.rs"));
    assert!(!outcome.output.contains("src/b.py"));

    let limited = tools
        .execute(&request(
            "glob",
            json!({"pattern": "**/*", "max_results": 1}),
        ))
        .unwrap();
    assert_eq!(limited.output.lines().count(), 1);

    let scoped = tools
        .execute(&request("glob", json!({"pattern": "*.py", "path": "src"})))
        .unwrap();
    assert!(scoped.output.contains("src/b.py"));
    assert!(!scoped.output.contains("README.md"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn glob_skips_hidden_ignored_and_symlinked_entries() {
    let root = temp_workspace("glob-hidden");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(
        &root,
        ".gitignore",
        "ignored.rs
",
    );
    write(&root, "src/a.rs", "a");
    write(&root, "ignored.rs", "ignored");
    write(&root, ".hidden/h.rs", "hidden");
    let mut tools = make_tools(&root, false);
    let outcome = tools
        .execute(&request("glob", json!({"pattern": "**/*.rs"})))
        .unwrap();
    assert!(outcome.output.contains("src/a.rs"));
    assert!(!outcome.output.contains("ignored.rs"));
    assert!(!outcome.output.contains(".hidden/h.rs"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_code_truncates_large_output() {
    let root = temp_workspace("search-truncate");
    let mut content = String::new();
    for index in 0..100 {
        content.push_str(&format!(
            "needle {index}
"
        ));
    }
    write(&root, "many.txt", &content);
    let mut config = WorkspaceConfig::new(&root);
    config.max_output_bytes = 40;
    let mut tools = LocalTools::new(config).unwrap();
    let outcome = tools
        .execute(&request("search_code", json!({"query": "needle"})))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.ends_with("...(truncated)"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn glob_invalid_pattern_fails_closed() {
    let root = temp_workspace("glob-invalid");
    let mut tools = make_tools(&root, false);
    let error = tools
        .execute(&request("glob", json!({"pattern": "["})))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("glob")));

    let error = tools.execute(&request("glob", json!({}))).unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("pattern")));
    let _ = std::fs::remove_dir_all(&root);
}

fn shell_available() -> bool {
    if let Ok(shell) = std::env::var("TRIT_SHELL") {
        return Path::new(&shell).is_file();
    }
    #[cfg(windows)]
    {
        [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ]
        .iter()
        .any(|path| Path::new(path).is_file())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("bash")
            .arg("--version")
            .output()
            .is_ok()
    }
}

#[test]
fn persistent_shell_reports_status_and_survives_failure() {
    if !shell_available() {
        eprintln!("no bash available; skipping");
        return;
    }
    let root = temp_workspace("shell-status");
    let mut tools = make_tools(&root, true);
    let failed = tools.execute(&ToolRequest::shell("s1", "false")).unwrap();
    assert!(!failed.passed);
    assert!(!failed.in_doubt);
    let alive = tools
        .execute(&ToolRequest::shell("s2", "echo alive"))
        .unwrap();
    assert!(alive.passed);
    assert!(alive.output.contains("alive"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persistent_shell_merges_stderr_and_multiline_output() {
    if !shell_available() {
        eprintln!("no bash available; skipping");
        return;
    }
    let root = temp_workspace("shell-output");
    let mut tools = make_tools(&root, true);
    let outcome = tools
        .execute(&ToolRequest::shell(
            "s1",
            "echo one\necho err >&2\necho two",
        ))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("one"));
    assert!(outcome.output.contains("err"));
    assert!(outcome.output.contains("two"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persistent_shell_preserves_output_without_trailing_newline() {
    if !shell_available() {
        eprintln!("no bash available; skipping");
        return;
    }
    let root = temp_workspace("shell-no-newline");
    let mut tools = make_tools(&root, true);
    let outcome = tools
        .execute(&ToolRequest::shell("s1", "printf 'no-newline'"))
        .unwrap();
    assert!(outcome.passed);
    assert_eq!(outcome.output, "no-newline");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persistent_shell_recovers_after_exit() {
    if !shell_available() {
        eprintln!("no bash available; skipping");
        return;
    }
    let root = temp_workspace("shell-exit");
    let mut tools = make_tools(&root, true);
    let exited = tools.execute(&ToolRequest::shell("s1", "exit 7")).unwrap();
    assert!(!exited.passed);
    assert!(exited.in_doubt);
    let recovered = tools
        .execute(&ToolRequest::shell("s2", "echo recovered"))
        .unwrap();
    assert!(recovered.passed);
    assert!(recovered.output.contains("recovered"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persistent_shell_truncates_large_output() {
    if !shell_available() {
        eprintln!("no bash available; skipping");
        return;
    }
    let root = temp_workspace("shell-truncate");
    let mut config = WorkspaceConfig::new(&root);
    config.allow_shell = true;
    config.allow_unsandboxed_shell = true;
    config.max_output_bytes = 8;
    let mut tools = LocalTools::new(config).unwrap();
    let outcome = tools
        .execute(&ToolRequest::shell("s1", "printf 'abcdefghijklmnop'"))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.ends_with("...(truncated)"));
    let _ = std::fs::remove_dir_all(&root);
}
