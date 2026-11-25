# 仓库边界与串仓防护

## 三个仓不是一回事

| 仓库 | 角色 | 是否承载智能体运行时 |
| --- | --- | --- |
| `bfxh/trit` | 三进制、训练、位平面与其它工具链主库 | **否** |
| `bfxh/trit-agent-kernel` | 智能体内核与编排（本仓） | **是** |
| `bfxh/trit-mc` | Minecraft 具体宿主 | 否；只实现 I/O 与游戏交互 |

`bfxh/trit` 里的智能体文档、脚本和旧工具只能作为**迁移参考**，不能当成
本仓已实现的能力；反过来，本仓的 `trit-orch` / `trit-host` 也不替三进制主库
承担训练、位平面或 GPU 工具链。

## 为什么会出现串仓

1. 两个仓都叫 `trit`，且本仓是从 `bfxh/trit` 抽出的；目录、crate 名和历史
   有重叠。
2. 旧任务把 `bfxh/trit/docs/engineering/AGENT-*.md` 当成智能体实现现状，
   但这些文档描述的是主库里的 Python 工具链，不是本仓的 Rust crate。
3. 浏览器当前打开的 PR、最近一次对话、共享 worktree，都不等于“任务所属仓库”；
   代理不能从这些信号推断仓库。

## 防护：repo identity 门

本仓在 `.agent/repo.json` 声明身份：

```json
{
  "schema": "repo-identity/1",
  "repo": "bfxh/trit-agent-kernel",
  "role": "agent-kernel",
  "owns_agent_runtime": true,
  "agent_runtime_repo": "bfxh/trit-agent-kernel"
}
```

派发前先跑：

```bash
printf '%s' '{"root":".","expected":"bfxh/trit-agent-kernel","task_kind":"agent"}' \
  | trit-orch repo-gate
```

- 身份文件缺失、坏 JSON、`schema` 不对：退出 1，`repo_scope_rejected`。
- `expected` 与实际 repo 不一致：退出 1。
- `task_kind=agent` 但 `owns_agent_runtime=false`：退出 1，并给出真实
  `agent_runtime_repo`。

这不是“提醒”，而是 fail-closed 的准入前门。宿主在 `plan` / `dispatch` 之前
必须先过它；没有过门就不派活。

`trit-host run` 已提供入口参数：

```bash
trit-host run --task "..." --workspace . \
  --repo-identity --repo-expected bfxh/trit-agent-kernel --task-kind agent ...
```

`--repo-identity` 缺省不开；一旦打开，identity 缺失、坏 JSON、`schema` 不对、
`repo` 与 `--repo-expected` 不一致，或 `task-kind=agent` 但
`owns_agent_runtime=false`，都会在读取脚本 / 建 session / 调模型之前退出 2。
包装器应把它作为固定参数，不要交给模型自己决定是否检查。


## 给 `bfxh/trit` 的对应做法

`bfxh/trit` 也应放一份自己的 `.agent/repo.json`：

```json
{
  "schema": "repo-identity/1",
  "repo": "bfxh/trit",
  "role": "ternary-main",
  "owns_agent_runtime": false,
  "agent_runtime_repo": "bfxh/trit-agent-kernel"
}
```

这样任何代理在 `bfxh/trit` 里接“智能体”任务时，`repo-gate` 会直接拒绝，
而不是把训练库的 `AGENT-*.md` 当成智能体实现。

## 诚实边界

- identity 是仓内自报文件，不是远程认证；能改 `.agent/repo.json` 的人也能改错。
  它防的是“没检查就开工”，不防恶意宿主。
- 跨仓迁移仍需显式记录 `source_repo`、`source_commit`、`target_repo`；不能靠
  “名字像”自动搬。
