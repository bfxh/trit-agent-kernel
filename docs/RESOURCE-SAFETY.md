# 资源与生命周期安全审计

状态：随本轮回滚、HTTP MCP 与浏览器能力更新。原则是“谁获取、谁释放；失败路径也要释放”。

## 1. 浏览器（Chrome / Chromium / Edge）

| 资源 | 获取 | 释放 |
| --- | --- | --- |
| Chrome 子进程 | `Browser::launch` spawn | `Browser::drop` kill + wait；启动中途失败由 `LaunchGuard` kill |
| 临时 user-data-dir | `unique_temp_dir` | `Browser::drop` 删除；启动失败由 `LaunchGuard` 删除 |
| WebSocket | `WebSocket::connect` | 随 `Browser` drop 关闭 |
| CDP target | `Target.createTarget` | 每次 `open` 关闭旧 target；进程退出兜底 |

边界：单页面、单实例；不等待 `load` 事件；截图路径限制在 workspace 且需要 `allow-write`。

## 2. HTTP MCP

| 资源 | 上限 / 释放 |
| --- | --- |
| 响应体 | 8 MiB，超出截断 |
| 工具缓存 | TTL 60s（`tools_ttl_secs` 可配）或显式 `refresh()`；最多 4096 条 |
| 连接 | `ureq::Agent` 复用连接池，不每次 RPC 新建 |
| 初始化 | `AtomicBool` 幂等，避免重复 `initialize` |

边界：工具缓存没有 TTL；服务端工具变更后需要重建客户端（当前不自动刷新）。

## 3. 后台线程

- `background::register` 返回守卫，线程结束 Drop 注销；`started_total` 只是计数器，不保留条目。
- `remote-serve` / `remote-ledger` 每连接线程受 `max_connections` 限制，超额直接 503。
- `remote_fleet` 健康监控随派发结束退出。
- 多会话扫描使用 `thread::scope`，作用域结束回收线程。

## 4. 并发与连接

- `concurrency_limit` 名额守卫 Drop 释放；上限 `0` 表示不限。
- provider 建连超时（`connect_timeout`）与整次请求超时（`timeout`）分离。
- 写批量按全部路径冲突分组：同一路径串行，互不冲突才并行。

## 5. 写日志与崩溃恢复

- `session-advance --apply` 检查 `journal.in_doubt`：默认 fail closed。
- `--write-journal-rollback` 先做安全回滚；仍被活租约保护的操作进 `blocked`，拒绝自动回滚。
- journal 是 append-only JSONL，回滚后闭合记录，不随运行时间累积内存。

## 6. 已知边界与后续

- `McpToolClient::routes()` 有 5s 缓存与 `invalidate()`；临时分配只发生在缓存过期或失效时。
- HTTP MCP 工具缓存有 TTL（默认 60s）与 `refresh()`；服务端 `list_changed` 仍不会自动推送，需要重建客户端或调用 refresh。
- 浏览器单页面；多标签/页面池是后续能力。
- `background` 注册表没有条目上限，但条目随线程结束移除；长期服务应监控 `running()`。
