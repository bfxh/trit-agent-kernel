# 门链（Gate Chain）

`gate-chain/1`：把仓库身份门、交接包只读预检与对齐门串成一条**只读、stop-first**
的链。它回答一个很窄的问题：

> 现在这个仓、这个交接包、这份任务清单，是不是同时满足派发前提？

它**不**新造第四套判据：每一步都调用已有实现（`trit-orch repo-gate`、
`trit-orch delivery check`、`trit-orch alignment`），只负责顺序、停链与汇总。

实现：`crates/trit-orch/src/gate_chain.rs`；CLI：`trit-orch gate-chain`。

## 1. 为什么单独串一条

三扇门分开跑，常见两类事故：

- **前门没过，后门已经跑过**：串仓的仓库仍然跑了对齐检查，调用方看到“对齐通过”
  就以为任务可以派；
- **`Unknown` 被当成通过**：清单缺失、交接包状态哈希不一致，报告里写着 `unknown`，
  但流水线只看“没报错”。

门链把顺序固定成 `repo → delivery → alignment`，只有 `True` 放行；任何一步是
`Unknown` / `False` 都立即停链，后续门在报告里记为 `skipped`，不再执行。

## 2. 三道门各问什么

| 顺序 | 门 | 调用 | 失败意味着 |
| --- | --- | --- | --- |
| 1 | `repo` | `repo::gate(root, expected, task_kind)` | 这不是能接智能体任务的仓，或与 `expected` 不一致 |
| 2 | `delivery` | `delivery::check(root, packet, receiver_state_hash, at)` | 交接包结构不合法（`False`）、过期或状态哈希不一致（`Unknown`） |
| 3 | `alignment` | `alignment::check(root, manifest, options)` | 清单断链（`unaligned`）或无法判定（缺失/坏 JSON/schema 不符） |

`delivery` 用的是**只读** `check`，不是会落账的 `gate`：门链一个字节都不写，
包括 `.agent/delivery/`。需要补 `sent + delivered` 时仍单独跑
`trit-orch delivery gate`，这样“判断”和“落账”两步分开，失败重跑不会留下半截账本。

## 3. 报告

```json
{
  "schema": "gate-chain/1",
  "status": "passed",
  "exit": 0,
  "root": ".",
  "steps": [
    {"id": "repo", "gate": "True", "state": "passed", "reason": "", "detail": {}},
    {"id": "delivery", "gate": "True", "state": "passed", "reason": "handoff gate passed; receipt missing", "detail": {}},
    {"id": "alignment", "gate": "True", "state": "passed", "reason": "fallback: alignment aligned (must score 1.00, 0 failure(s))", "detail": {}}
  ],
  "counts": {"passed": 3, "blocked": 0, "unknown": 0, "skipped": 0}
}
```

字段：

| 字段 | 含义 |
| --- | --- |
| `status` | `passed` / `blocked` / `unknown`；`blocked` 优先于 `unknown` |
| `exit` | `0` 全链通过、`1` 有明确拒绝、`3` 有无法判定的门 |
| `steps[].gate` | 该门的三值裁决：`True` / `Unknown` / `False` |
| `steps[].state` | 链上语义：`passed` / `blocked` / `unknown` / `skipped` |
| `steps[].reason` | 一句话原因；`alignment` 取第一条 failure |
| `steps[].detail` | 该门自己的原始报告（`ScopeDecision` / `DeliveryCheck` / `AlignmentReport`） |
| `failed_at` | 第一个没有放行的门；全通过时缺省 |
| `counts` | 各状态步数，便于 CI 直接读计数 |

`skipped` 只出现在停链之后。它不会被算进 `passed`，所以“只跑了第一道门就失败”
不会长得像“三道门都通过”。

## 4. CLI

`trit-orch gate-chain` 从 stdin 读一个 JSON 对象，stdout 输出报告 JSON：

```json
{
  "root": ".",
  "expected": "bfxh/trit-agent-kernel",
  "task_kind": "agent",
  "packet": ".agent/handoff/h-001.json",
  "receiver_state_hash": "H",
  "at": 0,
  "manifest": ".agent/alignment.json",
  "changed": ["crates/trit-orch/src/gate_chain.rs"],
  "changed_known": true,
  "strict": true,
  "executed": ["cargo test -p trit-orch --test gate_chain"]
}
```

| 字段 | 缺省 | 说明 |
| --- | --- | --- |
| `root` | `.` | 所有相对路径（`packet`、`manifest`）的基准 |
| `expected` | 空 | 空表示只校验 identity 有效性与 agent runtime，不校验 owner/repo |
| `task_kind` | 空 | `agent` / `agent-runtime` / `智能体` 要求 `owns_agent_runtime=true` |
| `packet` | 空 | 空表示 `delivery` 门未配置，按 `Unknown` 停链（fail-closed） |
| `receiver_state_hash` | 空 | 交接门要求与包里的 `state_hash` 一致 |
| `at` | `0` | `0` 由 delivery 侧取当前 Unix 秒 |
| `manifest` | `.agent/alignment.json` | 对齐清单；缺失是 `Unknown`，不是 `aligned` |
| `changed` / `changed_known` | 未给 / `false` | 与 `alignment` 子命令同语义 |
| `strict` | `true` | `false` 时 `should` 只警告 |
| `executed` | 未给 | `alignment/3` 的 run provenance 复核 |

退出码与 `alignment` 一致：

| 退出码 | `status` | 含义 |
| --- | --- | --- |
| `0` | `passed` | 三道门都是 `True` |
| `1` | `blocked` | 至少一道门是 `False`；`failed_at` 指出哪一道 |
| `3` | `unknown` | 至少一道门无法判定（没有 `False`）；不是通过 |

## 5. 库入口

```rust
use std::path::PathBuf;

use trit_orch::alignment::AlignmentOptions;
use trit_orch::gate_chain::{self, AlignmentStep, DeliveryStep, GateChainOptions};

let report = gate_chain::run(&GateChainOptions {
    root: PathBuf::from("."),
    expected_repo: "bfxh/trit-agent-kernel".to_owned(),
    task_kind: "agent".to_owned(),
    delivery: Some(DeliveryStep {
        packet: PathBuf::from(".agent/handoff/h-001.json"),
        receiver_state_hash: "H".to_owned(),
        at: 0,
    }),
    alignment: Some(AlignmentStep {
        manifest: PathBuf::from(".agent/alignment.json"),
        options: AlignmentOptions::default(),
    }),
});
assert_eq!(report.exit, 0);
```

`delivery` / `alignment` 传 `None` 表示该门未配置：不跳过、不默认通过，而是记
`Unknown` 并停链。要让调用方显式选择“只跑前两道门”，当前版本不支持——门链就是
三道门齐备的派发前检查；只做局部检查请直接调用对应子命令。

## 6. 硬规则

- **只有 `True` 放行**：三值里 `Unknown` 与 `False` 都不继续。
- **stop-first**：停链后不再执行任何门；`skipped` 不计入通过数。
- **只读**：`delivery` 走 `check`，不写账本；`alignment` 与 `repo` 本身只读。
- **相对路径以 `root` 为基准**：`packet` 与 `manifest` 都先按 `root` 解析，绝对路径原样使用。
- **不新增判据**：每道门的规则仍由各自模块负责；门链只做顺序与汇总。
- **报告可复算**：同一输入重复运行得到同一 `status` / `exit` / `steps[].state`；
  `detail` 里的原始报告也来自各自门。

## 7. 诚实边界

- 不证明“产物在语义上实现了需求”：`alignment` 只做结构检查，`passed` 只表示清单无断链。
- 不是 ACL、不是沙盒、不是密码学签名：`repo identity` 与交接包都是仓内自报，
  防串仓与防“门没过就宣称完成”，不防伪造。
- 不自动修复：清单缺证据、交接包过期、仓库身份写错，都要调用方改输入或改仓内文件。
- 不做网络或进程探测：只看本地文件与 `at` 时间。
- 门链通过 ≠ 四态账本走完：`delivery` 在 `check` 模式下即使 `ready=true`，账本仍可能
  停在 `missing` / `incomplete`；要推进四态仍需显式 `delivery` 动作。
