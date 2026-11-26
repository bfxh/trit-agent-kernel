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
- `FleetOutput.content` 是该轮 `TurnOutcome::render_markdown()` 的 Markdown 报告，
  含状态、步数预算、单轮指标与工具回执；结构化数据仍在 `FleetOutput.tokens` 与
  每任务 session JSONL 中；
- 写权限沿用 `WorkspaceConfig` / `WriteGuard`，不存在 fleet 旁路；
- `turn_spec_from_task`：`guidance` → `TurnSpec`，`produces.min_assertions` 与
  `max_tokens` 原样传递。

## 7. 子 agent IPC 契约（`trit_orch::subagent`）

帧格式：4 字节大端长度 + UTF-8 JSON，单帧上限 8 MiB。

- `SubagentRequest { task, epoch, capsule, profile }`；
- `SubagentResponse { task, epoch, status, content, tokens }`；`content` 是 UTF-8
  字节，参考实现写 `TurnOutcome::render_markdown()`；协议本身不限定内容格式；
- `validate_for` 强制响应回显 `task + epoch`，旧响应/串话直接拒绝；
- `exit_status`：零退出但无响应、非零退出、被信号/超时终止都不产生 `Completed`；
- `decode_frame` 支持半包与粘包，超限/非法载荷一律 `subagent_protocol` 错误。

宿主可用 stdin/stdout 管道、socket 或测试替身承载同一套帧。仓库内提供参考实现
`trit_host::subagent_process`：

- `SubagentProcessConfig { program, args, timeout }`（默认 30s）；
- `run_subagent`：spawn → 写请求帧并关 stdin（子进程看到 EOF）→ 读线程**有界**收集
  stdout（最多 `MAX_FRAME_BYTES + 4`，再多 1 字节只用于判定超限）、主线程
  `recv_timeout`；超时 `kill` + `wait` 并返回 `Aborted`；stdout 超限即 kill 并返回
  I/O 错误，不无限读；收到响应强制 `validate_for`；无响应走 `exit_status`；
- `trit-host subagent-echo`（`--fail` / `--silent` / `--sleep-ms` / `--task` /
  `--output-bytes`）是可执行的真实子 agent 替身，用于端到端回归（真实 spawn、
  超时 kill、无响应、引用不匹配、超大 stdout 五类）。

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
  幂等键 `X-Trit-Idempotency: [<scope>|]<task>@<epoch>`）→
  有界读响应 → `validate_for` → `DelegationResult`；
- 客户端**不跟随重定向**（3xx 直接记 `Failed`），单次调用**不自动重试**；传输
  失败/超时记 `Aborted`；非 2xx、空体、截断帧、超过 `MAX_RESPONSE_BYTES` 记
  `Failed`；解帧失败或 `task + epoch` 不匹配返回协议错误。以上都不产生
  `Completed`；跨机重试由 §9 的调度层显式开启。
- 参考服务端 `trit_host::remote_serve` 与 CLI `trit-host remote-echo`：一次
  连接一个 `POST`，`Content-Length` 有界（≤ 8 MiB + 4），可选 Bearer 校验
  （期望头启动时转 SHA-256 摘要，实际头摘要后用固定长度常量时间比较，不逐字节
  泄漏 token 前缀），
  `--once` / `--fail` / `--silent` / `--sleep-ms` 供端到端回归；带
  `X-Trit-Idempotency` 且与帧的 `task@epoch` 后缀一致时，按完整键（可带
  `<scope>|` 前缀）缓存首次响应（重试直接回缓存、不再执行），缓存同时限制 1024
  条与逻辑字节（默认 64 MiB，`--max-cache-bytes`，按 key + 帧估算）及保留时间
  （默认 1h，`--cache-ttl-ms` 可调、0 关闭；过期条目在命中或写入时清理），按写入
  顺序 FIFO 淘汰、单条超预算不缓存；头与帧不一致回 400。不同计划用不同 scope 时互不回放。启动先打印
  `{"bind":"127.0.0.1:PORT"}`，便于 `--bind 127.0.0.1:0` 取真实端口；
  `--max-connections` 默认 64，超额连接直接回 503、不再 spawn 线程。
  `GET /health` 是只读存活探针：配置 token 时沿用同一 Bearer，鉴权通过即回
  `200` 空体；不读任务帧、不触碰缓存/账本、不执行任务。请求行与头部有界：
  单行 8 KiB、总计 64 KiB、最多 64 个，超限直接 431、不继续缓冲；头部读取仍受
  30s socket 读写超时约束。请求行 + 头部 + 请求体另共享总读取时限（默认 60s，
  `--request-read-timeout-ms`，0 关闭），慢速滴流到点回 408、不进入执行。
  `RemoteMode::Exec` / `trit-host remote-exec (--workspace <dir> | --workspace-root <dir>) --script <json>`：
  每个请求 spawn 一个 `subagent-run` 子进程，请求帧走 stdin、响应帧走 stdout；
  只有子进程判定完成才回 `Completed`，进程错误按 `Failed` 回帧。权限仍由请求
  profile 与 workspace 决定（`ReadOnly` 拒绝写）。`--workspace-root` 按任务 ID
  建 `<root>/<task>` 独立目录；请求键带 `scope` 时建 `<root>/<scope>/<task>`，
  `--script` 与 args 里的 `{task}`/`{workspace}` 按请求展开；任务 ID 与 scope
  只允许 ASCII 字母数字、`-`、`_`、`.`，含路径分隔符或为 `.`/`..` 时直接回
  `Failed`、不创建目录。同一计划内任务 ID 仍需唯一；不同计划靠 scope 隔离。
- 共享幂等账本 `trit_host::remote_ledger`（CLI `trit-host remote-ledger`）：
  `POST /claim` + `POST /renew` + `POST /complete`，键是
  `X-Trit-Idempotency: [<scope>|]<task>@<epoch>`；
  complete 校验 `task@epoch` 后缀并保留 scope，因此同名任务在不同 scope 下各有
  独立的 claim/完成记录。另提供 `GET /health` 只读探针：走同一 Bearer，鉴权通过
  即回 200 空体，不读取请求体、不触碰 claim/complete 状态，供负载均衡或外部监控
  判断账本进程存活。
  claim 返回 `200` 空体 + `X-Trit-Lease` = 获得执行权，`200` + 帧 = 已完成、直接回放，
  `409` = 他人持有；`/renew` 与 `/complete` 必须携带同一个租约令牌，缺失或不匹配
  返回 `403`，因此过期接管后旧持有者不能再续租或写完成。完成记录保存写入者令牌：
  同一持有者重复 complete 是幂等 `204`，不同令牌即使键已完成也返回 `403`，不会
  让旧执行者误以为自己的结果获胜（旧格式没有令牌，仍按幂等成功兼容）。占位 TTL 默认 60s，过期允许接管；执行中的
  节点按 `ledger_renew_interval`（默认 10s，`--ledger-renew-ms`）`POST /renew`
  延长租约，未知键 404、已完成 204 + `X-Trit-Lease-Done: 1`、非持有者 403。
  心跳收到 403/404 或已完成标记会立即取消本机在途执行（睡眠或 Exec 子进程），
  服务端回 `Aborted`（带 `X-Trit-Lease-Lost: 1`）且不登记完成，避免旧持有者继续
  产生副作用；但网络分区使心跳完全不可达时，旧执行者仍可能跑完，这部分需任务
  幂等兜底。
  容量有键数（默认 4096）与字节（默认 64 MiB，`--max-state-bytes`）
  双重上限，键数含占位与完成项，字节按完成帧估算，超出按 key 顺序淘汰已完成项。
  `--state <path>` 打开追加日志：claim/续租先追加占位记录（key、令牌、绝对到期
  时间），complete 再追加完成帧与写入者令牌；写盘失败回 5xx 且不改内存状态。
  重启时逐行回放全部记录，按 key 覆盖旧状态，再按键数与字节上限裁剪；旧版
  没有 `lease` 的完成记录仍可加载，但只能按无主幂等成功处理。未过期占位的令牌与剩余
  TTL 一并恢复，过期占位直接丢弃；物理日志超过逻辑上限 1.5 倍时压缩为当前快照（完成项 + 未过期
  占位，临时文件 + 原子替换，启动时从残留 `.compact` 恢复），已完成任务不会再
  执行。未配 `--state` 时占位只在内存，账本重启仍会丢失在途令牌；状态文件含完成
  帧与租约令牌，应按敏感数据保护。同一状态文件只允许一个账本进程：绑定端口前在
  `<path>.lock` 上取 OS 排他锁，重复启动直接失败（`AddrInUse`），进程退出自动释放，
  避免两个实例各自回放后追加或压缩互相覆盖。连接并发默认 64（`--max-connections`），超额
  连接直接 503、不排队不 spawn；账本是跨机协调单点，不能由慢客户端耗尽线程。
  请求读取有界：请求行/头单行 8 KiB、总计 64 KiB、最多 64 个，超限 431；请求行 +
  头部 + 请求体共享总读取时限（默认 60s，`--request-read-timeout-ms`，0 关闭），
  慢速滴流到点回 408、不进入协议处理。
  服务端单请求执行时限：`RemoteServerConfig::exec_timeout` /
  `remote-echo|remote-exec --request-timeout-ms`（0 表示不限制）。deadline 从
  请求体解码完成后启动，覆盖账本 claim 等待、`sleep_ms` 与 `Exec` 子进程；请求行/
  头部读取与请求体上传阶段由请求读取总时限、30s socket 读写超时和上述头/体长度上限约束，不计入
  `exec_timeout`。到点先置
  `timed_out` 再取消与请求共用的令牌，最终强制 `Aborted`、清空内容，并跳过账本
  complete 与本地缓存（占位保留到 TTL）。响应带 `X-Trit-Deadline-Exceeded: 1`，
  客户端 `RemoteReply::deadline_exceeded` 转成调度层 `NodeOutcome::OverBudget`：
  计入 attempts/failures、连续失败清零、节点保持健康，并按 `retry_limit` 换节点；
  租约丢失与超时同时发生时以租约丢失优先。deadline 线程 spawn 失败时该请求不受
  时限约束；超时 `Aborted` 仍不代表远端一定没有产生副作用。
  `remote-echo --ledger <url>` 接入后，带键请求以账本为权威：账本不可达回 503
  且不执行；执行后 complete 失败也回 5xx。`--ledger-token` 用于 Bearer 鉴权
  （同样把期望头转 SHA-256 摘要后做常量时间比较），
  `--ledger-renew-ms` 调整续约间隔（必须小于账本 TTL）。

回归在 `crates/trit-host/tests/remote_subagent.rs`：loopback 服务端与真实
`remote-echo` 子进程覆盖鉴权（精确 token 通过，前缀/后缀/大小写/缺失均拒绝）、重定向、非 2xx、空体/截断/超限、引用不匹配、
超时与连接失败，以及幂等头发送、去重命中与键不一致拒绝、取消慢请求、连接上限
503、请求行/头长度超限 431、慢头/慢体触发请求读取总时限 408。`tests/remote_ledger.rs` 覆盖 claim/pending/完成回放/TTL 接管、租约续约、
慢执行期间不被接管、租约丢失中止在途执行、完成帧已由别人写入时旧执行者回 Aborted 而非旧结果、租约被接管不隔离节点且换机重试回放完成、过期后旧持有者不能续租/完成、共享账本下双节点只执行一次、账本不可达
时 fail-closed，以及落盘账本重启回放、占位租约跨重启恢复/过期丢弃、高频续租不挤掉完成帧、容量淘汰顺序回放一致、字节上限淘汰与压缩、账本连接上限 503、账本慢请求总读取时限 408、
同一状态文件只允许一个账本进程、`GET /health` 走同一 Bearer 且只读；`exec_timeout` 到点回 `Aborted` 且不登记完成、CLI 看门狗中止慢请求也各有回归；`tests/remote_exec.rs` 覆盖 `remote-exec`
真实写文件、`--workspace-root` 按任务隔离与非法任务 ID 拒绝、`run_remote_plan`
派发到真实执行器。

**仍然非目标（本批不做）**：动态节点发现（注册中心 / mDNS）与跨进程常驻周期
探测；单轮演练内的库级 `health_probe_on_start` / `health_probe_interval`
（CLI `remote-plan --probe` / `--probe-interval-ms`）已提供。静态
节点表、派发与有界换机重试见 §9。

## 9. 跨机器调度（`trit_orch::NodeTable` + `trit_host::run_remote_plan`）

DAG、worker 并发与失败传播仍由 `trit_orch::fleet`（`trit-schedule` runtime）负责；
这一层只补“任务放哪台机器、失败怎么记账”。

- `RemoteNode { id, max_in_flight, endpoint }`：静态节点表（发现 v1 由调用方/配置
  注入）；建表校验 id 唯一非空白、容量 ≥ 1、端点通过准入校验。
- `NodeTable`：按负载比 `in_flight / max_in_flight` 选最轻节点（平手按 id 升序）；
  各节点容量独立，满载时阻塞等待并每 25ms 复查取消令牌；取消返回 `Ok(None)`；
  `acquire_excluding(cancel, excluded)` 在故障重试时跳过已失败的节点。
- 故障隔离：`NodeOutcome::{Succeeded, TaskFailed, NodeFailed, Superseded}`。连续
  `failure_threshold`（默认 1）次节点级失败后该节点退出本轮调度；远端明确回
  `Failed` 只算任务失败，不隔离节点；带 `X-Trit-Lease-Lost` 的 `Aborted` 记
  `Superseded`，计入 attempts、不计失败并清零连续失败计数，不隔离节点；`NodeLease` 未
  `finish` 就 drop 按节点级失败记账，避免 panic 泄漏容量。
- `run_remote_plan(plan, nodes, config, cancel, build_request)`：在 fleet runner
  内申请节点 → 编码请求 → ureq POST → 归还；返回
  `RemotePlanReport { report, results, assignments, attempts, nodes }`：
  - `assignments`：`task -> node id`（重试时是最后一次尝试的节点）；
  - `results`：远端自报的精确状态（`Aborted` 不会在汇总里被改写成 `Failed`）；
  - `attempts`：每次尝试的 `{ node, status }`，按发生顺序；
  - `nodes`：`attempts / failures / healthy` 快照。
- **有界重试（默认 0，显式 opt-in）**：节点级失败（传输错误 / 超时 / 没有
  `X-Trit-Lease-Lost` 的 `Aborted` / 远端回 `Deferred`）、执行超预算
  （`OverBudget`，带 `X-Trit-Deadline-Exceeded`）或租约被接管（`Superseded`）
  才换节点重试，最多 `retry_limit` 次；任务级 `Failed` 不重试。
  `Superseded` 与 `OverBudget` 不隔离节点；前者不计失败并唤醒等待者，后者计入
  failures 与 attempts 但清零连续失败。租约接管后的重试请求会在账本上等待当前
  持有者并回放其完成帧。
  重试复用同一幂等键 `X-Trit-Idempotency: [<scope>|]<task>@<epoch>`。
  未配置共享账本时，参考实现只在**单个端点**内缓存首次响应（1024 条 / 64 MiB /
  默认 1h，FIFO 淘汰，`--cache-ttl-ms` 可调、0 关闭）；同键并发请求会在端点内排队，只有首个执行，其余等待并回放首次
  结果（等待受 `exec_timeout` 约束，未配置时最多等 30s；首个未缓存时下一个继续
  执行）。换到一台从未
  见过该 key 的机器会再次执行，因此默认语义是
  **at-least-once**。配置 `remote-ledger` 后，所有节点共享 claim/renew/complete，
  换机重试也能做到**完成结果至多一次**：先 claim 的节点执行，其余节点等待完成帧回放；
  账本不可达时 fail-closed（503）。配合 `--state` 时完成帧与未过期占位都跨重启
  保留；例外是 TTL 过期后的接管：老执行者可能仍在本地跑完（重复工作），但下一次
  心跳会看到租约失效或已完成标记并中止，完成写入也会被租约令牌拒绝，最终只记录
  当前持有者的结果。
- **取消传播**：`run_remote_subagent_reply_cancellable` 在独立线程执行请求，
  等待期间每 25ms 检查取消令牌；置位后向端点 origin 的 `/cancel` 发同一幂等
  键，最多再等 2s 收尾，超时按 `Aborted` 返回。服务端按幂等键登记在途请求：
  同一个键的并发请求全部登记，`/cancel` 会把它们全部置位；等待账本、`sleep_ms`
  与执行前都会检查，`Exec` 模式直接 kill 子进程；取消
  回合法 `Aborted` 帧并照常登记账本/本地缓存，后续重试不会再次执行同一键。
  跨机派发对应用户取消：在途任务以 `Aborted` 结束、容量按
  `NodeOutcome::Abandoned` 归还（不计 attempts/failures、不换节点重试）。
  边界：对端未实现 `/cancel` 时，客户端仍会在 2s 后返回，但远端请求会继续到
  端点超时，可能已经写盘；`Aborted` 不等于“远端一定没执行”。
- CLI `trit-host remote-plan [--input <json|->] [--health <path>] [--probe]
  [--probe-interval-ms N] [--probe-concurrency N] [--health-flush-ms N]`：输入
  `{revision, tasks, nodes, workers?, failure_threshold?, retry_limit?, scope?}`，把每个
  任务映射成只读 subagent 请求（capsule 的 `goal` 取任务引导或任务 ID），输出
  含 `completed/failed/deferred/results/assignments/attempts/nodes/probes/metrics` 的
  JSON 报告；`scope` 是计划命名空间，多个计划共享账本或 `--workspace-root`
  时必须给出不同值，否则同名任务会共用 key 与目录。用于跨机演练与运维冒烟。远端可以是 `remote-exec`（真实执行
  `subagent-run`）或任何实现同一帧协议的服务端。`--health` 读取上一次的节点
  快照（`healthy/attempts/failures`），跳过已隔离节点，并在本次结束后原子写回；
  文件里已删除的节点 ID 会被忽略，未知/损坏文件按错误退出。`--probe` 必须先配
  `--health`：派发前对健康文件里标记为隔离、且仍存在于本次节点表的节点逐个做
  `GET /health`，成功的调用 `NodeTable::readmit` 重新放行（保留累计 attempts/
  failures、清零连续失败），失败保持隔离；探测结果写进报告的 `probes`，探测本身
  不计 attempt/failure。`--probe-interval-ms`（需配 `--health`）会在本次
  `remote-plan` 运行期间启动周期监控：只探当前已隔离节点，成功后调用
  `NodeTable::readmit` 并唤醒等待容量的任务，失败保持隔离；事件带
  `phase: periodic` 写入 `probes`，最多保留 1024 条。`--probe-concurrency N`
  （需配 `--probe` 或 `--probe-interval-ms`，取值 1..=64）把一次 preflight/周期
  扫描内的已隔离节点探测并行化：只并行 `GET /health`，单个节点探测成功即
  `readmit` 放行、不必等最慢的探测返回；事件按本轮节点 id 升序整理后追加；
  未指定时沿用库级默认串行。这两项能力已下沉到库级：
  `RemotePlanConfig::with_health_probe_on_start` / `with_health_probe_interval`
  / `with_health_probe_concurrency`
  让 `run_remote_plan_with_table` 自己探测并把 `RemoteProbeEvent` 写入
  `RemotePlanReport::probes`；CLI 只负责把 flag 映射成配置、读写 `--health`
  文件。配 `--health` 时 CLI 默认每 30s 把当前节点表快照写回一次
  （`--health-flush-ms N` 可调、0 关闭；需配 `--health`），避免长演练只在
  正常结束才落盘：写回线程每次先把 `in_flight` 清零，再走同一 `.write.lock`
  合并协议；CLI 在计划结束后先停止并 join 写回线程，再做最终原子写回，因此
  崩溃最多丢最近一个周期的健康变化。库级调用方可直接用
  `spawn_remote_health_flusher(table, path, interval)` 获得同一能力。计划内的
  监控随计划结束退出；跨进程常驻恢复由 `trit-host remote-health` 承担：
  `--nodes <json> --health <path> [--interval-ms N] [--once]
  [--probe-concurrency N]` 每轮重读同一节点表与
  健康文件（`Vec<NodeSnapshot>`），只探已隔离节点，`GET /health` 成功即写回
  `healthy`（保留 `attempts/failures`、清零 `in_flight`），失败保持隔离并记录原因；
  默认常驻、每轮输出一行 JSON，`--once` 适合 cron / 任务计划程序。`--probe-concurrency`
  默认 4、上限 64，只并行探测、不改变单次写回；事件与快照仍按节点 id
  升序，`1` 为串行。节点增删或 token
  轮换无需重启守护，但节点文件应完整落盘：读取或解析失败会让守护 fail-closed 退出。
  文件不存在时按全部健康初始化；表里不存在的旧节点保留原快照但
  不再探测，并在输出的 `ignored` 中列出。守护持有 `<health>.lock` 上的 OS 排他锁，
  同一健康文件只允许一个实例，进程退出（包括崩溃）自动释放；所有写者
  （`remote-health` 与 `remote-plan`）先在 `<health>.write.lock` 上取短临界区，读回旧
  文件并合并本次未覆盖的节点（表外条目保留，按 id 升序），再写进程唯一临时文件并
  直接替换，避免整份覆盖别家节点、半截文件或读者看到目标短暂缺失；同一节点的并发
  更新仍以最后写入者为准。守护只复用现有判定标准，不自动拉起计划进程，也不替代 `remote-plan`
  的派发前 preflight。全隔离启动时周期监控来不及在首个 acquire 前放行，仍需先用
  `--probe` 做一次 preflight，否则派发会因没有健康节点立即失败。

回归：`crates/trit-orch/tests/remote_nodes.rs`（容量、最轻负载、阻塞/取消、
隔离阈值、排除集合、drop 记账、`Abandoned` 不记账、`OverBudget` 健康语义、
快照恢复、显式 `readmit`、JSON 默认值）、
`crates/trit-host/tests/remote_fleet.rs`（真实双节点均衡、无响应节点隔离、远端
任务失败不重试、换节点重试复用幂等键、重试上限、预取消、非法节点表、库级
preflight 探测重新放行隔离节点并返回 `probes`、preflight 有界并发探测且事件有序、
单节点恢复即放行不被最慢探测阻塞）、
`crates/trit-host/tests/remote_subagent.rs`（往返、鉴权、幂等去重、同键并发只执行
一次、缓存条数/字节预算与 TTL 过期、scope 隔离与非法 scope 拒绝、取消覆盖同键并发）、
`crates/trit-host/tests/subagent_process.rs`（取消 kill 慢子进程、stdout 超帧上限有界拒绝）、
`crates/trit-host/tests/remote_plan_cli.rs`（`remote-plan` 真实双节点演练、健康
文件跨运行隔离、计划写回保留节点表外的健康条目、运行期间用 `--health-flush-ms`
写回健康、`--probe` 成功放行/失败保持隔离、`--probe-interval-ms` 演练期间恢复节点、
`--probe-concurrency` 参数校验、重复节点 ID 与非法 scope 拒绝）、
`crates/trit-host/tests/remote_health.rs`（独立守护成功放行并保留累计计数、探测
失败保持隔离、未知节点保留原快照、保存时合并未提及条目、并发写者不丢条目、周期
写回线程持久化隔离与放行且清零 `in_flight`、有界并发探测且事件按 id 有序、
非法并发值拒绝、缺失文件
初始化、节点表热加载与单实例锁、`--once` CLI 输出）与 `crates/trit-host/tests/remote_ledger.rs`
（共享账本下双节点单次执行、scope 隔离）。

## 非目标

- 不把 fleet 依赖图塞进 `TaskSpec`：`TaskSpec.deps` 只有三元依赖，图的调度归属
  runtime；
- 不做动态节点发现、跨机抢占与无限自动重试；静态节点表内的派发、故障隔离与
  有界换机重试见 §9；
- 真实子 agent 进程的 spawn/超时 kill 由宿主实现，本批只固化 IPC 契约；
- provider 侧的模型调用本身（HTTP/SSE 接线）不在本批：本批把模型输出解析成
  `PlanHandoff`（`submit_plan`），发请求仍走既有 provider 路径。
