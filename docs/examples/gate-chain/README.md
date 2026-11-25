# gate-chain 最小示例

这是一份**可以直接跑通**的示例工作区：仓库身份、交接包、对齐清单、产物与证据都齐备。
它对应 [docs/GATE-CHAIN.md](../../GATE-CHAIN.md) 里的三道门：

```text
repo-gate        -> .agent/repo.json
delivery check   -> .agent/handoff/h-001.json
alignment        -> .agent/alignment.json
```

## 目录

```text
workspace/
  .agent/repo.json              仓库身份
  .agent/handoff/h-001.json     交接包（state_hash = H）
  .agent/alignment.json         alignment/2 目标树 + 需求 + 产物/证据
  src/report.md                 产物 A1
  tests/report_check.md         证据 E1（locator = text:## 检查）
```

## 跑一次

把 `workspace/` 的内容复制到一个空目录（或你自己的项目根），然后在那个目录运行：

```bash
mkdir -p /tmp/gate-chain-demo
cp -r docs/examples/gate-chain/workspace/. /tmp/gate-chain-demo/
cd /tmp/gate-chain-demo

printf '%s' '{
  "root": ".",
  "expected": "bfxh/trit-agent-kernel",
  "task_kind": "agent",
  "packet": ".agent/handoff/h-001.json",
  "receiver_state_hash": "H",
  "manifest": ".agent/alignment.json"
}' | trit-orch gate-chain
```

预期输出 `status: "passed"`、`exit: 0`、`counts.passed: 3`。

需要同时补 `sent + delivered` 时在输入里加 `"attach": true`；账本写到
`.agent/delivery/h-001.json`（这个目录应进 `.gitignore`）。

## 用宿主跑

```bash
trit-host run --task "演示门链" --workspace . \
  --gate-chain \
  --handoff-packet .agent/handoff/h-001.json \
  --handoff-state-hash H \
  --alignment-manifest .agent/alignment.json
```

门链会在 `--script` 解析之前执行；拒绝时退出码 `2`，stderr 以
`gate chain rejected at <门>:` 开头。要再用**实际写盘**复核一次，加
`--gate-chain-post`：run 结束后以完整变更集重跑对齐检查，失败退出 `2`，但不会回滚
已经发生的写入。

## 改成你自己的任务

| 要改的 | 位置 | 说明 |
| --- | --- | --- |
| 仓库身份 | `.agent/repo.json` | `repo` 改成实际 `owner/name`；智能体任务要求 `owns_agent_runtime=true` |
| 交接包 | `.agent/handoff/h-001.json` | `state_hash` 与调用方的 `--handoff-state-hash` 必须一致 |
| 目标与需求 | `.agent/alignment.json` | 每个 goal 至少被一条 requirement 的父链覆盖 |
| 产物 | `src/report.md` | 路径必须相对 workspace，且在清单里登记 |
| 证据 | `tests/report_check.md` | `locator` 只支持 `Lx`、`Lx-Ly`、`text:<片段>` |

## 边界

- 这是**结构检查**的可复算示例，不证明报告语义正确，也不证明证据真的跑过。
- `state_hash`、`repo.json` 都是仓内自报，不防伪造。
- 示例里的 `expires_at` 是 2100 年，真实交接包应写实际过期时间。
