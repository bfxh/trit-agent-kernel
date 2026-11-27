# 设计评分：代码设计 / 网络设计 / 前端设计 / 审美

评分回答的是"这东西做得有没有设计感、有没有审美"，不是"这一轮跑得顺不顺"。
四个维度各自从 100 分起扣，可解释、可复算，只读产物内容，不靠模型自评。

## 1. 四个维度

### 代码设计（`code`）

| 规则 | 扣分 | 触发 |
| --- | --- | --- |
| `file-too-long` | −10 | 单文件超过 800 行 |
| `error-handling` | −10 | `unwrap()` / `expect(` / `panic!` 超过 5 处 |
| `no-tests` | −10 | 文件超过 200 行却没有任何测试 |
| `no-comments` | −5 | 文件超过 200 行、注释不足 2% |
| `duplicated-block` | −10 | 连续 3 行以上完全重复的代码块 |

### 网络设计（`network`）

只对包含网络调用的文件生效（`reqwest`/`ureq`/`hyper`/`axios`/`fetch(`/`client.post(` 等）。

| 规则 | 扣分 | 触发 |
| --- | --- | --- |
| `no-timeout` | −15 | 有网络调用但没有超时 |
| `no-status-classification` | −10 | 不区分 4xx/5xx |
| `unbounded-response` | −10 | 响应体没有上限（`take`/`limit`/`MAX_`） |
| `hardcoded-secret` | −30 | 疑似硬编码密钥（`sk-` + 长串） |

### 前端设计（`frontend`）

| 规则 | 扣分 | 触发 |
| --- | --- | --- |
| `no-accessibility` | −10 | 没有 `aria-` / `alt=` / `role=` / `<label` |
| `missing-ui-states` | −10 | 加载 / 空 / 错误等状态少于 2 种 |
| `no-responsive` | −5 | 没有 `@media` 或断点前缀 |

### 审美（`aesthetics`）

| 规则 | 扣分 | 触发 |
| --- | --- | --- |
| `too-many-colors` | −10 | 超过 8 种颜色 |
| `too-many-font-sizes` | −5 | 超过 5 种字号 |
| `inconsistent-spacing` | −5 | 超过 6 种 padding 值 |
| `long-lines` | −5 | 超过 120 字符的行占比 > 20% |

四维平均分即 `overall`。

## 2. 分数怎么用

- **默认只打分、只记录**：`Settings.scoring.participate_in_decision = false`。
  分数不会改变完成裁决（那仍由 `judge_completion` 与回执决定）。
- **喂回模型**：请求 system 里已经写明设计质量会被评分、以及要遵守的要点
  （模块边界、显式错误处理、网络超时与有界响应、无障碍 / 状态 / 响应式 / 克制的设计令牌）。
- 需要让分数参与调度或交付时，再打开 `participate_in_decision`，由上层按分数处理。

## 3. 与结果分的关系

`crate::scoring::score_turn` 是"结果合规分"（完成是否被证明、存疑/失败回执、重复校验）；
`crate::design_score::score_artifacts` 是"设计质量分"（本文件描述的四个维度）。
两者不互相替代：证据裁决照样 fail closed，设计分只描述产出质量。

## 4. 检查

```bash
cargo test -p trit-host --lib design_score
cargo test -p trit-host --test scoring
```
