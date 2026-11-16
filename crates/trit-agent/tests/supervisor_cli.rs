use std::path::PathBuf;
use std::process::Command;

use trit_agent::{ProcessSpec, Proposal, RestartPolicy, SupervisorConfig, ToolRequest};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-agent-supervisor-cli-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn cli_starts_supervised_gateway_and_reports_it() {
    let root = temp_dir("run");
    let script_path = root.join("proposal.json");
    let config_path = root.join("supervisor.json");
    let proposal = Proposal {
        tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", "from-cli")],
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
    };
    std::fs::write(
        &script_path,
        serde_json::to_string(&vec![proposal]).unwrap(),
    )
    .unwrap();
    let gateway = if cfg!(windows) {
        ProcessSpec::new("cmd").with_args(["/C", "exit 0"])
    } else {
        ProcessSpec::new("sh").with_args(["-c", "exit 0"])
    };
    let config = SupervisorConfig {
        gateway: vec![gateway],
        mcp: Vec::new(),
        restart: RestartPolicy {
            max_attempts: 0,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        interval_ms: 10,
        mcp_timeout_ms: 1_000,
    };
    std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-agent"))
        .args([
            "run",
            "--task",
            "supervised",
            "--workspace",
            root.to_str().unwrap(),
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--supervisor-config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("supervisor: gateways=1 mcp=0"),
        "stderr: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "from-cli"
    );
    let _ = std::fs::remove_dir_all(&root);
}
