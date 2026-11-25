# 交接账本（DeliveryReceipt）

状态：已实现（`crates/trit-orch/src/delivery.rs`、`delivery/check.rs`、`src/bin/cli_delivery`）。

来源：从 `bfxh/trit` 的 DeliveryReceipt 账本（C079 / L169）迁入；本仓保留
`receipt::DeliveryReceipt` 的四态状态机，补上持久化账本、包字节哈希和 CLI。

## 它解决什么

`receipt::DeliveryReceipt` 只是内存里的四态状态机。交接真正丢东西的地方在宿主侧：
包发出去后，没人知道对方有没有收到、读没读、有没有真的动手。`delivery` 把这四态
落成可审计的账本，并把包字节用 SHA-256 钉住。

## 账本位置与格式

- 默认路径：`<root>/.agent/delivery/<packet-id>.json`
- 格式标识：`trit-delivery/1`
- 一条账本绑定：`packet_id`、`packet_sha256`、`from`、`to`、`goal`、`created_at`、`states`
- 四态顺序固定：`sent` → `delivered` → `read` → `acted`，不能跳级
- `sent` 的 actor 必须是包里的 `from`；后三态的 actor 必须是包里的 `to`
- 时间戳不能倒退；`acted` 必须带非空 `evidence`
- `actor`、`evidence`、`note` 不允许出现本机盘符或 UNC 路径

包字节变化后，已有账本不能续；`verify` 会返回 `invalid`。`gate` 只在交接门为
`True` 时补 `sent + delivered`，不会自动写 `read/acted`。

## CLI

`trit-orch delivery` 从 stdin 读 JSON，向 stdout 写 JSON；失败退出码 1。

```bash
# 发送方登记
printf '%s' '{"action":"sent","root":".","packet":"packet.json","at":0}' \
  | trit-orch delivery

# 交接门通过后落 sent + delivered（Unknown/False 不落账）
printf '%s' '{"action":"gate","root":".","packet":"packet.json","receiver_state_hash":"H","at":0}' \
  | trit-orch delivery

# 只读预检：交接门 + 账本状态，一个字节都不写
printf '%s' '{"action":"check","root":".","packet":"packet.json","receiver_state_hash":"H","at":0}' \
  | trit-orch delivery

# 接收方推进；acted 必须带 evidence
printf '%s' '{"action":"advance","root":".","packet_id":"p1","state":"Read","actor":"agent-b","at":0}' \
  | trit-orch delivery

# 校验账本与包字节
printf '%s' '{"action":"verify","root":".","packet_id":"p1","packet":"packet.json"}' \
  | trit-orch delivery
```

动作全集：`sent` / `attach` / `deliver` / `read` / `act` / `advance` / `verify` /
`check` / `show` / `gate`。`deliver/read/act` 是 `advance` 的固定状态快捷方式。

## 诚实边界

- 这是**本地自报账本**，不是密码学签名。能写 `.agent/delivery/` 的人也能伪造状态。
- `read/acted` 只证明接收方宿主在本地账本登记了事件，不证明对方模型真的读了或做了。
- 账本只绑定包字节，不替交接门判断“树还是不是交接时那棵”；树的一致性仍由
  `handoff_gate` 的 `base_commit + state_hash` 回答。
- 跨机交接仍要把包放到共享位置；账本各自维护，不做跨机合并。

## 复算

```bash
cargo test -p trit-orch --test delivery --test delivery_cli --test delivery_check
# 4 + 2 + 3 = 9/9
```
