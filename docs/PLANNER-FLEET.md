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

宿主可用 stdin/stdout 管道、socket 或测试替身承载同一套帧。仓库内提供参考实现
`trit_host::subagent_process`：

- `SubagentProcessConfig { program, args, timeout }`（默认 30s）；
- `run_subagent`：spawn → 写请求帧并关 stdin（子进程看到 EOF）→ 读线程收集
  stdout、主线程 `recv_timeout`；超时 `kill` + `wait` 并返回 `Aborted`；收到响应
  强制 `validate_for`；无响应走 `exit_status`；
- `trit-host subagent-echo`（`--fail` / `--silent` / `--sleep-ms` / `--task`）是
  可执行的真实子 agent 替身，用于端到端回归（真实 spawn、超时 kill、无响应、
  引用不匹配四类）。

## 8. 跨机器执行（`trit_orch::RemoteEndpoint` + `trit_host` 远程 runner）

跨机器的载荷与进程内完全相同（subagent 帧）。准入仍由 `trit_orch::RemoteEndpoint`
负责，真实传输由宿主实现（`trit_host::remote_subagent`）。

- `RemoteEndpoint { url, auth_token, timeout_secs }`；
- 远程必须 `https://`；明文 `http://` 只允许 loopback
  （`127.0.0.1` / `localhost` / `::1`，含 IPv6 方括号）；
- 空 host（如 `https://`、`https://:8080`）、空白 token、0 超时一律
  `subagent_protocol` 错误；
- `run_remote_subagent`：`validate` → 编码单帧 → `POST`
  （`Content-Type: application/x-trit-frame`，可选 `Authorization: Bearer`，
  幂等键 `X-Trit-Idempotency: <task>@<epoch>`）→
  有界读响应 → `validate_for` → `DelegationResult`；
- 客户端**不跟随重定向**（3xx 直接记 `Failed`），单次调用**不自动重试**；传输
  失败/超时记 `Aborted`；非 2xx、空体、截断帧、超过 `MAX_RESPONSE_BYTES` 记
  `Failed`；解帧失败或 `task + epoch` 不匹配返回协议错误。以上都不产生
  `Completed`；跨机重试由 §9 的调度层显式开启。
- 参考服务端 `trit_host::remote_serve` 与 CLI `trit-host remote-echo`：一次
  连接一个 `POST`，`Content-Length` 有界（≤ 8 MiB + 4），可选 Bearer 校验，
  `--once` / `--fail` / `--silent` / `--sleep-ms` 供端到端回归；带
  `X-Trit-Idempotency` 且与帧一致时按 `<task>@<epoch>` 缓存首次响应（重试直接
  回缓存、不再执行），缓存上限 1024、FIFO 淘汰；头与帧不一致回 400。启动先打印
  `{"bind":"127.0.0.1:PORT"}`，便于 `--bind 127.0.0.1:0` 取真实端口。
  `RemoteMode::Exec` / `trit-host remote-exec --workspace <dir> --script <json>`：
  每个请求 spawn 一个 `subagent-run` 子进程，请求帧走 stdin、响应帧走 stdout；
  只有子进程判定完成才回 `Completed`，进程错误按 `Failed` 回帧。权限仍由请求
  profile 与固定 workspace 决定（`ReadOnly` 拒绝写）。
- 共享幂等账本 `trit_host::remote_ledger`（CLI `trit-host remote-ledger`）：
  `POST /claim` + `POST /complete`，键仍是 `X-Trit-Idempotency: <task>@<epoch>`。
  claim 返回 `200` 空体 = 获得执行权，`200` + 帧 = 已完成、直接回放，`409` = 他人
  持有；complete 用同一帧登记（`204`，重复登记幂等）。占位 TTL 默认 60s，过期
  允许接管；容量默认 4096，满时先淘汰已完成项。
  `remote-echo --ledger <url>` 接入后，带键请求以账本为权威：账本不可达回 503
  且不执行；执行后 complete 失败也回 5xx。`--ledger-token` 用于 Bearer 鉴权。

回归在 `crates/trit-host/tests/remote_subagent.rs`：loopback 服务端与真实
`remote-echo` 子进程覆盖鉴权、重定向、非 2xx、空体/截断/超限、引用不匹配、
超时与连接失败，以及幂等头发送、去重命中与键不一致拒绝。`tests/remote_ledger.rs`
覆盖 claim/pending/完成回放/TTL 接管、共享账本下双节点只执行一次、账本不可达
时 fail-closed；`tests/remote_exec.rs` 覆盖 `remote-exec` 真实写文件与
`run_remote_plan` 派发到真实执行器。

**仍然非目标（本批不做）**：动态节点发现（注册中心 / mDNS / 主动健康探测）。
静态节点表、派发与有界换机重试见 §9。

## 9. 跨机器调度（`trit_orch::NodeTable` + `trit_host::run_remote_plan`）

DAG、worker 并发与失败传播仍由 `trit_orch::fleet`（`trit-schedule` runtime）负责；
这一层只补“任务放哪台机器、失败怎么记账”。

- `RemoteNode { id, max_in_flight, endpoint }`：静态节点表（发现 v1 由调用方/配置
  注入）；建表校验 id 唯一非空白、容量 ≥ 1、端点通过准入校验。
- `NodeTable`：按负载比 `in_flight / max_in_flight` 选最轻节点（平手按 id 升序）；
  各节点容量独立，满载时阻塞等待并每 25ms 复查取消令牌；取消返回 `Ok(None)`；
  `acquire_excluding(cancel, excluded)` 在故障重试时跳过已失败的节点。
- 故障隔离：`NodeOutcome::{Succeeded, TaskFailed, NodeFailed}`。连续
  `failure_threshold`（默认 1）次节点级失败后该节点退出本轮调度；远端明确回
  `Failed` 只算任务失败，不隔离节点；`NodeLease` 未 `finish` 就 drop 按节点级
  失败记账，避免 panic 泄漏容量。
- `run_remote_plan(plan, nodes, config, cancel, build_request)`：在 fleet runner
  内申请节点 → 编码请求 → ureq POST → 归还；返回
  `RemotePlanReport { report, results, assignments, attempts, nodes }`：
  - `assignments`：`task -> node id`（重试时是最后一次尝试的节点）；
  - `results`：远端自报的精确状态（`Aborted` 不会在汇总里被改写成 `Failed`）；
  - `attempts`：每次尝试的 `{ node, status }`，按发生顺序；
  - `nodes`：`attempts / failures / healthy` 快照。
- **有界重试（默认 0，显式 opt-in）**：只有节点级失败（传输错误 / 超时 /
  `Aborted` / 远端回 `Deferred`）才换节点重试，最多 `retry_limit` 次；任务级
  `Failed` 不重试。重试复用同一幂等键 `X-Trit-Idempotency: <task>@<epoch>`。
  未配置共享账本时，参考实现只在**单个端点**内缓存首次响应（上限 1024，FIFO
  淘汰）；换到一台从未见过该 key 的机器会再次执行，因此默认语义是
  **at-least-once**。配置 `remote-ledger` 后，所有节点共享 claim/complete，
  换机重试也能做到**至多一次**：先 claim 的节点执行，其余节点等待完成帧回放；
  账本不可达时 fail-closed（503）。例外是占位 TTL 过期后的接管：若老执行者
  仍然存活并完成，会再登记一次，TTL 越长越安全。
- CLI `trit-host remote-plan [--input <json|->]`：输入
  `{revision, tasks, nodes, workers?, failure_threshold?, retry_limit?}`，把每个
  任务映射成只读 subagent 请求（capsule 的 `goal` 取任务引导或任务 ID），输出
  含 `completed/failed/deferred/results/assignments/attempts/nodes/metrics` 的
  JSON 报告；用于跨机演练与运维冒烟。远端可以是 `remote-exec`（真实执行
  `subagent-run`）或任何实现同一帧协议的服务端。

回归：`crates/trit-orch/tests/remote_nodes.rs`（容量、最轻负载、阻塞/取消、
隔离阈值、排除集合、drop 记账、JSON 默认值）、`crates/trit-host/tests/remote_fleet.rs`
（真实双节点均衡、无响应节点隔离、远端任务失败不重试、换节点重试复用幂等键、
重试上限、预取消、非法节点表）、`crates/trit-host/tests/remote_plan_cli.rs`
（`remote-plan` 真实双节点演练与重复节点 ID 拒绝）与
`crates/trit-host/tests/remote_ledger.rs`（共享账本下双节点单次执行）。

## 非目标

- 不把 fleet 依赖图塞进 `TaskSpec`：`TaskSpec.deps` 只有三元依赖，图的调度归属
  runtime；
- 不做动态节点发现、跨机抢占与无限自动重试；静态节点表内的派发、故障隔离与
  有界换机重试见 §9；
- 真实子 agent 进程的 spawn/超时 kill 由宿主实现，本批只固化 IPC 契约；
- provider 侧的模型调用本身（HTTP/SSE 接线）不在本批：本批把模型输出解析成
  `PlanHandoff`（`submit_plan`），发请求仍走既有 provider 路径。
