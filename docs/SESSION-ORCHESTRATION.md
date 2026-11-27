# 多会话扫描、分配与调度

> 状态：已实现为 `crates/trit-host/src/session_*.rs`、`trit-host session-*` 与 `remote-ledger` CLI。
> 原则：写操作默认 dry-run；写目标不覆盖；只读适配的会话永不自动派发；预算耗尽 fail closed。

## 0. 一句话

扫描会话目录 → 生成确定性分配 → 自动/手动派发 → 并发执行 → 结果回填 → 对账防假完成。

这条链路只做“会话之间的事”，不替模型做语义判断，也不把 UI 状态写进内核。

## 1. 命令链

| 命令 | 作用 | 默认 |
| --- | --- | --- |
| `session-scan --root <dir> [--cache <path>]` | 扫描会话、任务、链接、健康信息 | 只读 |
| `session-plan --root <dir> [--cache <path>]` | 生成 resume/inspect/idle/skip 分配 | 只读 |
| `session-auto --root <dir> [--max-items N] [--cache <path>]` | 只自动派发 `resume` 项 | dry-run |
| `session-advance --root <dir> [--workspace <dir>] [--cache <path>]` | 一条命令：scan → plan → auto dispatch → fleet run → callback → verify | dry-run |
| `session-dispatch --root <dir> [--cache <path>]` | 手动派发全部 `resume` 项 | dry-run |
| `session-run --session <resume.jsonl> [--apply]` | 读取派发单并执行单任务 | dry-run |
| `session-callback --root <dir> [--apply]` | 写 parent 的 `delegations.jsonl` | dry-run |
| `session-callback-verify --root <dir>` | 对账回填记录与 child 当前状态 | 只读 |
| `session-fleet --root <dir> [--workers N] [--budget-tokens N] [--cache <path>]` | 生成依赖分层 + 预算 | 只读 |
| `session-fleet-run --root <dir> [--workers N] [--retry N] [--budget-tokens N] [--cache <path>]` | 并发执行 + 自动回填 + 对账 | dry-run |
| `session-fleet-remote --root <dir> --nodes <node-table.json> [--retry N] [--scope S] [--apply]` | 把 fleet 任务按节点表跨机派发 | dry-run |
| `remote-ledger --token-budget N` | 跨机 claim/complete 的 token 预算 | 预算不设则不限 |

## 2. 数据流

```text
SessionScanReport
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

## 5. 预算

- 本地：`--budget-tokens` 均分到 fleet 任务，预算小于任务数直接 fail closed。
- 跨机：`remote-ledger` 累计 `/complete` 帧里 `SubagentResponse.tokens`；达到预算后新 `/claim` 返回 503。
- 预算随状态文件回放，ledger 重启不清零。

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
- 基准：`cargo bench -p trit-host --bench session_scan`（200 会话 × 20 turn，全量 ~33 ms、缓存命中 ~13 ms）。

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

- `session-advance` 是唯一的自动闭环命令；`session-auto` 只生成派发单，不自动接 `session-fleet-run`。
- `session-advance` 的任务 id 使用 `<source>.run`，执行会话写到独立 `<source>.run.jsonl`，不覆盖原会话或派发单。
- `session-fleet-remote` 复用 `NodeTable` + `run_remote_plan_with_table`：节点容量、失败隔离、换节点重试与幂等键由现有层负责；跨机预算在 ledger 服务端生效。
- `--scope S` 把幂等键变成 `[scope|]task@epoch`：不同计划必须用不同 scope，否则同名任务会在共享 ledger 中互相去重；`session-advance` 会把 scope 透传到远程 fleet 阶段。
- 扫描缓存是 JSON 侧车，不是 SQLite 投影；`session_index` 仍只服务单个会话。
- 跨机预算只在 ledger 端累计，不把 usage 写回 remote-exec 的本地账本。
- UI/设置栏只消费这些 JSON，不参与内核判定。
