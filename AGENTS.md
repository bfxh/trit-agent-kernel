# AGENTS.md — trit-agent-kernel

本文件约束所有在本仓库工作的智能体与编程模型。全局规则见 `~/AGENTS.md`，两者冲突时以全局为准。

## 1. 语言：按项目文档，不按模型偏好

- 机器契约：`docs/agent-language-policy.json`（`trit.agent-language-policy/v1`）；人类面说明：`docs/LANGUAGE-POLICY.md`。
- 本仓库产物语言是**中文（`zh-CN`）**：文档、代码注释、commit message、CLI 帮助与错误、测试断言消息、UI 文案、日志、回执。
- 模型的语言偏好、厂商标识、训练语料分布都不是依据；换模型、换 provider、换会话都不改变产物语言。
- 不翻译标识符、文件路径、shell 命令、协议字段名、代码。
- 检查：`cargo test -p trit-host --test agent_policy`。

## 2. 模型无关

- 上述策略约束**所有现在与将来的模型与 provider**，不是某个模型的个人约定。
- 接入新模型必须：提供 provider 适配层、确认遵守语言策略、通过 `agent_policy` 契约测试。
- 模型不得自带语言偏好、项目偏好或厂商特定的输出形状。

## 3. 交付标准：一次做完

- 接到任务就做到可交付为止，**不许把未完成部分写成"已知边界 / 后续切片"来交差**。
- 文档里承诺过的（写着"后续仍要做""尚未实现"）要么实现，要么改成如实描述，不许留双重标准。
- 提交前必须跑相关测试与 `cargo fmt --all -- --check`；失败要修到全绿。测试失败先重复跑确认是否稳定，稳定失败就是必须修的回归，不许当成 flake 放过。
- 只有用户的要求会损害项目质量时才提异议，其余按指令执行。

## 4. 构建与验证

```bash
cargo fmt --all -- --check
cargo test -p trit-orch
cargo test -p trit-host
```

## 5. 提交与推送

- commit message 用中文，写清做了什么与为什么。
- 禁止 AI 厂商账号/身份出现在 author、committer、trailer、远程地址（见 `~/AGENTS.md` 第一、二条）。
- 阶段完成即推送（`git push origin HEAD`），不留未推送分支。
