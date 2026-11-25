# 对齐门（Alignment Gate）

`alignment/1`–`alignment/3` 的 Rust 迁移版。它回答一个很窄的问题：

> 这份“目标 → 需求 → 产物/证据/变更”清单，在给定输入下有没有断链？

它**不**回答“产物在语义上真的实现了需求”，也**不**替代执行器、沙盒、人工复核或 `trit-reasonix` 的完成裁决。`aligned` 只表示静态清单可复算、可追溯；`unaligned` 才表示有明确断链。

实现：`crates/trit-orch/src/alignment.rs`；CLI：`trit-orch alignment`。源语义来自 `bfxh/trit` 的 `tools/alignment_gate.py`（Python 版），本仓库不把 Python 作为主路径。

## 1. 三个 schema

### `alignment/1`：单目标

```json
{
  "schema": "alignment/1",
  "goal": { "id": "G1", "text": "交付一个可复算的账本" },
  "requirements": [
    {
      "id": "R1",
      "text": "账本四态顺序不可跳",
      "priority": "must",
      "parent": "G1",
      "artifacts": ["A1"],
      "evidence": ["E1"],
      "changes": ["crates/trit-orch/src/delivery.rs"]
    }
  ],
  "artifacts": [
    { "id": "A1", "path": "crates/trit-orch/src/delivery.rs", "locator": "text:pub struct Ledger" }
  ],
  "evidence": [
    { "id": "E1", "path": "crates/trit-orch/tests/delivery.rs", "locator": "L1-L40" }
  ]
}
```

### `alignment/2`：单根目标树

```json
{
  "schema": "alignment/2",
  "goals": [
    { "id": "G1", "text": "智能体内核可交付" },
    { "id": "G2", "text": "交接账本可复算", "parent": "G1" },
    { "id": "G3", "text": "串仓被拒绝", "parent": "G1" }
  ],
  "requirements": [
    { "id": "R1", "text": "四态账本", "priority": "must", "parent": "G2", "artifacts": ["A1"], "evidence": ["E1"] },
    { "id": "R2", "text": "repo-gate", "priority": "must", "parent": "G3", "artifacts": ["A2"], "evidence": ["E2"] }
  ],
  "artifacts": [
    { "id": "A1", "path": "crates/trit-orch/src/delivery.rs" },
    { "id": "A2", "path": "crates/trit-orch/src/repo.rs" }
  ],
  "evidence": [
    { "id": "E1", "path": "crates/trit-orch/tests/delivery.rs" },
    { "id": "E2", "path": "crates/trit-orch/tests/repo_gate.rs" }
  ]
}
```

### `alignment/3`：证据绑定本次实际成功 run

`alignment/3` 在 v2 之上要求每条 `must` 至少有一条 `kind: "run"` 的证据，且 `command` 必须点名证据文件；如果调用方给出本次成功 run 的命令列表（`--executed` / `run_command`），命令必须逐字归一匹配。

```json
{
  "schema": "alignment/3",
  "goals": [{ "id": "G1", "text": "测试真实跑过" }],
  "requirements": [
    {
      "id": "R1",
      "text": "定向测试通过",
      "priority": "must",
      "parent": "G1",
      "artifacts": ["A1"],
      "evidence": ["E1"]
    }
  ],
  "artifacts": [{ "id": "A1", "path": "crates/trit-orch/src/delivery.rs" }],
  "evidence": [
    {
      "id": "E1",
      "path": "crates/trit-orch/tests/delivery.rs",
      "kind": "run",
      "command": "cargo test -p trit-orch --test delivery"
    }
  ]
}
```

v1/v2 里出现 `kind` / `command` 会被拒绝（unknown field），不会静默降级。

## 2. 硬规则

- **目标树**：v2/v3 必须有且只有一个根 `goal`（`parent` 为空）；每个 `goal.parent` 必须存在；不能成环；每条 requirement 的 `parent` 链必须终止在一个 `goal`。
- **目标覆盖**：每条 `goal` 必须至少被一条 requirement 的父链覆盖；否则 `goal-uncovered`。
- **需求优先级**：`must` 必须有有效 artifact + evidence；v3 的 `must` 还必须有 run provenance。`should` 在 `strict:true`（默认）下按 must，`strict:false` 只警告；`may` 只警告。
- **清单路径**：manifest 本身也必须在 root 内；拒绝 `..`、绝对路径逃逸和符号链接逃逸，否则 `unknown` + `manifest-outside-root`。
- **产物/证据**：必须存在、是普通文件、在 root 内（拒绝 `..`、绝对路径、符号链接逃逸）；`sha256` 是 64 位十六进制且与磁盘一致；`locator` 只支持 `Lx`、`Lx-Ly`、`text:<片段>`。
- **non-goal**：`paths` 必须是仓库相对 glob；产物路径落进 non-goal、requirement 的 `changes` / `allow_unlinked` 与 non-goal 重叠，都失败。
- **声明变更**：`changed` 是实际变更集；`changed_known` 表示这是完整集（空集也算已知）。完整集下，每条声明的 `changes` 必须被实际变更兑现；must（strict 下 should）缺失失败，may 警告。只给 `changed` 不给 `changed_known` 时，只检查“已给的变化有没有对齐”，不要求声明变更全部出现。
- **未对齐变更**：实际变更必须命中 requirement 的 `changes`、artifact 路径或 `allow_unlinked`；否则 `unlinked-change`。
- **矛盾**：requirement 的 `contradicts` 必须指向存在的 requirement，不能自指；成对只报一次。

## 3. CLI

`trit-orch alignment` 从 stdin 读一个 JSON 对象，stdout 输出报告 JSON：

```json
{
  "root": ".",
  "manifest": ".agent/alignment.json",
  "strict": true,
  "changed": ["crates/trit-orch/src/delivery.rs"],
  "changed_known": true,
  "executed": ["cargo test -p trit-orch --test delivery"]
}
```

字段：

| 字段 | 含义 |
| --- | --- |
| `root` | 仓库根；relative path 的解析基准 |
| `manifest` | 清单相对路径；缺省 `.agent/alignment.json` |
| `strict` | 缺省 `true`；`should` 按 must |
| `changed` | 实际变更路径列表；可省略 |
| `changed_known` | `changed` 是否是完整集；缺省 `false` |
| `executed` | 本次**成功** run 的命令列表；用于 v3 provenance 复核 |

退出码：

| 退出码 | `status` | 含义 |
| --- | --- | --- |
| `0` | `aligned` | 没有失败项 |
| `1` | `unaligned` | 有明确断链；报告里逐条列出 `code` / `message` / `node` |
| `3` | `unknown` | 清单缺失、坏 JSON、schema 不符；**不是** aligned |

报告字段：`schema`、`status`、`exit`、`manifest`、`goal`、`score`、`must_score`、`counts`、`coverage`、`failures`、`warnings`。`score` / `must_score` 只统计 `artifact + evidence` 双全的需求，不是语义完成度。

### `alignment-scaffold`：生成故意留空的 v2 骨架

`trit-orch alignment-scaffold` 也走 stdin JSON；它只负责写出清单骨架，不替调用方填产物、证据或变更：

```json
{
  "root": ".",
  "manifest": ".agent/alignment.json",
  "goal": "交付一个可复算的账本",
  "requirements": ["四态顺序不可跳", "拒绝串仓"],
  "force": false
}
```

输出：

```json
{
  "ok": true,
  "manifest": ".agent/alignment.json",
  "goals": 1,
  "requirements": 2
}
```

生成的内容固定为 `alignment/2`：一个根目标 `G1`，需求 `R1…Rn`（`priority=must`、`parent=G1`），`artifacts` / `evidence` / `changes` 及顶层 `artifacts` / `evidence` / `non_goals` / `allow_unlinked` 全部为空。因此紧接着跑 `trit-orch alignment` 必然得到 `unaligned`，直到调用方补齐引用链。这是**检查清单生成器**，不是自动对齐声明。

规则：

- `goal` 为空或过滤空白后没有 requirement：退出 2，不落盘。
- `manifest` 必须是 root 内相对路径；拒绝 `..`、绝对路径、目录目标、符号链接目标，以及经父目录符号链接逃出 root 的路径。
- 目标已存在且 `force=false`：退出 2，原文件字节不变。
- `force=true`：先写同目录临时文件，再替换目标；临时文件写入失败不会留下半截目标。
- 写入 IO 失败：退出 2，错误为 `alignment_scaffold_io`。

## 4. 与 Python 版的关系

- 源文件：`bfxh/trit` 的 `tools/alignment_gate.py`；本仓库只移植语义，不复制 Python 文件、不把 Python 作为运行依赖。
- 已移植：v1/v2/v3 结构、目标树、父链、覆盖、产物/证据/sha256/locator、non-goal、声明变更、矛盾、v3 run provenance，以及 `scaffold` 清单生成。
- `stress` 的随机对拍已由 Rust 集成测试 `crates/trit-orch/tests/alignment_stress.rs` 承接：100 个确定性随机用例，每个同时验证有效清单 aligned 和三类变异 unaligned，不引入 Python 运行时。
- `self-test` 的核心契约已由 Rust 集成测试 `crates/trit-orch/tests/alignment_selftest.rs` 承接：目标树/schema、v3 provenance、字段与路径、声明变更、non-goal、manifest 错误和确定性输出。不逐条复制 Python 的终端文案，也不把自检脚本做成运行时子命令。
- 仍不同：Python 版对“所有字段长度/数组上限”的逐字段诊断；Rust 版用 `serde(deny_unknown_fields)` + 数组上限先做 fail-closed，未知字段直接报 `manifest-unreadable`，不细分到 Python 的 `unknown-field` 文案。
- 迁移不是整仓 merge：`bfxh/trit` 的 `trit-orch` 与本仓库的 `trit-orch` 已分叉，必须按模块搬，不能把 Python 主路径或 `trit` 的额外 crate 一起带进来。

## 5. 诚实边界

- 这是**准入前 / 交付前**的清单门，不是运行时监控；它防“清单没对齐就宣称完成”，不防宿主伪造清单或伪造 `executed`。
- `aligned` 不代表需求语义被满足；语义判断仍归 `trit-reasonix` / 人工复核 / 红队。
- 它不做自动修复、不自动改代码；`scaffold` 只生成留空的骨架清单，不替调用方补图；`stress` / `self-test` 的核心契约以 Rust 测试承接，不提供运行时自检 CLI。
- 文件检查是本地文件系统快照；检查后被替换的文件不会被再次发现，除非重新跑门。
