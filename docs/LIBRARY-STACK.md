# 库引入规范与依赖栈

状态：现行规范。上层组件只需要本文作为“库引入”入口；做前端时再加 [FRONTEND-CONTRACT.md](FRONTEND-CONTRACT.md)。

一句话规则：**只向下依赖，只使用公开 API，三进制原生库必须显式引入且不得复刻。**

## 1. 依赖方向

```text
L3  前端 / 桌面 / CLI 展示 / 编辑器插件
        │
L2  trit-host（无头宿主：模型循环、工具、MCP、会话、检查点、缓存）
        │
L1  trit-schedule   trit-orch   trit-reasonix
        │
L0  trit-core（三进制原生库：值、代数、AST、位并行）
        │
L4  第三方 crate / 模型 API / OS / MCP server
```

- 箭头只能向下。L0 不知道 L1/L2，L1 不知道前端。
- 上层可以组合下层，但不能把下层的类型复制一份“差不多”的实现。
- 公开 API 一旦发布，破坏性修改需要提升主版本并写迁移说明；工作区内部仍按此执行。

## 2. 栈清单

| 层 | Crate | 职责 | 可依赖 | 责任角色 | 许可 |
| --- | --- | --- | --- | --- | --- |
| L0 | `trit-core` | `Ternary`、K3/Ł3/B3、AST、解析、位并行、三态总线、拓扑 | 仅第三方 | 原生库所有者 | MIT OR Apache-2.0 |
| L1 | `trit-schedule` | 三值依赖 DAG 的静态调度 | L0 | 内核维护者 | MIT OR Apache-2.0 |
| L1 | `trit-orch` | 准入、快照、义务 / 证据、预算、影子学习、`tick` | L0/L1 | 内核维护者 | MIT OR Apache-2.0 |
| L1 | `trit-reasonix` | 完成声明、证据裁决、用量适配 | L0/L1 | 内核维护者 | MIT OR Apache-2.0 |
| L2 | `trit-host` | 模型 / 工具循环、MCP、工具边界、会话、检查点、上下文 / 缓存 | L0/L1 | 内核维护者 | MIT OR Apache-2.0 |
| L3 | 前端 / 桌面 | 展示、启动调度、游戏模式、扩展坞 | L2 + 前端契约 | 前端维护者 | 按仓库 |
| L4 | 第三方 | crate、模型、MCP server、OS 工具 | 不适用 | 引入方 | 逐个审计 |

工作区统一 `edition = "2024"`、`rust-version = "1.88"`。上层组件不得声明更低的 MSRV 却依赖本栈。

## 3. 三进制原生库：`trit-core`

这是整个栈的地基，**所有上层组件必须显式引入**，不能自己定义 `enum Tri { False, Unknown, True }`。

### 3.1 必知类型

| 类型 / API | 语义 | 备注 |
| --- | --- | --- |
| `Ternary::{False, Unknown, True}` | 三值逻辑 | `False < Unknown < True`，顺序即格序 |
| `index()` / `from_index()` | 0/1/2 索引与还原 | 9 项 LUT 用 `a*3 + b` |
| `as_f64()` | 0 / 0.5 / 1 | 仅作为 Ł3 数值定义基础 |
| `to_bool_strict()` | 仅 `True` 为真 | `Unknown` 折成假 |
| `to_bool_loose()` | 仅 `False` 为假 | `Unknown` 折成真 |
| `is_false()` / `is_true()` | 精确判断 | `Unknown` 对两者都返回 false |
| `parse_char` / `as_char` | `0/1/X` 互转 | `?` 可解析为 `Unknown` |
| `Algebra::{Kleene, Lukasiewicz, Bochvar}` | K3 / Ł3 / B3 运算 | 非默认语义必须走 `Algebra` |
| `TriState` / `TriStateBus` | 0 / 1 / Z 与冲突 | `Z` 是未驱动，不是 `X` |
| `Expr` / `Parser` / `TruthTable` | AST 与真值表 | 表达式入口 |

### 3.2 不可破坏的语义

1. `Unknown` 不是 `False`，也不是 `True`。任何二值化必须显式调用 `to_bool_strict` / `to_bool_loose`。
2. `BitAnd` / `BitOr` / `Not` 采用 **Kleene**；`BitXor` 是经典异或。Ł3 / B3 必须用 `Algebra`，不能用运算符“顺便”表达。
3. K3 的 `X ∧ 0 = 0`、`X ∨ 1 = 1`；B3 的 `X` 吸收一切；Ł3 的 `X ⊕ X = 1`、`X ⊗ X = 0`。三者不能互相替代。
4. `X` 表示“不知道”时，不能用 `is_true()` 当“可能为真”；需要三值结论就保留三值到裁决层。
5. 序列化枚举顺序与 `repr(u8)` 是兼容面；新增变体属于破坏性变更。
6. 三态总线冲突必须保持冲突，不能折成 `Unknown` 后假装“未定”。

### 3.3 上层使用规则

- 公开 API 中传递三值语义时优先直接使用 `Ternary`；不要在上层再造一个“差不多”的枚举。
- 跨进程 / JSON 边界可以转 `0/1/X` 或 `index`，但必须在边界文档写清映射。
- 业务层需要四值证据（支持 / 反对 / 冲突 / 无证据）时，用 `trit-orch` 的 `EvidenceVerdict`，不要硬塞进 `Ternary` 后丢失冲突。
- 禁止在 L1/L2 复制真值表或手写 `match` 代替 `Algebra`；发现缺失运算先补 L0 测试。
- 依赖升级必须跑 L0 的全量测试与上层回归；三值语义变更不接受“只改了一个 match”。

### 3.4 最低测试矩阵

引入 `trit-core` 的上层组件至少要覆盖：

- `False / Unknown / True` 对 `Not`、`And`、`Or`、`Xor` 的 9 种组合。
- K3 / Ł3 / B3 三套 `Algebra` 在关键运算上的差异。
- `Unknown` 不被意外二值化；严格 / 宽松二值化行为明确。
- 序列化往返、`index` 往返、真值表槽位不重叠。
- 三态 `Z` 与逻辑 `X` 的转换边界（需要时）。

## 4. 新库准入模板

任何新 crate 进入本栈前，必须在 README 回答：

```text
名称 / 层 / 责任角色：
职责（一句话）：
公开 API：
不变量 / 语义：
失败模式与错误类型：
安全边界（文件 / 网络 / 进程 / 密钥）：
依赖（内部 / 第三方）：
许可（SPDX）：
MSRV：
测试命令与覆盖率重点：
兼容性策略（什么算破坏性变更）：
问题标签与通知对象：
```

少一项就不算“可引入”。只写“工具库”“通用能力”不算职责。

## 5. 依赖写法

工作区内：

```toml
[dependencies]
trit-core.workspace = true
```

外部上层组件（尚未发布 crates.io 时）：

```toml
[dependencies]
trit-core = { git = "https://github.com/bfxh/trit-kernel", tag = "v0.1.0" }
```

规则：

- 不使用 `*`、`latest`、浮动分支；必须 tag / 精确版本 / 锁文件。
- 默认 `default-features = false`，按需开启；不要为了省事打开全部特性。
- 不直接依赖其他 crate 的 `internal` / 私有模块；只走公开 API。
- 不在上层重复定义下层类型；需要扩展时用新类型包裹并明确转换。
- 第三方依赖必须能通过许可证检查；GPL / AGPL 进入分发核心需要所有者书面批准。
- 关键依赖（TLS、解析器、文件系统、进程管理）升级要跑跨平台测试，不只看 Linux。

## 6. 第三方库政策

优先顺序：

1. 标准库 / 已有依赖能解决，就不新增。
2. 纯 Rust、维护活跃、许可证宽松、跨平台。
3. 必须引入时，记录：用途、版本、许可证、维护状态、替代方案、移除条件。
4. 安全敏感依赖（TLS、沙箱、解析器）必须有升级负责人和漏洞响应路径。
5. 不把“能编译”当成“可发布”；Windows / macOS / Linux 至少各跑一次相关测试。

Reasonix 是 MIT 许可的参考实现。移植其架构或代码时保留版权与许可声明，具体见 [RIGHTS.md](RIGHTS.md)。

## 7. 当前第三方依赖清单

直接依赖（以 `Cargo.lock` 为准；升级时同步本表）：

| Crate | 当前版本 | 用途 | 许可 | 备注 |
| --- | --- | --- | --- | --- |
| `serde` / `serde_json` | 1.0.x / 1.0.x | 序列化 / JSON | MIT OR Apache-2.0 | 全栈基础 |
| `thiserror` | 2.0.x | 错误类型 | MIT OR Apache-2.0 | 不引入运行时 |
| `ureq` | 2.12.x | OpenAI-compatible HTTP | MIT OR Apache-2.0 | TLS 传递依赖是 LoongArch 构建阻塞点 |
| `cap-std` / `cap-fs-ext` | 3.4.x | 能力式文件系统 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 工具边界 |
| `notify` | 8.2.x | roots 文件事件 | CC0-1.0 | 平台后端不同 |
| `ignore` | 0.4.x | `.gitignore` 语义 | Unlicense OR MIT | 搜索 / glob |
| `regex` | 1.13.x | 正则 | MIT OR Apache-2.0 | 搜索 |
| `globset` | 0.4.x | glob 匹配 | Unlicense OR MIT | 搜索 / glob |
| `libc` | 0.2.x | Unix 系统调用 | MIT OR Apache-2.0 | 仅 Unix |
| `windows-sys` | 0.61.x | Windows API | MIT OR Apache-2.0 | 仅 Windows |
| `proptest` | 1.12.x | 属性测试 | MIT OR Apache-2.0 | 仅 dev |

说明：

- `notify` 是 CC0-1.0（公共领域奉献），仍需在 NOTICE / 第三方清单中保留来源与版本。
- `ureq` 的 TLS 传递依赖（`ring` / `rustls` 等）未列入本表；CI 的 LoongArch 检查因此只覆盖不含 `trit-host` 的核心 crates。要宣称全栈 LoongArch 支持，必须先解决或替换该 TLS 栈。
- 这张表只列直接依赖。发布前必须用工具生成完整传递依赖清单，不能用手写表代替许可证扫描。

## 8. 上层组件检查清单

- [ ] 显式依赖 `trit-core`，没有自造三值枚举。
- [ ] 只使用公开 API，没有 `pub(crate)` / 私有路径穿透。
- [ ] 依赖方向没有反向或成环。
- [ ] 版本用 tag / 精确范围，锁文件已提交。
- [ ] 三值语义测试覆盖 K3 / Ł3 / B3 的差异点。
- [ ] 新库 README 满足准入模板。
- [ ] 许可与 NOTICE 已更新，第三方许可证清单可生成。
- [ ] 出问题能找到责任角色和标签，而不是“找作者”。
