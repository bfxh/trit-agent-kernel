# 权限与只读边界

内核把“能不能写”拆成三层，默认都 fail-closed：

| 层 | 作用 | 信任来源 |
| --- | --- | --- |
| `PermissionPolicy` / `PermissionGate` | Ask/Auto/Yolo 与规则匹配，决定是否放行一次调用 | 宿主配置 + 工具静态元数据 |
| `WriteClaim` / `ClaimingTools` | 把写路径收窄到声明根；shell 直接拒绝 | 宿主构造的 claim |
| checkpoint / mutation journal | 写前快照、失败回滚、`in_doubt` 对账 | 宿主文件 |

## `read_only` 不可由模型自报

- `read_file` / `search_code` / `glob` 等本地工具的 `read_only` 来自宿主
  `ToolSpec`，模型参数不参与。
- `bash` 的只读属性**无法从命令文本证明**。`bash` 工具 schema 不再暴露
  `read_only` 参数；即使调用方手工构造 `ToolRequest { tool: "bash",
  read_only: true }`，`PermissionGate` 也不会走只读快路径，`WriteClaim` 也会先拒。
- 这条约束是安全边界，不是 UX 偏好：一条写命令只要附上 `read_only: true`
  就能绕过审批、写声明、checkpoint 和 journal。

## MCP `readOnlyHint` 只做展示

MCP 服务端返回的 `annotations.readOnlyHint` 是**对端自报**，不可信。内核把它
保留在 `McpToolRoute.declared_read_only` 供诊断，但映射到 `ToolSpec.read_only`
时恒为 `false`。因此 MCP 工具默认按可写处理，必须经过 `PermissionGate` /
`WriteClaim`；需要只读能力时由宿主显式配置工具名。

## shell 与 claim

`ClaimingTools::allows_request` 对 `bash` 无条件返回 `false`（在 `read_only`
快路径之前），与 `write_file` / `move_path` 的路径校验分开。没有 claim 时，
shell 仍受 `allow_shell` / `allow_unsandboxed_shell` / 沙箱约束。

## 快照与回滚

- checkpoint 快照先逐段拒绝符号链接祖先，再以 cap-std `Dir` + `FollowSymlinks::No`
  读取；最终路径是符号链接时跳过。单文件上限 8 MiB，超限 fail-closed，不会把大文件
  静默记成“不存在”。
- mutation journal 的备份与回滚同样走 cap-std；路径与备份文件名都只接受单段归一化
  名称，回滚拒绝符号链接父目录。

## 子进程环境

`bash`、`subagent-run`、MCP/supervisor 与 git checkpoint 的子进程 spawn 前统一
`env_clear`，只透传路径、临时目录、locale 与 Windows 进程启动所需的宿主变量；
`OPENAI_API_KEY` 等 provider 凭据不再被任何工具子进程继承。需要额外变量的调用方
必须显式传入（`ProcessSpec.env` 或 `SubagentProcessConfig::with_env`）。

## 已知待补

- 远程执行面需要 token / Host / Origin 与保留路径名校验（审计第 5 项）。
- git journal 的 `backup` 字段与控制面文件完整性校验（审计第 6 项）。
