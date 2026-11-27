//! 协议选择的进程级回归：误配必须 fail closed，且报错要点明该配哪个变量。

use std::path::PathBuf;
use std::process::Command;

fn temp_workspace(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-protocol-cli-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn run(workspace: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_MODEL")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_MODEL")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("DEEPSEEK_MODEL")
        .env_remove("DEEPSEEK_BASE_URL")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GEMINI_MODEL")
        .env_remove("GEMINI_BASE_URL")
        .env_remove("TRIT_PROTOCOL")
        .env_remove("TRIT_API_KEY")
        .env_remove("TRIT_MODEL")
        .env_remove("TRIT_BASE_URL")
        .current_dir(workspace)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn protocol_requires_a_non_scripted_provider() {
    let workspace = temp_workspace("scripted");
    let path = workspace.join("proposals.json");
    std::fs::write(&path, "[]").unwrap();
    let (ok, _stdout, stderr) = run(
        &workspace,
        &[
            "run",
            "--task",
            "t",
            "--workspace",
            workspace.to_str().unwrap(),
            "--provider",
            "scripted",
            "--protocol",
            "anthropic-messages",
            "--script",
            path.to_str().unwrap(),
        ],
    );
    assert!(!ok, "scripted + --protocol 必须 fail closed");
    assert!(
        stderr.contains("--protocol requires a non-scripted"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn unknown_protocol_lists_supported_names() {
    let workspace = temp_workspace("unknown");
    let (ok, _stdout, stderr) = run(
        &workspace,
        &[
            "run",
            "--task",
            "t",
            "--workspace",
            workspace.to_str().unwrap(),
            "--provider",
            "openai",
            "--protocol",
            "bedrock",
        ],
    );
    assert!(!ok, "未知协议必须 fail closed");
    assert!(stderr.contains("unknown protocol"), "{stderr}");
    assert!(stderr.contains("anthropic-messages"), "{stderr}");
    assert!(stderr.contains("gemini-generate-content"), "{stderr}");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn anthropic_provider_names_the_missing_key() {
    let workspace = temp_workspace("anthropic-key");
    let (ok, _stdout, stderr) = run(
        &workspace,
        &[
            "run",
            "--task",
            "t",
            "--workspace",
            workspace.to_str().unwrap(),
            "--provider",
            "anthropic",
        ],
    );
    assert!(!ok, "缺 key 必须 fail closed");
    assert!(stderr.contains("ANTHROPIC_API_KEY"), "{stderr}");
    assert!(
        !stderr.contains("OPENAI_API_KEY"),
        "不能悄悄回落到别的厂商凭据: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn gemini_provider_names_the_missing_key() {
    let workspace = temp_workspace("gemini-key");
    let (ok, _stdout, stderr) = run(
        &workspace,
        &[
            "run",
            "--task",
            "t",
            "--workspace",
            workspace.to_str().unwrap(),
            "--provider",
            "gemini",
        ],
    );
    assert!(!ok, "缺 key 必须 fail closed");
    assert!(stderr.contains("GEMINI_API_KEY"), "{stderr}");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn deepseek_provider_names_the_missing_key() {
    let workspace = temp_workspace("deepseek-key");
    let (ok, _stdout, stderr) = run(
        &workspace,
        &[
            "run",
            "--task",
            "t",
            "--workspace",
            workspace.to_str().unwrap(),
            "--provider",
            "deepseek",
        ],
    );
    assert!(!ok, "缺 key 必须 fail closed");
    assert!(stderr.contains("DEEPSEEK_API_KEY"), "{stderr}");
    assert!(
        !stderr.contains("OPENAI_API_KEY"),
        "不能悄悄回落到别家凭据: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}
