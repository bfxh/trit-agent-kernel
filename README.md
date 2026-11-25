# Trit Agent Kernel

[![CI](https://github.com/bfxh/trit-agent-kernel/actions/workflows/ci.yml/badge.svg)](https://github.com/bfxh/trit-agent-kernel/actions/workflows/ci.yml)

Rust 三值（ternary）**智能体内核（Agent Kernel，含编排）**。**实验阶段，WIP，不承诺稳定 API。**

本仓库从 [`bfxh/trit`](https://github.com/bfxh/trit) 中抽出智能体决策内核相关 crate，保留三值逻辑、义务/证据裁决、影子学习、引导干预与编排（`trit-orch`）。它不包含 `trit` 的位平面、GPU、训练、语言服务器等其它工具链。

> **与 [`bfxh/trit-mc`](https://github.com/bfxh/trit-mc) 的关系**：同一套智能体内核的两个落地切片。
> 本仓库是**智能体内核 + 编排**（`trit-core` / `trit-schedule` / `trit-orch` / `trit-reasonix` / 无头 `trit-host` 宿主）；
> `bfxh/trit-mc` 是**面向 Minecraft 的具体宿主**（零依赖纯 std 的守护进程 + 游戏侧交互）。
> **内核以本仓库为准**，宿主仓库只实现 I/O 与游戏交互。
> 它是智能体的决策内核与编排层：每步输出 `True / Unknown / False`，`Unknown` 一律弃权；模型调用、工具执行与具体界面由宿主负责。

## 先看这里

- **名字**：Trit Agent Kernel（仓库 `trit-agent-kernel`）。
- **是干嘛的**：给智能体提供三值决策、义务/证据、预算、影子学习和编排；`trit-host` 是无头宿主，不是具体桌面或游戏界面。
- **怎么用**：先 `cargo test --workspace`；再 `cargo run -p trit-host -- --help`；上层组件从 [`docs/LIBRARY-STACK.md`](docs/LIBRARY-STACK.md) 的库引入入口开始。
- **人类面 / 智能体面**：README、面板和论文式看板给人看；模型/插件清单在 [`docs/agent-model-manifest.json`](docs/agent-model-manifest.json)，不要拿 README 当机器契约。

## Crates

| Crate | 作用 |
| --- | --- |
| `trit-core` | K3 / Ł3 / B3 / Belnap B4 三值语义、AST、位并行表示 |
| `trit-schedule` | 三值依赖 DAG 的静态调度器 |
| `trit-orch` | 准入、快照、义务、预算、证据 rail、影子学习、引导/干预与 `tick` 主循环 |
| `trit-reasonix` | Reasonix 风格的完成声明、证据裁决与用量适配 |
| `trit-host` | 无头宿主：模型/工具循环、效果台账、JSONL 会话、上下文压缩、文件/Git 检查点 |

## 已实现

- 三值裁决：`True` / `Unknown` / `False`，以及 Belnap 冲突处理。
- 任务 brief、准入义务、预算与 claim/fencing。
- 写工具效果台账；证据不足时 completion 不会自证。
- 交接账本 `trit-orch delivery`：四态 `Sent → Delivered → Read → Acted` 落盘到 `.agent/delivery/<packet-id>.json`，包字节用 SHA-256 钉住；`gate` 只在 handoff gate 为 True 时补 `sent + delivered`，`check` 是只读预检，`acted` 必须带证据。见 [docs/DELIVERY.md](docs/DELIVERY.md)。
- 仓库身份门 `trit-orch repo-gate`：读 `.agent/repo.json`，在 `plan` / `dispatch` 前拒绝“在三进制主库接智能体任务”这类串仓；`trit-host run` 在 workspace 有 identity 时自动 fail-closed，也可用 `--repo-identity` / `--repo-expected <owner/repo> --task-kind agent` 显式强制；要跑完整门链用 `--gate-chain --handoff-packet <path> --alignment-manifest <path>`。见 [docs/REPO-BOUNDARY.md](docs/REPO-BOUNDARY.md) 与 [docs/GATE-CHAIN.md](docs/GATE-CHAIN.md)。
- 对齐门 `trit-orch alignment`：`alignment/1`–`alignment/3` 的目标树、需求父链、产物/证据/sha256/locator、non-goal、声明变更与 v3 run provenance；`aligned=0` / `unaligned=1` / `unknown=3`。见 [docs/ALIGNMENT.md](docs/ALIGNMENT.md)。
- 门链 `trit-orch gate-chain`：把 `repo-gate` → `delivery check` → `alignment` 固定成一条默认只读、stop-first 的链；只有 `True` 放行，`Unknown` / `False` 立即停链并把后续门记为 `skipped`，退出码 `0` / `1` / `3`；可选 `attach` / `--handoff-attach` 在门通过后补 `sent + delivered`，`trit-host run --gate-chain` 默认从 git 取 changed 子集，`--gate-chain-post` 在 run 后用实际变更复核，pre/post 报告可用 `--gate-chain-report` / `--gate-chain-post-report` 落盘。见 [docs/GATE-CHAIN.md](docs/GATE-CHAIN.md)。
- 影子策略学习：安全选择、增量/加权/窗口/跨宿主合并、Beta 先验与漂移窗口。
- 引导遵循度、跨任务干预与主循环 `tick`；Prometheus 指标导出。
- 分层上下文组装初版：`Session/Project/Shared` scope、引用句柄、稳定前缀/动态尾段、印象对比、三值 autonomy、模型/推理 profile 与真实 usage 校准。
- 状态层：从 session 事件重建当前/进行中/压缩后的 AI 状态、压缩历史和校验结论；迁移 bundle 可携带该状态层。
- 上下文压缩：`compact_receipts` 保留最近回执，旧回执折叠为有界摘要（工具、失败 / 不确定、写入路径、输出首行摘录）；摘要预算计入 `max_chars`，截断标记也受单条预算约束；session 压缩事件与状态层会记录丢弃 / 截断的字符数，旧日志缺字段时按 0 兼容。
- Append-only 上下文投影：`ContextProjection` 的 canonical 回执只追加不删除；压缩只移动可见起点，并把 epoch / generation / freshness 写入 session 压缩事件、状态层与 Rebasis 义务。投影视图进入 `ProviderRequest` / OpenAI user prompt；完成裁决只接受当前 epoch 且 freshness 为 True 的回执；默认在新 epoch 出现成功且有断言的证据后恢复 freshness，`Manual` 策略交给宿主。压缩会 rebase Plan task epoch 并同步 Observation epoch / freshness。`SessionMigrationBundle` 可携带投影，prefix 不匹配时 freshness 降为 Unknown。`replay_projection` 可从 session 事件重建投影并校验 epoch/freshness 一致性。
- 权限与 write claim（B7.4 首版）：`PermissionGate` 支持 `Ask / Auto / Yolo`、deny / ask / allow 规则，deny 永远优先，`Ask` 无 approver 时 fail-closed；`WriteClaim` + `ClaimingTools` 把 `write_file` 限制在声明路径子树内，claim 存在时 shell 写入 fail-closed。包装器 opt-in，默认工具路径不变。
- Supervisor：`ProcessSupervisor` 用 OS wait 线程检测退出并自动重启网关；`McpSupervisor` 做 stdio 握手、capability 协商、tools/resources/prompts 分页发现与调用、`McpToolExecutor` 把工具以 `mcp__server__tool` 暴露给模型工具循环（list_changed 动态增删、失败/超时回执）、内置 `OpenAiSamplingHandler`（text/image/audio、includeContext 并发实时读取资源正文）、roots/list_changed、`roots_watch` OS 文件事件监听、logging/setLevel、list_changed 自动刷新、断线重连，并发送含状态层/钩子/cache 的恢复包；采样上下文预取使用有界工作池（`resource_read_concurrency`，默认 8，不截断资源数量或正文长度）；`PortBridge` 保持前端端口常驻；`SupervisorRuntime` + CLI `--supervisor-config <json>` / `--mcp-sampling` 启动并守护 sidecar，运行结束刷新恢复包。
- 会话迁移：`SessionMigrationBundle` 携带上下文版本、钩子契约、状态层和 cache ledger；缓存只在 provider/model/prefix 兼容时复用，旧上下文只保留已有摘要。
- Provider 对接：`ContextPlan` 进入 `ProviderRequest`/OpenAI 消息；`context_search` / `context_add` / `context_read` 让 agent 自主搜索、加入动态上下文并读取句柄；inclusion receipt、实际 cache hit/miss 与按 prefix 聚合的 cache ledger 会被记录。
- Reasonix wire 兼容层（B7.1）：`WireUsage` / `WireEvent` / `WireTool` 版本化契约，保留未知上游字段，映射 usage → `HostUsage` / `TokenUsage` 与 `cacheDiagnostics` → `PrefixSignal`；带契约 fixture harness，尚未接入现有 provider / 工具执行路径。
- 流式 Provider（B7.2，可选）：`StreamingProvider` / `OpenAiProvider::complete_stream` 支持 OpenAI-compatible SSE、文本 / reasoning / tool-call 增量累积、finish/usage 校验与 `[DONE]` 终止；默认 `Provider::complete` 同步路径不变。新增纯 `ConnectionHealth` 状态机；真实 DeepSeek 流式 fixture 已纳入测试。
- CLI：`--context-catalog <json>` 启用 agent 自主上下文；`--context-read-budget` / `--context-read-max` 限制单轮/单次读取；`--cache-hit-threshold`、`--cache-low-hit-policy`、`--cache-window-samples`/`--cache-window-ms`、`--cache-pressure-scale-bp`、`--cache-search-scale-bp`、`--cache-search-order`、`--cache-task-selection`、`--cache-ledger <json>` 控制校准窗口、低命中策略、搜索排序/候选、动态预算、任务选批压力与可选 ledger 持久化；运行结束向 stderr 输出 cache ledger；`--supervisor-config <json>` 启动受监督的 gateway/MCP sidecar，并自动把发现的 MCP 工具接入模型工具循环。
- Cache 校准：`AgentContextPlanner` 与 AgentHost 共享 `CacheLedger`；ledger 按 provider + model + prefix 聚合、只用窗口内精确样本计算校准率；`--resume` 会从历史 `Proposal.context` 重建 ledger；同一桶至少有 3 次精确观测后，实际命中率覆盖下一轮 `hit_potential` 并写入 `observed_hit_rate_bp` / `calibration`；`CacheLedger` 支持 TTL / LRU / 最大条目数回收，按命中质量分为 hot / warm / cold 三层（容量回收先淘汰 cold），并可把低优先级条目压缩成摘要。宿主会对比 inclusion receipt 的 prefix fingerprint，变化时即使 provider 未上报 `prefix_changed` 也产生 Rebasis 义务。
- 内核-前端能力门禁：`FeatureGate` 按 `api_level`、前端能力表、依赖和显式开关决定启用 / 关闭 / 锁住；锁住的能力对前端隐藏并产生带 owner 的通知，扩展坞按组 fail-closed。实现见 `frontend_compat.rs`，契约见 `docs/FRONTEND-CONTRACT.md`。
- 无头 agent：OpenAI-compatible provider、scripted replay、session JSONL、`--resume`。
- 文件级 checkpoint/rewind：预检、symlink/类型拒绝、应用失败逆序回滚。
- Git 影子仓库 checkpoint：每 turn 独立快照，`read-tree` 回退，忽略路径/嵌套仓库/路径碰撞 fail-closed。
- 工具边界：`cap-std` 目录能力、symlink/硬链接拒绝、shell 默认拒绝并要求外部 OS 沙箱；原生 `search_code`/`glob` 使用 ripgrep 同源 ignore/regex/globset，尊重 `.gitignore`，无需 shell；无沙箱 shell 默认复用持久 bash 会话（cwd/env 保持），沙箱模式仍逐条执行。

## 快速开始

需要 Rust 1.88+；Git 后端额外需要 `git`。

```bash
cargo test --workspace
cargo run -p trit-host -- --help
```

脚本化回放：

```bash
cargo run -p trit-host -- run \
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
cargo run -p trit-host -- rewind \
  --session ./session.jsonl \
  --checkpoint-dir ./checkpoints \
  --checkpoint-backend git \
  --workspace ./workspace \
  --turn 0
```

## 安全边界

- `bash` 默认关闭；开启后需要宿主提供外部 OS 沙箱（bwrap / sandbox-exec / 自定义 wrapper），或显式 `--allow-unsandboxed-shell` 降级。
- 持久 bash 仅在无沙箱降级路径启用，cwd/env 会跨命令保留；沙箱模式为每条命令独立进程。
- 检查点回退默认对 shell 命令 fail-closed；`--allow-unrewindable` 才放行。
- Git 后端只管理非忽略的普通文件与受支持的 symlink；`.gitignore` 路径、嵌套仓库/子模块、工作区外路径、网络/数据库等副作用不可回退。
- 回退的“原子”是进程内语义：应用失败会回滚；进程中途崩溃没有持久化 journal，不会自动恢复半完成回退。
- Session JSONL 可能包含工具输出，宿主应按敏感数据管理（脱敏/轮转不在本层）。

## 文档

- [docs/README.md](docs/README.md)：按用户 / 前端 / 上层组件 / 维护者划分的文档地图。
- [REPO-BOUNDARY.md](docs/REPO-BOUNDARY.md)：三进制主库 / 智能体内核 / 宿主的边界，以及 `repo-gate` 串仓防护。
- [ALIGNMENT.md](docs/ALIGNMENT.md)：`alignment/1`–`alignment/3` 的目标树、需求父链、产物/证据、non-goal、声明变更与 run provenance。
- [DELIVERY.md](docs/DELIVERY.md)：交接四态账本、`trit-orch delivery` CLI、包字节钉住与诚实边界。
- [GATE-CHAIN.md](docs/GATE-CHAIN.md)：三门串联的顺序、默认只读约束、stop-first 与 `gate-chain/1` 报告。
- [AGENT-MODEL-MANIFEST.md](docs/AGENT-MODEL-MANIFEST.md)：智能体 / 模型 / 插件清单、人类面与智能体面分工、多模态准入与论文式面板契约。
- [CONTEXT-ORCHESTRATION.md](docs/CONTEXT-ORCHESTRATION.md)：上下文引用/复用、会话地址、推理记录、语义判断、调用缝合与交互式 UI 提案。
- [SESSION-MIGRATION.md](docs/SESSION-MIGRATION.md)：会话/缓存/上下文版本/钩子迁移契约，以及网关/MCP 自动重启与恢复的 supervisor 实现。
- [KERNEL-REASONIX-FUSION.md](docs/KERNEL-REASONIX-FUSION.md)：与 Reasonix 的组合边界、认证平面与 Rust 迁移策略。
- [TERNARY-ORCHESTRATION.md](docs/TERNARY-ORCHESTRATION.md)：三值编排模型。
- [TRACE-AND-TIME.md](docs/TRACE-AND-TIME.md)：轨迹、重放与时间语义。
- [STARTUP-AND-GAME-MODE.md](docs/STARTUP-AND-GAME-MODE.md)：开机调度、桌面清理、游戏模式与外部关闭方式。
- [FRONTEND-CONTRACT.md](docs/FRONTEND-CONTRACT.md)：内核 / 前端能力协商、锁住 / 隐藏规则、扩展坞 fail-closed 与通知。
- [RESPONSIBILITY.md](docs/RESPONSIBILITY.md)：五类问题的责任分层、通知渠道、升级时限与原始库移交。
- [LIBRARY-STACK.md](docs/LIBRARY-STACK.md)：依赖方向、三进制原生库使用规则、新库准入与第三方库政策。
- [RIGHTS.md](docs/RIGHTS.md)：分层许可、商业使用、贡献授权、Reasonix 移植与全仓权益清单。
- [MERGE-POLICY.md](docs/MERGE-POLICY.md)：合并批次、顺序、门禁、停止条件、回退与当前队列。
- [REASONIX-PORT-MATRIX.md](docs/REASONIX-PORT-MATRIX.md)：Reasonix 能力基线、当前落点、差距与 B7.1–B7.9 Rust 移植批次。
- [crates/trit-orch/docs/COORDINATION.md](crates/trit-orch/docs/COORDINATION.md)：协调/派发设计。

## 平台支持

- CI 在 Ubuntu、Windows、macOS 跑 fmt / clippy / 全量测试。
- CI 额外做 ARM64 整工作区 `cargo check`，以及 LoongArch64 核心 crates（`trit-core` / `trit-schedule` / `trit-orch` / `trit-reasonix`）的 `cargo check`；`trit-host` 的 LoongArch 构建仍受 TLS 依赖限制，未宣称支持。
- Linux 使用 inotify、macOS 使用 FSEvents、Windows 使用 ReadDirectoryChangesW 做 roots 监听；持久 shell 优先 bash，Windows 显式走 Git Bash。
- 国产系统（麒麟 / 统信 UOS、ARM64 / LoongArch 等）尚未完成运行时验证。后续适配重点是 bash 路径探测、文件通知后端、OS 沙箱封装、TLS 依赖和 Git/其他 VCS 的可用性；在完成前不宣称支持。

## 来源与状态

代码抽取自 `bfxh/trit` 的 `1a91f6f`，并包含随后完成的检查点原子回退与 Git 保真契约改动。原仓库保留完整历史与其它工具链 crate。

当前仍是开发中版本：API、schema 与安全边界可能继续变化。欢迎 issue 讨论，暂不承诺合并节奏。

## License

MIT OR Apache-2.0，见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)。
