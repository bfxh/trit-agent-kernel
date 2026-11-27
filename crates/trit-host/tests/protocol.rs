//! 协议适配的契约测试：三种协议的端点/认证头/请求体/响应解析/流式组装。
//!
//! 不依赖真实网络：请求体与响应都是 fixture JSON，流式事件直接喂给解码器。

use std::collections::BTreeMap;

use trit_host::protocol::{ANTHROPIC_VERSION, DEFAULT_ANTHROPIC_MAX_TOKENS};
use trit_host::{OpenAiConfig, Protocol, ProviderRequest, ProviderStreamEvent, SseEvent, ToolSpec};
use trit_orch::{
    Limits, Observation, Plan, Resources, Snapshot, TaskBrief, Ternary, TokenUsage, task_brief,
};
use trit_reasonix::TurnSpec;

fn brief() -> TaskBrief {
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

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "run it".into(),
        read_only: false,
        input_schema: None,
    }
}

fn request() -> ProviderRequest {
    ProviderRequest {
        brief: brief(),
        tools: vec![tool("bash"), tool("read_file")],
        receipts: Vec::new(),
        step: 0,
        usage: TokenUsage::default(),
        context_digest: None,
        context: None,
        projection: None,
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

#[test]
fn endpoint_and_headers_per_protocol() {
    assert_eq!(
        Protocol::OpenAiChat.endpoint("https://api.openai.com/v1", "gpt-4o", false),
        "https://api.openai.com/v1/chat/completions"
    );
    let headers = Protocol::OpenAiChat.headers("sk-a");
    assert_eq!(headers[0].0, "Authorization");
    assert!(headers[0].1.contains("sk-a"));

    assert_eq!(
        Protocol::AnthropicMessages.endpoint("https://api.anthropic.com", "claude-x", false),
        "https://api.anthropic.com/v1/messages"
    );
    // base 已带 /v1 时不重复拼接。
    assert_eq!(
        Protocol::AnthropicMessages.endpoint("https://api.anthropic.com/v1", "claude-x", false),
        "https://api.anthropic.com/v1/messages"
    );
    let headers = Protocol::AnthropicMessages.headers("sk-ant");
    assert!(headers.iter().any(|(name, _)| *name == "x-api-key"));
    assert!(
        headers
            .iter()
            .any(|(name, value)| *name == "anthropic-version" && value == ANTHROPIC_VERSION)
    );
    assert!(!headers.iter().any(|(name, _)| *name == "Authorization"));

    assert_eq!(
        Protocol::GeminiGenerateContent.endpoint(
            "https://generativelanguage.googleapis.com",
            "gemini-x",
            false
        ),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:generateContent"
    );
    assert_eq!(
        Protocol::GeminiGenerateContent.endpoint(
            "https://generativelanguage.googleapis.com",
            "gemini-x",
            true
        ),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        Protocol::GeminiGenerateContent.headers("g-key")[0].0,
        "x-goog-api-key"
    );
}

#[test]
fn protocol_names_parse_and_unknown_fails_closed() {
    assert_eq!(Protocol::parse("openai").unwrap(), Protocol::OpenAiChat);
    assert_eq!(
        Protocol::parse("openai-chat").unwrap(),
        Protocol::OpenAiChat
    );
    assert_eq!(
        Protocol::parse("anthropic-messages").unwrap(),
        Protocol::AnthropicMessages
    );
    assert_eq!(
        Protocol::parse("claude").unwrap(),
        Protocol::AnthropicMessages
    );
    assert_eq!(
        Protocol::parse("gemini").unwrap(),
        Protocol::GeminiGenerateContent
    );
    assert_eq!(Protocol::parse("").unwrap(), Protocol::OpenAiChat);

    let error = Protocol::parse("bedrock").unwrap_err();
    let message = error.to_string();
    assert!(message.contains("unknown protocol"), "{message}");
    assert!(message.contains("anthropic-messages"), "{message}");
    assert_eq!(Protocol::ALL.len(), 3);
}

#[test]
fn base_url_inference_picks_known_hosts() {
    assert_eq!(
        Protocol::from_base_url("https://api.anthropic.com"),
        Protocol::AnthropicMessages
    );
    assert_eq!(
        Protocol::from_base_url("https://generativelanguage.googleapis.com"),
        Protocol::GeminiGenerateContent
    );
    assert_eq!(
        Protocol::from_base_url("https://api.openai.com/v1"),
        Protocol::OpenAiChat
    );
    assert_eq!(
        Protocol::from_base_url("https://my-gateway.internal/v1"),
        Protocol::OpenAiChat
    );
}

#[test]
fn anthropic_body_carries_system_max_tokens_and_tools() {
    let request = request();
    let body =
        Protocol::AnthropicMessages.build_body(&config(Protocol::AnthropicMessages), &request);

    assert_eq!(body["model"], "model-x");
    assert_eq!(body["max_tokens"], DEFAULT_ANTHROPIC_MAX_TOKENS);
    let system = body["system"].as_str().expect("system 是顶层字符串");
    assert!(!system.is_empty());
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    let user = body["messages"][0]["content"][0]["text"]
        .as_str()
        .expect("user 文本");
    assert!(user.contains("Continue the task"), "{user}");

    let tools = body["tools"].as_array().expect("tools 数组");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "bash");
    assert_eq!(tools[0]["input_schema"]["type"], "object");
    assert!(
        tools[0].get("type").is_none(),
        "Anthropic 不用 function 包装"
    );
    assert_eq!(body["tool_choice"]["type"], "auto");
}

#[test]
fn gemini_body_carries_system_instruction_and_declarations() {
    let request = request();
    let body = Protocol::GeminiGenerateContent
        .build_body(&config(Protocol::GeminiGenerateContent), &request);

    let system = body["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .expect("systemInstruction");
    assert!(!system.is_empty());
    assert_eq!(body["contents"][0]["role"], "user");
    let user = body["contents"][0]["parts"][0]["text"]
        .as_str()
        .expect("user text");
    assert!(user.contains("Continue the task"), "{user}");

    let declarations = body["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("functionDeclarations");
    assert_eq!(declarations.len(), 2);
    assert_eq!(declarations[0]["name"], "bash");
    assert!(declarations[0]["parameters"]["type"].as_str().is_some());
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
}

#[test]
fn anthropic_parse_maps_tool_use_and_usage() {
    let tools = vec![tool("bash")];
    let body = r#"{
        "content": [
            {"type": "text", "text": "working"},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5,
                  "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2}
    }"#;
    let response = Protocol::AnthropicMessages
        .parse_body(body, &tools)
        .unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool, "bash");
    assert_eq!(response.tool_calls[0].command.as_deref(), Some("ls"));
    assert_eq!(response.usage.prompt_tokens, 15);
    assert_eq!(response.usage.cache_hit_tokens, 3);
    assert_eq!(response.usage.cache_miss_tokens, 12);
    assert_eq!(response.usage.completion_tokens, 5);
    assert!(!response.usage.estimated);
}

#[test]
fn anthropic_parse_maps_completion_claim() {
    let tools = vec![tool("bash")];
    let body = r#"{
        "content": [{"type": "tool_use", "id": "toolu_2", "name": "complete_subtask",
                     "input": {"criteria": [{"id": "c1", "kind": "diff", "paths": ["a.txt"]}]}}],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }"#;
    let response = Protocol::AnthropicMessages
        .parse_body(body, &tools)
        .unwrap();
    assert!(response.tool_calls.is_empty());
    let claim = response.completion.expect("completion claim");
    assert_eq!(claim.criteria.len(), 1);
    assert_eq!(claim.criteria[0].id, "c1");
}

#[test]
fn anthropic_parse_maps_error() {
    let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
    let error = Protocol::AnthropicMessages
        .parse_body(body, &[])
        .unwrap_err();
    assert!(error.to_string().contains("overloaded"), "{error}");
}

#[test]
fn gemini_parse_maps_function_call_and_usage() {
    let tools = vec![tool("bash")];
    let body = r#"{
        "candidates": [{"content": {"parts": [
            {"text": "ok"},
            {"functionCall": {"name": "bash", "args": {"command": "pwd"}}}
        ]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 7,
                          "cachedContentTokenCount": 5}
    }"#;
    let response = Protocol::GeminiGenerateContent
        .parse_body(body, &tools)
        .unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].command.as_deref(), Some("pwd"));
    assert_eq!(response.usage.prompt_tokens, 20);
    assert_eq!(response.usage.cache_hit_tokens, 5);
    assert_eq!(response.usage.cache_miss_tokens, 15);
    assert_eq!(response.usage.completion_tokens, 7);
}

#[test]
fn anthropic_stream_assembles_tool_use_and_usage() {
    let tools = vec![tool("bash")];
    let mut decoder = Protocol::AnthropicMessages.stream_decoder();
    let chunks = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_9","name":"bash"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"and\":\"ls\"}"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(decoder.push(&sse(chunk)).unwrap());
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderStreamEvent::ToolCallDelta { .. })),
        "必须转出工具增量"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderStreamEvent::Done))
    );
    let response = decoder.finish(&tools).unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool, "bash");
    assert_eq!(response.tool_calls[0].command.as_deref(), Some("ls"));
    assert_eq!(response.usage.completion_tokens, 4);
}

#[test]
fn anthropic_stream_maps_error_event() {
    let mut decoder = Protocol::AnthropicMessages.stream_decoder();
    let error = decoder
        .push(&sse(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("stream error"), "{error}");
}

#[test]
fn gemini_stream_assembles_function_call() {
    let tools = vec![tool("bash")];
    let mut decoder = Protocol::GeminiGenerateContent.stream_decoder();
    let chunks = [
        r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"bash","args":{"command":"ls"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2}}"#,
    ];
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(decoder.push(&sse(chunk)).unwrap());
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderStreamEvent::TextDelta(text) if text == "hi"))
    );
    let response = decoder.finish(&tools).unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].command.as_deref(), Some("ls"));
    assert_eq!(response.usage.prompt_tokens, 4);
}
