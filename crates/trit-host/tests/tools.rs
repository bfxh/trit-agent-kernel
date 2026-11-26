use std::path::PathBuf;

use trit_host::tools::{LocalTools, SandboxCommand, WorkspaceConfig};
use trit_host::{
    AgentError, HeadlessConfig, Proposal, ScriptedModel, ToolExecutor, ToolRequest,
    effect_confirmed, run_headless,
};
use trit_orch::EvidenceVerdict;
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn make_tools(root: &std::path::Path, allow_write: bool, allow_shell: bool) -> LocalTools {
    let mut config = WorkspaceConfig::new(root);
    config.allow_write = allow_write;
    config.allow_shell = allow_shell;
    config.allow_unsandboxed_shell = allow_shell;
    LocalTools::new(config).unwrap()
}

fn turn() -> TurnSpec {
    TurnSpec {
        task: "headless".into(),
        objective: "write a file".into(),
        constraints: vec![],
        stop_when: vec![],
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

#[test]
fn write_and_read_stay_inside_the_workspace() {
    let root = temp_workspace("roundtrip");
    let mut tools = make_tools(&root, true, false);

    let written = tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "hello"))
        .unwrap();
    assert!(written.passed);
    assert_eq!(written.written_paths, ["a.txt"]);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "hello"
    );

    let read = tools.execute(&ToolRequest::read("r1", "a.txt")).unwrap();
    assert_eq!(read.output, "hello");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn path_escape_and_forbidden_paths_are_rejected() {
    let root = temp_workspace("escape");
    let mut tools = make_tools(&root, false, false);
    let error = tools
        .execute(&ToolRequest::read("r1", "../secret"))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("escapes")),
        "ORCH_AGENT_PATH_ESCAPE: {error:?}"
    );

    let mut forbidden = WorkspaceConfig::new(&root);
    forbidden.forbid = vec![PathBuf::from("private")];
    let mut guarded = LocalTools::new(forbidden).unwrap();
    let error = guarded
        .execute(&ToolRequest::read("r2", "private/secret.txt"))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("forbidden")),
        "ORCH_AGENT_FORBIDDEN: {error:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn writes_and_shell_require_explicit_permission() {
    let root = temp_workspace("permissions");
    let mut tools = make_tools(&root, false, false);

    let error = tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "x"))
        .unwrap_err();
    assert!(matches!(&error, AgentError::Tool(message) if message.contains("not allowed")));
    let error = tools
        .execute(&ToolRequest::shell("s1", "echo hello"))
        .unwrap_err();
    assert!(matches!(&error, AgentError::Tool(message) if message.contains("not allowed")));

    let mut allowed = make_tools(&root, false, true);
    let outcome = allowed
        .execute(&ToolRequest::shell("s1", "echo hello"))
        .unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.to_lowercase().contains("hello"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn native_search_and_glob_work_without_shell() {
    let root = temp_workspace("native-search");
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "fn alpha() {}
// needle here
",
    )
    .unwrap();
    std::fs::write(
        root.join("src/nested/c.rs"),
        "// needle nested
",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.py"),
        "needle in python
",
    )
    .unwrap();
    std::fs::write(
        root.join("README.md"),
        "needle docs
",
    )
    .unwrap();
    std::fs::write(
        root.join(".hidden/secret.txt"),
        "needle hidden
",
    )
    .unwrap();
    std::fs::write(root.join("binary.bin"), b"needle\0binary").unwrap();
    let mut tools = make_tools(&root, false, false);

    let search = ToolRequest {
        call_id: "s1".into(),
        tool: "search_code".into(),
        arguments: Some(serde_json::json!({"query": "needle"})),
        ..ToolRequest::default()
    };
    let outcome = tools.execute(&search).unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("src/a.rs:2:"));
    assert!(outcome.output.contains("src/b.py:1:"));
    assert!(outcome.output.contains("README.md:1:"));
    assert!(!outcome.output.contains(".hidden/secret.txt"));
    assert!(!outcome.output.contains("binary.bin"));

    let regex = ToolRequest {
        call_id: "s2".into(),
        tool: "search_code".into(),
        arguments: Some(serde_json::json!({
            "query": r"needle\s+in",
            "regex": true
        })),
        ..ToolRequest::default()
    };
    let outcome = tools.execute(&regex).unwrap();
    assert!(outcome.output.contains("src/b.py:1:"));
    assert!(!outcome.output.contains("src/a.rs"));

    let filtered = ToolRequest {
        call_id: "s3".into(),
        tool: "search_code".into(),
        arguments: Some(serde_json::json!({
            "query": "needle",
            "glob": "**/*.rs"
        })),
        ..ToolRequest::default()
    };
    let outcome = tools.execute(&filtered).unwrap();
    assert!(outcome.output.contains("src/a.rs"));
    assert!(outcome.output.contains("src/nested/c.rs"));
    assert!(!outcome.output.contains("src/b.py"));

    let glob = ToolRequest {
        call_id: "g1".into(),
        tool: "glob".into(),
        arguments: Some(serde_json::json!({"pattern": "**/*.rs"})),
        ..ToolRequest::default()
    };
    let outcome = tools.execute(&glob).unwrap();
    assert!(outcome.passed);
    assert!(outcome.output.contains("src/a.rs"));
    assert!(outcome.output.contains("src/nested/c.rs"));
    assert!(!outcome.output.contains("README.md"));
    let _ = std::fs::remove_dir_all(&root);
}

fn shell_session_available() -> bool {
    if let Ok(shell) = std::env::var("TRIT_SHELL") {
        return std::path::Path::new(&shell).is_file();
    }
    #[cfg(windows)]
    {
        [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ]
        .iter()
        .any(|path| std::path::Path::new(path).is_file())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("bash")
            .arg("--version")
            .output()
            .is_ok()
            || std::process::Command::new("sh")
                .args(["-c", "true"])
                .output()
                .is_ok()
    }
}

#[test]
fn persistent_shell_keeps_cwd_and_environment() {
    if !shell_session_available() {
        eprintln!("no bash/sh available; skipping persistent shell test");
        return;
    }
    let root = temp_workspace("persistent-shell");
    let mut tools = make_tools(&root, false, true);
    let setup = tools
        .execute(&ToolRequest::shell(
            "s1",
            "export TRIT_KEEP=42; mkdir -p sub; cd sub",
        ))
        .unwrap();
    assert!(setup.passed, "{}", setup.output);
    let check = tools
        .execute(&ToolRequest::shell(
            "s2",
            "printf 'KEEP=%s CWD=%s' \"$TRIT_KEEP\" \"$(pwd)\"",
        ))
        .unwrap();
    assert!(check.passed, "{}", check.output);
    assert!(check.output.contains("KEEP=42"), "{}", check.output);
    assert!(check.output.contains("sub"), "{}", check.output);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn headless_run_writes_and_proves_from_local_receipts() {
    let root = temp_workspace("headless");
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", "hello")],
        completion: Some(ClaimRequest {
            criteria: vec![CriterionRequest {
                id: "c1".into(),
                kind: CriterionKind::Diff {
                    paths: vec!["a.txt".into()],
                },
            }],
        }),
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let mut workspace = WorkspaceConfig::new(&root);
    workspace.allow_write = true;
    let outcome = run_headless(
        turn(),
        model,
        HeadlessConfig {
            workspace,
            max_steps: 2,
            session: None,
            context: trit_host::ContextPolicy::default(),
            write_guard: None,
        },
    )
    .unwrap();

    assert_eq!(outcome.judgment.verdict, EvidenceVerdict::Proved);
    assert!(effect_confirmed(
        &outcome.effects,
        "reasonix:headless:write_file:w1"
    ));
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "hello"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn headless_run_rejects_escape_before_writing() {
    let root = temp_workspace("headless-escape");
    let model = ScriptedModel::new(vec![Proposal {
        tool_calls: vec![ToolRequest::write("w1", "write_file", "../evil.txt", "x")],
        completion: None,
        usage: HostUsage::default(),
        ..Proposal::default()
    }]);
    let mut workspace = WorkspaceConfig::new(&root);
    workspace.allow_write = true;
    let error = run_headless(
        turn(),
        model,
        HeadlessConfig {
            workspace,
            max_steps: 2,
            session: None,
            context: trit_host::ContextPolicy::default(),
            write_guard: None,
        },
    )
    .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("escapes")),
        "ORCH_AGENT_HEADLESS_ESCAPE: {error:?}"
    );
    assert!(!root.parent().unwrap().join("evil.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hardlinked_files_are_rejected() {
    let root = temp_workspace("hardlink");
    let outside = root.parent().unwrap().join(format!(
        "trit-host-outside-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&outside, "secret").unwrap();
    std::fs::hard_link(&outside, root.join("linked.txt")).unwrap();

    let mut tools = make_tools(&root, false, false);
    let error = tools
        .execute(&ToolRequest::read("r1", "linked.txt"))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("hardlinked")),
        "ORCH_AGENT_HARDLINK: {error:?}"
    );
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn symlinks_are_rejected() {
    use std::os::unix::fs::symlink;

    let root = temp_workspace("symlink");
    let outside = root.parent().unwrap().join(format!(
        "trit-host-symlink-outside-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&outside, "secret").unwrap();
    symlink(&outside, root.join("link.txt")).unwrap();

    let mut tools = make_tools(&root, false, false);
    let error = tools
        .execute(&ToolRequest::read("r1", "link.txt"))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("symlink")),
        "ORCH_AGENT_SYMLINK: {error:?}"
    );
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shell_requires_a_sandbox_or_explicit_unsandboxed_opt_in() {
    let root = temp_workspace("sandbox");
    let mut config = WorkspaceConfig::new(&root);
    config.allow_shell = true;
    let mut tools = LocalTools::new(config).unwrap();
    let error = tools
        .execute(&ToolRequest::shell("s1", "echo hello"))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("requires an OS sandbox")),
        "ORCH_AGENT_SHELL_SANDBOX: {error:?}"
    );

    let sandbox = SandboxCommand {
        program: "sandbox-exe".into(),
        args: vec!["--root".into(), "w".into()],
    };
    let (program, args) = sandbox.argv("echo hello");
    assert_eq!(program, PathBuf::from("sandbox-exe"));
    assert_eq!(&args[..2], ["--root", "w"]);
    #[cfg(windows)]
    assert_eq!(&args[2..], ["cmd", "/C", "echo hello"]);
    #[cfg(unix)]
    assert_eq!(&args[2..], ["sh", "-c", "echo hello"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn writes_through_hardlinks_are_rejected() {
    let root = temp_workspace("hardlink-write");
    let outside = root.parent().unwrap().join(format!(
        "trit-host-hardlink-write-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&outside, "original").unwrap();
    std::fs::hard_link(&outside, root.join("linked.txt")).unwrap();

    let mut tools = make_tools(&root, true, false);
    let error = tools
        .execute(&ToolRequest::write(
            "w1",
            "write_file",
            "linked.txt",
            "evil",
        ))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("hardlinked")),
        "ORCH_AGENT_HARDLINK_WRITE: {error:?}"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn writes_through_symlinks_and_symlinked_dirs_are_rejected() {
    use std::os::unix::fs::symlink;

    let root = temp_workspace("symlink-write");
    let outside_dir = root.parent().unwrap().join(format!(
        "trit-host-symlink-dir-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&outside_dir);
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside_file = outside_dir.join("secret.txt");
    std::fs::write(&outside_file, "original").unwrap();
    symlink(&outside_file, root.join("link.txt")).unwrap();
    symlink(&outside_dir, root.join("linkdir")).unwrap();

    let mut tools = make_tools(&root, true, false);
    let error = tools
        .execute(&ToolRequest::write("w1", "write_file", "link.txt", "evil"))
        .unwrap_err();
    assert!(
        matches!(&error, AgentError::Tool(message) if message.contains("symlink")),
        "ORCH_AGENT_SYMLINK_WRITE: {error:?}"
    );
    assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "original");

    let error = tools
        .execute(&ToolRequest::write(
            "w2",
            "write_file",
            "linkdir/secret.txt",
            "evil",
        ))
        .unwrap_err();
    assert!(
        !outside_file.with_file_name("other.txt").exists(),
        "parent symlink must not create files outside"
    );
    assert!(
        matches!(&error, AgentError::Tool(_) | AgentError::Io(_)),
        "ORCH_AGENT_SYMLINK_DIR: {error:?}"
    );
    assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "original");
    let _ = std::fs::remove_dir_all(&outside_dir);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn local_tools_move_path_moves_file() {
    let root = temp_workspace("move");
    let mut tools = make_tools(&root, true, false);
    std::fs::write(root.join("a.txt"), "hello").unwrap();
    let outcome = tools
        .execute(&ToolRequest::move_path("m1", "a.txt", "sub/b.txt"))
        .unwrap();
    assert!(outcome.passed, "{outcome:?}");
    assert_eq!(outcome.written_paths, ["a.txt", "sub/b.txt"]);
    assert!(!root.join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("sub/b.txt")).unwrap(),
        "hello"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn local_tools_move_path_requires_two_paths_and_write() {
    let root = temp_workspace("move-args");
    let mut tools = make_tools(&root, true, false);
    let error = tools
        .execute(&ToolRequest::write("m1", "move_path", "a.txt", ""))
        .unwrap_err();
    assert!(error.to_string().contains("two paths"), "{error}");

    let mut readonly = make_tools(&root, false, false);
    let error = readonly
        .execute(&ToolRequest::move_path("m1", "a.txt", "b.txt"))
        .unwrap_err();
    assert!(error.to_string().contains("not allowed"), "{error}");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(target_os = "linux")]
#[test]
fn builtin_sandbox_wraps_workspace_with_bwrap() {
    let root = temp_workspace("builtin-sandbox");
    let sandbox = SandboxCommand::builtin(&root).unwrap();
    let (program, args) = sandbox.argv("echo hi");
    assert_eq!(program, PathBuf::from("bwrap"));
    let root_text = std::fs::canonicalize(&root)
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        args.windows(3)
            .any(|w| w[0] == "--bind" && w[1] == root_text && w[2] == root_text)
    );
    assert!(args.iter().any(|arg| arg == "--unshare-all"));
    assert!(args.iter().any(|arg| arg == "--unshare-net"));
    assert!(args.iter().any(|arg| arg == "--die-with-parent"));
    let tail: Vec<&String> = args.iter().rev().take(3).collect();
    assert_eq!(tail[2], "sh");
    assert_eq!(tail[1], "-c");
    assert_eq!(tail[0], "echo hi");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(target_os = "macos")]
#[test]
fn builtin_sandbox_wraps_workspace_with_sandbox_exec() {
    let root = temp_workspace("builtin-sandbox-macos");
    let sandbox = SandboxCommand::builtin(&root).unwrap();
    let (program, args) = sandbox.argv("echo hi");
    assert_eq!(program, PathBuf::from("sandbox-exec"));
    assert_eq!(args[0], "-p");
    let root_text = std::fs::canonicalize(&root)
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(args[1].contains(&root_text), "{}", args[1]);
    assert!(!args[1].contains("allow network"), "{}", args[1]);
    let tail: Vec<&String> = args.iter().rev().take(3).collect();
    assert_eq!(tail[2], "sh");
    assert_eq!(tail[1], "-c");
    assert_eq!(tail[0], "echo hi");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[test]
fn builtin_sandbox_is_rejected_off_unix() {
    let root = temp_workspace("builtin-sandbox-off");
    let error = SandboxCommand::builtin(&root).unwrap_err();
    assert!(
        error.to_string().contains("only available on Linux"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[test]
fn builtin_sandbox_uses_appcontainer_run() {
    let root = temp_workspace("builtin-sandbox-win");
    let sandbox = SandboxCommand::builtin(&root).unwrap();
    let (program, args) = sandbox.argv("echo hi");
    assert_eq!(program, std::env::current_exe().unwrap());
    assert_eq!(args[0], "appcontainer-run");
    assert_eq!(args[1], "--workspace");
    assert_eq!(args[3], "--");
    let tail: Vec<&String> = args.iter().rev().take(3).collect();
    assert_eq!(tail[2], "cmd");
    assert_eq!(tail[1], "/C");
    assert_eq!(tail[0], "echo hi");
    let _ = std::fs::remove_dir_all(root);
}
