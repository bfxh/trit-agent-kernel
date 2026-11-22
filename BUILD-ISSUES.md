# 构建问题清单（2026-11-21 实测）

> 给接手方：这里只记**构建/CI 层面**踩到的问题，每条都有复现命令、
> 判定与规避方式。代码本身没有构建错误（见 §1 的通过记录）。

## 1. 结论先行

| 仓库 | `cargo check --all-targets` | 备注 |
|---|---|---|
| `bfxh/trit-agent-kernel`（本仓库） | **通过**（17.4 s，5 个 crate） | 前提：`RUSTC_WRAPPER=` 绕开 sccache，见 §2 |
| `bfxh/trit-mc`（Minecraft 托管，独立项目） | **通过**（3.8 s） | 零依赖纯 std；**没有 CI workflow** |

## 2. 构建故障：sccache 连接被重置（本机，不是代码问题）

**现象**（首次构建，未加任何环境变量）：

```
error: process didn't exit successfully: `sccache '...\1.99.0-x86_64-pc-windows-gnu\bin\rustc.exe' -vV` (exit code: 2)
--- stderr
sccache: error: failed to execute compile
sccache: caused by: Failed to send data to or receive data from server
sccache: caused by: Failed to read response header
sccache: caused by: 远程主机强迫关闭了一个现有的连接。 (os error 10054)
```

**根因**：`RUSTC_WRAPPER=sccache` 指向的 sccache 服务端已经死掉/端口被重置；cargo 在探测
`rustc -vV` 时就失败，于是整次构建在「读 Cargo.toml」之前就中止——看起来像代码编译不过，
实际一行代码都没编。

**复现**：

```bash
cd trit-agent-kernel
cargo check --workspace --all-targets        # 红：sccache ... os error 10054
```

**规避 / 判定**：

```bash
RUSTC_WRAPPER= CARGO_INCREMENTAL=0 cargo check --workspace --all-targets   # 绿：Finished in 17.44s
```

- 判定口径：**只有带 `RUSTC_WRAPPER=` 复跑仍然报错，才算代码问题**。仅 sccache 报错 = 环境问题。
- 长期修法（任选一）：重启 sccache 服务（`sccache --stop-server && sccache --start-server`），
  或在 `~/.cargo/config.toml` 里去掉 `rustc-wrapper`，或按需用上面的环境变量单次绕开。

## 3. CI 现状（`bfxh/trit-agent-kernel`）

最近 3 次 `main` 上的 CI：`2026-11-17T17:32` success、`14:29` success、`14:04` **failure**。
（14:04 那次失败的具体 job 需要在 Actions 页面确认；本地全量 `cargo check` 已通过，
说明大概率同样是环境/缓存类问题，而不是编译错误。）

`bfxh/trit-mc` 仓库**没有 `.github/workflows/`**，所以「构建是否一直绿」没有任何自动证据。

## 4. 给接手者的最小验证顺序

```bash
# 1) 先排除环境：sccache 死掉会让一切构建看起来都红
RUSTC_WRAPPER= cargo check --workspace --all-targets

# 2) 再看测试
RUSTC_WRAPPER= cargo test --workspace --quiet

# 3) 只有 1、2 都带 RUSTC_WRAPPER= 仍失败，才按编译错误处理
```

## 5. 尚未做的（别当成已解决）

- 本仓库没有 `rust-toolchain.toml` 与本机 toolchain 对齐的说明（本机实测 1.99.0）。
- 没有 `cargo fmt --check` / `cargo clippy -D warnings` 的本地门与 CI 门对照表。
- `bfxh/trit-mc` 没有 CI，构建与测试完全靠人工。
