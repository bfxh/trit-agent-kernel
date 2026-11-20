# Recorded streaming fixtures

这些是 **真实录制** 的 DeepSeek OpenAI-compatible SSE 响应，不是手写样例。

- 来源：`1012905/DeepSeek-Reasonix-rs`，`studio` 分支，提交 `5edbda1a71e2cd8acd5cfdf2d7f1f2c5462f4dd9`。
- 原始路径：`internal/model/openai/testdata/deepseek_stream_stop.sse`、`deepseek_stream_length.sse`。
- 许可：上游为 MIT；保留原始内容，不做字段改写。
- 用途：验证 SSE 解码、文本 delta 累积、finish_reason、usage 解析与 `[DONE]` 终止。
- `deepseek_stream_stop.sse`：正常 `stop`，文本为 `Hello there, how are you?`。
- `deepseek_stream_length.sse`：`length` 截断，文本为 `1, `。
- tool-call delta 目前用内联 chunk 测试；等录制到真实工具调用流后再补第三份 fixture。
- 更新规则：真实录制文件只增不改；契约变化用新版本 fixture，并保留旧文件。
