# 与 DeepSeek-Reasonix-rs 的融合设计

> 基线（2026-11-17）：上游 [`esengine/DeepSeek-Reasonix`](https://github.com/esengine/DeepSeek-Reasonix) 已归档（v2.61.0，`studio@cd20bb5`），Rust 重写参考为 [`1012905/DeepSeek-Reasonix-rs`](https://github.com/1012905/DeepSeek-Reasonix-rs)（`studio@5edbda1`）。逐能力状态与移植批次见 [REASONIX-PORT-MATRIX.md](REASONIX-PORT-MATRIX.md)。

> 结论：**不合并代码库，做“认证平面”。**
> Reasonix 管执行、上下文、权限、UI 与分发；`trit-orch` 管判定、义务、证据、效果与回放。
> 两者通过一个薄适配层组合，先影子、后强制，每一步都有指标与回放证据。
> 这样既拿到 Reasonix 的“好东西”，又不把内核拖进 I/O、时钟、模型调用和 UI 复杂度。

---

## 1. 盘点：Reasonix 有什么好东西

仓库：`1012905/DeepSeek-Reasonix-rs`，默认分支 `studio`，MIT。
它是 Go 1.25+ CLI / Rust contract crates / Vue + Tauri Studio 的编码智能体重写；
强调“程序跑在本机、模型自选、缓存友好的上下文维护”。以下能力对融合最有价值。

| 能力 | 仓库证据 | 对内核的价值 | 融合方式 |
|---|---|---|---|
| 多端宿主：TUI / Studio / Web / ACP 编辑器 | `README.zh-CN.md`、`docs/ACP.md`、`docs/GUIDE.zh-CN.md` | 内核不需要自己做 UI，专注判定 | 适配层接事件，不碰 UI |
| 配置驱动 provider / 双模型协作 | `docs/SPEC.md` §3.1、§3.5、`reasonix.toml` | 执行器与规划器可分开评估 | 规划器提议交给 `policy`/影子学习，内核决定 |
| MCP / Extension / Hooks | `docs/SPEC.md` §3.3、`docs/EXTENSIONS.md` | 工具面很宽，正好需要准入与效果门 | MCP/工具调用映射为任务/效果/证据 |
| 缓存优先的上下文维护 | `docs/SPEC.md` §3.6；`benchmarks/compaction/cache-prefix` | prompt cache 命中率是质量/成本核心指标 | `TokenUsage` + cache 指标；压缩触发 **Rebasis** |
| 用量与成本完整性 | `crates/contract/src/eventwire/usage.rs`、`docs/BUDGET_USAGE_COMPLETENESS.md` | 估算用量不能当精确用量；未知 ≠ 零 | 内核需区分 estimated/exact，预算与完成门不得被估算值“满足” |
| 宿主裁决的完成声明 | `docs/SPEC.md` §3.11 | 模型自述不是结论；citation 必须由宿主回执背书 | 直接对应 `CheckReport` / `EvidenceRails` / 完成闸门 |
| 写路径 claim 是强制的 | `docs/SPEC.md` §3.12 | 写权限不是建议；越界必须拒绝 | 与 `ClaimStore` + 效果台账对齐，避免双权威 |
| OS 操作租约 / 并发写入协调 | `docs/ISOLATION_OPERATION_LEASE_SECURITY.md`、`docs/CONCURRENT_MUTATIONS_REVIEW_SERIES.md` | 跨进程/跨 Store 的真实互斥 | 宿主保留 OS 租约；内核 claim 做任务级语义，适配层映射 |
| 检查点 / 回退 | `docs/CHECKPOINTS.md` | 文件级回退是真实需求 | 回退 → PlanPatch/epoch 失效；bash 副作用仍需对账 |
| Fleet 小依赖图 | `docs/SPEC.md` §3.14 | 子任务并行与失败传播 | 初期保留宿主 fleet，图边不硬塞进 TaskSpec |
| MCP 并发策略 | `docs/SPEC.md` §3.16 | read-only ≠ stateless，串行是真实约束 | 作为宿主并发策略/资源类，不复制成第二套锁 |
| 委派收益测量 | `docs/SPEC.md` §3.17 | 用宿主事实而不是模型自述评估编排 | 指标层直接吸收这些 counter |
| 工程纪律 | `docs/CONCURRENT_MUTATIONS_REVIEW_SERIES.md`、`docs/BUDGET_USAGE_COMPLETENESS.md` | 每片独立可构建、不降门禁、写清 cause / blast radius / 邻接行为 | 融合按同样的切片纪律推进 |

**特别重要的三个发现：**

1. **他们的“完成声明”已经是我们的问题域。**
   `complete_subtask` 的 status 只是 claim；宿主用回执逐条校验 acceptance criteria，
   不能背书的降级为 `unsatisfied`，且只降不升。这和我们“证据决定判定、模型自述不是证据”
   完全同向。融合后，内核可以做更严格的证书、双轨证据与义务闭环。
2. **缓存命中率是他们的头号可观测信号。**
   `cache-prefix` 基准记录 prompt / hit / miss / output / median ms / USD；
   `Usage` 事件带 `cacheHitTokens`、`cacheMissTokens`、prefix hash 与 change reasons。
   这正好补齐我们 `TokenUsage` 的缓存维度，也让 `cache_hit_rate_bp` 有真实来源。
3. **他们的并发安全是“多套机制 + 明确边界”。**
   OS 租约、mutation coordinator、write claims、workspace lease 各管一段；
   文档明确写了未覆盖范围（硬链接、bind mount、bash 副作用、跨版本作者）。
   融合时最忌讳再叠一层“内核 claim 也来管文件系统”，必须做权威矩阵。

---

## 2. 我们内核的对照优势

| 内核能力 | 现有落点 | Reasonix 对应/缺口 |
|---|---|---|
| 三值 / Belnap 判定 | `trit-core`、`evidence_rails` | Reasonix 用 prose + host adjudication，没有显式 U/B |
| 五门 + 效果门 | `selection.rs` / `effects.rs` | Reasonix 有权限/写 claim；缺任务级准入与在途对账 |
| 义务队列与调度 | `obligation*.rs` | Reasonix 有 yield/approval，缺统一义务与 EDF/评分 |
| 预算三池 | `budget_book.rs` | Reasonix 有真金白银用量；缺引擎级三池语义 |
| claim / fencing / TTL | `claim.rs` | Reasonix 有 OS 租约与写 claim；缺任务级租约语义 |
| 回执 / 交接 | `receipt.rs` | Reasonix 有 completion claim；交接面较薄 |
| 轨迹 / 回放 / 不变量 | `trace*.rs` | Reasonix 有事件与 JSONL；缺确定性重算与不变量 |
| 计划补丁 / epoch | `plan.rs` | Reasonix 有 checkpoint/rewind；缺证据失效模型 |
| 引导 / 遵循度 / 干预 / 主循环 | `brief.rs` / `adherence.rs` / `intervention.rs` / `tick.rs` | Reasonix 有 approvals/steering；缺结构化契约与跨任务干预 |
| 指标 | `orch_metrics.rs` | Reasonix 有 usage/cache/delegation counters；可互相补齐 |

一句话：**Reasonix 有“手和眼”，我们有“判据和账本”。**

---

## 3. 融合原则：怎么做到不降质

### 3.1 组合，不合并

- 不 fork、不把内核塞进 Reasonix 的 Go 运行时，也不把 Reasonix 的 agent loop 重写进内核。
- 新增薄适配层 `reasonix-orch-adapter`：只做事件翻译、状态映射、协议版本与错误边界。
- 内核保持纯库：不读系统时钟、不做 I/O、不调用模型、不依赖 UI。
- 适配层是唯一允许接触 I/O / 时间 / provider 的地方。

### 3.2 每个事实只有一个权威

| 事实 | 权威 | 另一方如何使用 |
|---|---|---|
| 会话 transcript / 用户输入 | Reasonix session | 内核只拿事件引用与摘要，不复制全文 |
| 文件系统写权限 / OS 租约 | Reasonix sandbox + claims + leases | 内核任务 claim 只做语义/调度，不替代 OS 强制 |
| 任务准入 / 判定 / 证据 | `trit-orch` | Reasonix 把宿主回执映射为 CheckReport |
| 效果意图 / 在途 / 对账 | `trit-orch` ledger + **Reasonix 的 mutation journal 作为外部事实** | 对账时读 mutation record，不另造事实 |
| 用量 / 成本 | Reasonix provider usage（带 estimated） | 内核计量时保留“不完整”标记 |
| 检查点 / 回退 | Reasonix checkpoint | 内核收到 epoch/freshness 失效信号 |
| 引导文本 / 完成契约 | `TaskBrief`（内核） | Reasonix 渲染给模型；模型自述不回流为证据 |

**反模式：两套文件锁、两套 write claim、两套 transcript。**
一旦出现两个权威，回放和审计就会互相打架。

### 3.3 证据优先于叙述

- Reasonix 的 prose completion 只能作为低权威证据；测试命令、文件 diff、日志、回执才是硬证据。
- 模型说“完成了”不改变 verdict；`Unknown` 必须继续产生义务。
- 宿主裁决（§3.11）可以作为一条证据源进入 `EvidenceRails`，但不能覆盖相反硬证据；
  冲突走 `Adjudicate`，不做多数投票。

### 3.4 影子 → 建议 → 强制，逐级放开

1. **影子**：只观测、只算指标，不改任何行为。
2. **建议**：brief 注入、遵循度报告、干预建议，宿主可忽略。
3. **强制**：只对“硬证据充分、误报可控”的闸门启用，例如完成闸门、效果重发、越界写。

每一级都必须有回放对比、误报率、延迟/成本数据，不能凭直觉升级。

### 3.5 先测量，再声称

Reasonix 的测量纪律值得照抄：固定模型、读宿主事实、报告 counter、考虑噪声底
（他们同一 arm 重跑每任务 token 中位波动 19%，最高 54%）。
融合的收益不能靠“看起来更快”，必须用 `false_completions`、`duplicate_work_paths`、
`write_scope_violations`、cache hit、token、wall 等事实说话。

---

## 4. 目标架构

```
┌──────────────────────────── Reasonix Host ────────────────────────────┐
│  TUI / Studio / Web / ACP                                             │
│  provider & model config · permissions · MCP/extensions · UI          │
│  session transcript · checkpoints · mutation journal · OS leases      │
│  agent loop / fleet scheduler / approvals                             │
└───────────────┬───────────────────────────────────────────────────────┘
                │  turn/tool/usage/approval/mutation/checkpoint events
                ▼
┌──────────────────── reasonix-orch-adapter ────────────────────────────┐
│  event schema (versioned) · usage mapping · evidence mapping          │
│  task mapping · effect/claim mapping · trace append · metrics         │
│  只做翻译与边界，不做策略，不复制权威                                 │
└───────────────┬───────────────────────────────────────────────────────┘
                │  TickInput / Observations / Receipts / Claims / Ledger
                ▼
┌──────────────────────── trit-orch kernel (pure) ──────────────────────┐
│  select / brief / adherence / interventions / obligations / tick       │
│  evidence rails · completion gate · plan patch / epoch · trace replay  │
│  budget book · claim gate · effect gate · metrics                     │
└───────────────┬───────────────────────────────────────────────────────┘
                │  TaskBrief / verdicts / obligations / PlanPatch / metrics
                ▼
        Host executes or renders; adapter logs the round-trip
```

### 4.1 两种部署形态

| 形态 | 适用 | 优点 | 代价 |
|---|---|---|---|
| **Sidecar（当前首选）** | Go CLI / Tauri / 混合栈 | 语言无关、崩溃隔离、内核保持纯库、可独立升级 | IPC/序列化开销、进程监督 |
| **进程内 Rust crate（迁移后首选）** | Reasonix Rust kernel 成为主路径后 | 零 IPC、类型安全、同一 workspace 构建 | 绑定上游 crate 边界、panic 边界、编译耦合 |

**不推荐 CGo 嵌入**：构建链复杂，且会把内核拖进宿主运行时。

### 4.3 Reasonix 迁移到 Rust 之后

`docs/RUST_MIGRATION.md` 已明确：Rust 重写**仍把 kernel 作为独立进程**（Tauri 通过 sidecar 拉起
`reasonix-kernel`），HTTP/SSE 是契约，Go↔Rust 用 golden fixtures 做 wire parity。
这反而让融合更干净：

- 两个 Rust workspace 可以共享“纯库”边界：`trit-orch` 不依赖 Reasonix，Reasonix 的
  `crates/kernel` 或新增 `crates/orch` 依赖 `trit-orch`；依赖方向永远是
  **host → adapter → kernel**，反向禁止。
- 进程内链接只发生在 Reasonix 的 kernel 进程里；UI/Provider/Tauri 仍在外层，
  内核依旧不接触 I/O、时钟、模型调用。
- 如果担心内核 panic 影响会话，可以保留独立 `orch` sidecar；Rust 下两种形态共用同一套
  序列化类型与 golden 测试，切换成本低。
- **不要接 Reasonix 尚在迁移中的内部 crate**：等它对应层通过 wire parity 与 golden
  fixtures 再接入；先接 `contract`（usage/event）与 `runtime/session` 的稳定边界。
- Reasonix 的 golden fixture 纪律可以直接复用：为内核增加“同一 turn 的 verdict /
  obligations / metrics”golden 输出，任何协议或策略变化都必须重录并解释差异。
- Rust 迁移期两栈并存：适配层必须能在 Go 宿主与 Rust 宿主上跑同一套契约测试，
  否则“迁移完成”会悄悄改变判定语义。

### 4.4 Rust 下的类型边界

| Reasonix 类型 | 适配层 | 内核类型 |
|---|---|---|
| `eventwire::Usage`（hit/miss/estimated/cost） | 映射 + 完整性标记 | `TokenUsage`（需扩展 estimated） |
| tool call / result | 事件翻译 | `CheckReport` / `EffectSpec` / `Attempt` |
| completion claim + criteria | 裁决证据 | `EvidenceRails` / `Verdict` |
| write claim / OS lease | 只读事实 | `ClaimStore` / 效果台账 |
| checkpoint / rewind | 失效信号 | `PlanPatch` / epoch / freshness |
| prefix hash / change reasons | 上下文事实 | `Rebasis` / `Freshness` |

原则：**内核不依赖 Reasonix 的任何 crate**；适配层可以依赖双方；所有跨边界类型都带版本。

### 4.5 无头模式（先做，不碰 UI）

Reasonix 的四种表面（TUI / Studio / Web / ACP）都不是 Rust 内核的前置条件。
先把“无头宿主”做出来，UI 以后接：

- `trit-host run`：读一个 JSON proposal 脚本，执行工具、效果台账、完成裁决，
  输出结构化 JSON；退出码 0 表示 `Proved`，1 表示未完成，2 表示运行错误。
- 本地工具：`read_file` / `write_file` / `bash`；
  - 所有路径先做词法归一化，再检查是否仍在 workspace root 内；
  - 已存在父目录会 canonicalize 再查一次 symlink 逃逸；
  - 写与 shell **默认关闭**，必须 `--allow-write` / `--allow-shell`；
  - 工具输出截断，避免无界回灌。
- 写工具仍走效果台账：执行前 `prepare` + `mark_sending`；**效果声明先通过 `PlanPatch`
  加到任务上**（替换任务 → epoch+1，重发 brief），再执行工具；已声明的 operation 跳过 patch。
- 完成声明由宿主用工具回执逐条背书，模型（脚本）不能自己写 `backed/passed`。
- **prefix 变化**：`ProviderResponse` 可带 `PrefixSignal`；`changed` 时产生一条
  `Rebasis` 义务（subject `prefix`），并在 session 记 `PrefixChanged`。
- 当前是确定性脚本 / 真实 HTTP 两种驱动；provider 契约与 OpenAI-compatible 传输都已接。
- **Provider 契约**：`Provider` / `ProviderRequest` / `ProviderResponse` / `ProviderModel`，
  brief、tools、receipts、usage 都是纯数据；`ScriptedProvider` 支持确定性测试与回放。
- **OpenAI-compatible HTTP**：`OpenAiProvider`（阻塞 `ureq`）调用 `/chat/completions`，
  用 function calling 映射 `read_file / write_file / bash / complete_subtask`；
  `OPENAI_API_KEY / OPENAI_MODEL / OPENAI_BASE_URL` 配置，API key 在 Debug 输出里脱敏；
  缺失 usage 标 `estimated`；请求构造与响应解析有单测，**未做 live 网络测试**。
- **Session JSONL**：`SessionLog` 追加 `TurnStarted / Proposal / ToolRequested / ToolCompleted /
  CompletionClaimed / Judgment / TurnFinished`，带 schema 与单调 seq；已有日志可续写，
  坏行/坏 schema fail-closed。CLI 用 `--session <file>` 开启。
- **文件约束**：`cap-std` 目录能力 + 最终组件 no-follow 打开；路径相对 root 解析，
  父目录逃逸由能力边界拒绝；硬链接在**打开后的句柄**上计数（Unix `nlink`、
  Windows `GetFileInformationByHandle`），`forbid_hardlinks` 默认开启。
  创建/覆盖不再依赖“先检查路径再写路径”，TOCTOU 已收窄到内容层面。
- **shell 约束**：默认拒绝；开启后若无沙箱，必须显式 `allow_unsandboxed_shell`；
  `SandboxCommand` 把 `program + args + 平台 shell` 交给宿主提供的 OS 沙箱
  （bwrap / firejail / 自定义 wrapper）。内核不假装 `Command` 本身能限制写根。
- CLI 显式暴露这些开关：`--allow-shell (--sandbox <program> [--sandbox-arg ...] | --allow-unsandboxed-shell)`、
  `--allow-hardlinks`；缺省拒绝。
- **多轮恢复**：`Session<M,T>` 从 JSONL 重建 `TurnRecord`（task / objective / verdict /
  complete / finished / usage / tool_calls / criteria）并累计 usage；未收到 `TurnFinished`
  的 turn 保持未完成；CLI `--resume` 在同一 log 上续写。
- **checkpoint / rewind**：两种后端，共用同一套 turn / scope / fail-closed 语义。
  - `files`：`CheckpointingTools` 在写工具执行前快照 turn 开始前的文件内容
    （None = 当时不存在），每个 turn 一份 sidecar。回退先归一化全部路径、拒绝
    symlink/非普通文件并捕获当前内容，再开始写入；中途失败按逆序回滚，回滚也失败时
    返回 `RewindRollback`，不会静默留下半恢复状态。
  - `git`：`GitCheckpointingTools` 在 turn 开始时把工作区（遵守 `.gitignore`）写入
    `--checkpoint-dir` 下的裸影子仓库，每个 turn 一个无父提交与 `refs/trit/turn-*`；
    不碰用户 `.git`。回退先校验目标树，再把当前工作区放进索引并提交一个临时回滚点，
    最后 `read-tree --reset -u <snapshot>`；失败时用回滚点恢复。
  - **git 保真契约**：只管理非忽略的普通文件与 `core.symlinks=true` 时的 symlink。
    嵌套仓库/子模块（gitlink）、`.git` 路径、逃逸路径、不支持的 mode、Windows
    大小写碰撞、目标路径当前为目录、以及目标路径当前未跟踪且被 `.gitignore` 忽略时
    一律 fail-closed；忽略文件本身不会被触碰，工作区外与网络/数据库副作用仍不可回退。
  - 两种后端都审计 bash 命令；默认遇到命令 fail-closed（回退前先检查，不会先改代码再报错），
    `--allow-unrewindable` 才显式放行。`--checkpoint-retain N` 保留最近 N 个 turn；
    git 后端删除旧 ref 后执行 `gc --prune=now`。
  - `rewind_session` / `rewind_session_git` 可恢复 code、截断 conversation，或两者都做。
    CLI：`--checkpoint-dir <dir> --checkpoint-backend files|git` 记录，
    `trit-host rewind` 用相同后端参数回退。
- **context 压缩**：`ContextPolicy` 保留最近 receipts + 字符预算，旧回执折叠成确定性摘要
  （工具名、失败/in-doubt 计数、写入路径数）；全量回执仍在 `TurnOutcome` / session，
  压缩只影响模型上下文；session 记 `ContextCompacted`。CLI 可调
  `--context-receipts / --context-chars / --context-output`。
- Session 日志可能含工具输出，宿主必须按敏感数据管理（脱敏/保留策略不在本层）。

**无头模式的边界（诚实版）：**

- checkpoint/rewind 的覆盖范围取决于后端：`files` 只覆盖写工具触碰的路径；`git` 覆盖
  工作区非忽略文件，嵌套仓库、`.gitignore` 路径、工作区外路径、网络/数据库等
  shell 副作用仍不可回退，只能靠命令审计与效果台账对账。
- git 后端依赖外部 `git` 可执行文件；Windows 下路径经 verbatim 前缀转换后传给 git。
  symlink 在 `core.symlinks=false` 时直接拒绝；文件 mode 的跨平台保真仍取决于
  Git 配置与文件系统，不能承诺。
- 回退的“原子”是进程内语义：应用失败会回滚；进程在 `read-tree`/文件写入中途崩溃时
  没有持久化 journal，下一次启动不会自动恢复半完成回退。
- context 压缩是确定性摘要，不是语义摘要；摘要质量/忠实度没有评测。
- 工具沙箱：文件 symlink / 硬链接 / 目录能力已落地；shell 需要宿主提供外部 OS 沙箱，
  内置 bwrap/AppContainer 未实现。残余风险：打开后新建硬链接、外部进程并发替换内容。
- HTTP provider 已接，但重试、超时分级、live 兼容性矩阵与 cache 诊断仍未验证。
- 效果声明会通过 PlanPatch bump epoch；长 turn 中多次写调用会多次 bump，
  目前没有合并 patch 或“无证据时免 bump”的优化。
- 脚本模式适合回放、集成测试和外部调度；真实模型仍需要 provider 传输层。

### 4.2 Sidecar 协议

- 传输：stdio JSON-RPC 或本机 HTTP/SSE；消息带 `schema`、`adapter_version`、`kernel_version`。
- 输入：`tick`（plan/snapshot/limits/ledger/claims/briefed/trace 摘要/策略）。
- 输出：`selection`、`briefs`、`adherence`、`interventions`、`obligations`、`metrics`。
- 约束：未知字段保留/忽略策略明确；时钟字段由宿主传入；sidecar 不主动读盘。
- 幂等：同一 `turn_id` 重放必须得到同一结果；事件带 seq，重复事件按 ID 去重。

---

## 5. 语义映射：最关键的几刀

### 5.1 粒度：turn、task、tool call 怎么落

这是融合的第一决策，直接影响质量。

| 粒度 | 建议 | 理由 |
|---|---|---|
| 用户 turn / 子智能体运行 | **Plan 的任务边界** | 完成/失败/成本有明确起点终点 |
| 工具调用 | **effect（有副作用）或证据（只读）** | 写操作必须过效果台账；读操作产出证据 |
| 子智能体 / fleet item | **子任务（可选）** | 初期保留宿主 fleet 图，避免两套依赖图 |
| 单次模型调用 | **计量点，不是任务** | token/cache 按 attempt 记账 |

初期不要试图把 fleet 的 `depends_on` 塞进 `TaskSpec`。Reasonix 文档明确说依赖属于 fleet plan、
不属于 TaskSpec；我们的 Plan 依赖是证据图 / 执行图。两套图可以先并存：
**fleet 管“谁先跑”，内核管“凭什么算完成”。**

### 5.2 上下文压缩 → Rebasis，不是普通 tick

Reasonix 的 cache-first 设计把压缩定义为**唯一一次 prompt prefix 变化**（cache-reset point）。
对我们的含义：

- `CacheDiagnostics.prefix_changed` / `prefix_change_reasons` 是外部事实；
- prefix 变化应触发受影响任务的 `Freshness`/`Rebasis` 语义，旧证据不能继续背书；
- 压缩前后的 verdict 必须按 epoch 区分，不能“摘要之后仍用旧断言”；
- `cache_hit_tokens` / `cache_miss_tokens` 映射为 `TokenUsage.cached/fresh`；
- 缓存未命中率上升是质量/成本信号，不是单纯性能指标。

**质量风险**：如果只在适配层“记一下 cache 命中”，却不把 prefix 变化接到内核 freshness，
就会出现“证据过期但判定仍为 True”。这比不融合更危险。

### 5.3 宿主裁决 → 双轨证据

Reasonix §3.11 的“citation 必须由宿主回执背书”直接映射：

- `verification` 命令回执 → `CheckReport { kind: command, result }`
- `diff` / `files` → 路径级回执（谁改了什么）
- `manual` → **不能自证**，只能进 inconclusive 或低权威轨
- 不能被回执背书的 criterion → 降级，不允许 parent 抬升
- 冲突（宿主回执 vs 模型自述）→ `Conflicted`，走 `Adjudicate`

这和我们的 `EvidenceRails`（support / counter / inconclusive）是天然一对。

### 5.4 写 claim / OS 租约 → 任务 claim + 效果台账

Reasonix 已经有很强的文件系统边界（symlink 解析、最深祖先、move 双端、bash 沙箱重绑）。
不要重复实现：

- **OS 强制**：Reasonix 保留；
- **任务级语义**：内核 `ClaimStore` 管“哪个任务声明了哪些路径、是否过期、是否被他人占用”；
- **效果台账**：内核管“这次写操作是否 Prepared/Sending/Confirmed/Unknown”，重发前必须对账；
- 适配层把 Reasonix 的 mutation record 作为对账的外部事实，而不是再写一份 journal。

**质量风险**：两边都“以为自己是权威”时，一个说写成功、一个说未发生。
权威矩阵必须写进适配层代码与测试。

### 5.5 用量完整性：estimated 不能当 exact

Reasonix 的 `Usage` 明确区分 provider-reported 与 `estimated`，且“未知费用不是零”。

**已落地（2026-11-14）：**

- `TokenUsage` 增加 `estimated: bool`；`checked_add` 传播该标记，`is_exact()` 可查；
- `OrchestrationMetrics` 增加 `usage_estimated` 与同名 Prometheus gauge；
- 新增 `crates/trit-reasonix` 适配层：`HostUsage::into_token_usage()` 把
  `cache_hit_tokens / cache_miss_tokens / completion_tokens / estimated` 映射到内核口径。

后续仍要做：预算结算与完成闸门在 `estimated` 时的显式策略（拒绝结算 / 降级为 Unknown），
不能只“记一个标记”却继续当精确值使用。

### 5.6 检查点 / 回退 → PlanPatch + epoch

- 文件回退会改变工作区事实 → 相关任务的证据必须失效（epoch bump / freshness）；
- 回退后重跑不是“重试”，而是新定义/新证据；
- bash 副作用不在 checkpoint 覆盖范围，仍需效果台账对账；
- 不要把回退做成“删除旧 trace”；保留历史，用 epoch 区分。

### 5.7 从 OpenAI 的数学验证论文借鉴什么
两篇最相关的论文：

- **Let's Verify Step by Step**（Lightman et al., 2023，arXiv:2305.20050）：
  过程监督（每个推理步骤给反馈）显著优于只看最终答案的结果监督；
  PRM800K 约 80 万条步骤标签；在 MATH 代表性子集上达到 78%。
- **Prover-Verifier Games improve legibility of LLM outputs**（Kirchner et al., 2024，arXiv:2407.13692）：
  让强 prover 与**小 verifier** 博弈，verifier 只需要判断“这一步是否可被人类看出错误”；
  在小学数学任务上提升人类核查准确率，同时保持解题性能。

对我们最有价值的不是“再训一个 PRM”，而是四个结构性结论：

1. **验证粒度要下沉到步骤，而不是只看最终 prose。**
   我们的 `produces.min_assertions` 目前更像 outcome 级；Reasonix 的 tool receipt 天然是 step 级。
   融合时应把“每个工具调用/每条 acceptance criterion”映射为可核对的步骤证据，
   完成闸门只在步骤证据齐全时放行。这比训练一个 PRM 更便宜、更可回放。
2. **verifier 应该小而独立，prover 可以很强。**
   内核就是那个“小 verifier”：确定性、可审计、便宜；Reasonix 的模型是 prover。
   不能让 prover 自证，也不能让 verifier 依赖 prover 的自然语言解释。
   数学里 verifier 检查证明步骤；代码里 verifier 检查命令回执、diff、测试结果。
3. **“legibility” 是产品能力，不只是安全属性。**
   Prover-Verifier 的目标是让人类更容易发现错误；对应到我们，是 `TaskBrief` / trace /
   verdict 证书要能让用户快速看出“哪一步没被证据支持”。这应该进入 UI 设计，
   而不是只藏在日志里。
4. **可判定性决定该不该给结论。**
   数学题有 ground truth，代码有测试/编译/静态检查；没有可检查事实的任务，
   正确输出是 `Unknown` + 义务/人工，而不是让 learned verifier 猜。
   PRM 的标签成本、reward hacking、分布漂移都是已知风险；
   所以 learned verifier 只能做影子/建议，确定性 gate 仍是最终权威。

**具体落地建议：**

- 在 `CheckReport` 之上增加“步骤证据链”视图：tool call → receipt → verdict，
  完成时能回答“哪一步、哪条命令、哪个 diff 支撑了哪个 criterion”。
- 把 Reasonix §3.11 的 acceptance criteria 直接作为 step-level evidence 的骨架，
  内核负责降级/冲突/未知，模型负责产出。
- 对没有硬证据的任务，明确走 `Escalate`/人工确认；不要用 PRM 分数代替证据。
- 如果以后训练/接入 learned verifier：只在 shadow 模式评估，指标是
  `false_completions`、`criterion_downgrades`、人工复核时间，不是“看起来更聪明”。

---

## 6. 分阶段路线（每阶段都有验收门）

> 原则：任何阶段都不降低既有门禁；每片独立可构建、可测试、可回放。

| 阶段 | 内容 | 验收门 | 不改什么 |
|---|---|---|---|
| **F0 观测影子** | 适配层只读 Reasonix 事件，产出 `OrchestrationMetrics` / trace 摘要；不注入 brief、不做决定 | 行为零变化；指标与宿主 usage/cache 对账一致；sidecar 故障不影响宿主 | 不改 agent loop、权限、UI |
| **F1 简报影子** | 每个 turn 生成 `TaskBrief`，宿主可选择注入；同时影子计算 verdict/adherence，和宿主裁决对比 | 不注入时零差异；注入后 completion/prose 比例可测；误报清单可审计 | 不强制任何闸门 |
| **F2 完成闸门** | 把内核完成判定接到子智能体/turn 的 completion；`false_completions`、`criterion_downgrades` 作为硬指标 | 在 Reasonix e2e 与 false-completion 语料上，false completion 不升、完成率不降；可回放 | 不碰文件权限与模型调用 |
| **F3 效果 / claim 强制** | 写工具先 `prepare_effects`，崩溃/重启后先对账；越界写保留 Reasonix 强制，内核做任务级 claim | 注入重复写、崩溃窗口、并发写测试；`write_scope_violations` 不升；无重复副作用 | 不替代 OS 租约 |
| **F4 上下文耦合** | prefix 变化 → Rebasis；缓存指标进主循环；旧 epoch 证据不可用 | 压缩后无陈旧证据；cache hit 不显著下降；token 成本可解释 | 不改 Reasonix 的压缩算法 |
| **F5 义务恢复 / 回退** | 失败原因映射为 Reconcile/Defer/Escalate/Measure；checkpoint 回退触发 epoch；fleet 失败传播接义务 | 无义务的 U = 0；抖动上限生效；回退后可重跑 | 不改 fleet 图语法 |
| **F6 学习与干预** | 用 Reasonix 回放语料训练/评估 planner 策略；干预 UI 展示 held/升级；只在 shadow 通过后启用 | `unsafe_extra = 0`；干预后 false completion / duplicate work 有可测改善；可回滚 | 不让策略改内核决定 |
| **F7 多宿主** | 把 OS 租约 / 多进程事实接入 claim/fencing；外部租约服务 | 两宿主并发写同一路径被测；fence 单调；崩溃恢复 | 不做共识、不伪造强一致 |

**建议的第一个真实里程碑：F2，而不是 F0 之后一路铺开。**
完成闸门是 Reasonix 已经痛、我们已经有解、且收益可测的一刀。
文件权限（F3）风险更高，放在完成闸门之后。

---

## 7. 质量保障

### 7.1 回放语料

- 用 Reasonix 的 `benchmarks/e2e/tasks` 与真实 session JSONL 建语料；
- 每条轨迹重放时用当时的 plan / snapshot / limits / ledger 重新跑内核；
- 对比影子判定与宿主裁决，记录分歧，不追求“全一致”，追求“分歧可解释”。

### 7.2 影子对比（上线前必做）

- 同一 turn 同时跑 Reasonix 原生裁决与内核影子判定；
- 统计：false completion、criterion downgrade、unknown 率、冲突率、义务数、干预数；
- 只有在内核侧**没有新增错误完成**、且误报可接受时，才进入强制阶段。

### 7.3 故障注入

- provider 不返回 usage（estimated）；
- 工具调用重复 / 超时 / 取消；
- 写入前崩溃、写入后崩溃、对账前崩溃；
- 压缩发生在 turn 中、回退发生在 turn 后；
- sidecar 崩溃/重启、协议版本不匹配；
- 并发子智能体写重叠路径。

### 7.4 性能预算

- 内核 `tick` 不看墙钟，但适配层要测：p50/p99 tick 开销、IPC 往返、序列化大小；
- 目标：相对一次模型调用的延迟可忽略；不因观测拖慢交互；
- token 开销必须为 **0**（内核不调用模型）；任何“为了判定再问一次模型”的设计都要拒绝。

### 7.5 安全与审计

- 授权不写在 prompt 里（沿用现有原则）；
- 所有审批走回执/事件，可追溯；
- trace 里不落原始 secret；大载荷存引用/哈希（已有脱敏与引用设计）；
- 写路径、命令、网络出口分别有明确的 sink 与拒绝测试；
- sidecar 协议拒绝未知/越权字段，版本不匹配 fail-closed。

### 7.6 工程纪律（照抄 Reasonix 的好的部分）

- 每片必须独立构建、独立测试，不继承“整体分支测过”的结论；
- PR/提交写清 Cause、Blast radius、Neighbouring behaviours、Why this layer；
- 不降低 lint / 安全门禁；
- 明确 non-goals 与未覆盖边界；
- 作者自评不等于独立评审。

---

## 8. 风险与反模式

1. **两套权威**：Reasonix 的 write claim / OS lease / mutation journal 与内核 claim / effect ledger。
   必须画权威矩阵，适配层只做映射；否则审计会出现“两个真相”。
2. **把估算当精确**：`Usage.estimated` 必须进内核模型；否则预算与完成门会被“估算 token”欺骗。
3. **缓存与新鲜度脱节**：prefix 变化不触发 Rebasis，会把旧证据当新证据。
4. **过度保守**：每个 Unknown 都产义务、都等人，会拖垮交互体验。
   先用指标证明“误报可接受”，再强制。
5. **粒度错配**：把一次模型调用当任务、或把一个 turn 拆成几十个任务，都会让义务/预算失真。
   第一版坚持 turn=任务、tool call=效果/证据。
6. **复制而不是映射**：不要把 Reasonix 的 checkpoint、mutation journal、usage 原样再存一份；
   引用 + 事件 + epoch 即可。
7. **sidecar 性能/可用性成为新故障点**：必须可降级（观测失败不影响主流程）；
   强制阶段则要失败关闭，二者策略不同，要显式配置。
8. **上游漂移**：Reasonix 活跃开发，协议/字段会变；适配层版本化、固定 commit、契约测试。
9. **用 prose 证明收益**：必须用 counter + 语料 + 重复实验；注意噪声底。
10. **过早合并代码库**：会把两种质量文化绑死；先做可独立升级的组合。

---

## 9. 未来方向

- **认证平面即产品**：内核不只服务 Reasonix，可作为 ACP/HTTP 服务给任何 agent harness 提供
  “判定 + 义务 + 效果 + 回放”能力；Reasonix 是第一个宿主。
- **预测型干预**：用 Reasonix 的 usage/cache/prefix 事实 + 内核 P(flip) 做风险预测，
  但干预仍走“策略提议、内核 gate 兜底”。
- **学习型规划**：用回放语料评估 planner/executor 策略；`ShadowLearner` 只产出推荐。
- **证据归一化**：测试断言、diff、日志、人工确认、静态分析各有可信度；
  在 `EvidenceRails` 上做权重与来源可信度，而不是多数投票。
- **分布式 claim**：Reasonix 已有 OS 租约与多进程经验；再接外部租约服务做跨主机协调。
- **缓存感知调度**：把 cache hit/prefix 变化纳入义务评分，避免“为了补证把缓存全打掉”。
- **统一评测**：Reasonix e2e + cache-prefix + false-completion 语料 + 内核回放，
  形成“质量 / 成本 / 延迟 / 证据完整性”四维报表。

---

## 10. 立即可以做的六件事

1. **定义适配层事件 schema（v0）**：turn、tool call、usage、approval、mutation、checkpoint、compaction。
2. **补内核 `TokenUsage.estimated`**：让“未知用量 ≠ 0”从一开始就是真的；配预算与指标测试。
3. **搭 F0 影子骨架**：sidecar 只读事件 → `OrchestrationMetrics`；用 Reasonix 一个 e2e 任务跑通。
4. **建回放语料**：从 Reasonix `benchmarks/e2e/tasks` 选 10 条，录 plan/snapshot/limits/ledger/usage。
5. **写权威矩阵测试**：模拟“Reasonix 说写了 / 内核说没写”的分歧，确保适配层不会制造第二个真相。
6. **等 Rust parity 后接进程内形态**：在 Reasonix `crates/` 新增薄 `orch` crate 依赖 `trit-orch`，
   复用其 golden fixtures；Go 宿主继续用 sidecar，两边跑同一套契约测试。
   `crates/trit-host` 是这一步之前的独立 Rust 宿主骨架，可整体或按模块移植。

---

## 11. 当前已落地（2026-11-14）

- `TokenUsage.estimated` + `OrchestrationMetrics.usage_estimated` / Prometheus gauge。
- 新 crate `crates/trit-reasonix`：
  - `HostUsage::into_token_usage()`：cache hit/miss、estimated 映射；
  - `CriterionClaim` / `CompletionClaim`：宿主裁决的完成声明；
  - `claim_rails()`：backed/passed → Passed/Refuted，未背书与 Manual → Inconclusive；
  - `judge_completion()`：三值 / Belnap 判定 + 遵循度检查；
  - `TurnSpec::task_spec()`：objective/constraints/stop_when/produces；
  - `HostToolCall::effect_spec()/claim_paths()`：写工具效果与路径。
- 新 crate `crates/trit-host`：Rust 无头宿主。
  - `Model` / `ToolExecutor` trait，不绑定 provider、异步运行时或 UI；
  - `AgentHost::run_turn`：选批 → brief → 模型提议 → 工具执行 → 效果台账 →
    宿主逐条背书 criterion → `judge_completion`；
  - `LocalTools`：`read_file` / `write_file` / `bash`；workspace 边界，symlink / 硬链接拒绝，
    shell 默认拒绝并要求外部 OS 沙箱（或显式无沙箱降级）；
  - `PlanPatch` 效果声明、`PrefixSignal` → Rebasis、session JSONL；
  - `trit-host run` CLI：scripted / openai、效果与 shell 权限、session；
  - 写工具在真正执行前 `prepare` + `mark_sending`，结果不明进 `Unknown`；
  - 步数上限、工具结果错配、空提议、task 不匹配都 fail-closed。
- 测试：适配层 9 条 + 宿主 46 条；全 workspace 333 个 test target、3080 项通过、0 失败、1 ignored。

**仍未做**：内置 OS 沙箱（bwrap/AppContainer）、打开后新建硬链接的最终竞态、
回退进程崩溃的持久化 journal / 启动恢复、git 后端对文件 mode 与跨平台 symlink 的
完全保真、语义压缩评测、Reasonix 事件协议对接、Reasonix golden fixture 对照、
HTTP live 兼容性测试、estimated 用量的预算/完成策略。

---

## 附录：本文引用的 Reasonix 材料

- `README.zh-CN.md`：定位、权限分档、可回退、四种用法。
- `docs/SPEC.md`：§3.5 双模型、§3.6 缓存优先上下文、§3.7 权限、§3.11 完成声明裁决、
  §3.12 写 claim、§3.14 fleet、§3.16 MCP 并发、§3.17 委派测量。
- `docs/BUDGET_USAGE_COMPLETENESS.md`：estimated / priced / 未知费用。
- `docs/CHECKPOINTS.md`：文件快照回退，bash 副作用不覆盖。
- `docs/ISOLATION_OPERATION_LEASE_SECURITY.md`：按 root + result ID 的 OS 租约。
- `docs/CONCURRENT_MUTATIONS_REVIEW_SERIES.md`：分批审查与切片纪律。
- `docs/MODEL_CONNECTION_HEALTH.md`：连接/余额检测、缓存与隐私边界。
- `docs/SESSION_MEMORY_RETRIEVAL.md`：指令、记忆、检索与缓存契约。
- `crates/contract/src/eventwire/usage.rs`：cache hit/miss、prefix 诊断、estimated、cost 完整性。
- `benchmarks/compaction/cache-prefix/README.md`：缓存命中、延迟与美元估算的真实测量。
- `docs/RUST_MIGRATION.md`：Rust + Vue + Tauri 重写计划；kernel 仍独立进程、wire parity、golden fixtures。
- [Let's Verify Step by Step](https://arxiv.org/abs/2305.20050)：过程监督、PRM800K、MATH 78%。
- [Prover-Verifier Games improve legibility of LLM outputs](https://arxiv.org/html/2407.13692v1)：小 verifier 检查强 prover，提升可核查性。
