# 模型协议适配

同一份 `ProviderRequest` / `ProviderResponse`，通过协议层适配不同模型厂商的 HTTP 形状。
协议差异只集中在 `crates/trit-host/src/protocol.rs`：端点、认证头、请求体、响应解析、流式事件。

## 1. 支持的协议

| `--protocol` | `--provider` | 端点 | 认证头 |
| --- | --- | --- | --- |
| `openai-chat`（默认） | `openai` | `{base}/chat/completions` | `Authorization: Bearer <key>` |
| `anthropic-messages` | `anthropic` | `{base}/v1/messages`（base 带 `/v1` 不重复） | `x-api-key: <key>` + `anthropic-version: 2023-06-01` |
| `gemini-generate-content` | `gemini` | `{base}/v1beta/models/{model}:generateContent`（流式 `:streamGenerateContent?alt=sse`） | `x-goog-api-key: <key>` |

别名：`openai`、`chat-completions`；`anthropic`、`claude`；`gemini`、`google`、`generate-content`。

## 2. 环境变量

通用变量优先，其次才是厂商前缀：

| 用途 | 通用 | OpenAI | Anthropic | Gemini |
| --- | --- | --- | --- | --- |
| API key | `TRIT_API_KEY` | `OPENAI_API_KEY` | `ANTHROPIC_API_KEY` | `GEMINI_API_KEY` |
| 模型 | `TRIT_MODEL` | `OPENAI_MODEL` | `ANTHROPIC_MODEL` | `GEMINI_MODEL` |
| 端点 | `TRIT_BASE_URL` | `OPENAI_BASE_URL` | `ANTHROPIC_BASE_URL` | `GEMINI_BASE_URL` |

- `TRIT_PROTOCOL` 可强制协议（优先级高于 `--provider` 推断与 base_url 推断）。
- 没显式指定协议时按 base_url 主机推断：含 `anthropic` → Anthropic Messages；含
  `generativelanguage.googleapis.com` → Gemini；其他（含自建网关）→ OpenAI Chat。
- 缺 key 或 model 一律 fail closed，报错里点明该配哪个变量，**不会**回落到别家凭据。

## 3. 用法

```bash
# OpenAI（默认）
trit-host run --task "fix the bug" --workspace . --provider openai

# Anthropic Messages
ANTHROPIC_API_KEY=... ANTHROPIC_MODEL=claude-sonnet-4-5 \
  trit-host run --task "fix the bug" --workspace . --provider anthropic

# Gemini
GEMINI_API_KEY=... GEMINI_MODEL=gemini-2.5-pro \
  trit-host run --task "fix the bug" --workspace . --provider gemini

# 自建 OpenAI 兼容网关；显式固定协议
TRIT_API_KEY=... TRIT_MODEL=my-model TRIT_BASE_URL=https://gw.internal/v1 \
TRIT_PROTOCOL=openai-chat trit-host run --task "..." --workspace .

# 只给 base_url，让协议按主机推断
TRIT_API_KEY=... TRIT_MODEL=claude-x TRIT_BASE_URL=https://api.anthropic.com \
  trit-host run --task "..." --workspace .
```

`plan-exec` 同样支持 `--provider openai|anthropic|gemini` 与 `--protocol`。

## 4. 请求体差异

以 OpenAI Chat 请求体为中间态再转换，避免三份 prompt 构造各自漂移：

- OpenAI：`messages[{role:system},{role:user}]`，`tools[].type=function`，`tool_choice=auto`，
  流式用 `stream: true` 与 `stream_options.include_usage`。
- Anthropic：顶层 `system` 字符串 + `max_tokens`（必填，默认 4096）+ `messages[0].content[0].type=text`，
  `tools[].input_schema`，`tool_choice: {"type":"auto"}`，流式用 `stream`。
- Gemini：`systemInstruction` + `contents[0].parts[0].text`，
  `tools[0].functionDeclarations[].parameters`，`toolConfig.functionCallingConfig.mode=AUTO`，
  流式走 `:streamGenerateContent?alt=sse`。

## 5. 响应与流式

| 协议 | 工具调用表示 | usage 字段 | 流式事件 |
| --- | --- | --- | --- |
| OpenAI | `choices[0].message.tool_calls[].function` | `prompt_tokens` / `completion_tokens` / `prompt_cache_hit_tokens` | `delta` 增量 |
| Anthropic | `content[].type=tool_use`（`id` / `name` / `input`） | `input_tokens` / `output_tokens` / `cache_read_input_tokens` / `cache_creation_input_tokens` | `message_start` / `content_block_start` / `content_block_delta`（`text_delta`、`thinking_delta`、`input_json_delta`）/ `message_delta` / `message_stop` |
| Gemini | `candidates[0].content.parts[].functionCall` | `usageMetadata.promptTokenCount` / `candidatesTokenCount` / `cachedContentTokenCount` | 每个 SSE chunk 一个 JSON，含 parts 与 finishReason |

三者都把工具调用归一成 OpenAI `message.tool_calls` 后复用同一份 `parse_message`：
`read_file` / `write_file` / `bash` / `context_*` 的语义与 `complete_subtask` 的完成声明
在三协议下完全一致。

usage 映射口径：
- Anthropic：`prompt = input + cache_read + cache_creation`，`cache_hit = cache_read`，
  `cache_miss = input + cache_creation`。
- Gemini：`prompt = promptTokenCount`，`cache_hit = cachedContentTokenCount`，
  `cache_miss = prompt - cached`。
- 缺 usage 字段时标 `estimated: true`，下游按估算处理，不当精确值结算。

## 6. 边界

- Gemini 的 function call 没有 id，适配层生成 `gemini-call-<n>`；会话与回执按该 id 对齐。
- Gemini 的 JSON Schema 子集比 OpenAI 窄：动态工具若用到 `additionalProperties` 等关键字，
  需要在上游 schema 里先降级，否则可能被服务端拒绝。
- 流式只在显式调用 `complete_stream` 时启用；同步 `complete` 行为不变。
- 不支持 Bedrock / Vertex 专属协议；未知 `--protocol` 直接报错并列出支持项。

## 7. 检查

```bash
cargo test -p trit-host --test protocol       # 端点/请求体/响应/流式/错误映射
cargo test -p trit-host --test protocol_cli   # 误配 fail closed
```
