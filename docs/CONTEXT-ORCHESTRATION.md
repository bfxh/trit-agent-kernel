# 上下文编排、会话地址与交互式 UI

> 状态：**提案（Proposal）/ WIP**。本文只定义问题、契约与设计空间，不承诺近期实现。
> 目标读者：内核、后端、前端、产品与后续接手团队。

## 0. 一句话

把“模型自己拉上下文”改成“编排层分配上下文”：模型只看到短句柄（如 `C1 C2`），
由宿主完成解析、权限、版本、预算与缓存；同时把会话/项目地址、调用缝合、长命令资源和
UI 展示都变成可审计的结构化事件。

---

## 1. 问题与现状

当前内核已经有：session JSONL、trace/replay、义务/证据裁决、checkpoint/rewind、资源预算。
但以下问题还没有统一模型：

| 问题 | 现状 | 代价 |
| --- | --- | --- |
| 复用上下文 | 内容反复进入 prompt 或由模型重新读取 | token、延迟、缓存未命中 |
| 上下文层级 | private session / project / 公共上下文没有显式契约 | 该给的没给，不该给的泄露 |
| 引用稳定性 | 路径、仓库名、备注会变；模型可能编造路径 | 解析失败、误写、难回放 |
| 推理过程 | 只有 trace 事件，没有统一的 reasoning summary | 难解释“为什么这样做” |
| 角色/模型调用 | 没有区分 agent role call 与 model call | 成本、缝合度、责任边界不清 |
| 死循环 | 依赖步数上限，缺少语义/状态级判断 | 空转、重复工具调用 |
| 长命令 | bash 输出是文本，等待编译不可视化 | UI 只能看日志，模型被迫轮询 |
| UI | 单线文本流，缺少内嵌交互卡片与时间线 | 信息密度低，前端难以接手 |

---

## 2. 设计原则

1. **编排分配，不交给模型自由拉取**：模型看到的是引用；展开由宿主做。
2. **引用优先于复制**：能传句柄就不传全文；哈希用于去重/缓存，不是唯一身份。
3. **稳定身份 + 可变显示路径**：ID 不可变；路径、备注、别名可改且留历史。
4. **分类只服务决策**：分类会改变路由/预算/UI 时才做；否则只打轻标签。
5. **边界 fail-closed，UI 降级**：引用错误不静默替换；UI 渲染失败退化为文本。
6. **缓存优先**：稳定前缀 + 动态尾段；不要把易变内容塞进系统前缀。
7. **所有调用可计数、可回放**：模型、角色、工具、资源、人类操作分开记账。
8. **不把原始 CoT 当产品数据**：默认记录摘要、决策、证据与引用，不默认存原始思维链。

---

## 3. 上下文引用与复用

### 3.1 模型可见的三种形态

| 形态 | 模型看到 | 适用 |
| --- | --- | --- |
| 内联片段 | 真实内容（可截断） | 小、稳定、必读 |
| 摘要 | 确定性摘要/语义摘要 + 引用 | 大块背景 |
| 句柄 | `C1` + 一行标签 | 可选、按需展开 |

推荐默认：**句柄 + 一行标签**。模型需要细节时调用 `context_read(C1, span)`；
宿主按预算返回切片，而不是一次性灌入全部内容。注意：**只有 tool calling 还不够**，
模型还必须具备 `context_read` 这类句柄展开能力，否则句柄不可用，必须降级为摘要/全文。

### 3.2 `ContextRef`

```text
ContextRef {
  id: "C7"                         // 模型可见的短句柄
  kind: Session | Project | Doc | Artifact | Resource | Receipt | Reasoning
  uri: "trit://project/<id>/doc/<id>?rev=<rev>"
  stable_id: UUID/ULID             // 不随改名变化
  revision: Version | ContentHash  // 可选
  content_hash: Option<Hash>       // 可选：去重/缓存，不暴露给模型
  label: "README 摘要"             // 一行语义提示
  scope: SessionPrivate | ProjectShared | OrgPublic
  sensitivity: Public | Internal | Secret
  tokens: Option<u32>
  expires_at: Option<Timestamp>
}
```

要点：

- **哈希可选**：内容寻址适合去重、缓存与完整性校验；敏感内容可用不透明 ID，
  不把哈希暴露给模型。二者可以并存：`stable_id` 管身份，`content_hash` 管复用。
- **模型不能发明路径**：模型只能引用宿主已注册的 `ContextRef`，不能直接写文件系统路径。
- **引用是权限边界**：解析前做 scope/ACL/sensitivity 检查。

### 3.3 校验管线（模型输入有问题时）

顺序固定，任一步失败都返回**带类型的错误码**，不静默替换：

```text
parse -> resolve -> ACL/scope -> revision/freshness -> size/token cap
      -> schema -> budget -> expand
```

- 未知句柄、过期 revision、越权、超预算、schema 不合法 → `ContextRejected { code, ref }`。
- 模型收到可修复错误（`unknown_ref` / `stale_ref` / `too_large`），而不是空内容。
- 展开结果写入 receipt：谁引用、展开多少 token、是否命中缓存，便于审计。

### 3.4 成本模型

一次引用展开的成本 = `prompt tokens + cache miss + tool roundtrip + latency`。
编排层需要同时优化：

- 稳定前缀命中（prompt cache / prefix cache）；
- 复用内容按 content hash 去重；
- 可选内容留在句柄层，按需读取；
- 大内容优先摘要，原文按 span 读取。

参考：Prompt Cache / SGLang RadixAttention / CAG / 上下文工程综述（见 §15）。

### 3.5 缓存优先、印象对比与用量校准（第一版实现）

- `CacheClass`：`StablePrefix` / `DynamicTail` / `Uncacheable`。稳定前缀按稳定 ID 排序，
  不随语义分数变化；`CacheReport` 记录 prefix/dynamic tokens、FNV 指纹、失效列表。
- `PriorImpression`：记录上一轮的 revision/content hash；对比结果为
  `Same / Changed / Unknown`。`Changed` 会使稳定前缀失效；`Unknown` 不假装命中。
- `AutonomyPolicy`（三值）：`may_add_optional`、`may_expand_handles`、`may_break_prefix`。
  `Unknown` 默认不当作允许；前缀变化在未获准时降级为动态尾段。
- `UsageCalibration`：从真实 `TokenUsage` 累计 cache hit/miss 与 output 峰值；
  至少 3 次精确观测才把 output 预留降到实测值；估算样本/模型不匹配时不信任。
- Provider 对接：`ContextPlan` 已进入 `ProviderRequest` 与 OpenAI-compatible 消息；
  `ContextPlanner` 每轮提供计划；`ProviderResponse.context` 与 session 的
  `Proposal.context` 记录 inclusion receipt 和“前缀指纹 → 实际 cache hit/miss”的对照。
  但 CLI 尚未配置 catalog/planner，`hit_potential` 仍需要和实际观测对照验证。
  `context_read` 也只在有 `ContextPlan` 时向模型暴露，避免 CLI 默认广告一个不可执行的工具；
  宿主启用 planner 时还必须用 `ContextReadTools` 包装工具执行器。
- 边界：token 仍是粗估；校准用最大值而非分位数；“印象”只比较 revision/hash，不做语义 diff。

### 3.6 非目标

- 不做通用向量数据库；检索只是 ref 解析的一种来源。
- 不自动把整个项目内联进 prompt。
- 不把“模型能自己读文件”当作免设计理由。

---

## 4. 会话、项目与请求批次

### 4.1 层级

```text
Org
└── Project (仓库/子项目)
    └── Session (对话)
        └── Turn
            └── RequestBatch (一次模型请求及其上下文)
                └── Call (model / agent role / tool / resource)
```

### 4.2 地址

- **稳定身份**：`SessionId = ULID`，创建后不变。
- **规范地址**：`trit://project/<project_id>/session/<session_id>`。
- **显示路径**：可编辑的 `display_path` / 备注 / 别名，类似“书签”。
- **改名/移动**：记录 `AddressEvent { moved, renamed, relabeled, at }`；
  旧别名保留解析重定向，避免历史引用断裂。
- **任务仓目标**：项目标签可以变；真正的锚是 `project_id + root commit / remote identity`。
- **模型只拿短别名**：`P1/S2`；UI 显示完整路径与备注。

### 4.3 上下文层级

| Scope | 默认 | 例子 |
| --- | --- | --- |
| `SessionPrivate` | 仅本会话 | 当前 turn、receipts、reasoning summary |
| `ProjectShared` | 项目内可选 | 项目约定、架构文档、历史任务 |
| `OrgPublic` | 显式选择 | 公共规范、模板、共享知识 |

动态组装时，每次请求都要有 **inclusion receipt**：包含哪些 ref、谁选的、为什么。
公共上下文必须显式选择，不能因为“看起来相关”自动注入。

### 4.4 动态组装

```text
prefix (稳定：system / tools / project rules)
+ selected ContextRefs (有序)
+ retrieved tail (按需)
+ current turn tail
```

- 易变内容放尾部，避免破坏缓存前缀。
- 组装结果可回放：相同 plan + refs + revision 应产生相同的序列化 payload。
- 句柄顺序、标签和摘要都要确定性生成。

---

## 5. 推理过程与决策记录

### 5.1 分开四种“过程”

| 类型 | 内容 | 是否默认展示 |
| --- | --- | --- |
| Model reasoning | provider 暴露的 thinking/summary | 可折叠 |
| Agent decision | 编排/选批/升级/重试的决策 | 是（结构化） |
| Tool/Resource event | 命令、资源、进度、结果 | 是 |
| Human action | 审批、修改、打断 | 是 |

### 5.2 记录策略

- 默认记录 **reasoning summary + 引用的证据 + 决策理由码**；
- 原始 reasoning 是否落盘由 provider 与策略决定，默认不作为产品数据；
- 每条记录带 `request_batch_id`、`session_id`、`project_id`、`actor_id`、`model_id`；
- 支持按项目/批次回放，支持脱敏与保留策略；
- 不把“模型的解释”当作事实；事实仍由 evidence rails / receipt 裁决。

Zed 的 Agent Panel 已把 thinking blocks、工具调用指示和 “Context Compacted” 条目
作为可展开的 UI 单元，这是一个可借鉴的展示形态（见 §15）。

---

## 6. 语义判断、分类与防死循环

### 6.1 分类轴（只在改变行为时使用）

| 轴 | 用途 | 例子 |
| --- | --- | --- |
| task kind | 选择 brief/工具/预算 | 修复、重构、检索、部署 |
| ref intent | 判断“要摘要还是要原文” | 概览、精确行、跨文件 |
| resource kind | 选择 UI 呈现 | 编译、测试、长任务、交互式命令 |
| loop suspicion | 触发升级/隔离 | 重复计划、重复工具、无进展 |
| UI card kind | 选择内嵌组件 | 表格、终端、diff、审批 |

分类实现分层：确定性规则/结构判断 → 轻量模型或 embedding router → 强模型 judge。
每层带置信度；低置信度返回 `Unknown` 并升级，不要硬猜。
优先用结构（AST、schema、工具名、状态哈希），不要靠提示词里的措辞表。

### 6.2 死循环判断

至少同时看：

1. **动作指纹**：`tool + normalized args + workspace state hash`；
2. **计划相似度**：连续 plan/目标语义重复；
3. **交接环**：agent role A→B→A 的重复；
4. **无进展预算**：连续 N 步没有新证据/新写入/新资源状态；
5. **资源等待**：命令仍在运行不等于死循环，必须区分 waiting 与 stuck。

触发后交给编排：`RequireEvidence` / `Quarantine` / `Escalate`，而不是让模型“再试一次”。
所有判定记录分类器版本、置信度、证据；允许人工反馈纠正误报。

---

## 7. 资源库与长命令可视化

长命令（编译、测试、安装、服务启动、交互式会话）应当是 `Resource`，不是聊天文本：

```text
Resource {
  id, kind: Command | Build | Test | Service | Job
  state: Queued | Running | Waiting | Succeeded | Failed | Cancelled
  progress: Option<0..100>
  owner: Session/Task
  started_at, updated_at, ttl
  log_ref: ContextRef
}
```

- 资源事件由宿主/进程监控产生；**不需要模型轮询**。
- UI 直接渲染进度条/终端/折叠日志；模型只在需要摘要时读取 `log_ref`。
- 编译等待、测试运行、长时间 shell 都走同一套状态机。
- 资源也可以有地址、别名和移动事件；改名不影响 `stable_id`。
- 若资源需要人类输入（确认、密码、选择），进入 human-in-the-loop 义务，而不是卡死。

---

## 8. 角色与模型调用分离 / “缝合度”

### 8.1 调用图

```text
CallEvent {
  call_id, parent_call_id,
  actor: Human | Orchestrator | Model | AgentRole | Tool | Resource | Policy
  kind: model_call | role_call | tool_call | resource_op | decision | approval
  started_at, ended_at,
  usage: { tokens, cache_hit, cost },
  status: ok | failed | in_doubt | cancelled
}
```

一个 `AgentRole` 调用可以包含多次 `model_call`、多次 `tool_call`；
一个 `Orchestrator` 决策不一定调用模型。三者必须分开计数。

### 8.2 指标（定义先于数字）

| 指标 | 定义 |
| --- | --- |
| `model_calls` | provider 推理请求次数 |
| `agent_role_calls` | 角色/子智能体调用次数（可包含多次模型） |
| `tool_calls` | 工具执行次数 |
| `handoffs` | actor 之间的控制权转移次数 |
| `stitch_ratio` | `model_calls / max(1, agent_role_calls)` |
| `interleaving_depth` | 单任务内 actor 交替的最大深度 |
| `human_interventions` | 人工审批/打断/改写次数 |
| `tokens_per_handoff` | 每次交接消耗的 token |
| `cache_hit_rate` | cached / (fresh + cached) |

“缝合度”不是一个标量，而是一组向量：调用次数、交接次数、交替深度、token/交接。
这样才能回答“是模型在干活，还是角色/编排在干活”。

### 8.3 UI：多轨时间线

不要单线聊天流。开发者视图应像剪辑软件的多轨时间线：

- 轨道：Human / Orchestrator / Model / AgentRole / Tool / Resource；
- 每个块是一次调用，宽度是时间，颜色是状态；
- 点击展开参数、usage、证据、错误类型；
- 支持缩放、过滤、按 request batch 对齐；
- 与 trace/replay 共用事件 ID，不复制状态。

用户视图可以是聊天内嵌卡片；开发者视图是时间线。两者共享同一事件源。

---

## 9. 交互式 UI 与 TokUI

### 9.1 参考形态

TokUI（JBoltAI，MIT）提供了“后端 DSL → SSE/WebSocket → 前端增量渲染”的流式 UI：

- 极简 DSL，Token 成本低于 HTML/JSON；
- 首个 token 即渲染，状态机增量解析；
- 组件覆盖工具调用、推理链、diff、终端、Agent 状态、Artifact；
- 事件处理器是命名引用，DSL 不含可执行代码；
- 未注册组件/渲染错误可降级。

这正好对应“有些东西显示在对话框里，有些不显示”的需求：**事件 → 组件映射由宿主决定**，
模型可以输出 UI 描述，但不能直接执行代码。

### 9.2 建议的边界

- 采用 TokUI 与否是**待定决策**；本文只留接口空间。
- 任何模型生成的 UI DSL 都必须经过 schema 校验与白名单；
- 组件注册表由宿主维护；未知组件降级为文本；
- 支持 `visibility: inline | collapsed | hidden`，用户可以关闭某类卡片；
- 无障碍、i18n、CSP、sanitization 是前端契约的一部分；
- 前端团队需要拿到：事件 schema、组件映射、状态机、错误降级、交互回调协议。

### 9.3 双表面

| 表面 | 面向 | 内容 |
| --- | --- | --- |
| 对话内嵌卡片 | 普通用户 | 审批、进度、diff、表格、结果卡 |
| 多轨时间线 | 开发者/运维 | 调用图、资源、缝合、回放 |

### 9.4 前端交接清单

如果交给前端/外包团队实现，至少交付这些契约，而不是只给截图：

- `CallEvent` / `Resource` / `ReasoningTrace` 的 JSON Schema 与示例；
- 事件到组件的映射表，以及未知组件的文本降级；
- 每种卡片的 `loading | empty | error | cancelled | stale` 状态；
- 多轨时间线的缩放、过滤、选中、回放接口；
- 回调协议（批准/拒绝/展开/重试/取消）与幂等要求；
- 安全：CSP、sanitization、禁止任意脚本、URL 白名单；
- 可访问性：键盘导航、焦点、屏幕阅读器、对比度；
- i18n：文案 key、日期/数字格式、RTL（若需要）；
- 性能：首 token 渲染、增量更新、长日志虚拟化；
- 兼容性：桌面/Web/TUI 哪些能力可选，缺省降级是什么。

---

## 10. 与现有代码的映射

> **实现状态（2026-11-15）**：第一刀已落在 `crates/trit-agent/src/context_layers.rs`：
> `ContextRef` / `ContextDocument` / `InMemoryContext` / `ModelProfile` / `ReasoningEffort` /
> `assemble_context`，覆盖 scope 校验、句柄/摘要/全文预算、工具/`context_read` 能力降级、
> 稳定前缀/动态尾段、`PriorImpression` 对比、三值 autonomy 闸门与真实 usage 校准。
> 语义分数仍由宿主传入（`ContextSelection.score`）。`ContextPlan` 已经进入
> `ProviderRequest`/OpenAI provider；宿主可通过 `ContextPlanner` 每轮提供计划，
> `ProviderResponse.context` 记录 inclusion receipt 与实际 cache 观察。CLI 还没配置
> catalog/planner，所以“自动组装”尚未成为默认路径；语义判断也仍是外部输入。

| 现有能力 | 可复用的部分 | 需要新增/扩展 |
| --- | --- | --- |
| `SessionLog` / `SessionEvent` | turn、tool、usage、compaction 事件 | `AddressEvent`、`CallEvent`、`ContextAssembly` |
| `ContextPolicy` / `compact_receipts` | receipts 保留与压缩 | `ContextRef` 注册、校验、展开 |
| `TaskBrief` | 任务 brief 的 guidance/obligations | brief 中携带 refs 与 selection receipt |
| `TraceLog` / `replay` | 轨迹、不变量、回放 | reasoning summary、调用图、多轨时间线索引 |
| `OrchestrationMetrics` | token/cache/concurrency 指标 | model/role/tool 调用数、handoffs、stitch 向量 |
| `ToolOutcome` / `ToolRequest` | 工具回执与写入路径 | `context_read` 工具、`Resource` 状态机、进度/等待事件 |
| `ProviderRequest` / OpenAI provider | brief、tools、receipts | `ContextPlan`、prefix/tail 消息、cache receipt |
| `InterventionPolicy` / 义务 | 隔离、升级、补证 | loop signal → intervention 的统一映射 |
| `checkpoint/rewind` | 工作区快照与回退 | 与 session/address 的关联（回退后 epoch/freshness） |

实现顺序建议：先加事件与引用注册表，再改 brief/组装；不要先做 UI。

---

## 11. 建议的数据契约（草案）

```text
ContextAssembly {
  request_batch_id, prefix_revision,
  refs: Vec<ContextRef>,
  inline: Vec<InlineSlice>,
  budget: { tokens, chars },
  inclusion_receipt: ReceiptId,
}

Address {
  stable_id, canonical_uri,
  display_path, aliases, labels,
  history: Vec<AddressEvent>,
}

ReasoningTrace {
  request_batch_id,
  summary, decisions: Vec<DecisionEvent>,
  citations: Vec<ContextRefId>,
  provider, model, confidence,
}

LoopSignal {
  kind, evidence: Vec<CallEventId>,
  classifier: { name, version }, confidence,
  action: RequireEvidence | Quarantine | Escalate,
}
```

不变量：

- 同一 `stable_id` 不能指向两个不同实体；
- 同一 `(context_ref, revision)` 的展开结果必须可复现；
- 没有 inclusion receipt 的公共上下文不得进入 prompt；
- 任何模型可见句柄都必须能解析；否则不进入请求；
- 事件 ID 全局唯一，回放不得重写历史。

---

## 12. 分阶段路线（只做“留空间”，不是承诺）

**P0：身份与可观测**
- 稳定 session/project ID + 地址别名 + 改名事件；
- `ContextRef` 注册表与校验管线；
- `CallEvent`（model/role/tool/resource 分开计数）；
- 资源状态机与长命令事件；
- 基础 loop guard（动作指纹 + 无进展预算）。

**P1：编排与展示**
- 动态上下文组装 + inclusion receipt；
- reasoning summary + request batch 关联；
- 多轨时间线（先开发者视图）；
- TokUI/自研渲染的 PoC 与降级路径。

**P2：语义与共享**
- 分层语义分类/路由（规则 → router → judge）；
- 公共上下文选择与 ACL 治理；
- 对话内嵌交互卡片；
- 跨项目上下文引用与缓存命中优化。

**暂不做**
- 模型管理/部署平台；
- 任意 HTML/JS UI 生成；
- 原始 CoT 长期存储；
- 完整的人类身份/组织权限系统；
- 用措辞匹配代替结构判断。

---

## 13. 评估

| 目标 | 指标 |
| --- | --- |
| 降 token | prompt tokens / task、cache hit rate |
| 降错误 | invalid_ref rate、stale_ref rate、loop incidents |
| 降人工 | human interventions / task |
| 可解释 | reasoning receipt 覆盖率、调用图完整率 |
| UI | 卡片降级率、交互完成率、首屏延迟 |
| 成本 | tokens_per_handoff、model_calls / role_calls |

离线用 JSONL + trace replay 做对照；线上先影子观察，不直接改变用户可见行为。

---

## 14. 开放问题

1. “模型管理上线”和“超微”具体指模型服务/硬件部署的哪一层？本文暂不覆盖。
2. 原始 reasoning 是否允许落盘？不同 provider 的 thinking 格式差异如何统一？
3. 公共上下文的治理：谁能发布、谁可见、如何撤回？
4. 哈希句柄 vs 不透明句柄：隐私与缓存去重如何权衡？
5. TokUI 作为依赖还是只借鉴 DSL？前端归属与版本策略。
6. 分类的粒度：哪些事件真的需要分类，哪些只需要轻标签？
7. 多租户下 session/project 地址与缓存隔离。
8. “国际/前端兼容”具体指国际化、编辑器宿主还是多端 UI？需要产品确认。

---

## 15. 参考

- [A Survey of Context Engineering for Large Language Models (arXiv:2507.13334)](https://arxiv.org/abs/2507.13334)
- [Lost in the Middle: How Language Models Use Long Contexts (arXiv:2307.03172)](https://arxiv.org/abs/2307.03172)
- [MemGPT: Towards LLMs as Operating Systems (arXiv:2310.08560)](https://arxiv.org/abs/2310.08560)
- [Prompt Cache: Modular Attention Reuse for Low-Latency Inference (arXiv:2311.04934)](https://arxiv.org/abs/2311.04934)
- [SGLang / RadixAttention (arXiv:2312.07104)](https://arxiv.org/abs/2312.07104)
- [Don't Do RAG: When Cache-Augmented Generation is All You Need (arXiv:2412.15605)](https://arxiv.org/abs/2412.15605)
- [RouteLLM: Learning to Route LLMs with Preference Data (arXiv:2406.18665)](https://arxiv.org/abs/2406.18665)
- [Uncovering Infinite Agentic Loops in LLM Agents (arXiv:2607.01641)](https://arxiv.org/abs/2607.01641)
- [AgentGUI: An Interface for Observing and Steering Long-Horizon Agents (arXiv:2607.26300)](https://arxiv.org/abs/2607.26300)
- [OpenTelemetry GenAI agent spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md)
- [Git Internals — Git Objects (content-addressable storage)](https://git-scm.com/book/en/v2/Git-Internals-Git-Objects)
- [TokUI — streaming UI DSL](https://tokui.jboltai.com/) · [GitHub](https://github.com/jboltai/tokui)
- [Zed Agent Panel](https://zed.dev/docs/ai/agent-panel) · [Zed thinking blocks issue](https://github.com/zed-industries/zed/issues/52536)
- [Anthropic prompt caching](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching) · [OpenAI prompt caching](https://platform.openai.com/docs/guides/prompt-caching)
- [JSON Schema](https://json-schema.org/)
