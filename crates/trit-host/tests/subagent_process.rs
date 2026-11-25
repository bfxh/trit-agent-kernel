//! 真实子 agent 进程：spawn、帧往返、失败/无响应/超时/引用不匹配。

use std::time::{Duration, Instant};

use trit_host::{AgentError, SubagentProcessConfig, run_subagent, run_subagent_reply_cancellable};
use trit_orch::{
    AccessMode, CancelToken, CapsuleEntry, ContextCapsule, DelegationStatus, SubagentProfile,
    SubagentRequest, Task, TaskSpec,
};

fn request() -> SubagentRequest {
    let task = TaskSpec {
        task: Task {
            id: "a".into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        epoch: 0,
        ..TaskSpec::default()
    };
    let capsule = ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "goal".into(),
            value: "ship".into(),
        }],
        &["goal"],
        64,
    )
    .unwrap();
    let profile = SubagentProfile {
        name: "child".into(),
        access: AccessMode::ReadOnly,
        write_roots: Vec::new(),
        tools: vec!["read_file".into()],
        max_tokens: 100,
    };
    SubagentRequest::new(&task, capsule, profile)
}

fn config() -> SubagentProcessConfig {
    SubagentProcessConfig::new(env!("CARGO_BIN_EXE_trit-host"))
        .with_args(vec!["subagent-echo".into()])
}

#[test]
fn real_subagent_process_round_trips() {
    let result = run_subagent(&config(), &request()).unwrap();
    assert!(result.reusable(), "{:?}", result.status);
    assert_eq!(result.task, "a");
    assert_eq!(result.epoch, 0);
    assert!(result.reference().starts_with("a@0#"));
    assert_eq!(result.tokens, 1);
}

#[test]
fn scripted_failure_is_reported_as_failed() {
    let mut config = config();
    config.args.push("--fail".into());
    let result = run_subagent(&config, &request()).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
}

#[test]
fn silent_exit_without_response_is_never_completed() {
    let mut config = config();
    config.args.push("--silent".into());
    let result = run_subagent(&config, &request()).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Failed { .. }),
        "{:?}",
        result.status
    );
    assert!(!result.reusable());
}

#[test]
fn timeout_kills_the_process_and_returns_aborted() {
    let mut config = config();
    config.args.extend(["--sleep-ms".into(), "2000".into()]);
    let config = config.with_timeout(Duration::from_millis(100));
    let result = run_subagent(&config, &request()).unwrap();
    assert!(
        matches!(result.status, DelegationStatus::Aborted),
        "{:?}",
        result.status
    );
}

#[test]
fn cancel_kills_slow_child_and_returns_aborted() {
    let mut config = config();
    config.args.extend(["--sleep-ms".into(), "2000".into()]);
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let started = Instant::now();
    let worker =
        std::thread::spawn(move || run_subagent_reply_cancellable(&config, &request(), &token));

    std::thread::sleep(Duration::from_millis(100));
    cancel.cancel();
    let reply = worker.join().unwrap().unwrap();
    assert!(
        matches!(reply.result.status, DelegationStatus::Aborted),
        "{:?}",
        reply.result.status
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancel must kill before the 2s sleep: {:?}",
        started.elapsed()
    );
}

#[test]
fn mismatched_response_is_rejected() {
    let mut config = config();
    config.args.extend(["--task".into(), "other".into()]);
    let error = run_subagent(&config, &request()).unwrap_err();
    assert!(
        matches!(error, AgentError::Kernel(_)),
        "expected protocol rejection, got {error:?}"
    );
}
