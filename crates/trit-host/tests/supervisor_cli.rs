use std::path::PathBuf;
use std::process::Command;

use trit_host::{ProcessSpec, Proposal, RestartPolicy, SupervisorConfig, ToolRequest};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-host-supervisor-cli-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn mcp_tool_stub(dir: &std::path::Path) -> ProcessSpec {
    if cfg!(windows) {
        let script = dir.join("mcp-stub.ps1");
        std::fs::write(
            &script,
            r#"$id = 0
while ($null -ne ($line = [Console]::In.ReadLine())) {
  if ($line -match '"id":([0-9]+)') { $id = $Matches[1] }
  if ($line -like '*"method":"initialize"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0"}}}')
  } elseif ($line -like '*"method":"tools/list"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}')
  } elseif ($line -like '*"method":"tools/call"*') {
    Write-Output ('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"mcp-ok"}]}}')
  }
}
"#,
        )
        .unwrap();
        ProcessSpec::new("powershell").with_args([
            "-NoProfile".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            script.display().to_string(),
        ])
    } else {
        let script = dir.join("mcp-stub.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '%s
' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0"}}}' ;;
    *'"method":"tools/list"'*) printf '%s
' '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}' ;;
    *'"method":"tools/call"'*) printf '%s
' '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"mcp-ok"}]}}' ;;
  esac
done
"#,
        )
        .unwrap();
        ProcessSpec::new("sh").with_args([script.display().to_string()])
    }
}

#[test]
fn cli_exposes_and_executes_mcp_tools() {
    let root = temp_dir("mcp-tools");
    let script_path = root.join("proposal.json");
    let config_path = root.join("supervisor.json");
    let mut spec = mcp_tool_stub(&root);
    spec.name = "stub".into();
    let proposals = vec![
        Proposal {
            tool_calls: vec![ToolRequest {
                call_id: "mcp-1".into(),
                tool: "mcp__stub__echo".into(),
                arguments: Some(serde_json::json!({"text": "hi"})),
                ..ToolRequest::default()
            }],
            ..Proposal::default()
        },
        Proposal {
            tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", "done")],
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
        },
    ];
    std::fs::write(&script_path, serde_json::to_string(&proposals).unwrap()).unwrap();
    let config = SupervisorConfig {
        mcp: vec![spec],
        interval_ms: 20,
        mcp_timeout_ms: 30_000,
        ..SupervisorConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "run",
            "--task",
            "bridge mcp",
            "--workspace",
            root.to_str().unwrap(),
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--supervisor-config",
            config_path.to_str().unwrap(),
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}
stderr: {stderr}"
    );
    let outcome: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let receipts = outcome["receipts"].as_array().unwrap();
    assert!(receipts.iter().any(|receipt| {
        receipt["tool"] == "mcp__stub__echo"
            && receipt["passed"] == true
            && receipt["output"]
                .as_str()
                .is_some_and(|value| value.contains("mcp-ok"))
    }));
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "done");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_executes_native_search_tools() {
    let root = temp_dir("native-search");
    let script_path = root.join("proposal.json");
    std::fs::write(
        root.join("notes.txt"),
        "needle in notes
",
    )
    .unwrap();
    let proposals = vec![
        Proposal {
            tool_calls: vec![ToolRequest {
                call_id: "search-1".into(),
                tool: "search_code".into(),
                read_only: true,
                arguments: Some(serde_json::json!({"query": "needle"})),
                ..ToolRequest::default()
            }],
            ..Proposal::default()
        },
        Proposal {
            tool_calls: vec![ToolRequest::write("w1", "write_file", "a.txt", "done")],
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
        },
    ];
    std::fs::write(&script_path, serde_json::to_string(&proposals).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "run",
            "--task",
            "native search",
            "--workspace",
            root.to_str().unwrap(),
            "--allow-write",
            "--script",
            script_path.to_str().unwrap(),
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}
stderr: {stderr}"
    );
    let outcome: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        outcome["receipts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|receipt| {
                receipt["tool"] == "search_code"
                    && receipt["passed"] == true
                    && receipt["output"]
                        .as_str()
                        .is_some_and(|value| value.contains("notes.txt"))
            })
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_rejects_sampling_without_a_real_provider() {
    let root = temp_dir("sampling");
    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
        .args([
            "run",
            "--task",
            "sample",
            "--workspace",
            root.to_str().unwrap(),
            "--mcp-sampling",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("--mcp-sampling requires a real provider"),
        "stderr: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
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
        bridges: Vec::new(),
        roots: Vec::new(),
        roots_watch: Vec::new(),
        roots_watch_debounce_ms: 50,
        log_level: None,
        restart: RestartPolicy {
            max_attempts: 0,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        interval_ms: 10,
        mcp_timeout_ms: 1_000,
        resource_read_concurrency: 8,
        mcp_http: Vec::new(),
    };
    std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trit-host"))
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
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("supervisor: gateways=1 mcp=0 bridges=0"),
        "stderr: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "from-cli"
    );
    let _ = std::fs::remove_dir_all(&root);
}
