use trit_agent::{
    REASONIX_WIRE_VERSION, WireContractError, WireEvent, WireFixture, WireTool, WireUsage,
    parse_fixture,
};

const USAGE_FIXTURE: &str = include_str!("fixtures/reasonix/usage.json");
const EVENT_FIXTURE: &str = include_str!("fixtures/reasonix/event.json");
const TOOL_FIXTURE: &str = include_str!("fixtures/reasonix/tool.json");

#[test]
fn usage_fixture_roundtrips_and_maps_cache_signal() {
    let fixture: WireFixture<WireUsage> = parse_fixture(USAGE_FIXTURE).unwrap();
    assert_eq!(fixture.version, REASONIX_WIRE_VERSION);
    assert!(fixture.source.contains("DeepSeek-Reasonix-rs@5edbda1"));

    let host = fixture.payload.to_host_usage().unwrap();
    assert_eq!(host.prompt_tokens, 1_200);
    assert_eq!(host.completion_tokens, 300);
    assert_eq!(host.cache_hit_tokens, 900);
    assert_eq!(host.cache_miss_tokens, 300);
    assert!(!host.estimated);

    let token = fixture.payload.to_token_usage().unwrap();
    assert_eq!(token.fresh, 300);
    assert_eq!(token.cached, 900);
    assert_eq!(token.output, 300);
    let signal = fixture.payload.prefix_signal().unwrap();
    assert!(signal.changed);
    assert_eq!(signal.reasons, vec!["compaction".to_owned()]);

    let encoded = serde_json::to_string(&fixture).unwrap();
    let decoded: WireFixture<WireUsage> = parse_fixture(&encoded).unwrap();
    assert_eq!(decoded, fixture);
}

#[test]
fn event_fixture_roundtrips_tool_usage_and_unknown_fields() {
    let fixture: WireFixture<WireEvent> = parse_fixture(EVENT_FIXTURE).unwrap();
    fixture.payload.validate().unwrap();
    assert_eq!(fixture.payload.kind, "tool_result");
    assert!(fixture.payload.is_known_kind());
    assert_eq!(fixture.payload.seq, 42);
    assert_eq!(fixture.payload.extra["retryAttempt"], 1);
    assert_eq!(fixture.payload.extra["nonPersistable"], true);

    let request = fixture
        .payload
        .tool
        .as_ref()
        .unwrap()
        .to_tool_request()
        .unwrap();
    assert_eq!(request.call_id, "call-1");
    assert_eq!(request.tool, "read_file");
    assert!(request.read_only);
    assert_eq!(request.arguments.unwrap()["path"], "src/lib.rs");

    let token = fixture.payload.token_usage().unwrap().unwrap();
    assert_eq!(token.fresh, 300);
    assert_eq!(token.cached, 900);

    let encoded = serde_json::to_string(&fixture).unwrap();
    let decoded: WireFixture<WireEvent> = parse_fixture(&encoded).unwrap();
    assert_eq!(decoded, fixture);
}

#[test]
fn tool_fixture_maps_resolved_name_and_arguments() {
    let fixture: WireFixture<WireTool> = parse_fixture(TOOL_FIXTURE).unwrap();
    let tool = &fixture.payload;
    tool.validate().unwrap();
    assert_eq!(tool.id, "call-2");
    assert_eq!(tool.capability_id, "shell.exec");
    assert_eq!(tool.parent_id, "turn-1");
    assert!(tool.passed());
    assert!(!tool.in_doubt());

    let request = tool.to_tool_request().unwrap();
    assert_eq!(request.tool, "bash");
    assert!(!request.read_only);
    assert_eq!(request.arguments.unwrap()["command"], "cargo test");
}

#[test]
fn unsupported_fixture_version_fails_closed() {
    let json = USAGE_FIXTURE.replace("\"version\": 1", "\"version\": 99");
    assert!(matches!(
        parse_fixture::<WireUsage>(&json),
        Err(WireContractError::UnsupportedVersion {
            found: 99,
            expected: REASONIX_WIRE_VERSION
        })
    ));
}

#[test]
fn malformed_fixture_json_is_a_typed_error() {
    assert!(matches!(
        parse_fixture::<WireUsage>("not json"),
        Err(WireContractError::InvalidJson { .. })
    ));
}
