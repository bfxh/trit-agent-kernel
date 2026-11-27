# 技术栈策略：编程语言与软件设施跟项目走，不跟模型偏好走

机器契约：[`agent-tech-stack-policy.json`](agent-tech-stack-policy.json)（`trit.agent-tech-stack-policy/v1`），
由 `crates/trit-host/tests/tech_stack_policy.rs` 校验。

> 注意区分两种"语言"：
> - **自然语言**（文档、注释、commit、文案用什么语言）：由 [`LANGUAGE-POLICY.md`](LANGUAGE-POLICY.md)
>   管理，**可以在设置里调整**。
> - **编程语言 / 框架 / 包管理器**：**不在设置里放开**，必须按项目现状，禁止模型偏好。

## 1. 一条规则

**默认技术栈由项目现状决定。** 探测依据是项目里的清单与锁文件（`Cargo.toml`、`package.json`、
`pyproject.toml`、`go.mod`、`pom.xml`、`*.csproj` 等）以及已有目录与构建脚本；模型的偏好、
厂商标识、训练语料都不是依据。

## 2. 明确禁止

- 在没有用户授权的情况下引入新编程语言；
- 无理由引入新框架或包管理器；
- 把已有代码改写成模型偏好的语言；
- 为任务加不需要的依赖；
- 静默切换构建工具链。

## 3. 唯一例外

只有**用户在任务 brief 里显式授权**时才允许新增，且必须记为 `explicit_override`。
"用户要求支持某语言"不等于"允许模型偏好某语言"：授权是具体的、一次性的、
写在 brief 里的，不是模型的口味。

## 4. 子智能体同样受限

`SubagentRequest.stack_policy`：

```json
{ "existing": ["rust", "cargo"], "allowed_additions": [] }
```

`SubagentResponse.stack`：

```json
{ "used": ["rust"], "added": [] }
```

**`added` 必须是 `allowed_additions` 的子集**，否则 `validate_for` 直接判协议违约。
这对帧协议子智能体与外部 CLI 子智能体（codex / claude-code / qoder / zcode / 豆包）同样生效：
适配层把 CLI 自报的技术栈填进 `stack`，越权新增一样被拒。

## 5. 与评分的配合

前后端评分（`docs/SCORING.md`）可以看"是否遵守技术栈策略"作为扣分项；是否让评分参与
决策由设置 `scoring.participate_in_decision` 控制（默认关闭）。

## 6. 检查

```bash
cargo test -p trit-host --test tech_stack_policy
cargo test -p trit-host --test scoring
```
