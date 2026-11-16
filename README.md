# trit-agent-kernel

[![CI](https://github.com/bfxh/trit-agent-kernel/actions/workflows/ci.yml/badge.svg)](https://github.com/bfxh/trit-agent-kernel/actions/workflows/ci.yml)

Rust 三值（ternary）智能体内核。**实验阶段，WIP，不承诺稳定 API。**

本仓库从 [`bfxh/trit`](https://github.com/bfxh/trit) 中抽出智能体相关 crate，保留三值逻辑、义务/证据裁决、影子学习、引导干预与无头宿主能力。它不包含 `trit` 的位平面、GPU、训练、语言服务器等其它工具链。

> 注意：本仓库与 [`bfxh/trit-agent`](https://github.com/bfxh/trit-agent)（Minecraft 角色托管项目）不是同一个项目。

## Crates

| Crate | 作用 |
| --- | --- |
| `trit-core` | K3 / Ł3 / B3 / Belnap B4 三值语义、AST、位并行表示 |
| `trit-schedule` | 三值依赖 DAG 的静态调度器 |
| `trit-orch` | 准入、快照、义务、预算、证据 rail、影子学习、引导/干预与 `tick` 主循环 |
| `trit-reasonix` | Reasonix 风格的完成声明、证据裁决与用量适配 |
| `trit-agent` | 无头宿主：模型/工具循环、效果台账、JSONL 会话、上下文压缩、文件/Git 检查点 |

## 已实现

- 三值裁决：`True` / `Unknown` / `False`，以及 Belnap 冲突处理。
- 任务 brief、准入义务、预算与 claim/fencing。
- 写工具效果台账；证据不足时 completion 不会自证。
- 影子策略学习：安全选择、增量/加权/窗口/跨宿主合并、Beta 先验与漂移窗口。
- 引导遵循度、跨任务干预与主循环 `tick`；Prometheus 指标导出。
- 分层上下文组装初版：`Session/Project/Shared` scope、引用句柄、稳定前缀/动态尾段、印象对比、三值 autonomy、模型/推理 profile 与真实 usage 校准。
- 状态层：从 session 事件重建当前/进行中/压缩后的 AI 状态、压缩历史和校验结论；迁移 bundle 可携带该状态层。
- Supervisor：`ProcessSupervisor` 用 OS wait 线程检测退出并自动重启网关；`McpSupervisor` 做 stdio 握手、capability 协商、tools/resources/prompts 分页发现与调用、内置 `OpenAiSamplingHandler`（text/image/audio/resource、includeContext 注入）、roots/list_changed、`roots_watch` 文件系统自动监听、logging/setLevel、list_changed 自动刷新、断线重连，并发送含状态层/钩子/cache 的恢复包；`PortBridge` 保持前端端口常驻；`SupervisorRuntime` + CLI `--supervisor-config <json>` / `--mcp-sampling` 启动并守护 sidecar，运行结束刷新恢复包。
- 会话迁移：`SessionMigrationBundle` 携带上下文版本、钩子契约、状态层和 cache ledger；缓存只在 provider/model/prefix 兼容时复用，旧上下文只保留已有摘要。
- Provider 对接：`ContextPlan` 进入 `ProviderRequest`/OpenAI 消息；`context_search` / `context_add` / `context_read` 让 agent 自主搜索、加入动态上下文并读取句柄；inclusion receipt、实际 cache hit/miss 与按 prefix 聚合的 cache ledger 会被记录。
- CLI：`--context-catalog <json>` 启用 agent 自主上下文；`--context-read-budget` / `--context-read-max` 限制单轮/单次读取；`--cache-hit-threshold`、`--cache-low-hit-policy`、`--cache-window-samples`/`--cache-window-ms`、`--cache-pressure-scale-bp`、`--cache-search-scale-bp`、`--cache-search-order`、`--cache-task-selection`、`--cache-ledger <json>` 控制校准窗口、低命中策略、搜索排序/候选、动态预算、任务选批压力与可选 ledger 持久化；运行结束向 stderr 输出 cache ledger；`--supervisor-config <json>` 启动受监督的 gateway/MCP sidecar。
- Cache 校准：`AgentContextPlanner` 与 AgentHost 共享 `CacheLedger`；ledger 按 provider + model + prefix 聚合、只用窗口内精确样本计算校准率；`--resume` 会从历史 `Proposal.context` 重建 ledger；同一桶至少有 3 次精确观测后，实际命中率覆盖下一轮 `hit_potential` 并写入 `observed_hit_rate_bp` / `calibration`。
- 无头 agent：OpenAI-compatible provider、scripted replay、session JSONL、`--resume`。
- 文件级 checkpoint/rewind：预检、symlink/类型拒绝、应用失败逆序回滚。
- Git 影子仓库 checkpoint：每 turn 独立快照，`read-tree` 回退，忽略路径/嵌套仓库/路径碰撞 fail-closed。
- 工具边界：`cap-std` 目录能力、symlink/硬链接拒绝、shell 默认拒绝并要求外部 OS 沙箱。

## 快速开始

需要 Rust 1.88+；Git 后端额外需要 `git`。

```bash
cargo test --workspace
cargo run -p trit-agent -- --help
```

脚本化回放：

```bash
cargo run -p trit-agent -- run \
  --task "write a.txt" \
  --workspace ./workspace \
  --script proposals.json \
  --allow-write \
  --session ./session.jsonl \
  --checkpoint-dir ./checkpoints \
  --checkpoint-backend git
```

回退：

```bash
cargo run -p trit-agent -- rewind \
  --session ./session.jsonl \
  --checkpoint-dir ./checkpoints \
  --checkpoint-backend git \
  --workspace ./workspace \
  --turn 0
```

## 安全边界

- `bash` 默认关闭；开启后需要宿主提供外部 OS 沙箱（bwrap / sandbox-exec / 自定义 wrapper），或显式 `--allow-unsandboxed-shell` 降级。
- 检查点回退默认对 shell 命令 fail-closed；`--allow-unrewindable` 才放行。
- Git 后端只管理非忽略的普通文件与受支持的 symlink；`.gitignore` 路径、嵌套仓库/子模块、工作区外路径、网络/数据库等副作用不可回退。
- 回退的“原子”是进程内语义：应用失败会回滚；进程中途崩溃没有持久化 journal，不会自动恢复半完成回退。
- Session JSONL 可能包含工具输出，宿主应按敏感数据管理（脱敏/轮转不在本层）。

## 文档

- [CONTEXT-ORCHESTRATION.md](docs/CONTEXT-ORCHESTRATION.md)：上下文引用/复用、会话地址、推理记录、语义判断、调用缝合与交互式 UI 提案。
- [SESSION-MIGRATION.md](docs/SESSION-MIGRATION.md)：会话/缓存/上下文版本/钩子迁移契约，以及网关/MCP 自动重启与恢复的 supervisor 实现。
- [KERNEL-REASONIX-FUSION.md](docs/KERNEL-REASONIX-FUSION.md)：与 Reasonix 的组合边界、认证平面与 Rust 迁移策略。
- [TERNARY-ORCHESTRATION.md](docs/TERNARY-ORCHESTRATION.md)：三值编排模型。
- [TRACE-AND-TIME.md](docs/TRACE-AND-TIME.md)：轨迹、重放与时间语义。
- [crates/trit-orch/docs/COORDINATION.md](crates/trit-orch/docs/COORDINATION.md)：协调/派发设计。

## 来源与状态

代码抽取自 `bfxh/trit` 的 `1a91f6f`，并包含随后完成的检查点原子回退与 Git 保真契约改动。原仓库保留完整历史与其它工具链 crate。

当前仍是开发中版本：API、schema 与安全边界可能继续变化。欢迎 issue 讨论，暂不承诺合并节奏。

## License

MIT OR Apache-2.0，见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)。
