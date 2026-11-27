//! 校验次数统计与训练信号：去重计数、请求体注入、重复校验提示。

use trit_host::settings::Settings;
use trit_host::{
    AgentHost, OpenAiConfig, Proposal, Protocol, ProviderRequest, ScriptedModel, ScriptedTools,
    ToolOutcome, ToolRequest, TurnContext, VerificationSignal,
};
use trit_orch::{TaskBrief, Ternary, TokenUsage};
use trit_reasonix::{ClaimRequest, CriterionKind, CriterionRequest, HostUsage, TurnSpec};

fn outcome(call_id: &str, command: Option<&str>) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.into(),
        tool: "bash".into(),
        command: command.map(str::to_owned),
        passed: true,
        assertions: 1,
        written_paths: Vec::new(),
        in_doubt: false,
        output: String::new(),
    }
}

fn turn() -> TurnSpec {
    TurnSpec {
        task: "verify".into(),
        objective: "run checks".into(),
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 1,
        max_tokens: 10_000,
    }
}

fn brief() -> TaskBrief {
    TaskBrief {
        task: "verify".into(),
        epoch: 0,
        guidance: Default::default(),
        resolves: Vec::new(),
        produces: Default::default(),
        effects: Vec::new(),
        requirements: Default::default(),
        max_tokens: 0,
        admission: Ternary::True,
        granted_tokens: 0,
        blockers: Vec::new(),
        obligations: Vec::new(),
    }
}

fn request(verification: Option<VerificationSignal>) -> ProviderRequest {
    ProviderRequest {
        brief: brief(),
        tools: Vec::new(),
        receipts: Vec::new(),
        step: 0,
        usage: TokenUsage::default(),
        context_digest: None,
        context: None,
        projection: None,
        verification,
    }
}

#[test]
fn verification_signal_counts_and_dedups_commands() {
    let receipts = vec![
        outcome("1", Some("cargo test")),
        outcome("2", Some("cargo test")),
        outcome("3", Some("cargo clippy")),
        outcome("4", None),
    ];
    let signal = VerificationSignal::from_receipts(&receipts).expect("signal");
    assert_eq!(signal.calls, 3);
    assert_eq!(signal.unique, 2);
    assert_eq!(signal.repeated, 1);
    assert_eq!(
        signal.commands,
        vec!["cargo test".to_owned(), "cargo clippy".to_owned()]
    );
    assert!(
        VerificationSignal::from_receipts(&[outcome("1", None)]).is_none(),
        "没有命令时不注入空信号"
    );
}

#[test]
fn request_body_carries_verification_and_repeat_hint() {
    let config = OpenAiConfig::new("https://example.test/v1", "k", "m");
    let signal = VerificationSignal {
        calls: 3,
        unique: 2,
        repeated: 1,
        commands: vec!["cargo test".into(), "cargo clippy".into()],
    };
    let body = Protocol::OpenAiChat.build_body(&config, &request(Some(signal)));
    let user: serde_json::Value =
        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["verification"]["calls"], 3);
    assert_eq!(user["verification"]["unique"], 2);
    assert_eq!(user["verification"]["repeated"], 1);
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("Do not repeat a verification command"),
        "重复校验时必须给出提示: {system}"
    );

    // 没有信号：字段与提示都不出现。
    let body = Protocol::OpenAiChat.build_body(&config, &request(None));
    let user: serde_json::Value =
        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(user.get("verification").is_none());
    assert!(
        !body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Do not repeat")
    );
}

#[test]
fn turn_metrics_record_repeated_verifications() {
    let model = ScriptedModel::new(vec![
        Proposal {
            tool_calls: vec![
                ToolRequest::verification("c1", "cargo test"),
                ToolRequest::verification("c2", "cargo test"),
            ],
            completion: None,
            usage: HostUsage::default(),
            ..Proposal::default()
        },
        Proposal {
            tool_calls: Vec::new(),
            completion: Some(ClaimRequest {
                criteria: vec![CriterionRequest {
                    id: "c1".into(),
                    kind: CriterionKind::Verification {
                        command: "cargo test".into(),
                    },
                }],
            }),
            usage: HostUsage::default(),
            ..Proposal::default()
        },
    ]);
    let tools = ScriptedTools::new(vec![
        outcome("c1", Some("cargo test")),
        outcome("c2", Some("cargo test")),
    ]);
    let mut host = AgentHost::new(model, tools);
    let outcome = host.run_turn(&turn()).unwrap();
    assert_eq!(outcome.metrics.verification_calls, 2);
    assert_eq!(outcome.metrics.unique_verifications, 1);
    assert_eq!(outcome.metrics.repeated_verifications, 1);
}

#[test]
fn recording_switch_defaults_to_enabled() {
    let settings = Settings::default();
    assert!(
        settings.recording.verification_signals,
        "默认记录并注入校验信号"
    );
}

#[test]
fn turn_context_is_reachable_for_signal_construction() {
    // 防止 `TurnContext` 导入被误删：信号就是从这里给出的回执算出来的。
    let context = TurnContext {
        task: "verify".into(),
        step: 0,
        usage: TokenUsage::default(),
        receipts: vec![outcome("1", Some("cargo test"))],
        digest: None,
        projection: None,
        history: Vec::new(),
    };
    assert!(VerificationSignal::from_receipts(&context.receipts).is_some());
}
