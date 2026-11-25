# planner / executor 与 fleet（B7.6）

三块能力，各自有独立不变量与可复算验收。它们**不引入第二套调度**：依赖语义、
并发上限、失败传播全部复用 `trit-schedule` 的 runtime。

| 能力 | 入口 | 不变量 |
| --- | --- | --- |
| 委派契约 | `trit_orch::delegation` | 权限单调、显式继承、结果可引用、委派有收益 |
| fleet 执行 | `trit_orch::fleet` | 依赖顺序、并发上限、失败传播、稳定引用 |
| 会话分离 | `trit_host::separation` | 只读工具集、独立 session、独立缓存账本 |

## 1. 委派契约（`trit_orch::delegation`）

`SubagentProfile` 描述一个子智能体能做什么：

- `access`：`ReadOnly` / `ReadWrite`；只读 profile 声明写根直接报错
  （`delegate_read_only_writes`）。
- `write_roots`：workspace 相对子树。绝对路径、盘符、反斜杠、`.`、`..`、空段一律
  拒绝（`delegate_write_root`）；`permits_write` 只放行子树内的规范化路径。
- `is_subset_of(parent)`：子 profile 的写权限、工具白名单、token 预算都不能超过父
  （`delegate_privilege_escalation`）。子只能比父更窄。

`ContextCapsule::inherit(entries, allowed, budget)` 是“显式继承”的唯一入口：

- 白名单外的键 → 错误，**不静默丢弃**（静默丢弃会把隐式继承藏起来）；
- 重复键 → 错误；超过字节预算 → 错误，**不截断**；
- 通过后按键排序，同一输入得到同一胶囊。

`DelegationResult` 给出稳定引用 `task@epoch#digest`：

- `digest_hex` 是 FNV-1a 64 位（16 位十六进制），用于稳定引用而非密码学校验；
- `epoch` 变化后旧引用自然失效；
- 只有 `Completed` 的 `reusable()` 为 true；`Failed` / `Aborted` / `Deferred` 都不可
  作为下游输入或完成证据。

`should_delegate` 用指标回答“值不值得并行”，判据可复算：

1. 少于两个并行子任务 → 不委派；
2. 胶囊超过预算 → 不委派（先裁上下文，不是先并行）；
3. `steps_per_task + coordination_steps >= serial_steps` → 不委派。

## 2. fleet 执行层（`trit_orch::fleet`）

`execute(plan, FleetConfig, CancelToken, runner)` 把 `Plan` 的每个 `TaskSpec` 映射成
`trit_schedule::runtime::RuntimeTask`：

- `deps` 原样传入，三元语义由 runtime 执行：`True` 必需、`False` 可忽略、`Unknown`
  延迟并沿必需依赖传播；
- `FleetConfig { workers, capacity }` 直接成为 runtime 的并发上限；
- 任务失败后 runtime 不再启动新任务；fleet 把未启动任务归类：
  - `CancelToken` 已置位 → `Aborted`；
  - 上游失败 / `Unknown` 依赖 → `Deferred`；
- 每个任务的产出映射为 `DelegationResult`，`FleetReport::references()` 给出
  `task -> task@epoch#digest` 的稳定视图；`FleetReport::reusable(task)` 只对
  `Completed` 返回 true；
- `FleetReport::metrics` 携带 runtime 统计（started/completed/failed/deferred/
  cancelled/peak_usage），作为并发与成本基线。

## 3. planner / executor 分离（`trit_host::separation`）

- `ReadOnlyTools<T>` 把任意 `ToolExecutor` 收窄为只读白名单
  （`read_file` / `grep` / `glob` / `ls`）。非白名单工具或 `read_only == false` 的
  请求在**转发之前**被拒绝（`AgentError::ToolDenied`），内层执行器看不到写请求。
- `SessionPaths::under(dir)` 给出 `planner.jsonl` 与 `executor.jsonl`；两者必须不同，
  `Session::open` 只从自己的记录重建状态。
- 两个 session 各自持有独立 `CacheLedger`（不同 `Arc`）：planner 的研究上下文不会
  污染 executor 的缓存命中率统计，反之亦然。

装配示例：

```text
let paths = SessionPaths::under(session_dir);
let planner = Session::open(planner_model, ReadOnlyTools::new(planner_tools), paths.planner)?;
let executor = Session::open(executor_model, executor_tools, paths.executor)?;
```

## 验收（可复算）

```bash
# 委派契约：权限单调 / 显式继承 / 结果引用 / 委派收益
cargo test -p trit-orch --lib delegation

# fleet：依赖顺序 / 并发上限 / 失败传播 / 取消归类 / 引用稳定
cargo test -p trit-orch --test fleet

# 会话分离：工具隔离 / 会话隔离 / 缓存账本隔离
cargo test -p trit-host --test separation
```

## 4. 计划交接（`trit_orch::PlanHandoff`）

planner 产出、executor 消费的结构化交接物，**构造即校验**：

```text
PlanHandoff { revision, tasks, capsule, profile }
  new(...)     -> Plan::new 校验依赖图 + SubagentProfile::validate 校验执行契约
  digest()     -> 内容摘要（FNV-1a 64）
  reference()  -> plan@revision#digest
  into_plan()  -> 还原 Plan，交给 fleet::execute
```

- 构造时保存排序后的规范形式：同一组任务不同输入顺序得到同一 digest；
- 未知依赖、环、只读 profile 带写根都在构造期拒绝，失败不产生交接物；
- JSON 往返后 digest 不变，可直接作为交接产物落盘或跨进程传递。

planner 侧的组合方式：`ReadOnlyTools` 包执行器、`ReadOnlyCatalog` 包工具目录、
独立 `SessionPaths`；产出计划后用 `PlanHandoff::new` 固化。

## 5. CLI：`trit-orch fleet-run`

stdin 收 JSON、stdout 回 JSON，用于确定性演练（**不执行外部命令**）：

```json
{"revision": 1, "workers": 2, "fail": ["b"], "cancel_after": 1, "tasks": [
  {"task": {"id": "a", "duration": 1}, "requirements": {"cpu": 0, "memory": 0}, "max_tokens": 1000}
]}
```

- `fail` 指定的任务返回失败，用来验证下游归入 `deferred`；
- `cancel_after` 在执行满 N 个任务后触发取消，用来验证 `cancelled`（Aborted）；
- 输出 `completed` / `failed` / `deferred` / `cancelled` / `references` /
  `metrics` / `metrics_prometheus`；`references` 是 `task -> task@epoch#digest`。

## 6. 宿主执行器接线（`trit_host::fleet_runner`）

fleet 的 runner 由宿主提供，内核不假设模型或工具的实现：

```text
FleetJob::run(task, cancel) -> Result<FleetOutput, String>
run_fleet(plan, config, cancel, job) -> FleetReport
HeadlessFleetRunner::new(workspace, dir, model_factory)
```

- `run_fleet` 把 job 放进 `Mutex` 后交给 `fleet::execute`，并发上限与失败传播仍由
  runtime 决定；
- `HeadlessFleetRunner` 给每个子任务一个 `<dir>/<task-id>.jsonl`，用 `run_headless`
  在调用方给的 `WorkspaceConfig` 里跑真实一轮（模型 + 本地工具）；
- 写权限沿用 `WorkspaceConfig` / `WriteGuard`，不存在 fleet 旁路；
- `turn_spec_from_task`：`guidance` → `TurnSpec`，`produces.min_assertions` 与
  `max_tokens` 原样传递。

## 7. 子 agent IPC 契约（`trit_orch::subagent`）

帧格式：4 字节大端长度 + UTF-8 JSON，单帧上限 8 MiB。

- `SubagentRequest { task, epoch, capsule, profile }`；
- `SubagentResponse { task, epoch, status, content, tokens }`；
- `validate_for` 强制响应回显 `task + epoch`，旧响应/串话直接拒绝；
- `exit_status`：零退出但无响应、非零退出、被信号/超时终止都不产生 `Completed`；
- `decode_frame` 支持半包与粘包，超限/非法载荷一律 `subagent_protocol` 错误。

宿主可用 stdin/stdout 管道、socket 或测试替身承载同一套帧；真实进程 spawn 与超时
kill 属于宿主实现。

## 非目标

- 不把 fleet 依赖图塞进 `TaskSpec`：`TaskSpec.deps` 只有三元依赖，图的调度归属
  runtime；
- 不做跨机器调度；
- 真实子 agent 进程的 spawn/超时 kill 由宿主实现，本批只固化 IPC 契约；
- provider 侧的模型调用本身（HTTP/SSE 接线）不在本批：本批把模型输出解析成
  `PlanHandoff`（`submit_plan`），发请求仍走既有 provider 路径。
