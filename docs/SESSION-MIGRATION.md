# 会话迁移与网关/MCP 重启

> 状态：迁移计划已实现为 `crates/trit-agent/src/session_migration.rs`；
> 网关/MCP supervisor 是**契约提案**，当前仓库还没有网关或 MCP 运行时。

## 1. 迁移什么

一次迁移把下面四类东西作为一个 bundle 处理：

```text
SessionMigrationBundle {
  session_id, role(Primary|Subagent), project,
  provider, model, prefix_fingerprint,
  contexts: [ContextVersion],
  hooks: [HookDescriptor],
  cache: CacheLedger,
  state: Option<StateLayer>
}
```

- **会话身份**：`session_id` + `role`；不同对话框、主/代理会话可以保留来源。
- **上下文版本**：`id + version + content_hash + summary`。
- **钩子**：`id + event + contract_hash + enabled`；没有 contract 的钩子不迁移。
- **缓存**：按 `provider + model + prefix_fingerprint` 复用的实际 cache ledger。
- **状态层**：当前状态、进行中的 turn/step/tool、每次压缩后的状态与摘要、校验结论。

## 2. 缓存迁移契约

缓存只有在下面三项完全相同时才复用：

1. provider 相同；
2. model 相同；
3. prefix fingerprint 相同。

否则不迁移，并记录明确原因（`ProviderMismatch` / `ModelMismatch` / `PrefixMismatch`）。
不兼容时不做“近似复用”——错误复用会把另一个模型的命中率当成当前模型的。

## 3. 状态层

`StateLayer` 从 `SessionLog` 事件重建：

- `state`：`Idle / Planning / ToolsRunning / Compacting / Judging / Finished / Failed`；
- `in_progress`：当前 turn、step、pending tools；
- `compactions[]`：每次压缩的 step、dropped/truncated、digest、`state_before`、
  `state_after`；`state_after` 是压缩完成后的可继续状态（当前为 Planning），
  当前 `state` 在压缩事件后仍可显示 `Compacting`；
- `usage`：Proposal 事件的聚合用量；
- `validation`：`True / Unknown / False`，附 issue code 与 seq。

校验覆盖：turn 外事件、嵌套 turn 开始、无 start 的 finish、step 回退、压缩缺 digest、
无压缩却有 digest、未完成 turn（warning）。迁移前可先 `session.validate_state()`；
`MigrationPolicy.migrate_state=false` 时状态层不随 bundle 迁移。

## 4. 新想法 vs 旧想法

上下文按 `version` 合并：

| 情况 | 默认行为 |
| --- | --- |
| 目标没有该 id | 导入 |
| 迁入版本更高 | 替换；旧版本若有 `summary`，保存为 superseded |
| 目标版本更高 | 保留目标 |
| 同版本、同 content hash | 视为相同 |
| 同版本、不同 hash | 按 `ContextConflictPolicy` |

`ContextConflictPolicy`：

- `PreferIncoming`（默认）：新想法胜出；旧摘要保留；
- `PreferTarget`：保留目标版本，并记录 rejected；
- `Reject`：拒绝该 id，其余上下文继续迁移。

**零额外模型成本**：迁移不调用模型、不重新 embed、不重新摘要；旧版本只复用已有
`summary`。如果旧版本没有摘要，就只记录被替换，不生成新摘要。

## 5. 钩子迁移

钩子必须匹配目标端的 `contract_hash` 才迁移；不匹配的进入
`rejected_hooks`，由宿主重新注册，不能静默沿用旧钩子的副作用。

- `migrate_hooks=false`：全部不迁移；
- 钩子 `enabled=false`：跳过；
- 未来可增加 per-hook 开关和权限审查。

## 6. 用户开关

`MigrationPolicy`：

```text
enabled              // 总开关
migrate_cache        // 是否尝试复用缓存
migrate_hooks        // 是否迁移钩子
conflict             // PreferIncoming | PreferTarget | Reject
preserve_superseded  // 是否保留旧摘要
```

默认开启，但宿主/用户可全部关闭；关闭时迁移计划保持目标状态不变。

## 7. 网关与 MCP 自动重启（实现）

`crates/trit-agent/src/supervisor.rs` 已实现：

- `ProcessSupervisor`：启动/监控/退避重启子进程，`max_attempts=0` 表示只启动不重启；
  子进程退出由阻塞在 OS wait 上的监视线程发事件，不再用 `try_wait` 轮询；
- `SupervisorLoop`：后台线程按 interval 自动调用 `ensure_running` / `ensure_connected`，
  不需要调用方逐次检查；
- `McpSupervisor`：stdio JSON-RPC `initialize` 握手、初始化通知、断线重连；
  协议版本在 2024-11-05 / 2025-03-26 / 2025-06-18 白名单内协商，未知版本 fail closed；
  解析 serverInfo 与完整 capabilities（tools/resources/prompts/logging/experimental/扩展字段），
  对 tools/resources/templates/prompts 全部按游标分页发现；提供 `call_tool`、
  `read_resource`、`subscribe_resource`/`unsubscribe_resource`、`get_prompt`；
  `McpToolExecutor` 把发现的工具以 `mcp__<server>__<tool>` 暴露给模型，
  `ToolRequest.arguments` 携带 MCP JSON 参数；`ProviderModel` 每轮从 `ToolCatalog`
  重新取工具列表，list_changed 后新增/删除工具无需重启即可生效；删除后的旧名字
  fail closed，调用失败转 failed receipt、超时标 in_doubt；
  服务端发起的 `roots/list` 与 `sampling/createMessage` 由独立 dispatcher 应答；
  roots 是共享可变状态，`set_roots` 更新后发送 `notifications/roots/list_changed`；
  `logging/setLevel` 可设置并记住期望级别，重连后自动重新应用；
- 服务端通知（tools/prompts/resources `list_changed`、`resources/updated`、日志）
  进入通知队列；`SupervisorLoop` 自动调用 `refresh_pending` 刷新已声明的集合；
- 重连后发送 `notifications/trit/recovered`，携带 `RecoveryContext`：
  session_id、状态层（含校验结论）、钩子契约、cache ledger；
- `GatewaySupervisor = ProcessSupervisor`，网关协议由网关自己处理；
- `PortBridge`：前端监听口常驻，向后端端口转发；后端进程重启期间新连接在超时
  窗口内重试，而不是立刻 connection refused；
- `Session::recovery_context()` / `recovery_context_for_session()` 可直接构造
  session/状态层/cache 恢复包，hooks 由宿主补齐；
- `SupervisorRuntime` 从 `SupervisorConfig` 启动所有 sidecar：首启失败即报错，后台
  loop 自动重启/重连，`refresh_recovery` 更新下一次重连使用的恢复包；
- 重启预算耗尽后保持 `GaveUp`，后续 `ensure_*` 不会当作首次启动重新拉起；
  只有显式 `drop_connection`（或新建 supervisor）才重置。

CLI 通过 `--supervisor-config <json>` 接入：

```json
{
  "gateway": [{"name": "gw", "program": "gateway-bin", "args": ["--stdio"]}],
  "mcp": [{"name": "tools", "program": "mcp-server", "args": []}],
  "bridges": [{"name": "front", "listen": "127.0.0.1:8080", "target": "127.0.0.1:18080"}],
  "roots": [{"uri": "file:///workspace", "name": "workspace"}],
  "roots_watch": ["/srv/workspaces"],
  "roots_watch_debounce_ms": 50,
  "log_level": "info",
  "restart": {"max_attempts": 3, "initial_backoff_ms": 50, "max_backoff_ms": 1000},
  "interval_ms": 500,
  "mcp_timeout_ms": 30000
}
```

CLI 在运行前用已有 session log + cache ledger 构造恢复包，运行结束后从新日志重建并
`refresh_recovery`。运行中崩溃会使用最近一次快照，宿主可自行在关键点刷新。
`--mcp-sampling` 会把 `sampling/createMessage` 接到同一个 OpenAI-compatible provider
（`--mcp-sampling-model` 可覆盖模型）；库模式仍可注入自定义 `SamplingHandler`。

边界：采样使用配置模型（库模式 `with_model_hints(true)` 才会采用 server 的 model hint），
`maxTokens` 受 `max_output_tokens` 上限约束；支持 text、image、audio（映射到 OpenAI
`input_audio`）、resource-text，未知内容 fail closed。`includeContext=thisServer/allServers`
会把受管 MCP 的 roots/tools/resources/prompts 元数据注入 systemPrompt，并在采样请求发生时
通过同一 MCP 连接并发发起 `resources/read`，跨 server/资源并行读取所有列出资源的文本
正文并按原顺序汇总，不设数量/字符上限；单次读取沿用 MCP request timeout，单个资源读取
失败只跳过该资源。`roots_watch` 用 notify 的 OS 文件
事件（inotify/FSEvents/ReadDirectoryChangesW）监听目录的直接子目录，事件经 debounce
合并后自动更新 roots 并发送 `notifications/roots/list_changed`；只把目录作为 root。
MCP logging 通知会入队，`logging/setLevel` 已实现但不会改写内核自身日志级别。MCP 工具结果
只能作为工具输出，不是本地 Diff/命令证据，完成裁决仍要求本地可核验条件。端口桥接
只保证前端监听口不断开，已建立连接会随后端进程退出而断，不是内核 FD 交接或连接迁移；
后台 loop 仍按 interval 检查通知与进程状态，但进程退出检测本身是 OS wait 事件；恢复包
是自定义通知，服务端不支持时应忽略而不是断连。

## 8. 当前边界

- 迁移目前是纯计划，不直接改目标 session/文件；宿主需要把 `MigrationPlan` 落地。
- 缓存只迁移 ledger，不迁移 provider 内部 KV cache；跨 provider 本来也不可复用。
- 旧版本摘要不重新生成，因此迁移不会“变聪明”，只保证不丢已有信息。
- 钩子运行时不在本仓库；MCP 客户端已覆盖握手、工具/资源/提示词发现与调用、
  sampling/roots 服务端请求应答、roots 变更通知与 logging 级别设置。
- roots 文件监听只扫描一级子目录；资源正文按需实时读取，单次超时沿用 MCP timeout。
- 端口桥接只覆盖新连接的等待与重试；没有连接排空、FD 交接或请求级重放。
