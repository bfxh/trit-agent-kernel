# 会话 SQLite 投影

会话的权威记录永远是 append-only 的 `<session>.jsonl`。SQLite 只是它的派生索引，
用于按 turn / 事件 / 工具调用做查询；投影随时可以删掉重建，不会影响会话本身。

实现见 `crates/trit-host/src/session_index.rs`，CLI 为 `trit-host session-index`。

## 表结构

`INDEX_SCHEMA = 1`，三张表：

| 表 | 列 | 说明 |
| --- | --- | --- |
| `meta` | `key`, `value` | `schema`、`source`（来源 JSONL）、`last_seq` |
| `records` | `seq`, `turn`, `kind`, `tool`, `passed`, `gate_status` | 一行一个事件；`kind` 与 JSONL 的 `type` 一致；`tool` 取 tool_requested / tool_completed |
| `turns` | `turn`, `task`, `started_seq`, `finished_seq`, `steps`, `complete` | `turn` 从 1 开始；首个 `turn_started` 之前的事件记在 turn 0 |

`passed` 只对 `tool_completed` 有意义（1/0/NULL）；`gate_status` 只对 `gate_chain`
有意义。投影不复制工具输出正文，避免把敏感内容二次落盘。

## 重建

```bash
trit-host session-index --session <session.jsonl> --db <session.index.db> --rebuild
```

- 先在 `<db>.rebuild.<pid>` 建库并整批写入，成功后删除旧投影、原子改名为目标；
  中途失败只会留下暂存文件（会被清理），不会出现半份可读投影。
- 旧库不需要迁移：schema 变化时直接重建；投影缺失时也走同一条路径。
- 输出 `{session, db, replaced, summary}`，`summary` 为记录数、turn 数、工具调用数、
  门链条数、最后 `seq`。

## 校验

```bash
trit-host session-index --session <session.jsonl> --db <session.index.db> --verify
```

- 按日志重建到暂存库，先比 `summary`，再比 `records` + `turns` 的全量行转储；
  任一不同即 `matches=false`，退出 1。
- 投影缺失时同样退出 1，`actual=null`，不会顺手建一个空库。
- `verify` 只读现有投影，不做替换；确认不一致后再显式 `--rebuild`。

## 边界

- 投影是只读派生层：没有从 SQLite 写回 JSONL 的路径，删库不会丢会话。
- `meta` 里的 `source` / `last_seq` 是构建快照，不参与逐行比较；比较只看 `records`
  与 `turns`。
- 单写者假设：同一投影同时被多个进程重建没有加锁保护，由调用方串行化。
- `history 归档`（跨会话汇总、冷数据搬迁）仍不在本切片范围。
