# 多会话扫描、分配与调度

> 状态：已实现为 `crates/trit-host/src/session_*.rs`、`trit-host session-*` 与 `remote-ledger` CLI。
> 原则：写操作默认 dry-run；写目标不覆盖；只读适配的会话永不自动派发；预算耗尽 fail closed。

## 0. 一句话

扫描会话目录 → 生成确定性分配 → 自动/手动派发 → 并发执行 → 结果回填 → 对账防假完成。

这条链路只做“会话之间的事”，不替模型做语义判断，也不把 UI 状态写进内核。

## 1. 命令链

| 命令 | 作用 | 默认 |
| --- | --- | --- |
| `session-scan --root <dir>... [--cache <path>]` | 扫描会话、任务、链接、健康信息 | 只读 |
| `session-plan --root <dir>... [--cache <path>]` | 生成 resume/inspect/idle/skip 分配 | 只读 |
| `session-auto --root <dir>... [--max-items N] [--cache <path>]` | 只自动派发 `resume` 项 | dry-run |
| `session-advance --root <dir>... [--workspace <dir>] [--cache <path>] [--budget-cny F]` | 一条命令：scan → plan → auto dispatch → fleet run → callback → verify | dry-run |
| `session-dispatch --root <dir>... [--cache <path>]` | 手动派发全部 `resume` 项 | dry-run |
| `session-run --session <resume.jsonl> [--apply]` | 读取派发单并执行单任务 | dry-run |
| `session-callback --root <dir>... [--apply]` | 写 parent 的 `delegations.jsonl` | dry-run |
| `session-callback-verify --root <dir>...` | 对账回填记录与 child 当前状态 | 只读 |
| `session-fleet --root <dir>... [--workers N] [--budget-tokens N] [--cache <path>]` | 生成依赖分层 + 预算 | 只读 |
| `session-fleet-run --root <dir>... [--workers N] [--retry N] [--budget-tokens N] [--budget-cny F] [--cache <path>]` | 并发执行 + 自动回填 + 对账 | dry-run |
| `session-fleet-remote --root <dir>... --nodes <node-table.json> [--retry N] [--scope S] [--apply]` | 把 fleet 任务按节点表跨机派发 | dry-run |
| `remote-ledger --token-budget N [--cny-budget F]` | 跨机 claim/complete 的 token/人民币预算 | 预算不设则不限 |

## 2. 数据流

```text
SessionScanReport
  root:       第一个扫描根（兼容单根调用）
  roots[]:    全部扫描根；多根结果合并成一份报告
  sessions[]: id/role/format/project/tasks/open_turn/in_doubt/last_gate
  links[]:    planner_executor | parent_child

SessionPlanReport
  items[]: priority + action(resume|inspect|idle|skip) + task + links
  unassigned[]: 链接指向未扫描会话

DispatchReport
  items[]: source_session/task/target_path/action/applied
  skipped[]: 非 resume、非 Trit、无 task

SessionFleetReport
  tasks[]: id/task/deps/workspace/parent/max_tokens
  layers[][]: 同层可并发
  cyclic[]: 有环任务，不调度

DelegationResult（parent 目录 delegations.jsonl）
  parent/child/task/status/steps/child_records/child_modified_ms
```

## 3. 分配与派发的物理门

- 只有 `action = resume` 且 `format = trit` 的会话会被派发。
- `in_doubt > 0`、最后一次门链阻断、坏行 → `inspect`，不自动续跑。
- Reasonix `role/content` 会话只读适配；`session-auto` 永不派发它们。
- 派发单写入 `<source>.resume.jsonl`；目标已存在时改用 `.resume.<n>.jsonl`，绝不覆盖。
- 派发单只写一条 `TurnStarted`，`objective` 记录来源会话。
- `session-run` 写到新的 `<resume>.run.jsonl`，不污染派发单。
- `session-fleet-run` 每任务写到 `<task>.jsonl`，写权限沿用 `WorkspaceConfig`（默认只读）。

## 4. 执行与失败语义

- 调度复用 `trit_orch::fleet::execute` + `trit-schedule` runtime。
- `workers` 是并发上限；依赖来自 `parent_child` / `planner_executor` 链接。
- 第一个任务失败后不再启动新任务；未启动项进 `cancelled` / `deferred`。
- `--retry N` 只重跑 `deferred + failed` 子集，移除集合外依赖后重新分层。
- Ctrl-C 接 `CancelToken`（Unix SIGINT / Windows SetConsoleCtrlHandler），协作式取消。
- 每个任务独立 provider/model，但共享 `ureq::Agent` 连接池。
- 内核并发：同一 proposal 内全是只读工具（`read_file`/`search_code`/`glob`）时走 `ToolExecutor::execute_batch` 并行执行，写工具与 `bash` 保持串行；多任务并发由 `trit_orch::fleet` 的 `workers` 控制。
- `tools_batch`：一次调用执行 1..=64 个只读工具（`read_file`/`search_code`/`glob`/`download`），返回按调用截断的紧凑结果，减少模型往返与上下文开销；写工具与 `bash` 不进批量。
- MCP 路由（`McpToolClient::routes`）有 5s 缓存与 `invalidate`，避免高频工具调用重复重建。
- GPU：内核与 CLI 不承载模型推理，GPU 加速属于 provider 侧；本层只做线程级并行，不对本地推理做承诺。

## 5. 预算与成本

- 本地：`--budget-tokens` 均分到 fleet 任务，预算小于任务数直接 fail closed。
- 跨机：`remote-ledger` 累计 `/complete` 帧的 token 与美元（帧携带 `model + usage` 时按价目表折算）；达到 `--token-budget` 或 `--cny-budget` 后新 `/claim` 返回 503。
- 预算随状态文件回放，ledger 重启不清零。
- 成本模型（`crates/trit-host/src/cost.rs`）：
  - `FleetRunOutcome` 汇总本次执行（含重试）的真实 `usage`，按模型单价折算 `cost.usd`；
  - 内置常见模型价目表（美元 / 百万 token，cache 命中单独计价）；`TRIT_MODEL_PRICES=<json>` 可覆盖或补充，文件读不到/非法时 fail closed；
  - 未知模型返回 `null` 并写 warning，绝不按 0 计价；
  - `--budget-cny F` 在已花达到 80% 时写 `warnings` 与结构化 `budget_alert`，不改变执行行为；超过预算时 `exceeded = true`。
  - 默认货币为人民币：价目表仍按美元/百万 token 维护，展示与预算按 `TRIT_USD_CNY`（默认 7.2）折算；`--budget-usd` 保留为兼容别名。
  - 跨机：子 agent 帧可选携带 `model` + `usage`（`SubagentUsage`）；宿主 `RemoteFleetOutcome` 输出 `usage` / `cost_usd` / `warnings`，ledger 用同一价目表累计美元。
  - `AdvanceReport.cost` 汇总本地 `run.cost` 与跨机 `remote.cost_cny`（人民币）；两者都无定价时为 `null`。

## 6. 结果回填与对账

- 执行前写 `<task>.jsonl.meta`（`id` + `parent`）。
- `session-fleet-run --apply` 执行后自动调用 `session-callback`（非 dry-run）。
- `delegations.jsonl` 是侧车，不修改原始会话日志。
- 幂等键：`parent + child + child_records + child_modified_ms`。
- `session-callback-verify` 报告 `ok / stale / mismatch / missing_child / missing_delegation`：
  - `stale`：状态一致但 child 日志在回填后继续增长；
  - `mismatch`：记录状态与当前 child 状态不一致，防“假完成”。

## 7. 性能

- 扫描缓存：`--cache` 按 `size + modified_time` 复用 `SessionEntry`；无变化时不重写缓存。
- 连接池：OpenAI provider 与 remote ledger/subagent 按 timeout 复用 `ureq::Agent`。
- 句柄复用：`LocalTools` 缓存 workspace `Dir`；`SessionLog` 缓存追加文件句柄。
- 工具目录：`default_tools()` 用 `OnceLock` 缓存。
- 基准：`cargo bench -p trit-host --bench session_scan`（200 会话 × 20 turn；全量约 62 ms、缓存命中约 27 ms；4 root × 50 会话并行扫描约 23 ms；200 文件写批量约 65 ms；数字随机器变化，仅作回归参照）。

## 8. 测试

```bash
cargo test -p trit-host --lib session
cargo test -p trit-host --test session_scan_cli
cargo test -p trit-host --test session_plan_cli
cargo test -p trit-host --test session_dispatch_cli
cargo test -p trit-host --test session_run_cli
cargo test -p trit-host --test session_callback_cli
cargo test -p trit-host --test session_verify_cli
cargo test -p trit-host --test session_fleet_cli
cargo test -p trit-host --test session_fleet_run_cli
cargo test -p trit-host --test remote_ledger
cargo bench -p trit-host --bench session_scan
```

## 9. 当前边界

- `session-advance --apply` 在本地执行前检查写日志：存在未闭合的 `begin`（上次崩溃）时 fail closed，提示先运行 `journal-status --rollback`；`--nodes` 跨机分支不检查本地 workspace。
- `session-advance` 的任务 id 使用 `<source>.run`，执行会话写到独立 `<source>.run.jsonl`，不覆盖原会话或派发单。
- `session-fleet-remote` 复用 `NodeTable` + `run_remote_plan_with_table`：节点容量、失败隔离、换节点重试与幂等键由现有层负责；跨机预算在 ledger 服务端生效。
- `--scope S` 把幂等键变成 `[scope|]task@epoch`：不同计划必须用不同 scope，否则同名任务会在共享 ledger 中互相去重；`session-advance` 会把 scope 透传到远程 fleet 阶段。
- 扫描缓存是 JSON 侧车，不是 SQLite 投影；`session_index` 仍只服务单个会话。
- `--root` 可重复：多个根合并成一份报告（`root` = 第一个，`roots[]` = 全部），跨根链接照常建立；缓存仍按文件绝对路径复用，重复根不会重复计数。
- 成本模型同时覆盖本地 fleet 与跨机帧：远端自报 `model + usage`，ledger 用同一价目表累计美元并可用 `--cny-budget` fail closed；未上报 usage 的旧节点不计价。
- 跨机预算（token 与美元）只在 ledger 端累计，不把 usage 写回 remote-exec 的本地账本。
- UI/设置栏只消费这些 JSON，不参与内核判定。
