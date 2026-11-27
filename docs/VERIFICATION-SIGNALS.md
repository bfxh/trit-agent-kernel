# 校验次数、训练信号与"少重复校验"

目标：让智能体的校验（跑测试、跑命令）**可计数、可观察、可反馈**，并把"重复校验"变成
模型能看到的信号，从而减少无意义的重复执行。

## 1. 记录什么

每轮 `TurnMetrics` 增三个字段（随 `TurnFinished` 落 session JSONL，也在 CLI JSON 输出里）：

| 字段 | 含义 |
| --- | --- |
| `verification_calls` | 本 turn 执行过的命令数（带 `command` 的回执，即 `bash` 类工具） |
| `unique_verifications` | 按命令文本去重后的数量 |
| `repeated_verifications` | `verification_calls - unique_verifications` |

旧日志没有这三个字段时按 `0` 读入（`#[serde(default)]`），不会误判成"零校验"以外的含义。

## 2. 给模型的信号

`ProviderRequest.verification`（`VerificationSignal`）在每轮提议前由回执算出：

```json
{
  "calls": 3,
  "unique": 2,
  "repeated": 1,
  "commands": ["cargo test", "cargo clippy"]
}
```

- 写进请求体 user payload 的 `verification` 字段（三种协议共用同一份 OpenAI 中间态，所以
  OpenAI / DeepSeek / Anthropic / Gemini 都能看到）。
- 当 `repeated > 0` 时，system 追加一句：*Do not repeat a verification command that already
  has a receipt; reuse its result.*
- 没有任何命令时 `verification` 为 `None`，不注入空对象。

## 3. 开关：在"记录那边"开启/关闭

| 位置 | 作用 |
| --- | --- |
| `Settings.recording.verification_signals`（默认 `true`） | 记录层总开关，供前端/配置文件使用 |
| `TRIT_VERIFICATION_SIGNALS=0\|false\|off\|no` | CLI 运行期关闭；其余值或未设置视为开启 |

关闭后：**不再向模型注入 `verification`，也不再追加重复校验提示**；`TurnMetrics`
仍会把三个计数写进 session（记录是廉价的，且排障需要）。若你连计数也不要，改
`TurnMetrics` 的输出过滤即可，不要改 capture 语义。

## 4. 怎么用它减少校验

1. 看 `repeated_verifications` 与 `repeated / calls`：持续偏高说明模型在重复跑同一条命令。
2. 训练/调优时把它当**负向信号**（同一 turn 内重复越多越差），而不是奖励"跑得多"。
3. 模型侧依赖 system 提示与 `commands` 列表判断"已经跑过什么"，避免重跑。
4. 不要用"减少校验"去削弱证据：完成声明仍必须由回执背书（`diff_backing` /
   `verification_backing`），少校验只应减少**重复**，不应减少**必要**证据。

## 5. 检查

```bash
cargo test -p trit-host --test verification
```
