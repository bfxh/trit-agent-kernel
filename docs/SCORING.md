# 前后端评分：规则、阈值、以及"是否参与决策"

评分只描述一轮做得好不好，不改变证据裁决（完成/否证/未知仍由 `judge_completion` 决定）。

## 1. 评分规则

基础分 100，按项扣分，最低 0：

| 触发 | 扣分 | 说明 |
| --- | --- | --- |
| 完成未证明（`not proved`） | −40 | 没有足够证据背书完成声明 |
| 完成被否证（`refuted`） | −30 | 证据明确否定 |
| 完成冲突（`conflicted`） | −20 | 账本与声明冲突 |
| 存疑回执 | −5 / 个（上限 −20） | `in_doubt` 的结果不明 |
| 失败回执 | −10 / 个（上限 −30） | 明确失败 |
| 重复校验 | −2 / 次（上限 −10） | 同一命令重复执行 |
| 前端面无前端产物 | −10 | 只对 `Surface::Frontend` 生效 |

`Surface::Backend` / `Frontend` / `Both` 三种打分对象；`Both` 目前等同后端面（不额外扣前端分）。

阈值 `BLOCK_THRESHOLD = 60`：`score < 60` 记 `blocked = true`，`reasons` 记录全部扣分原因。

## 2. 是否参与决策（可在设置里开关）

| 位置 | 默认 | 含义 |
| --- | --- | --- |
| `Settings.scoring.participate_in_decision` | `false` | 关闭：只打分并记录；开启：上层可按 `blocked` 阻断 |

**默认关闭**是刻意的：分数是启发式，不能替代证据裁决。需要它影响调度/交付时再打开。

## 3. 与技术栈策略的关系

评分可以看"是否遵守技术栈策略"作为扣分项（见 [`TECH-STACK-POLICY.md`](TECH-STACK-POLICY.md)）；
但技术栈本身是**硬约束**，不靠扣分兜底：子智能体越权新增语言/框架会被 `validate_stack` 直接拒绝。

## 4. 检查

```bash
cargo test -p trit-host --test scoring
cargo test -p trit-host --test tech_stack_policy
```
