//! 协议适配的失败面契约：坏 JSON、缺字段、未知工具、空流、无用量。
//!
//! 与 `protocol` 测试互补：那边钉住"正常路径产出什么"，这边钉住"出错时
//! 报什么错、不回退成静默成功"。四种协议（OpenAI Chat / DeepSeek / Anthropic /
//! Gemini）各有独立的解析器，任何一处静默吞错都会让宿主把坏响应当成完成。

use std::collections::BTreeMap;

use trit_host::protocol::DEFAULT_ANTHROPIC_MAX_TOKENS;
use trit_host::{OpenAiConfig, Protocol, ProviderRequest, SseEvent, ToolSpec};
use trit_orch::{Limits, Observation, Plan, Resources, Snapshot, Ternary, TokenUsage, task_brief};
use trit_reasonix::TurnSpec;

fn brief() -> trit_orch::TaskBrief {
    let spec = TurnSpec {
        task: "turn-1".into(),
        objective: "fix bug".into(),
        constraints: Vec::new(),
        stop_when: Vec::new(),
        min_assertions: 1,
        max_tokens: 1_000,
    }
    .task_spec();
    let plan = Plan::new(1, vec![spec]).unwrap();
    let snapshot = Snapshot {
        revision: 1,
        observations: BTreeMap::from([(
            "turn-1".to_owned(),
            Observation {
                authorization: Ternary::True,
                freshness: Ternary::True,
                ..Observation::default()
            },
        )]),
    };
    let limits = Limits {
        workers: 1,
        capacity: Resources::new(1, 1),
        tokens: 1_000,
    };
    let selection = plan.select(&snapshot, &limits).unwrap();
    task_brief(&plan, &selection, "turn-1").unwrap()
}

fn request() -> ProviderRequest {
    ProviderRequest {
        brief: brief(),
        tools: vec![
            ToolSpec {
                name: "bash".into(),
                description: "run it".into(),
                read_only: false,
                input_schema: None,
            },
            ToolSpec {
                name: "mcp_search".into(),
                description: "动态工具".into(),
                read_only: true,
                input_schema: None,
            },
        ],
        receipts: Vec::new(),
        step: 0,
        usage: TokenUsage::default(),
        context_digest: None,
        context: None,
        projection: None,
        verification: None,
    }
}

fn config(protocol: Protocol) -> OpenAiConfig {
    OpenAiConfig::new("https://api.example.test", "sk-test", "model-x").with_protocol(protocol)
}

fn sse(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        id: None,
        data: data.into(),
    }
}

const ALL: [Protocol; 4] = [
    Protocol::OpenAiChat,
    Protocol::AnthropicMessages,
    Protocol::GeminiGenerateContent,
    Protocol::DeepSeekChat,
];

#[test]
fn bad_json_fails_closed_for_every_protocol() {
    for protocol in ALL {
        let error = protocol
            .parse_body("not json at all {", &[])
            .expect_err("坏 JSON 必须报错");
        let message = error.to_string();
        assert!(
            message.contains("bad json"),
            "{}: {message}",
            protocol.as_str()
        );
    }
}

#[test]
fn provider_error_bodies_surface_as_errors() {
    for protocol in ALL {
        let error = protocol
            .parse_body(
                r#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
                &[],
            )
            .expect_err("error body 必须报错");
        let message = error.to_string();
        assert!(
            message.contains("provider error"),
            "{}: {message}",
            protocol.as_str()
        );
        assert!(message.contains("busy"), "{}: {message}", protocol.as_str());
    }
}

#[test]
fn missing_required_envelope_fields_are_errors() {
    // OpenAI 系：缺 choices[0].message。
    for protocol in [Protocol::OpenAiChat, Protocol::DeepSeekChat] {
        let error = protocol
            .parse_body("{}", &[])
            .expect_err("缺 choices 必须报错");
        assert!(
            error.to_string().contains("missing choices[0].message"),
            "{}: {error}",
            protocol.as_str()
        );
    }

    // Anthropic / Gemini 缺内容数组时是"空响应"，而不是错误：模型可能只回文本。
    for protocol in [Protocol::AnthropicMessages, Protocol::GeminiGenerateContent] {
        let response = protocol
            .parse_body("{}", &[])
            .unwrap_or_else(|error| panic!("{}: {error}", protocol.as_str()));
        assert!(response.tool_calls.is_empty());
        assert!(response.completion.is_none());
    }
}

#[test]
fn tool_use_without_name_is_an_explicit_error() {
    let error = Protocol::AnthropicMessages
        .parse_body(
            r#"{"content":[{"type":"tool_use","id":"toolu_1","input":{}}]}"#,
            &[],
        )
        .expect_err("tool_use 缺 name 必须报错");
    assert!(
        error
            .to_string()
            .contains("anthropic tool_use missing name"),
        "{error}"
    );
}

#[test]
fn gemini_function_call_without_name_is_an_explicit_error() {
    let error = Protocol::GeminiGenerateContent
        .parse_body(
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":{}}}]}}]}"#,
            &[],
        )
        .expect_err("functionCall 缺 name 必须报错");
    assert!(
        error
            .to_string()
            .contains("gemini functionCall missing name"),
        "{error}"
    );
}

#[test]
fn tool_call_without_function_or_name_is_an_explicit_error() {
    // OpenAI 系：tool_calls[0] 缺 function。
    let error = Protocol::OpenAiChat
        .parse_body(
            r#"{"choices":[{"message":{"tool_calls":[{"id":"c1"}]}}]}"#,
            &[],
        )
        .expect_err("缺 function 必须报错");
    assert!(
        error.to_string().contains("tool call missing function"),
        "{error}"
    );

    // 有 function 但缺 name。
    let error = Protocol::OpenAiChat
        .parse_body(
            r#"{"choices":[{"message":{"tool_calls":[{"id":"c1","function":{}}]}}]}"#,
            &[],
        )
        .expect_err("缺 name 必须报错");
    assert!(
        error.to_string().contains("tool call missing name"),
        "{error}"
    );
}

#[test]
fn unknown_tool_is_rejected_not_silently_dropped() {
    let body = r#"{"choices":[{"message":{"tool_calls":[
        {"id":"c1","function":{"name":"rm_rf","arguments":"{}"}}
    ]}}]}"#;
    let error = Protocol::OpenAiChat
        .parse_body(body, &request().tools)
        .expect_err("未知工具必须报错");
    assert!(error.to_string().contains("unknown tool: rm_rf"), "{error}");
}

#[test]
fn declared_dynamic_tool_keeps_raw_arguments() {
    let body = r#"{"choices":[{"message":{"tool_calls":[
        {"id":"c1","function":{"name":"mcp_search","arguments":"{\"q\":\"rust\"}"}}
    ]}}]}"#;
    let response = Protocol::OpenAiChat
        .parse_body(body, &request().tools)
        .unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    let call = &response.tool_calls[0];
    assert_eq!(call.tool, "mcp_search");
    assert!(call.read_only, "read_only 应取自工具声明");
    assert_eq!(
        call.arguments.as_ref().and_then(|value| value.get("q")),
        Some(&serde_json::json!("rust"))
    );
}

#[test]
fn context_read_span_requires_both_bounds() {
    let half = r#"{"choices":[{"message":{"tool_calls":[
        {"id":"c1","function":{"name":"context_read","arguments":"{\"handle\":\"C1\",\"offset\":2}"}}
    ]}}]}"#;
    let error = Protocol::OpenAiChat
        .parse_body(half, &request().tools)
        .expect_err("只有 offset 必须报错");
    assert!(
        error
            .to_string()
            .contains("context_read span needs both offset and length"),
        "{error}"
    );

    let full = r#"{"choices":[{"message":{"tool_calls":[
        {"id":"c1","function":{"name":"context_read","arguments":"{\"handle\":\"C1\",\"offset\":2,\"length\":3}"}}
    ]}}]}"#;
    let response = Protocol::OpenAiChat
        .parse_body(full, &request().tools)
        .unwrap();
    assert_eq!(response.tool_calls[0].span.unwrap().offset, 2);
}

#[test]
fn missing_usage_is_marked_estimated_per_protocol() {
    for protocol in ALL {
        let body = match protocol {
            Protocol::OpenAiChat | Protocol::DeepSeekChat => {
                r#"{"choices":[{"message":{"content":"done"}}]}"#
            }
            Protocol::AnthropicMessages => r#"{"content":[{"type":"text","text":"done"}]}"#,
            Protocol::GeminiGenerateContent => {
                r#"{"candidates":[{"content":{"parts":[{"text":"done"}]}}]}"#
            }
        };
        let response = protocol.parse_body(body, &[]).unwrap();
        assert!(
            response.usage.estimated,
            "{}: 缺 usage 必须标 estimated",
            protocol.as_str()
        );
        assert_eq!(response.usage.prompt_tokens, 0);
        assert_eq!(response.usage.completion_tokens, 0);
    }
}

#[test]
fn anthropic_cache_accounting_splits_read_and_creation() {
    let body = r#"{"content":[],"usage":{
        "input_tokens":10,"output_tokens":5,
        "cache_read_input_tokens":3,"cache_creation_input_tokens":2
    }}"#;
    let usage = Protocol::AnthropicMessages
        .parse_body(body, &[])
        .unwrap()
        .usage;
    assert_eq!(usage.prompt_tokens, 15, "缓存读 + 缓存写 + 新输入");
    assert_eq!(usage.cache_hit_tokens, 3);
    assert_eq!(usage.cache_miss_tokens, 12);
    assert_eq!(usage.completion_tokens, 5);
    assert!(!usage.estimated);

    // 只给缓存字段、不给 input_tokens：仍然算"有精确用量"，不能标 estimated。
    let body = r#"{"content":[],"usage":{"cache_read_input_tokens":4}}"#;
    let usage = Protocol::AnthropicMessages
        .parse_body(body, &[])
        .unwrap()
        .usage;
    assert!(!usage.estimated);
    assert_eq!(usage.cache_hit_tokens, 4);
    assert_eq!(usage.prompt_tokens, 4);
}

#[test]
fn gemini_cache_hit_is_subtracted_from_prompt() {
    let body = r#"{"candidates":[],"usageMetadata":{
        "promptTokenCount":20,"candidatesTokenCount":7,"cachedContentTokenCount":5
    }}"#;
    let usage = Protocol::GeminiGenerateContent
        .parse_body(body, &[])
        .unwrap()
        .usage;
    assert_eq!(usage.prompt_tokens, 20);
    assert_eq!(usage.cache_hit_tokens, 5);
    assert_eq!(usage.cache_miss_tokens, 15);
    assert_eq!(usage.completion_tokens, 7);
    assert!(!usage.estimated);

    // 缓存数大于 prompt 时不能下溢。
    let body = r#"{"candidates":[],"usageMetadata":{
        "promptTokenCount":3,"cachedContentTokenCount":10
    }}"#;
    let usage = Protocol::GeminiGenerateContent
        .parse_body(body, &[])
        .unwrap()
        .usage;
    assert_eq!(usage.cache_miss_tokens, 0);
}

#[test]
fn empty_streams_fail_closed() {
    let error = Protocol::AnthropicMessages
        .stream_decoder()
        .finish(&[])
        .expect_err("Anthropic 空流必须报错");
    assert!(
        error.to_string().contains("ended without any content"),
        "{error}"
    );

    let error = Protocol::GeminiGenerateContent
        .stream_decoder()
        .finish(&[])
        .expect_err("Gemini 空流必须报错");
    assert!(
        error.to_string().contains("ended without any content"),
        "{error}"
    );
}

#[test]
fn unknown_stream_events_are_ignored_not_errored() {
    let mut decoder = Protocol::AnthropicMessages.stream_decoder();
    let events = decoder
        .push(&sse(r#"{"type":"content_block_stop","index":0}"#))
        .unwrap();
    assert!(events.is_empty());

    let mut decoder = Protocol::GeminiGenerateContent.stream_decoder();
    let events = decoder.push(&sse(r#"{"candidates":[]}"#)).unwrap();
    assert!(events.is_empty());
}

#[test]
fn done_sentinel_finishes_gemini_stream_cleanly() {
    let mut decoder = Protocol::GeminiGenerateContent.stream_decoder();
    let events = decoder.push(&sse("[DONE]")).unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, trit_host::ProviderStreamEvent::Done))
    );
    let response = decoder.finish(&[]).unwrap();
    assert!(response.tool_calls.is_empty());
    assert!(response.usage.estimated);
}

#[test]
fn anthropic_tool_use_without_input_fails_closed_for_builtin_tools() {
    // 内置工具的参数是强类型的：`bash` 缺 command 直接报错，不能猜一个空命令去跑。
    let mut decoder = Protocol::AnthropicMessages.stream_decoder();
    decoder
        .push(&sse(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}"#,
        ))
        .unwrap();
    decoder.push(&sse(r#"{"type":"message_stop"}"#)).unwrap();
    let error = decoder
        .finish(&request().tools)
        .expect_err("bash 缺 command 必须报错");
    assert!(
        error
            .to_string()
            .contains("missing string argument: command"),
        "{error}"
    );

    // 动态工具（MCP）没有强类型 schema，空 input 就是空参数对象。
    let mut decoder = Protocol::AnthropicMessages.stream_decoder();
    decoder
        .push(&sse(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_2","name":"mcp_search"}}"#,
        ))
        .unwrap();
    decoder.push(&sse(r#"{"type":"message_stop"}"#)).unwrap();
    let response = decoder.finish(&request().tools).unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(
        response.tool_calls[0].arguments.as_ref(),
        Some(&serde_json::json!({}))
    );
}

#[test]
fn protocol_names_are_case_and_separator_insensitive() {
    for (input, expected) in [
        ("ANTHROPIC_MESSAGES", Protocol::AnthropicMessages),
        ("  Gemini-", Protocol::OpenAiChat), // 带空格与尾横线：非法，回退失败
        ("deepseek_reasoner", Protocol::DeepSeekChat),
        ("Chat_Completions", Protocol::OpenAiChat),
        ("generate-content", Protocol::GeminiGenerateContent),
    ] {
        match Protocol::parse(input) {
            Ok(protocol) => assert_eq!(protocol, expected, "{input}"),
            Err(error) => {
                // 唯一允许失败的是 "  Gemini- "：归一化后不是任何别名。
                assert!(input.contains("Gemini"));
                assert!(error.to_string().contains("unknown protocol"));
            }
        }
    }
}

#[test]
fn anthropic_max_tokens_falls_back_to_default() {
    let body =
        Protocol::AnthropicMessages.build_body(&config(Protocol::AnthropicMessages), &request());
    assert_eq!(body["max_tokens"], DEFAULT_ANTHROPIC_MAX_TOKENS);

    let mut config = config(Protocol::AnthropicMessages);
    config.max_output_tokens = Some(1234);
    let body = Protocol::AnthropicMessages.build_body(&config, &request());
    assert_eq!(body["max_tokens"], 1234);
}

#[test]
fn empty_tool_list_keeps_protocol_shape() {
    let mut request = request();
    request.tools.clear();

    let anthropic =
        Protocol::AnthropicMessages.build_body(&config(Protocol::AnthropicMessages), &request);
    assert_eq!(anthropic["tools"], serde_json::json!([]));

    let gemini = Protocol::GeminiGenerateContent
        .build_body(&config(Protocol::GeminiGenerateContent), &request);
    assert_eq!(
        gemini["tools"][0]["functionDeclarations"],
        serde_json::json!([])
    );
}
