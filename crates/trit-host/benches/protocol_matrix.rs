//! 多协议适配基准：请求体构造、响应解析、流式组装。
//!
//! 运行：`cargo bench -p trit-host --bench protocol_matrix`
//! 每个用例输出一行 JSON（`case` / `protocol` / `n` / `us`），便于比较与入库。
//!
//! 只测纯函数路径，不碰网络：请求体与响应都是本地构造的 JSON，流式事件直接喂给解码器。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use trit_host::{OpenAiConfig, Protocol, ProviderRequest, SseEvent, ToolSpec};
use trit_orch::{Limits, Observation, Plan, Resources, Snapshot, Ternary, TokenUsage, task_brief};
use trit_reasonix::TurnSpec;

const ROUNDS: usize = 5;
const PROTOCOLS: [Protocol; 4] = [
    Protocol::OpenAiChat,
    Protocol::AnthropicMessages,
    Protocol::GeminiGenerateContent,
    Protocol::DeepSeekChat,
];

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

fn tools(count: usize) -> Vec<ToolSpec> {
    (0..count)
        .map(|index| ToolSpec {
            name: format!("tool_{index}"),
            description: format!("run tool {index}"),
            read_only: index % 2 == 0,
            input_schema: None,
        })
        .collect()
}

fn request(tool_count: usize) -> ProviderRequest {
    ProviderRequest {
        brief: brief(),
        tools: tools(tool_count),
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

fn timed(mut run: impl FnMut()) -> Duration {
    std::hint::black_box(&mut run);
    run();
    let mut best = Duration::MAX;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        run();
        best = best.min(started.elapsed());
    }
    best
}

fn report(case: &str, protocol: &str, n: usize, elapsed: Duration) {
    println!(
        "{}",
        json!({
            "case": case,
            "protocol": protocol,
            "n": n,
            "us": elapsed.as_micros().max(1),
        })
    );
}

/// OpenAI / DeepSeek 形态的响应体：`n` 个工具调用。
fn openai_body(n: usize) -> String {
    let calls: Vec<Value> = (0..n)
        .map(|index| {
            json!({
                "id": format!("call-{index}"),
                "type": "function",
                "function": {
                    "name": "tool_0",
                    "arguments": "{\"command\":\"ls -la\"}"
                }
            })
        })
        .collect();
    json!({
        "choices": [{
            "message": {"content": "working", "tool_calls": calls},
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1_000,
            "completion_tokens": 200,
            "prompt_tokens_details": {"cached_tokens": 600}
        }
    })
    .to_string()
}

/// Anthropic Messages 形态的响应体：`n` 个 tool_use 块。
fn anthropic_body(n: usize) -> String {
    let mut content = vec![json!({"type": "text", "text": "working"})];
    content.extend((0..n).map(|index| {
        json!({
            "type": "tool_use",
            "id": format!("toolu_{index}"),
            "name": "tool_0",
            "input": {"command": "ls -la"}
        })
    }));
    json!({
        "content": content,
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 1_000,
            "output_tokens": 200,
            "cache_read_input_tokens": 600,
            "cache_creation_input_tokens": 400
        }
    })
    .to_string()
}

/// Gemini generateContent 形态的响应体：`n` 个 functionCall。
fn gemini_body(n: usize) -> String {
    let mut parts = vec![json!({"text": "working"})];
    parts.extend(
        (0..n).map(|_| json!({"functionCall": {"name": "tool_0", "args": {"command": "ls -la"}}})),
    );
    json!({
        "candidates": [{
            "content": {"parts": parts},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 1_000,
            "candidatesTokenCount": 200,
            "cachedContentTokenCount": 600
        }
    })
    .to_string()
}

fn body_for(protocol: Protocol, n: usize) -> String {
    match protocol {
        Protocol::OpenAiChat | Protocol::DeepSeekChat => openai_body(n),
        Protocol::AnthropicMessages => anthropic_body(n),
        Protocol::GeminiGenerateContent => gemini_body(n),
    }
}

fn main() {
    // 1) 请求体构造：协议 × 工具数。
    for protocol in PROTOCOLS {
        for tool_count in [1usize, 10, 50] {
            let request = request(tool_count);
            let config = config(protocol);
            let elapsed = timed(|| {
                std::hint::black_box(protocol.build_body(&config, &request));
            });
            report("build_body", protocol.as_str(), tool_count, elapsed);
        }
    }

    // 2) 响应解析：协议 × 工具调用数。
    for protocol in PROTOCOLS {
        for calls in [1usize, 10, 50] {
            let body = body_for(protocol, calls);
            let tool_list = tools(10);
            let elapsed = timed(|| {
                let response = protocol.parse_body(&body, &tool_list).unwrap();
                assert_eq!(response.tool_calls.len(), calls);
            });
            report("parse_body", protocol.as_str(), calls, elapsed);
        }
    }

    // 3) 流式组装：Anthropic 的工具增量与 Gemini 的 functionCall 分块。
    let anthropic_chunks: Vec<String> = std::iter::once(
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}"#.to_owned(),
    )
    .chain(std::iter::once(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"tool_0"}}"#.to_owned(),
    ))
    .chain((0..20).map(|index| {
        let fragment = if index == 0 {
            "{\"comm"
        } else if index == 19 {
            "and\":\"ls\"}"
        } else {
            "x"
        };
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": fragment}
        })
        .to_string()
    }))
    .chain(std::iter::once(
        r#"{"type":"message_stop"}"#.to_owned(),
    ))
    .collect();
    let tool_list = tools(10);
    let elapsed = timed(|| {
        let mut decoder = Protocol::AnthropicMessages.stream_decoder();
        for chunk in &anthropic_chunks {
            std::hint::black_box(decoder.push(&sse(chunk)).unwrap());
        }
        let response = decoder.finish(&tool_list).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
    });
    report(
        "stream_assemble",
        Protocol::AnthropicMessages.as_str(),
        anthropic_chunks.len(),
        elapsed,
    );

    let gemini_chunks: Vec<String> = (0..20)
        .map(|index| {
            json!({
                "candidates": [{
                    "content": {"parts": if index == 0 {
                        vec![json!({"text": "working"})]
                    } else {
                        vec![json!({"functionCall": {"name": "tool_0", "args": {"command": "ls"}}})]
                    }}
                }]
            })
            .to_string()
        })
        .collect();
    let elapsed = timed(|| {
        let mut decoder = Protocol::GeminiGenerateContent.stream_decoder();
        for chunk in &gemini_chunks {
            std::hint::black_box(decoder.push(&sse(chunk)).unwrap());
        }
        let response = decoder.finish(&tool_list).unwrap();
        // Gemini 流式会把同名 functionCall 合并成同一调用，这里只要求至少组装出一个。
        assert!(!response.tool_calls.is_empty());
    });
    report(
        "stream_assemble",
        Protocol::GeminiGenerateContent.as_str(),
        gemini_chunks.len(),
        elapsed,
    );

    // 4) 端点与鉴权头：纯字符串拼接，规模小但调用频繁。
    let elapsed = timed(|| {
        for protocol in PROTOCOLS {
            for index in 0..100 {
                std::hint::black_box(protocol.endpoint(
                    "https://api.example.test/v1",
                    "model-x",
                    index % 2 == 0,
                ));
                std::hint::black_box(protocol.headers("sk-test"));
            }
        }
    });
    report("endpoint_headers", "all", PROTOCOLS.len() * 100, elapsed);

    println!("protocol_matrix: ok");
}
