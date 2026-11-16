//! MCP sampling/createMessage 到 OpenAI-compatible chat completion 的映射。
//!
//! 这是内置模型调用：MCP server 请求采样，内核直接向配置的 provider 发请求，
//! 把 assistant message 回给 server。没有 handler 时才退化为 -32601。

use std::sync::Mutex;

use serde_json::{Value, json};

use crate::openai::{OpenAiConfig, OpenAiProvider};
use crate::supervisor::SamplingHandler;

pub struct OpenAiSamplingHandler {
    provider: Mutex<OpenAiProvider>,
    model: String,
    allow_model_hints: bool,
    max_output_tokens: Option<u64>,
}

impl OpenAiSamplingHandler {
    pub fn new(config: OpenAiConfig) -> Self {
        let model = config.model.clone();
        Self::with_model(config, model)
    }

    pub fn with_model(config: OpenAiConfig, model: impl Into<String>) -> Self {
        let max_output_tokens = config.max_output_tokens;
        Self {
            provider: Mutex::new(OpenAiProvider::new(config)),
            model: model.into(),
            allow_model_hints: false,
            max_output_tokens,
        }
    }

    /// 默认不采用 server 的 modelPreferences hints，避免外部进程选择任意模型。
    pub fn with_model_hints(mut self, allow: bool) -> Self {
        self.allow_model_hints = allow;
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }
}

impl SamplingHandler for OpenAiSamplingHandler {
    fn create_message(&self, request: Value) -> Result<Value, String> {
        let body = build_sampling_body(
            &self.model,
            &request,
            self.allow_model_hints,
            self.max_output_tokens,
        )?;
        let response = self
            .provider
            .lock()
            .map_err(|_| "sampling provider lock is poisoned".to_owned())?
            .complete_chat(&body)
            .map_err(|error| error.to_string())?;
        parse_sampling_response(&response, &self.model)
    }
}

fn build_sampling_body(
    fallback_model: &str,
    request: &Value,
    allow_model_hints: bool,
    max_output_tokens: Option<u64>,
) -> Result<Value, String> {
    let requested_tokens = request
        .get("maxTokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| "sampling request is missing maxTokens".to_owned())?;
    let max_tokens =
        max_output_tokens.map_or(requested_tokens, |cap| requested_tokens.min(cap.max(1)));
    let model = if allow_model_hints {
        request
            .get("modelPreferences")
            .and_then(|preferences| preferences.get("hints"))
            .and_then(Value::as_array)
            .and_then(|hints| hints.first())
            .and_then(|hint| hint.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
    } else {
        fallback_model
    };
    let mut messages = Vec::new();
    if let Some(system) = request.get("systemPrompt").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": system}));
    }
    let sampling_messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "sampling request is missing messages".to_owned())?;
    for message in sampling_messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "sampling message is missing role".to_owned())?;
        if role != "user" && role != "assistant" {
            return Err(format!("unsupported sampling role: {role}"));
        }
        let content = message
            .get("content")
            .ok_or_else(|| "sampling message is missing content".to_owned())?;
        messages.push(json!({"role": role, "content": convert_content(content)?}));
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
    });
    if let Some(temperature) = request.get("temperature").and_then(Value::as_f64) {
        body["temperature"] = json!(temperature);
    }
    if let Some(stop) = request.get("stopSequences").and_then(Value::as_array) {
        let stop: Vec<&str> = stop.iter().filter_map(Value::as_str).collect();
        if !stop.is_empty() {
            body["stop"] = json!(stop);
        }
    }
    Ok(body)
}

fn convert_content(content: &Value) -> Result<Value, String> {
    match content {
        Value::String(text) => Ok(json!(text)),
        Value::Object(_) => {
            let converted = convert_block(content)?;
            if converted.is_string() {
                Ok(converted)
            } else {
                Ok(Value::Array(vec![converted]))
            }
        }
        Value::Array(blocks) => {
            let mut text = Vec::new();
            let mut parts = Vec::new();
            let mut multimodal = false;
            for block in blocks {
                let converted = convert_block(block)?;
                if let Some(value) = converted.as_str() {
                    text.push(value.to_owned());
                    parts.push(json!({"type": "text", "text": value}));
                } else {
                    multimodal = true;
                    parts.push(converted);
                }
            }
            if multimodal {
                Ok(Value::Array(parts))
            } else {
                Ok(json!(text.join("\n")))
            }
        }
        _ => Err("unsupported sampling content".into()),
    }
}

fn convert_block(block: &Value) -> Result<Value, String> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| json!(text))
            .ok_or_else(|| "sampling text block is missing text".into()),
        Some("image") => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| "sampling image block is missing data".to_owned())?;
            let mime = block
                .get("mimeType")
                .and_then(Value::as_str)
                .ok_or_else(|| "sampling image block is missing mimeType".to_owned())?;
            Ok(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime};base64,{data}")}
            }))
        }
        Some("audio") => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| "sampling audio block is missing data".to_owned())?;
            let mime = block
                .get("mimeType")
                .and_then(Value::as_str)
                .ok_or_else(|| "sampling audio block is missing mimeType".to_owned())?;
            let format = match mime {
                "audio/wav" | "audio/x-wav" => "wav",
                "audio/mpeg" | "audio/mp3" => "mp3",
                other => other
                    .strip_prefix("audio/")
                    .ok_or_else(|| format!("unsupported audio mime type: {other}"))?,
            };
            Ok(json!({
                "type": "input_audio",
                "input_audio": {"data": data, "format": format}
            }))
        }
        Some("resource") => block
            .get("resource")
            .and_then(|resource| resource.get("text"))
            .and_then(Value::as_str)
            .map(|text| json!(text))
            .ok_or_else(|| "sampling resource block has no text content".into()),
        Some(other) => Err(format!("unsupported sampling content type: {other}")),
        None => Err("sampling content block is missing type".into()),
    }
}

fn parse_sampling_response(response: &Value, requested_model: &str) -> Result<Value, String> {
    if let Some(error) = response.get("error") {
        return Err(format!("provider error: {error}"));
    }
    let choice = response
        .get("choices")
        .and_then(|choices| choices.get(0))
        .ok_or_else(|| "provider response is missing choices[0]".to_owned())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "provider response is missing choices[0].message".to_owned())?;
    let content = message
        .get("content")
        .map(content_text)
        .transpose()?
        .unwrap_or_default();
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(requested_model);
    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("length") => "maxTokens",
        Some("content_filter") => "stopSequence",
        _ => "endTurn",
    };
    Ok(json!({
        "role": "assistant",
        "content": {"type": "text", "text": content},
        "model": model,
        "stopReason": stop_reason
    }))
}

fn content_text(content: &Value) -> Result<String, String> {
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(value);
                }
            }
            Ok(text)
        }
        Value::Null => Ok(String::new()),
        _ => Err("unsupported provider message content".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn maps_sampling_request_and_response() {
        let request = json!({
            "messages": [
                {"role": "user", "content": {"type": "text", "text": "hello"}},
                {"role": "user", "content": {"type": "image", "data": "aGk=", "mimeType": "image/png"}},
                {"role": "user", "content": {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"}}
            ],
            "systemPrompt": "be brief",
            "maxTokens": 32,
            "temperature": 0.2,
            "stopSequences": ["END"],
            "modelPreferences": {"hints": [{"name": "hint-model"}]}
        });
        let body = build_sampling_body("fallback", &request, false, Some(16)).unwrap();
        assert_eq!(body["model"], "fallback");
        assert_eq!(body["max_tokens"], 16);
        let body = build_sampling_body("fallback", &request, true, None).unwrap();
        assert_eq!(body["model"], "hint-model");
        assert_eq!(body["max_tokens"], 32);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["stop"][0], "END");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["messages"][2]["content"][0]["type"], "image_url");
        assert_eq!(
            body["messages"][2]["content"][0]["image_url"]["url"],
            "data:image/png;base64,aGk="
        );
        assert_eq!(body["messages"][3]["content"][0]["type"], "input_audio");
        assert_eq!(
            body["messages"][3]["content"][0]["input_audio"]["format"],
            "wav"
        );

        let response = json!({
            "model": "hint-model",
            "choices": [{
                "message": {"role": "assistant", "content": "hi there"},
                "finish_reason": "stop"
            }]
        });
        let result = parse_sampling_response(&response, "fallback").unwrap();
        assert_eq!(result["role"], "assistant");
        assert_eq!(result["content"]["text"], "hi there");
        assert_eq!(result["model"], "hint-model");
        assert_eq!(result["stopReason"], "endTurn");
    }

    #[test]
    fn handler_calls_openai_compatible_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                request.push_str(&line);
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(&String::from_utf8_lossy(&body));
            let response = r#"{"model":"fallback","choices":[{"message":{"role":"assistant","content":"sampled"},"finish_reason":"stop"}]}"#;
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            stream.flush().unwrap();
            request
        });
        let config = OpenAiConfig::new(format!("http://{address}/v1"), "test-key", "fallback");
        let handler = OpenAiSamplingHandler::new(config);
        let result = handler
            .create_message(json!({
                "messages": [{"role": "user", "content": "ping"}],
                "maxTokens": 8
            }))
            .unwrap();
        assert_eq!(result["content"]["text"], "sampled");
        let request = server.join().unwrap();
        assert!(request.contains("POST /v1/chat/completions"));
        assert!(request.contains("Bearer test-key"));
        assert!(request.contains("\"model\":\"fallback\""));
    }
}
