use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use trit_host::tools::{LocalTools, WorkspaceConfig};
use trit_host::{
    AgentError, GitCheckpointStore, GitCheckpointingTools, Proposal, RewindScope, ScriptedModel,
    Session, ToolExecutor, ToolRequest, rewind_session_git, rewind_session_git_with_policy,
};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-git-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn temp_file(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-git-{tag}-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn git_in(dir: &std::path::Path, root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .arg("--work-tree")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_stdin(dir: &std::path::Path, root: &std::path::Path, args: &[&str], input: &str) -> String {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .arg("--work-tree")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn turn() -> TurnSpec {
    TurnSpec {
        task: "headless".into(),
        objective: "write a.txt".into(),
        constraints: vec![],
        stop_when: vec![],
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

fn proposal() -> Proposal {
    Proposal {
        tool_calls: vec![
            ToolRequest::write("w1", "write_file", "a.txt", "v1"),
            ToolRequest::shell("b1", "echo hi"),
        ],
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
    }
}

#[test]
fn git_checkpoint_rewinds_workspace_changes() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("workspace");
    let store = GitCheckpointStore::open(root.join(".trit-checkpoints"), &root).unwrap();
    let mut config = WorkspaceConfig::new(&root);
    config.allow_write = true;
    let mut tools = GitCheckpointingTools::new(store.clone(), LocalTools::new(config).unwrap());

    tools.begin_turn(0, "empty").unwrap();
    tools
        .execute(&ToolRequest::write("w1", "write_file", "a.txt", "v1"))
        .unwrap();
    tools.end_turn(true).unwrap();

    tools.begin_turn(1, "before shell").unwrap();
    std::fs::write(root.join("a.txt"), "v2").unwrap();
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/c.txt"), "c").unwrap();
    tools.end_turn(true).unwrap();

    assert_eq!(store.turns().unwrap(), [0, 1]);
    let report = store.rewind(1).unwrap();
    assert!(report.restored.contains(&"a.txt".into()), "{report:?}");
    assert!(report.deleted.contains(&"d/c.txt".into()), "{report:?}");
    assert!(
        report
            .restored
            .iter()
            .chain(report.deleted.iter())
            .all(|path| !path.starts_with(".trit-checkpoints")),
        "checkpoint dir must be excluded: {report:?}"
    );
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");
    assert!(!root.join("d/c.txt").exists());

    let report = store.rewind(0).unwrap();
    assert!(report.deleted.contains(&"a.txt".into()), "{report:?}");
    assert!(!root.join("a.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_commands_fail_closed_and_allow_override() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("session");
    let session_path = temp_file("session");
    let checkpoint_dir = temp_dir("session-store");
    let mut config = WorkspaceConfig::new(&root);
    config.allow_write = true;
    config.allow_shell = true;
    config.allow_unsandboxed_shell = true;
    let store = GitCheckpointStore::open(&checkpoint_dir, &root).unwrap();
    let wrapped = GitCheckpointingTools::new(store, LocalTools::new(config).unwrap());
    let mut session =
        Session::open(ScriptedModel::new(vec![proposal()]), wrapped, &session_path).unwrap();
    assert!(session.run_turn(&turn()).unwrap().judgment.is_complete());
    assert!(root.join("a.txt").exists());
    let audit = checkpoint_dir.join("commands-00000000.json");
    assert!(audit.exists(), "command audit should be persisted");
    std::fs::remove_file(&audit).unwrap();

    assert_eq!(
        rewind_session_git(&session_path, &checkpoint_dir, &root, 0, RewindScope::Both),
        Err(AgentError::Unrewindable(vec!["echo hi".into()])),
        "ORCH_CKPT_GIT_FAIL_CLOSED"
    );
    assert!(
        root.join("a.txt").exists(),
        "fail-closed rewind must not restore code"
    );

    let report = rewind_session_git_with_policy(
        &session_path,
        &checkpoint_dir,
        &root,
        0,
        RewindScope::Both,
        true,
    )
    .unwrap();
    assert_eq!(report.unrewindable, ["echo hi"]);
    assert!(report.deleted.contains(&"a.txt".into()), "{report:?}");
    assert!(report.conversation_truncated);
    assert!(!root.join("a.txt").exists());
    let _ = std::fs::remove_file(&session_path);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_rerun_clears_command_audit() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("audit");
    let store = GitCheckpointStore::open(temp_dir("audit-store"), &root).unwrap();
    store.snapshot(0, "first").unwrap();
    store.record_commands(0, &["echo hi".into()]).unwrap();
    assert_eq!(store.commands_since(0).unwrap(), ["echo hi"]);
    store.record_commands(0, &[]).unwrap();
    assert!(store.commands_since(0).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_rejects_foreign_directory() {
    let root = temp_dir("foreign-root");
    let dir = temp_dir("foreign-store");
    std::fs::write(dir.join("keep.txt"), "user data").unwrap();
    let result = GitCheckpointStore::open(&dir, &root);
    assert!(result.is_err(), "must not init inside a foreign directory");
    assert_eq!(
        std::fs::read_to_string(dir.join("keep.txt")).unwrap(),
        "user data"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn git_checkpoint_rejects_nested_repository() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("nested");
    let nested = root.join("nested");
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(&nested)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(nested.join("file.txt"), "nested").unwrap();
    for args in [
        vec!["-C", nested.to_str().unwrap(), "add", "."],
        vec![
            "-C",
            nested.to_str().unwrap(),
            "-c",
            "user.name=trit",
            "-c",
            "user.email=trit@localhost",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let status = Command::new("git").args(args).status().unwrap();
        assert!(status.success());
    }
    let store = GitCheckpointStore::open(temp_dir("nested-store"), &root).unwrap();
    let error = store.snapshot(0, "nested").unwrap_err();
    assert!(error.to_string().contains("nested repositories"), "{error}");
    assert!(store.turns().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_rejects_directory_collision_before_mutation() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("dir-collision");
    let store = GitCheckpointStore::open(temp_dir("dir-collision-store"), &root).unwrap();
    store.snapshot(0, "empty").unwrap();
    std::fs::write(root.join("a.txt"), "v1").unwrap();
    store.snapshot(1, "file").unwrap();
    std::fs::remove_file(root.join("a.txt")).unwrap();
    std::fs::create_dir(root.join("a.txt")).unwrap();
    std::fs::write(root.join("a.txt/child.txt"), "child").unwrap();

    let error = store.rewind(1).unwrap_err();
    assert!(error.to_string().contains("replace a directory"), "{error}");
    assert!(root.join("a.txt").is_dir());
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt/child.txt")).unwrap(),
        "child"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn git_checkpoint_preserves_symlinks() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("symlink");
    let store = GitCheckpointStore::open(temp_dir("symlink-store"), &root).unwrap();
    store.snapshot(0, "empty").unwrap();
    std::fs::write(root.join("a.txt"), "v1").unwrap();
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    store.snapshot(1, "symlink").unwrap();
    std::fs::remove_file(root.join("link")).unwrap();

    let report = store.rewind(1).unwrap();
    assert!(report.restored.contains(&"link".into()), "{report:?}");
    let metadata = std::fs::symlink_metadata(root.join("link")).unwrap();
    assert!(metadata.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(root.join("link")).unwrap(),
        std::path::Path::new("a.txt")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_rejects_ignored_path_collision() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("ignored");
    let checkpoint_dir = temp_dir("ignored-store");
    let store = GitCheckpointStore::open(&checkpoint_dir, &root).unwrap();
    store.snapshot(0, "empty").unwrap();

    std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
    std::fs::write(root.join("secret.txt"), "user-data").unwrap();
    let blob = git_stdin(
        &checkpoint_dir,
        &root,
        &["hash-object", "-w", "--stdin"],
        "snapshot-secret",
    );
    let tree = git_stdin(
        &checkpoint_dir,
        &root,
        &["mktree"],
        &format!("100644 blob {blob}\tsecret.txt\n"),
    );
    let commit = git_in(
        &checkpoint_dir,
        &root,
        &["commit-tree", &tree, "-m", "crafted"],
    );
    git_in(
        &checkpoint_dir,
        &root,
        &["update-ref", "refs/trit/turn-00000000", &commit],
    );

    let error = store.rewind(0).unwrap_err();
    assert!(error.to_string().contains("currently ignored"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("secret.txt")).unwrap(),
        "user-data",
        "fail-closed rewind must not overwrite ignored files"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_retention_prunes_refs() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let root = temp_dir("retain");
    let store = GitCheckpointStore::open(temp_dir("retain-store"), &root).unwrap();
    for turn in 0..4 {
        if turn > 0 {
            std::fs::write(root.join("a.txt"), format!("v{turn}")).unwrap();
        }
        store.snapshot(turn, "retain").unwrap();
    }
    assert_eq!(store.prune(2).unwrap(), 2);
    assert_eq!(store.turns().unwrap(), [2, 3]);
    assert!(
        store.rewind(1).is_err(),
        "pruned checkpoint must fail closed"
    );
    let report = store.rewind(2).unwrap();
    assert!(report.restored.contains(&"a.txt".into()), "{report:?}");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v2");
    let _ = std::fs::remove_dir_all(&root);
}
