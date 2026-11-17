# Reasonix wire fixtures

这些是 **契约 fixture**，不是从真实 Reasonix 运行录制的 golden fixture。

- 字段来源：`1012905/DeepSeek-Reasonix-rs@5edbda1a71e2cd8acd5cfdf2d7f1f2c5462f4dd9` 的 `crates/contract/src/eventwire/{usage,event,tool}.rs`。
- 用途：锁定字段名、`omitempty` 行为、usage → `HostUsage` / `TokenUsage` 映射，以及工具参数解析。
- 后续：B7.2 接入真实 provider/MCP 后，用录制器生成同版本 fixture，保留这些手写用例作为字段级回归。
- 修改规则：字段形状变化必须提升 `REASONIX_WIRE_VERSION` 并保留旧 fixture；不能只改 fixture 让测试变绿。
