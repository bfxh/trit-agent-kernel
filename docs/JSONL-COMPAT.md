# JSONL 布局兼容矩阵

本仓有三类 append-only JSONL 账本。它们是崩溃恢复的唯一权威记录，因此布局兼容
按“向后兼容、向前 fail-closed”执行，并由 `crates/trit-host/src/jsonl_layout.rs`
作为单一事实来源、`crates/trit-host/tests/jsonl_compat.rs` 做机器校验。

## 读取规则

1. 只接受 `schema ==` 当前版本；遇到更大的 schema 直接报错（fail-closed），不猜布局。
2. 同版本内后加的字段一律 `#[serde(default)]`，老文件缺字段即取默认值。
3. 写入只整行追加，不重写既有行；升级不会改动老字节，`truncate_before_turn` 之外的
   重写路径不存在。
4. 崩溃尾行：mutation / rewind 账本容忍最后一行的半截 JSON（崩溃可能只写了一半）；
   会话日志不容忍，它是权威记录，半行必须显式报错。

## 矩阵

| 文件 | schema | 必填字段 | 后加可选字段（默认值） | 崩溃尾行 |
| --- | --- | --- | --- | --- |
| `.agent/write-journal.jsonl` | 1 | `schema, seq, op_id, phase, tool, at` | `call_id=""`, `paths=[]`, `lease_epoch=null`, `lease_locks=[]`, `passed=false`, `in_doubt=false`, `output=""`, `backups=[]` | 容忍 |
| `rewind-journal.jsonl` | 1 | `schema, seq, op_id, phase, turn, at` | `paths=[]`, `backups=[]`, `backend=""`（按 files）, `backup=""`, `path=""`, `restored=[]`, `deleted=[]`, `failures=[]` | 容忍 |
| `<session>.jsonl` | 1 | `schema, seq, event` | `event.proposal.context=null`；`event.context_compacted.{retained,dropped_chars,truncated_chars,epoch_before,epoch_after,freshness}` 默认 0/null；`event.turn_finished.metrics=null`；`event.gate_chain.{failed_at,failures,passed,blocked,unknown,skipped,report}` 默认空/0 | 不容忍 |

说明：

- `write-journal` 的 `lease_locks` 是 B7.7 第四刀新增；老记录只有 `lease_epoch`，
  恢复者不得据此自动回滚（`journal-status --rollback` 会拒绝并报 `blocked`）。
- `rewind-journal` 的 `backend` / `backup` 是第三刀新增；旧记录 `backend=""` 按
  `files` 处理，恢复入口仍然拒绝跨后端混用。
- `session` 的后加字段全部落在事件负载内，顶层三字段从首版起未变；旧会话目录
  可以直接用 `SessionLog::open` 读入并继续追加。

## 升级与回退

- **加字段**：保持 schema 不变，字段必须 `#[serde(default)]`；同步在
  `jsonl_layout.rs` 的 `optional` 列表与本表登记。
- **升 schema**：必须先提供迁移器与迁移后校验，再改常量；在此之前新版本写出的
  文件会被旧读者拒绝，这是有意的 fail-closed。
- **回退**：不允许把 schema 降回旧版本；旧读者读新文件必须报错而不是丢弃记录。

## 校验

- `crates/trit-host/tests/fixtures/jsonl/` 保存六份 golden fixture：
  `session|write-journal|rewind-journal` 各一份 legacy（缺后加字段）与一份
  future（schema 2）。
- `tests/jsonl_compat.rs` 断言：legacy 可读且默认值正确；追加不重写既有字节；
  future schema 在库内与 CLI 两条路径都 fail-closed；本文件的矩阵行与
  `jsonl_layout.rs` 逐行一致。
