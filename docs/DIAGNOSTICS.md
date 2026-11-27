# 静态能力诊断

`trit-host diagnostics` 输出一份只读 JSON 报告，给宿主、CI 和排障脚本消费。
它的硬约束是**不产生副作用**：不启动 MCP / gateway / 子进程，不创建目录（包括
`.agent/`），不写账本、不建投影。所有探测都是“文件存在才读”。

## 用法

```bash
trit-host diagnostics --workspace <dir> \
  [--session <session.jsonl>] [--db <session.index.db>] \
  [--live-mcp <program> [--live-mcp-arg <arg>]... [--live-wait-ms N] [--live-max-stderr N]]
```

- `--workspace`：扫描 write journal 的位置；缺省为当前目录。
- `--session`：可选，附加会话摘要（turn 数、门链数、最后一轮是否 blocked、最后 seq）。
- `--db`：可选，附加 SQLite 投影摘要；投影缺失时 `present=false` 并给出 warning。
- `--live-mcp`：可选，拉起一个 stdio MCP server 做启动探测；默认不拉任何进程。
- `--live-mcp-arg`：重复传入，按顺序作为该 server 的参数。
- `--live-wait-ms`：观测窗口，默认 1500；窗口结束后无论是否仍在运行都主动结束。
- `--live-max-stderr`：stderr 最多保留多少行，默认 40；其余丢弃并标记 `stderr_truncated`。

退出码：`0` 报告成功（包含 warnings）；非 0 只表示参数错误或读取失败。

## 报告字段

| 字段 | 说明 |
| --- | --- |
| `kernel` | 当前契约版本：`session` / `write_journal` / `rewind` / `session_index` |
| `tools` | 内置工具目录：`name`、`read_only`、`writes`、`requires_sandbox` |
| `workspace` | 被扫描的工作区路径 |
| `journal` | write journal 对账：`present`、`total`、`completed`、`failed`、`rolled_back`、`in_doubt` |
| `session` | 仅传 `--session` 时出现；字段同上 |
| `projection` | 仅传 `--db` 时出现；`present` + `summary` |
| `live` | 仅传 `--live-mcp` 时出现；`program`、`launched`、`exited_early`、`exit_code`、`stderr_lines`（已脱敏）、`stderr_truncated`、`error` |
| `warnings` | 可执行的排障提示（有 `in_doubt` 写、投影缺失、投影 schema 不匹配、live 启动失败或早退） |

工具目录是静态声明，与 `tools.rs` 的 `LocalTools` 分发对应；新增工具时两处同步，
单测会检查目录有序且无重复。

## 边界

- 默认不拉任何进程：不传 `--live-mcp` 时命令在任何情况下都不会创建子进程。
- `--live` 只做启动观测：不握手、不读 stdout、不读环境变量或 provider 凭据；stdin
  保持打开（多数 MCP server 把 stdin EOF 当退出信号），观测窗口结束后主动结束子进程，
  不让诊断挂住。
- stderr 逐行脱敏后才进报告：工作区绝对路径换成 `<workspace>`，`Bearer <token>`、
  `sk-…`、`(api_key|token|secret|password)=…` 一类的值换成 `<redacted>`。
- 不修复发现的问题：`in_doubt` 的处理仍走 `journal-status --rollback`，投影缺失
  仍走 `session-index --rebuild`。
