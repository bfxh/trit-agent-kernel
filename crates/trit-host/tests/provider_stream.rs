use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;

use trit_host::{
    OpenAiConfig, OpenAiProvider, OpenAiStreamAssembler, ProviderRequest, ProviderStreamEvent,
    StreamingProvider, read_sse_events,
};
use trit_orch::{TaskBrief, Ternary, TokenUsage};

const STOP_FIXTURE: &str = include_str!("fixtures/reasonix/streaming/deepseek_stream_stop.sse");
const LENGTH_FIXTURE: &str = include_str!("fixtures/reasonix/streaming/deepseek_stream_length.sse");

fn serve_sse(body: &'static str) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut content_length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse().unwrap();
            } else if let Some(value) = line.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut request_body = vec![0u8; content_length];
        reader.read_exact(&mut request_body).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        stream.flush().unwrap();
    });
    (format!("http://{address}/v1"), handle)
}

fn provider_request() -> ProviderRequest {
    ProviderRequest {
        brief: TaskBrief {
            task: "stream-test".into(),
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
        },
        tools: trit_host::default_tools(),
        receipts: Vec::new(),
        step: 0,
        usage: TokenUsage::default(),
        context_digest: None,
        context: None,
        projection: None,
    }
}

fn assemble(fixture: &str, tools: &[trit_host::ToolSpec]) -> (String, Vec<ProviderStreamEvent>) {
    let mut assembler = OpenAiStreamAssembler::new();
    let mut events = Vec::new();
    read_sse_events(Cursor::new(fixture), |event| {
        events.extend(assembler.push_sse(&event)?);
        Ok(())
    })
    .unwrap();
    let mut text = String::new();
    for event in &events {
        if let ProviderStreamEvent::TextDelta(delta) = event {
            text.push_str(delta);
        }
    }
    let response = assembler.finish(tools).unwrap();
    assert!(response.tool_calls.is_empty());
    assert!(response.prefix.is_none());
    (text, events)
}

#[test]
fn openai_provider_complete_stream_reads_recorded_sse_over_http() {
    let (base_url, server) = serve_sse(STOP_FIXTURE);
    let mut provider = OpenAiProvider::new(OpenAiConfig::new(base_url, "sk-test", "model-x"));
    let mut events = Vec::new();
    let response = provider
        .complete_stream(&provider_request(), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    server.join().unwrap();

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello there, how are you?");
    assert_eq!(response.usage.prompt_tokens, 10);
    assert_eq!(response.usage.completion_tokens, 7);
    assert_eq!(response.usage.cache_miss_tokens, 10);
    assert!(!response.usage.estimated);
}

#[test]
fn recorded_deepseek_stop_stream_roundtrips() {
    let (text, events) = assemble(STOP_FIXTURE, &[]);
    assert_eq!(text, "Hello there, how are you?");
    assert!(events.iter().any(|event| matches!(
        event,
        ProviderStreamEvent::Finish { reason: Some(reason) } if reason == "stop"
    )));
    assert!(events.contains(&ProviderStreamEvent::Done));
    let usage = events
        .iter()
        .find_map(|event| match event {
            ProviderStreamEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .expect("recorded final usage");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage.cache_hit_tokens, 0);
    assert_eq!(usage.cache_miss_tokens, 10);
    assert!(!usage.estimated);
}

#[test]
fn recorded_deepseek_length_stream_keeps_truncation_reason() {
    let (text, events) = assemble(LENGTH_FIXTURE, &[]);
    assert_eq!(text, "1, ");
    assert!(events.iter().any(|event| matches!(
        event,
        ProviderStreamEvent::Finish { reason: Some(reason) } if reason == "length"
    )));
}

#[test]
fn tool_call_delta_assembles_fragmented_arguments() {
    let fixture = r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"cargo "}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"test\"}"}}]}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":10}}}

data: [DONE]

"#;
    let mut assembler = OpenAiStreamAssembler::new();
    let mut events = Vec::new();
    read_sse_events(Cursor::new(fixture), |event| {
        events.extend(assembler.push_sse(&event)?);
        Ok(())
    })
    .unwrap();
    let response = assembler.finish(&trit_host::default_tools()).unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].call_id, "call_1");
    assert_eq!(response.tool_calls[0].tool, "bash");
    assert_eq!(
        response.tool_calls[0].command.as_deref(),
        Some("cargo test")
    );
    assert_eq!(response.usage.cache_hit_tokens, 10);
    assert_eq!(response.usage.cache_miss_tokens, 10);
    assert!(!response.usage.estimated);
    assert!(events.iter().any(|event| matches!(
        event,
        ProviderStreamEvent::ToolCallDelta {
            index: 0,
            id: Some(id),
            name: Some(name),
            ..
        } if id == "call_1" && name == "bash"
    )));
}

#[test]
fn stream_without_finish_or_done_fails_closed() {
    let fixture = r#"data: {"choices":[{"delta":{"content":"partial"}}]}

"#;
    let mut assembler = OpenAiStreamAssembler::new();
    read_sse_events(Cursor::new(fixture), |event| {
        assembler.push_sse(&event)?;
        Ok(())
    })
    .unwrap();
    assert!(assembler.finish(&[]).is_err());
}
