# 语言策略：产物语言由项目文档决定，不由模型偏好决定

机器契约：[`agent-language-policy.json`](agent-language-policy.json)（`trit.agent-language-policy/v1`），
由 `crates/trit-host/tests/agent_policy.rs` 校验。本文件是人类面说明，两者不一致时以 JSON 与测试为准。

## 1. 一条规则

**项目文档写什么语言，产物就用什么语言。** 模型自己的语言偏好、厂商标识、训练语料分布、
"我觉得这样更自然"都不是依据。换模型、换 provider、换会话，都不改变产物语言。

## 2. 本仓库的规定

本仓库（`trit-agent-kernel`）的产物语言是 **中文（`zh-CN`）**：

| 产物 | 语言 |
| --- | --- |
| 文档正文（`docs/*.md`、README） | 中文 |
| 代码注释、doc comment | 中文 |
| commit message | 中文 |
| CLI 帮助与错误消息 | 中文 |
| 测试断言消息 | 中文 |
| UI 文案 | 中文 |
| 日志与诊断输出 | 中文 |
| 给用户的回执、通知 | 中文 |

不翻译：标识符、文件路径、shell 命令、协议字段名、代码本身。

## 3. 项目没写时

项目文档没有声明语言时，跟随**用户当前请求**使用的语言，并在该仓库保持一致；
不得因为换了模型而换语言。新仓库应尽早把语言写进自己的 `AGENTS.md` 或本策略文件。

## 4. 明确禁止

- 因为模型偏好切换语言（同一个仓库里中文文档突然变英文、或反过来）；
- 因为厂商标识切换语言或输出风格；
- 因为"英文看起来更专业"而把已经用中文写成的产物改写；
- 在一个产物里混排两种语言（引用原文、术语、标识符除外）；
- 把标识符、路径、命令翻译成自然语言。

## 5. 与模型兼容的关系

这条策略是**模型无关**的：它约束所有现在与将来的模型与 provider。

- 接入新模型必须：提供 provider 适配层、确认遵守本策略、通过 `agent_policy` 契约测试；
- 模型不得自带语言偏好、项目偏好或厂商特定的输出形状；
- 清单里每个 agent / model / plugin 条目都必须显式声明
  `language_behavior: "follow-project-language-policy"`，
  `agent-model-manifest.json` 的 `language_policy` 指向本策略。

## 6. 检查方式

```bash
cargo test -p trit-host --test agent_policy
```

该测试校验：策略 JSON 的 schema、默认语言与允许语言、产物覆盖、禁止项；
manifest 是否绑定策略、每个条目是否声明遵守；本文件是否存在并写明"由项目文档决定、不随模型偏好"。
