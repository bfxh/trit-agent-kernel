# 静态能力诊断

`trit-host diagnostics` 输出一份只读 JSON 报告，给宿主、CI 和排障脚本消费。
它的硬约束是**不产生副作用**：不启动 MCP / gateway / 子进程，不创建目录（包括
`.agent/`），不写账本、不建投影。所有探测都是“文件存在才读”。

## 用法

```bash
trit-host diagnostics --workspace <dir> \
  [--session <session.jsonl>] [--db <session.index.db>]
```

- `--workspace`：扫描 write journal 的位置；缺省为当前目录。
- `--session`：可选，附加会话摘要（turn 数、门链数、最后一轮是否 blocked、最后 seq）。
- `--db`：可选，附加 SQLite 投影摘要；投影缺失时 `present=false` 并给出 warning。

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
| `warnings` | 可执行的排障提示（有 `in_doubt` 写、投影缺失、投影 schema 不匹配） |

工具目录是静态声明，与 `tools.rs` 的 `LocalTools` 分发对应；新增工具时两处同步，
单测会检查目录有序且无重复。

## 明确不做

- `--live`（真实启动 MCP 并采集启动阶段/脱敏 stderr）是后续切片；当前命令在任何
  情况下都不会拉进程。
- 不读取密钥、环境变量或 provider 凭据；报告里不出现路径以外的敏感内容。
- 不修复发现的问题：`in_doubt` 的处理仍走 `journal-status --rollback`，投影缺失
  仍走 `session-index --rebuild`。
