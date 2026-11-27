# 内核-前端能力契约

状态：内核门禁库已实现；桌面 / Web 前端适配待接入。实现见 [`frontend_compat.rs`](../crates/trit-host/src/frontend_compat.rs)。

适用对象：前端 / 桌面开发者、内核维护者。目标只有一个：**内核可以先更新，但前端不认识的新能力绝不能被默认打开或显示成可用。** 缺依赖时 fail-closed，锁住时通知责任方，而不是让用户点到半残按钮。

## 1. 不变量

1. 内核声明能力，前端声明自己实现了哪些入口；没有声明就不启用。
2. 前端不认识的能力默认锁住，并且从普通面板隐藏。
3. 依赖不完整、依赖成环、前端接口过旧，都锁住；不猜测、不降级成“可能能用”。
4. 用户显式关闭优先于默认开启；关闭一个能力会让依赖它的能力锁住，但原因是 `dependency_unavailable`，不算前端漏实现。
5. 锁住必须可解释：每条锁都有原因码、责任方、通知对象。
6. 能力门禁不是安全授权。它只决定“能不能显示入口”，不替代工具权限、沙箱或 ACL。

## 2. 数据模型

```text
FrontendCapabilities { api_level: u32, features: set<feature_id> }
FeatureSpec         { id, min_api_level, requires[], default_enabled, owner }
GateRequest         { enable: set<feature_id>, disable: set<feature_id> }
GateReport          { features: map<id, FeatureDecision>,
                      groups:   map<id, FeatureGroupDecision>,
                      notices:  FeatureNotice[] }
```

- `api_level`：单调递增的接口层级。只有破坏性契约变化才递增，不能拿它当版本号营销。
- `feature_id`：稳定字符串，例如 `context.catalog`、`mcp.tools`、`checkpoint.git`。发布后不复用、不改语义。
- `requires`：依赖的其他能力 id。依赖未启用则本能力锁住。
- `default_enabled`：依赖满足且前端支持时的默认值；`false` 表示用户可见但默认关闭。
- `owner`：出问题时首先负责的角色，例如 `kernel.agent`、`frontend.desktop`。不写个人账号。
- `enable` / `disable`：外部请求；`disable` 优先于 `enable`。

## 3. 判定顺序

对每个能力按以下顺序判定，先命中先返回：

1. 前端 `api_level < min_api_level`：锁住，原因 `frontend_api_too_old`。
2. 前端 `features` 不含该 id：锁住，原因 `frontend_feature_missing`。
3. 任一 `requires` 未启用：锁住，原因 `dependency_unavailable`。
4. 依赖成环：锁住，原因 `dependency_cycle`。
5. 外部 `disable` 包含该 id：关闭，但入口可见（用户主动选择）。
6. 外部 `enable` 包含该 id，或 `default_enabled = true`：启用。
7. 否则：关闭，入口可见（默认关闭，用户可开）。

状态与可见性：

| 状态 | `visible` | 含义 | 前端行为 |
| --- | --- | --- | --- |
| `enabled` | true | 能力可用 | 显示入口，允许调用 |
| `disabled` | true | 依赖满足，但用户 / 默认关闭 | 显示开关或置灰入口；不要调用内核能力 |
| `locked` | false | 契约或依赖不满足 | 普通面板不显示；诊断页可显示原因 |

## 4. 组 / 扩展坞 fail-closed

一个扩展坞、插件面板或设置分区可以声明为组：

```text
FeatureGroupSpec { id, members[] }
```

组内任一成员 `locked`，则：

- `available = false`
- `visible = false`
- `blocked_by` 列出锁住的成员

用户主动 `disabled` 的成员不算阻塞：面板可以打开，只是那个功能关着。只有“无法证明可用”的锁才让整组隐藏。前端不得先打开面板再逐个探测；必须先取门禁结果。

## 5. 前端上报示例

```json
{
  "api_level": 2,
  "features": ["context.catalog", "checkpoint.files", "mcp.tools"]
}
```

内核返回（裁剪）：

```json
{
  "features": {
    "context.catalog": {
      "id": "context.catalog",
      "state": "enabled",
      "visible": true,
      "reasons": []
    },
    "context.compression": {
      "id": "context.compression",
      "state": "locked",
      "visible": false,
      "reasons": [
        { "code": "frontend_feature_missing", "feature": "context.compression" }
      ]
    }
  },
  "groups": {
    "dock": {
      "id": "dock",
      "available": false,
      "visible": false,
      "blocked_by": ["context.compression"]
    }
  },
  "notices": [
    {
      "feature": "context.compression",
      "owner": "frontend",
      "reason": {
        "code": "frontend_feature_missing",
        "feature": "context.compression"
      },
      "message": "前端没有实现能力入口 context.compression"
    }
  ]
}
```

`LockReason` 使用 `code` 标签序列化；`FeatureState` 使用 `snake_case`。字段名和原因码是契约的一部分，已有原因码不得改义。

## 6. 锁住后的通知与展示

- `notices` 是给宿主 / 开发者的结构化通知，不是给普通用户的错误列表。
- `owner = frontend`：前端没跟上，通知前端维护者；内核侧记录一条兼容性缺口。
- `owner = 内核能力 owner`：依赖声明或打包有问题，通知内核维护者。
- 普通用户只在“主动请求该功能”时看到一句“该功能暂不可用”；不在启动时弹窗扫射。
- 同一 `feature + reason.code` 在进程内去重；发布说明里汇总一次即可。
- 前端诊断页可以展示完整原因，但不得把内部路径、密钥、会话内容放进通知。

责任路由和升级时限见 [RESPONSIBILITY.md](RESPONSIBILITY.md)。

## 7. 版本与发布规则

- 新增能力：给新 id，写 `min_api_level`、依赖、owner、默认值；旧前端不认识时自动锁住。
- 修改能力语义：不改变已有 id，新增 id 或提升 `min_api_level`；旧前端锁住。
- 删除能力：先在至少一个发布周期标记弃用并保留锁住状态，再删除；不能直接让前端调用 404。
- 提高 `min_api_level`：必须写迁移说明；不能静默让旧前端全部锁死。
- 前端能力表只能声明真实实现的入口；谎报会导致运行时未定义行为，属于前端 bug。
- 内核发布说明必须列出：新增 id、提升的最低 `api_level`、默认值变化、移除 id。

## 8. 前端接入清单

- [ ] 启动时上报真实的 `api_level` 与能力集合，不从内核反推。
- [ ] 先取 `GateReport`，再渲染面板；不根据本地常量猜测可见性。
- [ ] `visible = false` 的能力不出现在普通导航、命令面板、快捷键提示里。
- [ ] 组 `visible = false` 时整组不打开，不做空壳页。
- [ ] `disabled` 能力显示为关闭 / 可开启，不显示为错误。
- [ ] 收到 `notices` 后按 owner 路由，不向用户刷内部错误。
- [ ] 用户切换开关时写 `enable` / `disable`，下次协商重新取报告。
- [ ] 契约测试覆盖：前端过旧、缺 feature、缺依赖、用户关闭、组 fail-closed、解锁恢复。

## 9. 非目标

- 不用能力门禁替代工具权限、沙箱、工作区 ACL。
- 不用它做 A/B 实验或模型能力开关；那些需要独立开关和审计。
- 不让前端自行“猜一个默认值”绕过锁住状态。

## 10. 设置项契约

设置项不是能力门禁的替代：能力决定“入口能不能显示”，设置决定“显示后默认开不开、参数是多少”。设置由前端持久化，内核只接受显式参数或环境变量；内核不读写注册表、不替用户改系统设置。

### 10.1 两项自启动

| id | 类型 | 默认 | 作用 | 生效时机 | 内核行为 |
| --- | --- | --- | --- | --- | --- |
| `startup.app_autostart` | bool | `false` | 桌面应用随系统登录启动 | 下次登录 | 内核不实现；前端 / 安装器负责开机项，内核不写注册表 |
| `startup.agent_autostart` | bool | `false` | 应用启动后自动恢复上次 agent 会话或服务 | 应用启动时 | 前端调用 `session-advance --root ... --apply`，或启动 `remote-ledger` / `remote-serve`；内核不自动拉起 |

- 两项互不依赖：关闭应用自启动不影响手动打开；关闭 agent 自启动时应用启动后停在空会话。
- `startup.agent_autostart` 必须配合 `concurrency.max`，避免启动即打满。
- 失败回退：自动恢复失败（会话损坏、预算不足、provider 不可用）时展示一次可关闭的通知，不重试、不静默清空。

### 10.2 并发与预算

| id | 类型 | 默认 | 对应内核入口 |
| --- | --- | --- | --- |
| `concurrency.max` | u32，`0` = 不限 | `0` | `--max-concurrency` / `TRIT_MAX_CONCURRENCY` |
| `session.workers` | u32 | `4` | `--workers` |
| `budget.tokens` | u64，`0` = 不限 | `0` | `--budget-tokens` |
| `budget.cny` | f64，`0` = 不限 | `0` | `--budget-cny` |
| `cost.usd_cny` | f64 | `7.2` | `TRIT_USD_CNY` |
| `cost.model_prices` | path | 内置价目表 | `TRIT_MODEL_PRICES` |

- `concurrency.max` 是**总并发上限**：`--workers` 与它取小；多会话、多 root、跨机任务叠加时不会超过它。
- 预算耗尽 fail-closed：token 预算小于任务数直接拒绝；人民币预算达到 80% 写 warning、超过标记 `exceeded`；跨机 ledger 达到预算后 `/claim` 返回 503。
- 汇率与价目表只影响展示和预算折算，不改变真实计费；未知模型不按 0 计价。

### 10.3 浏览器

| id | 类型 | 默认 | 对应内核入口 |
| --- | --- | --- | --- |
| `browser.enabled` | bool | `false` | 前端是否展示 `browser` 工具 |
| `browser.chrome_path` | path | 自动探测 | `TRIT_CHROME` |
| `browser.headless` | bool | `true` | 启动参数 |
| `browser.screenshot_allow_write` | bool | `false` | 截图需要 workspace `allow-write` |

- 浏览器通过 CDP 驱动本机 Chrome / Chromium / Edge，不嵌入渲染引擎；没有可用浏览器时返回明确错误，不降级成“假装成功”。
- 截图路径限制在 workspace 内；未开启 `allow-write` 时拒绝截图。

### 10.4 观测

| id | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `diagnostics.show_background_tasks` | bool | `true` | `diagnostics` 输出 `background_tasks[]` 与 `background_started` |
| `diagnostics.token_rate` | bool | `true` | 会话报告展示 `tokens_per_second` |

- 观测字段只读，不参与完成裁决；旧日志缺少指标时 `tokens_per_second` 为 `null`。

### 10.5 前端要求

- 设置项必须显式持久化并在启动时上报；不要用本地常量猜默认值。
- 设置变化只影响下一次调用，不允许热改正在运行的任务。
- 每个开关都要有一句“为什么默认关闭”的说明；两个自启动必须默认关闭。
- 设置项 id 与本文一致；新增项走第 7 节的版本规则。
