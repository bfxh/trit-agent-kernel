use std::path::PathBuf;

use trit_agent::tools::{LocalTools, SandboxCommand, WorkspaceConfig};
use trit_agent::{
    AgentError, HeadlessConfig, Proposal, ScriptedModel, ToolExecutor, ToolRequest,
    effect_confirmed, run_headless,
};
use trit_orch::EvidenceVerdict;
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-agent-{tag}-{}-{:?}",
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
            context: trit_agent::ContextPolicy::default(),
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
            context: trit_agent::ContextPolicy::default(),
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
        "trit-agent-outside-{}-{:?}.txt",
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
        "trit-agent-symlink-outside-{}-{:?}.txt",
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
        "trit-agent-hardlink-write-{}-{:?}.txt",
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
        "trit-agent-symlink-dir-{}-{:?}",
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
