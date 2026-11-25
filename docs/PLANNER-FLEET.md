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

## 非目标

- 不把 fleet 依赖图塞进 `TaskSpec`：`TaskSpec.deps` 只有三元依赖，图的调度归属
  runtime；
- 不做跨机器调度；
- planner 的模型调用接线（provider 侧、CLI 暴露）不在本批：本批交付的是契约与
  执行原语，宿主按 `SessionPaths` + `ReadOnlyTools` 组合即可接入。
