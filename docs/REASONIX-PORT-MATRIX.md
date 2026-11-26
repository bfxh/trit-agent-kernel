# Reasonix 能力矩阵与 Rust 移植批次

状态：B7 规划基线，2026-11-17。本文回答两件事：**Reasonix 有什么值得拿、我们已有/缺什么**，以及**按什么批次移植到本仓而不降低质量**。

## 0. 基线与边界

| 对象 | 基线 | 用途 |
| --- | --- | --- |
| 上游 Reasonix | [`esengine/DeepSeek-Reasonix`](https://github.com/esengine/DeepSeek-Reasonix)，`studio` 最后提交 `cd20bb5`（2026-11-12），MIT，已归档，最终版 v2.61.0 | 行为参考、文档与历史证据 |
| Rust 重写分支 | [`1012905/DeepSeek-Reasonix-rs`](https://github.com/1012905/DeepSeek-Reasonix-rs)，`studio` 提交 `5edbda1`（2026-11-14），MIT | Rust 层映射、golden fixtures、迁移纪律 |
| 关键规格 | `docs/SPEC.md`、`docs/RUST_MIGRATION.md`、`docs/TOOL_CONTRACT.md`、`docs/CHECKPOINTS.md`、`docs/SUBAGENT_PROFILES.md`、`docs/CAPABILITY_DIAGNOSTICS.md` | 能力与不变量 |
| 本仓 | `trit-agent-kernel` `main` | 移植目标；内核保持纯库，宿主面单独适配 |

边界：

1. **移植行为与契约，不整仓合并。** 不把 Reasonix 的 Go/Tauri/前端 workspace 搬进来，也不做“大爆炸”重写。
2. **每个事实只有一个权威。** 文件系统租约、mutation journal、transcript 仍归宿主；内核只做任务级 claim、证据裁决、调度与效果语义。
3. **UI 不进入内核。** `serve` / TUI / ACP / Tauri 只作为宿主适配层，按批次接；内核不读时钟、不做 I/O、不调模型。
4. **每批可独立回退。** 没有 golden / 差分测试、迁移说明和回退路径的改动不进入下一批。
5. **MIT 义务。** 直接复制代码保留版权、许可与修改说明；只借鉴架构也要记录来源与基线提交。

## 1. 当前落点摘要

已有且可复用的地基：

- `trit-core`：K3 / Ł3 / B3、Belnap 形状证据、AST、真值表。
- `trit-orch`：准入、义务、预算、claim/fencing、效果台账、证据 rail、完成裁决、影子学习、干预、`tick`、trace。
- `trit-host`：Provider / 工具循环、OpenAI-compatible provider、本地工具边界、持久 bash、MCP supervisor / tools / resources / prompts / sampling / roots、会话 JSONL、状态层、文件 / Git checkpoint、上下文分层与 cache ledger、压缩摘要、前端能力门禁。
- 已吸收的 Reasonix 纪律：宿主裁决完成声明、estimated ≠ exact、cache hit/miss 可观测、写路径边界、checkpoint 回退、有界 MCP 资源预取。

主要缺口集中在：**流式 provider、per-call 权限、mutation journal / lease、planner/executor、子 agent/fleet、skills/hooks/memory、HTTP/SSE/ACP 表面、golden wire parity**。

## 2. 能力矩阵

状态定义：`已有` = 当前实现可支撑该能力；`部分` = 有相邻实现但契约/完整性不足；`缺口` = 尚无实现；`阶段外` = 明确不进入第一阶段。

| # | 能力 | Reasonix 证据 | 当前状态 | 缺口 / 目标 | 批次 |
| --- | --- | --- | --- | --- | --- |
| 1 | 事件 / 用量 wire 契约 | `contract/eventwire`、`BUDGET_USAGE_COMPLETENESS.md` | 部分 | 版本化 event/wire、usage/cost/cache 完整性、golden fixtures | B7.1 |
| 2 | Provider 流式输出与工具增量 | `SPEC §3.1`、`provider.Stream` | 部分（首版） | SSE / tool delta 已有；缺多 provider、中断恢复、默认路径接入 | B7.2 |
| 3 | 模型连接健康 / 别名 / 重试 | `MODEL_CONNECTION_HEALTH.md`、`MODEL_DISPLAY_ALIASES.md` | 部分（首版） | 纯健康状态机已有；缺自动探测、别名、超时分级 | B7.2 |
| 4 | 工具契约 / 注册表 / schema 规范化 | `TOOL_CONTRACT.md`、`SPEC §3.2` | 部分 | `ToolSpec` 已有，缺 schema 规范化、契约对照测试、工具家族 | B7.1/B7.2 |
| 5 | 缓存优先、追加式上下文 | `SPEC §3.6`、cache-prefix 基准 | 部分（投影+prompt+迁移） | `ContextProjection` 进入 prompt、迁移 bundle 携带投影；缺跨设备/跨存储实际落盘 | B7.3 |
| 6 | 压缩 → Rebasis / freshness | `SPEC §3.6`、`CacheLedger` | 部分（epoch+恢复+Plan 联动） | 完成裁决要求当前 epoch + freshness True；新证据恢复、Manual 策略、Plan/Observation epoch 已联动；缺跨设备落盘与多任务图迁移 | B7.3 |
| 7 | 会话 JSONL / sidecar / 迁移 | `state/store`、`RUST_MIGRATION.md` | 部分 | 本仓已有 session + migration；缺 sidecar 布局、byte parity、重建投影 | B7.7 |
| 8 | Checkpoint / rewind | `CHECKPOINTS.md`、`SPEC §3.4` | 已有（有界） | files/git 已实现；缺持久 journal、operation lease、崩溃恢复 | B7.7 |
| 9 | per-call 权限（ask/auto/yolo） | `SPEC §3.7`、`TOOL_APPROVAL_MODES.md` | 部分（首版） | `PermissionGate` 已有 ask/auto/yolo、deny 优先、无 approver fail-closed；缺交互审批 UI 与 MCP 策略映射 | B7.4 |
| 10 | Write claim / 路径范围强制 | `SPEC §3.12`、`OPAQUE_WRITE_ADMISSION_SECURITY.md` | 部分（首版） | `WriteClaim` + `ClaimingTools` 限制 write_file 路径子树，shell 写入 fail-closed；缺 move 双端、mutation journal 对账 | B7.4 |
| 11 | Mutation journal / OS lease | `MUTATION_COORDINATOR_SECURITY.md`、`ISOLATION_OPERATION_LEASE_SECURITY.md` | 缺口 | 宿主 mutation record 作为外部事实；内核不复制文件锁 | B7.4/B7.7 |
| 12 | 沙箱 / isolation runner | `ISOLATION_*`、`SPEC §3.7` | 部分 | 已有外部 sandbox wrapper；缺 runner 生命周期、fail-closed 矩阵 | B7.4 |
| 13 | 宿主裁决完成声明 | `SPEC §3.11`、`complete_subtask` | 已有 | 已有 EvidenceRails/义务；需补齐“criterion → 回执”逐条降级与 write scope | B7.5 |
| 14 | 双模型 planner / executor | `SPEC §3.5`、`COORDINATOR_ROOT_BUDGET.md` | 部分（原语 + 协议） | `ReadOnlyTools` + `ReadOnlyCatalog` + `SessionPaths` + 独立 `CacheLedger` + `PlanHandoff` + `submit_plan` 协议解析已实现；缺 provider 侧真实模型调用接线 | B7.6 |
| 15 | 子 agent / context capsule | `SUBAGENT_PROFILES.md`、`SPEC §3.10/3.13` | 部分（契约 + IPC + 进程 + 远程传输） | `SubagentProfile` 权限单调、`ContextCapsule` 显式继承、`DelegationResult` 稳定引用、IPC 帧契约（半包/粘包、引用校验、结局映射）、真实 spawn/超时 kill（`subagent_process` + `subagent-echo` + `subagent-run`）已实现；跨机器已有 `RemoteEndpoint` 准入 + 宿主侧 ureq 单帧 POST（`run_remote_subagent`，携带 `X-Trit-Idempotency: [<scope>|]<task>@<epoch>`）与参考服务端（`remote_serve` + `remote-echo`，按完整键（可带 scope 前缀）去重、`/cancel` 取消在途请求）及真实执行端点（`remote-exec`，spawn `subagent-run`，`--workspace-root` 按 `<scope>/<task>` 隔离目录）；节点发现仍不在范围，跨机调度与有界失败恢复见第 16 行 | B7.6 |
| 16 | Fleet 小依赖图 | `SPEC §3.14`、`fleet` | 部分（执行层 + CLI + 宿主接线 + 跨机派发） | `fleet::execute` 接 `trit-schedule` runtime：并发上限、失败传播、Aborted/Deferred 归类、稳定引用；`fleet-run` CLI 导出 JSON + Prometheus；`HeadlessFleetRunner` 每任务独立 headless session 已实现；跨机派发已有 `NodeTable`（静态节点、最轻负载、容量准入、故障隔离、排除集合）+ `run_remote_plan`（每任务节点归属、每次尝试记录、精确状态）；节点级失败可按 `retry_limit`（默认 0）换节点重试并复用同一 scope 的幂等键，用户取消会传播到远端并以 `Aborted` 结束且不误判节点故障，`remote-plan` CLI 可跑跨机演练、用 `--health` 跨运行保存节点健康并导出 JSON，`remote-ledger` 提供跨节点、按 scope 隔离的 claim/complete 严格去重，落盘状态有键数与字节双重上限并会压缩；动态节点发现、跨机抢占与无限自动重试仍缺 | B7.6 |
| 17 | Skills / commands / hooks | `SKILLS.md`、`EXTENSIONS.md`、`CAPABILITY_DIAGNOSTICS.md` | 部分（注册表） | `SkillIndex`（最长触发词优先）、`CommandRegistry`（斜杠命令解析、参数数量 fail-closed）、`HookRegistry`（事件 + 工具匹配、`*` 通配、`invalidate_on` 失效联动）已实现；缺技能目录发现与 hook 动作执行 | B7.9 |
| 18 | Memory / history 检索 | `SESSION_MEMORY_RETRIEVAL.md`、`HISTORY_SEARCH_CATALOG.md` | 部分（内存索引） | `MemoryStore` CRUD + BM25（k1=1.2、b=0.75，分数降序、同分按 id 升序）+ `Sensitivity::Secret` 默认排除已实现；缺持久化/归档与 history 目录投影 | B7.9 |
| 19 | Serve HTTP/SSE + loopback gate | `RUST_MIGRATION.md`、`serve` | 部分（准入 + 路由） | `ServeConfig::validate`（仅 loopback、token 非空）、`authorize`（Host / Origin / bearer）、`route`（golden 路由表，405 / 404）已实现；缺真实 HTTP/SSE 传输与响应编码 | B7.8 |
| 20 | CLI / TUI / ACP | `CLI.md`、`ACP.md` | 部分 | 无头 CLI 已有；缺 TUI、ACP 编辑器、结构化 JSON 输出 | B7.8 |
| 21 | Tauri / 桌面 host | `desktop-tauri` | 阶段外 | 不进入本仓；只保留 `FeatureGate` 与宿主契约 | B7.8 |
| 22 | Capability diagnostics | `CAPABILITY_DIAGNOSTICS.md` | 缺口 | 静态/实时能力报告、MCP 启动阶段、脱敏 stderr tail | B7.8 |
| 23 | Remote SSH / worktree | `remote/*`、`SESSION_WORKTREES.md` | 阶段外 | 先不移植；等本地 host 稳定后单独立项 | — |
| 24 | Golden fixture / wire parity | `RUST_MIGRATION.md` | 缺口 | 录制、回放、字段双向对齐、拒绝也是契约 | B7.1 |
| 25 | Metrics / usage catalog | `USAGE_CATALOG.md`、`SPEC §3.17` | 部分 | 已有 orch metrics；缺 catalog、成本、委派收益 counter | B7.1/B7.9 |
| 26 | 许可 / 署名 / 移植记录 | MIT 仓库与文档 | 已有流程 | 每批记录来源提交、修改点、NOTICE | 全程 |

## 3. 批次依赖图

```text
B7.1 Contract + golden
  ├─> B7.2 Streaming provider / model health
  │     └─> B7.3 Cache-first context / rebasis
  │           └─> B7.5 Completion + evidence hardening
  ├─> B7.4 Permissions / write claims / mutation authority
  │     └─> B7.5
  └─> B7.7 Persistence / recovery / lease
        └─> B7.9 Skills / hooks / memory

B7.5 ─> B7.6 Delegation ─> B7.8 Surfaces / diagnostics
```

原则：**先契约，再安全，再委派，最后表面。** 表面可以先做无头 JSON；UI 不阻塞内核。

## 4. 移植批次

### B7.1 契约、用量与 golden fixture（P0）

- 目标：把 Reasonix 的 wire/event/usage/tool 契约映射成本仓可版本化类型。
- 范围：
  - `WireUsage` / `WireEvent` / `WireToolCall` / `WireError` 的版本化 serde 类型；
  - `estimated`、cache hit/miss、cost 缺失与“未知不是零”的显式表示；
  - `ToolSpec` schema 规范化与 `TOOL_CONTRACT.md` 对照测试；
  - golden fixture 目录与回放 harness（请求/响应、SSE 帧、错误码）。
- 不做：streaming 实现、UI、MCP 新 transport。
- 验收：字段双向读写；未知版本 fail-closed；旧 fixture 缺字段有默认/拒绝策略；`cargo test` 覆盖错误与正常路径。
- 依赖：无。首批先做这个，后续所有批次共用。

### B7.2 流式 Provider、工具增量与模型健康（P0/P1）

- 目标：把整包 `Provider::complete` 扩展为可流式消费，同时保持现有同步 provider 可用。
- 范围：
  - SSE / chunk 解析、tool-call delta 按 index 累积、reasoning 内容；
  - 超时分级、重试边界、连接健康、模型别名；
  - usage 在流结束/中断时的完整性标记（`estimated` / `interrupted`）。
- 不做：双模型协作、ACP。
- 验收：录制 chunk 回放；中断/半包/重复 delta；工具调用参数不丢；缓存命中字段正确；错误可重试性分类。
- 依赖：B7.1。

### B7.3 缓存优先上下文与 Rebasis（P1）

- 目标：把“压缩是唯一前缀变更点”落实到 `ContextPlan` / `CacheLedger` / `PrefixSignal`。
- 范围：
  - append-only 模型可见投影；压缩前后 epoch / freshness；
  - 压缩摘要、cache ledger、actual hit/miss 回灌；
  - 旧证据在新 prefix 下失效，不能继续背书完成。
- 不做：记忆检索、跨会话共享。
- 验收：压缩前后的 verdict 按 epoch 区分；低命中压力影响 planner；golden cache 报告；回放同一 turn 得到同一结果。
- 依赖：B7.2。

### B7.4 权限、write claim 与 mutation 权威（P0）

- 目标：逐调用权限与写入范围强制，避免“工具开关”冒充安全边界。
- 范围：
  - `ask / auto / yolo` 决策、deny 优先、非交互 fail-closed；
  - 任务级 write claim（含 move 双端、symlink 解析后路径）；
  - 外部 mutation journal / OS lease 只读适配，不复制第二套锁；
  - sandbox runner 生命周期与失败矩阵。
- 不做：文件系统 journal 自研、远程 SSH。
- 验收：deny 永不被 auto/yolo 覆盖；越界写入拒绝并产生回执；mutation record 与效果台账对账；崩溃/取消不留下半状态。
- 依赖：B7.1；可与 B7.2 并行。

### B7.5 完成声明与证据加固（P0/P1）

- 目标：Reasonix 的 completion claim 与现有 EvidenceRails 完整对齐。
- 范围：
  - criterion → command/diff/files/manual 证据映射；
  - 宿主不能抬升状态；无回执的 manual 只能低权威；
  - write scope / path claim 与完成裁决联动；
  - 冲突进 `Conflicted` / `Unknown`，不默认通过。
- 不做：用模型自述替代回执；多数投票。
- 验收：伪完成、少断言、越界写、工具失败、超时未知五类测试；状态只降不升；回放一致。
- 依赖：B7.3 + B7.4。

### B7.6 双模型与委派（P2）

- 目标：planner / executor 分离、子 agent、fleet DAG。
- 范围：
  - planner 只读研究 + 结构化计划；executor 独立 session；
  - subagent profile、context capsule、结果引用、只读/写入权限；
  - fleet `depends_on`、并发上限、失败传播；
  - 用指标判断是否值得委派，不凭感觉并行。
- 不做：把 fleet 图硬塞进 `TaskSpec`；动态节点发现与跨机抢占（静态节点表的派发、隔离与有界重试已补，见第 16 行）。
- 验收：前缀缓存不被 planner 污染；子 agent 无隐式继承；越权写被拒绝；失败/中断结果可恢复；并发/成本有基线。
- 依赖：B7.5。

### B7.7 持久化、恢复与租约（P1）

- 目标：会话/sidecar/checkpoint/租约在崩溃后可解释、可恢复。
- 范围：
  - JSONL + sidecar 布局与字节级兼容；SQLite 投影可丢弃重建；
  - checkpoint 持久 journal（至少记录未完成回退）；
  - operation lease / mutation record 的恢复边界；
  - history 索引与归档。
- 不做：云同步、跨设备迁移。
- 验收：旧会话目录可读；投影删除后重建；崩溃点矩阵；回退失败不静默；租约过期/抢占测试。
- 依赖：B7.3；与 B7.4 联动。

### B7.8 宿主表面与诊断（P2）

- 目标：把内核能力暴露为稳定的无头/宿主接口，而不是塞进 UI。
- 范围：
  - HTTP/SSE `serve`、loopback 认证、Host/Origin 检查、错误码；
  - CLI 结构化 JSON、长期运行、取消；
  - ACP 适配（编辑器）；
  - capability diagnostics：静态默认、`--live` 显式启动 MCP、脱敏 stderr；
  - Tauri/桌面留给独立仓库，只消费本契约。
- 不做：主题、市场、窗口、Tauri 业务状态。
- 验收：loopback 拒绝用例字节一致；SSE 断线重连；静态诊断不启动进程；`--live` 有风险提示与超时；桌面只读运行中 host。
- 依赖：B7.5/B7.6。

### B7.9 Skills、hooks 与 memory（P2）

- 目标：补齐可配置能力层，但不让它们绕过权限/证据。
- 范围：
  - 技能索引、显式调用、只读/写入子 agent profile；
  - hooks 匹配（锚定正则）、失败边界、检查点/工作区失效联动；
  - memory CRUD、历史 BM25 检索、敏感内容与作用域；
  - 能力诊断列出 shadowed / disabled / 启动失败。
- 不做：市场分发、主题包、远程技能执行。
- 验收：技能不隐式授予工具；hook 失败可解释；memory 更新有 revision/归档；检索不会越过会话/项目边界。
- 依赖：B7.7 + B7.8。

## 5. 统一质量门

每批必须满足：

1. **契约测试**：新旧字段双向、未知版本 fail-closed、拒绝路径也有测试。
2. **差分 / golden**：同一输入在参考行为与本实现上结果一致，或差异有书面理由。
3. **安全测试**：deny 优先、路径逃逸、symlink/硬链接、越界写、取消/崩溃。
4. **回放测试**：同一 event/seq 重放得到同一 verdict/obligations/metrics。
5. **文档**：能力矩阵状态、迁移说明、回退路径同 PR 更新。
6. **许可**：记录来源提交与 MIT 署名；不复制来源不明代码。

## 6. 明确不做

- 不把 Reasonix 的 Go / Electron / Vue 代码整体搬入本仓。
- 不在内核里实现文件锁、OS 租约或 mutation journal 的第二套权威。
- 不让 planner/子 agent 的模型自述成为完成证据。
- 不为了“对齐上游”而绕过现有 fail-closed 边界。
- 不把桌面主题、市场、远程 SSH 当作内核能力。

## 7. 当前进度与下一步

B7.1 已落地：

- `reasonix_wire`：`WireUsage` / `WireEvent` / `WireTool` 版本化兼容子集；
- usage → `HostUsage` / `TokenUsage` 映射，`estimated` 保留，负 token / 负 seq fail-closed；
- `cacheDiagnostics` → `PrefixSignal` 映射；
- tool args JSON 解析与 `ToolRequest` 映射；
- fixture harness：版本校验、未知字段保留、三类手写契约 fixture 与回放测试；
- 未接入 provider / MCP / 工具执行路径，现有行为不变。

B7.2 首版已落地：

- `StreamingProvider` 与 `OpenAiProvider::complete_stream`，默认同步 `Provider::complete` 路径不变；
- SSE 解码：注释、多行 data、`[DONE]`、EOF；Text / Reasoning / ToolCall delta；
- tool-call 按 index 累积，参数分片拼接，id/name 冲突与缺失 name fail-closed；
- finish_reason / usage 校验；没有 finish 或 `[DONE]` 的截断流拒绝；
- `ConnectionHealth` 纯状态机：retryable 失败降级、连续失败不可用、auth/quota 立即不可用、成功恢复；
- 真实录制 DeepSeek SSE fixture（stop / length）与本地 HTTP `complete_stream` 回放测试。

B7.3 首版已落地：

- AgentHost 本地保存上一个 inclusion receipt 的 prefix fingerprint；
- provider 未上报 `prefix_changed` 时，指纹变化 / 稳定前缀消失也会产生 `PrefixChanged` 与 Rebasis；
- provider signal 与本地变化合并原因并去重；仍不改写 provider 的同步调用路径；
- `ContextProjection`：canonical 回执只追加，压缩移动 visible start，epoch / generation / freshness 进入 session、状态层与 context Rebasis；
- `replay_projection` 从 JSONL 重建投影并校验 epoch / freshness；真实 AgentHost + SessionLog 回放测试通过；
- `ProjectionView` 进入 `ProviderRequest` 与 OpenAI user prompt；
- 完成裁决只接受当前 epoch 的回执，压缩前旧证据不能为压缩后完成声明背书；
- `SessionMigrationBundle` 携带 canonical 投影、epoch 与边界；目标 prefix 不匹配时 freshness 降为 Unknown；迁移测试覆盖开关与跨 prefix 场景；
- `PermissionGate`：Ask/Auto/Yolo、deny/ask/allow 规则、subject 精确匹配、deny 优先、无 approver fail-closed；
- `WriteClaim` + `ClaimingTools`：write_file 路径子树校验；claim 存在时 shell 写入拒绝；read-only 仍放行；
- `FreshnessRecovery::OnVerifiedEvidence` 默认在新 epoch 的成功且带断言证据后恢复 True，`Manual` 保持 Unknown；完成裁决要求 freshness True；
- 压缩时 `Plan::rebase_epoch` 推进任务 epoch，Observation epoch / freshness 同步更新，brief 重新生成；
- `ContextFreshnessRecovered` session 事件进入状态层与 replay，保证 live / replay 一致。

剩余与下一步：

1. 录制真实 tool-call 流 fixture；当前真实 fixture 只有文本流，tool delta 用内联 chunk。
2. 补模型别名、超时分级、可重试错误分类，以及可选的 provider 健康接线。
3. B7.3 剩余：跨设备/跨存储实际落盘、多任务 Plan 的批量 rebase。
4. B7.4 继续：交互审批 UI、MCP 工具策略映射、move 双端与 mutation journal 对账。
5. 每批合并后更新本文状态列和 [MERGE-POLICY.md](MERGE-POLICY.md) 队列。
