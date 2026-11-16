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
- `compactions[]`：每次压缩的 step、dropped/truncated、digest、`state_after`；
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

## 7. 网关与 MCP 自动重启（提案）

现有仓库没有网关/MCP 运行时，不能假装已经实现。建议的 supervisor 契约：

```text
SupervisorPolicy {
  enabled,
  restart: Never | OnFailure { max_attempts, backoff_ms },
  health: { interval_ms, timeout_ms },
  migrate_session: bool,
  migrate_hooks: bool,
}
```

- **网关重启**：supervisor 只管理子进程，不重启整个桌面应用；重启后从 session log /
  cache ledger 恢复，必要时重新执行迁移计划。
- **MCP 自动重启**：每个 MCP server 独立 supervised；健康检查失败先重连，超过
  backoff 上限才上报；工具调用在重启窗口返回可重试错误，不让模型误判为“工具不存在”。
- **钩子**：supervisor 重启前后必须走同一个 hook contract 校验；不兼容的 hook 不恢复。
- **缓存**：进程重启后从 session replay 重建 ledger；只有 provider/model/prefix 不变才继续复用。
- **用户开关**：`SupervisorPolicy.enabled` 和 per-server/session 开关；关闭后回到手动重启。

这部分仍是设计，不应在文档里标成已完成。

## 8. 当前边界

- 迁移目前是纯计划，不直接改目标 session/文件；宿主需要把 `MigrationPlan` 落地。
- 缓存只迁移 ledger，不迁移 provider 内部 KV cache；跨 provider 本来也不可复用。
- 旧版本摘要不重新生成，因此迁移不会“变聪明”，只保证不丢已有信息。
- 钩子运行时和 MCP 客户端不在本仓库；当前只实现 contract 元数据校验。
- 没有网络层面的零停机保证；supervisor 只能缩短不可用窗口并保证恢复路径。
