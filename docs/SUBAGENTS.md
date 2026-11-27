# 子智能体：外部 CLI 后端、并行、以及"可以反对"的异议

子智能体是独立进程：宿主发请求帧、它回响应帧，或者由适配层把外部编码智能体 CLI
包装成同样的语义。

## 1. 两种后端

| 类型 | 说明 |
| --- | --- |
| 帧协议进程 | 任何实现 `SubagentRequest`/`SubagentResponse` 帧（4 字节长度 + JSON）的程序；本仓库自带 `trit-host subagent-run` 作为参考实现 |
| 外部 CLI | Codex / Qoder / ZCode / 豆包 / Claude Code；由 `subagent_cli` 适配：组装命令、传 prompt、读输出 |

外部 CLI 的默认调用约定是 `<program> <args...> <prompt>`（豆包用 stdin）。参数差异用
环境变量覆盖，不靠猜：

| 变量 | 作用 |
| --- | --- |
| `TRIT_SUBAGENT_<ID>_PROGRAM` | 可执行文件路径（ID 大写、`-` 换成 `_`，例如 `CLAUDE_CODE`） |
| `TRIT_SUBAGENT_<ID>_ARGS` | 固定参数（空白分隔；设空串清空默认） |
| `TRIT_SUBAGENT_<ID>_AUTO_ARGS` | 非交互/自动批准参数 |

内置后端与默认值：

| ID | program | 固定参数 | prompt |
| --- | --- | --- | --- |
| `codex` | `codex` | `exec --full-auto` | argv |
| `claude-code` | `claude` | `-p` | argv |
| `qoder` | `qoder` | `run` | argv |
| `zcode` | `zcode` | `run` | argv |
| `doubao` | `doubao` | — | stdin |

## 2. 并行

```bash
trit-host subagent-fanout --workspace <dir> --agents codex,claude-code \
  --tasks tasks.json --workers 4 [--timeout-ms 900000] [--dry-run]
```

`tasks.json`：

```json
[
  {"id": "t1", "prompt": "update the schema", "agent": "codex"},
  {"id": "t2", "prompt": "write the migration", "agent": "claude-code"}
]
```

- `--workers` 控制并行数；任务按 `--agents` 轮转或按任务里的 `agent` 指定。
- `--dry-run` 只输出计划（program / args / 是否 stdin），不拉进程，便于先核对命令。
- 输出 JSON：`results[]`（id / agent / exit / timed_out / content_chars / dissent / stderr_tail）
  与 `dissent` 汇总。

## 3. 子智能体可以反对主智能体

**这不到 settings 里**：它是运行时契约，不是配置项。

- 子智能体在输出里放一个异议块即可：

```
<<<DISSENT>>>{"target":"schema change","reason":"breaks old session logs","suggestion":"add a versioned field"}<<<END>>>
```

- 适配层把块解析成 `Dissent { target, reason, suggestion }`，其余输出了当正文。
- 主智能体**必须显式处理**：采纳，或说明为什么驳回。不允许静默丢弃——异议的目的是对项目
  质量负责，不是服从。
- 异议不影响响应有效性（`validate_for` 只校验 `task + epoch`），但会出现在 `dissent` 汇总里。

## 3.5 外部 CLI 事后审计

外部 CLI 子进程在宿主工具之外直接改盘，`write_file` / `bash` 的校验管不到它们。
`subagent-fanout` 因此在跑前跑后各拍一次工作区 `.py` 快照（相对路径 + 大小 + mtime），
对比出新增/修改的文件，再用 `tech_stack::violations` 过滤：

- 报告 JSON 增 `violations` 数组；
- 只要出现**非测试路径的 `.py`**，命令以非零退出并在 stderr 列出路径（fail closed）；
- 跳过 `.git` / `target` / `node_modules` / `.venv`，最多 20k 个 `.py`；
- 只审计 `--workspace` 指定的目录；任务自定义 workspace 时不覆盖。

## 4. 嵌套与安全边界

- 子智能体可以再派生子智能体；最大深度 3（`--depth` 或 `TRIT_SUBAGENT_DEPTH`），超过直接拒绝。
- 子进程默认清空继承环境（`child_env::sanitize_with`），不把 provider 凭据带给外部 CLI。
- 输出上限 8 MiB；超时后主动结束进程并标记 `timed_out`。

## 5. 检查

```bash
cargo test -p trit-host --lib subagent_cli
cargo test -p trit-orch --lib subagent
trit-host subagent-fanout --dry-run --agents codex,claude-code --tasks tasks.json
```
