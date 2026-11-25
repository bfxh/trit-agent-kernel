# 跨进程写租约（Operation Lease）

`OperationLease` 给“谁在写这棵工作树”提供一个**跨进程**的先后权，并把
`epoch` 作为 fencing token 暴露给写入包装器。它补的是 `WriteClaim` 的短板：
claim 只证明“这个任务声明了这些路径”，不证明“现在没有另一个宿主在同时写”。

实现：`crates/trit-host/src/lease.rs`；集成点：`ClaimingTools::with_lease`。

## 1. 锁文件与所有权

租约是 `<root>/.agent/write-lease` 上的 OS 排他锁：

| 平台 | 锁 | 释放 |
| --- | --- | --- |
| Unix | `flock(LOCK_EX \| LOCK_NB)` | `flock(LOCK_UN)` / 进程退出 |
| Windows | `LockFileEx`（byte 0，`LOCKFILE_FAIL_IMMEDIATELY`） | `UnlockFileEx` / 句柄关闭 |

- 锁文件第 0 字节是**锁区占位**，JSON 内容从 offset 1 开始；这样 Windows 上
  其他句柄仍可以读内容（读写不整段落在锁区里）。
- 进程崩溃时内核释放锁，不需要 stale 检测。
- 锁粒度是**整个 root**，不是单路径；同一 root 同一时间只有一个持有者。

## 2. fencing token

```json
{"owner":"host-a","epoch":3,"acquired_at":1760000000,"released":false}
```

- 每次成功获取都把 `epoch + 1` 写回文件；`OperationLease::epoch()` 返回本次 token。
- `check()` 读取文件并比较 `owner`/`epoch`：不匹配或 `released=true` 即失效。
- `ClaimingTools::with_lease` 在**每个非只读调用前**调用 `check()`；失效则返回
  `blocked: write lease is no longer valid`，不执行内部工具。

```rust
let lease = OperationLease::acquire(&root, "host-a")?;
let tools = ClaimingTools::new(inner, claim).with_lease(lease);
```

获取失败（另一个持有者仍占锁）返回 `LeaseError::Busy { owner, epoch }`；不排队、
不等待。

## 3. 平台差异（实测）

| 行为 | Unix | Windows |
| --- | --- | --- |
| 第二个进程/句柄获取 | `EWOULDBLOCK` → `Busy` | `ERROR_LOCK_VIOLATION` → `Busy` |
| 外部改写租约文件 | `flock` 是 advisory，能改写；`check()` 通过 epoch 发现 | 写 byte 0 会被字节范围锁拒绝；truncate 后内容可能被清空，`check()` 失败 |
| 崩溃后恢复 | 锁随 fd 关闭释放 | 锁随句柄关闭释放 |

## 4. 与 WriteClaim 的关系

```text
WriteClaim          路径范围：写只能落在声明的文件/子树
OperationLease      时间范围：同一 root 同时只有一个写权威
ClaimingTools       两者都在写调用前检查；lease 失效优先 fail-closed
```

`ClaimingTools` 的检查顺序：

1. 非只读 + 绑定了 lease → `lease.check()`；失败直接 blocked；
2. 再过 `WriteClaim::allows_request`；`bash` 在 claim 下仍然 fail-closed；
3. 才调用内部执行器。

## 5. CLI

```bash
trit-host run --task "..." --workspace . \
  --write-claim src/report.md --write-claim tests/ \
  --write-lease host-a
```

| 参数 | 作用 |
| --- | --- |
| `--write-claim <path>` | 声明可写路径（可重复）；`write_file` 只能落在这些路径，`bash` 写入 fail-closed |
| `--write-lease <owner>` | 运行前获取 `<workspace>/.agent/write-lease`；需要至少一个 `--write-claim` |

- 获取失败（另一个持有者）→ 退出 `2`，stderr `write lease is busy: owner=... epoch=...`。
- 命中 claim 之外的写入 → 工具结果
  `blocked: write claim does not allow this path or shell command`，不执行。
- 租约在运行期间持续持有；进程退出自动释放，下一次运行 `epoch + 1`。

## 6. 诚实边界

- **本地文件系统**的租约，不是跨机器写权威；分布式存储需要在存储端校验 token。
- 只约束经过 `ClaimingTools` 的写入；绕过包装器直接写文件不在保护范围内。
- `Unix` 上租约文件内容可以被外部改写（advisory lock），`check()` 能发现但不会
  自动回滚已经发生的写入。
- 不包含：move 双端校验、mutation journal 对账、operation 级 lease（当前是 root 级）、
  内置 OS 沙箱。这些仍是显式未落地的能力。
