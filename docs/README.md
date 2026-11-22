# 文档地图

这套文档按“谁需要做什么决定”拆分，不按代码目录拆分。代码与测试是能力事实的来源；文档负责解释边界、责任和升级路径。两者冲突时先修代码或文档中的错误一方，不能长期并存。

## 按角色阅读

| 角色 | 先读 | 再读 | 要回答的问题 |
| --- | --- | --- | --- |
| 用户 / 运维 | [STARTUP-AND-GAME-MODE.md](STARTUP-AND-GAME-MODE.md) | [RESPONSIBILITY.md](RESPONSIBILITY.md) | 开机后程序会动什么？游戏模式保留什么？出问题找谁？ |
| 前端 / 桌面开发者 | [FRONTEND-CONTRACT.md](FRONTEND-CONTRACT.md) | [STARTUP-AND-GAME-MODE.md](STARTUP-AND-GAME-MODE.md)、[RESPONSIBILITY.md](RESPONSIBILITY.md) | 内核更新而前端没跟上时，功能怎么锁？锁住后通知谁？ |
| 上层组件开发者 | [LIBRARY-STACK.md](LIBRARY-STACK.md) | [RIGHTS.md](RIGHTS.md) | 引入哪些库？三进制原生库怎么用？哪些 API 不能碰？ |
| 维护者 / 仓库所有者 | [RESPONSIBILITY.md](RESPONSIBILITY.md) | [RIGHTS.md](RIGHTS.md)、[MERGE-POLICY.md](MERGE-POLICY.md) | 五类问题谁负责？批次怎么合？权益怎么分层？ |
| 智能体 / 模型 / 插件接入 | [AGENT-MODEL-MANIFEST.md](AGENT-MODEL-MANIFEST.md) | [FRONTEND-CONTRACT.md](FRONTEND-CONTRACT.md) | 谁负责哪一层？哪些是后端、前端、内核各自的义务？人类面板与智能体清单怎么分开？ |
| 参与移植 / 合并的人 | [REASONIX-PORT-MATRIX.md](REASONIX-PORT-MATRIX.md) | [MERGE-POLICY.md](MERGE-POLICY.md)、[LIBRARY-STACK.md](LIBRARY-STACK.md)、[RIGHTS.md](RIGHTS.md) | Reasonix 哪些能力先移？每批验收什么？ |

## 最小引入集

- **任何上层组件**：只把 [LIBRARY-STACK.md](LIBRARY-STACK.md) 作为库引入入口。它包含依赖方向、三进制原生库规则、第三方库政策和新库准入模板。
- **需要做前端 / 桌面**：在最小引入集之外，再加 [FRONTEND-CONTRACT.md](FRONTEND-CONTRACT.md)。前端不得自行猜测内核能力。
- **需要发布商业版本**：再加 [RIGHTS.md](RIGHTS.md) 和 [MERGE-POLICY.md](MERGE-POLICY.md)。

## 文档状态

| 文档 | 状态 | 对应实现 |
| --- | --- | --- |
| [STARTUP-AND-GAME-MODE.md](STARTUP-AND-GAME-MODE.md) | 设计契约，桌面宿主待接入 | 内核不直接管理桌面进程 |
| [FRONTEND-CONTRACT.md](FRONTEND-CONTRACT.md) | 门禁库已实现，桌面适配待接入 | `crates/trit-host/src/frontend_compat.rs` |
| [RESPONSIBILITY.md](RESPONSIBILITY.md) | 现行流程 | GitHub 标签 / CODEOWNERS 待逐仓配置 |
| [LIBRARY-STACK.md](LIBRARY-STACK.md) | 现行规范 | 工作区依赖与公开 API |
| [RIGHTS.md](RIGHTS.md) | 政策草案，非法律意见 | 各仓库 LICENSE / NOTICE |
| [MERGE-POLICY.md](MERGE-POLICY.md) | 现行流程 | CI 与 PR 模板 |
| [REASONIX-PORT-MATRIX.md](REASONIX-PORT-MATRIX.md) | B7.1–B7.4 首版已落地 | `reasonix_wire` / `provider_stream` / `context_projection` / `permissions` |
| [AGENT-MODEL-MANIFEST.md](AGENT-MODEL-MANIFEST.md) | 契约草案，清单为模板 | [`agent-model-manifest.json`](agent-model-manifest.json)；运行时门禁仍归 `FeatureGate` |

## 维护规则

1. 新增或修改公开能力时，同一个 PR 必须更新对应文档和测试；不接受“先合代码，文档以后补”。
2. 能力 id、`api_level`、许可层级、责任归属属于契约字段，改动需要所有者签核。
3. 文档里的时间目标是最佳努力，不是对外 SLA；真正承诺写进单独的支持协议。
4. 任何“自动清理 / 自动关闭 / 自动合并”都必须有 dry-run、清单和回退路径，不能以“默认应该没问题”为理由跳过。
