# 交接：构建 + 安全措施（给接手方）

> 两条硬要求：**构建必须能一键判定红/绿**，**安全门必须默认开、可审计**。
> 这里写清现状、缺口和逐条可执行的做法；不要把「本机能编过」当成保障。

## 1. 构建门（当前缺什么）

| 项 | 现状 | 要求 |
|---|---|---|
| 工具链钉版 | 无 `rust-toolchain.toml`，本机实测 1.99.0 | 加 `rust-toolchain.toml`，CI 与本地同版本 |
| 本地构建门 | 无（只有你自己的 `cargo check`） | `RUSTC_WRAPPER= cargo check --workspace --all-targets`，并写进 `scripts/pre-commit` |
| 格式 | 无 fmt 门 | `cargo fmt --all -- --check` 进 pre-commit 与 CI |
| 静态检查 | 无 clippy 门 | `cargo clippy --workspace --all-targets -- -D warnings` |
| CI | `ci.yml` 存在，11-17 有一次 failure 未归因 | 失败必须查清并留记录；不得用「重跑就好了」关闭 |
| 构建缓存 | `RUSTC_WRAPPER=sccache` 服务挂掉会让整次构建看似编译失败 | 先 `RUSTC_WRAPPER= cargo check` 排除环境，再判代码 |

**判红口径（唯一）**：带 `RUSTC_WRAPPER=` 复跑仍失败 = 代码问题；仅 sccache 报
`os error 10054` = 环境问题。

## 2. 安全门（必须补上的五条）

```bash
# 1) 密钥：提交前与 CI 各扫一次（高置信度前缀，不做 "password=" 猜测）
python3 scripts/secret_scan.py

# 2) 依赖漏洞：CI 每周 + 每次改 Cargo.lock
cargo audit

# 3) 许可证与来源：不许引入会改变再许可条件的依赖
cargo deny check licenses sources

# 4) 构建产物/权重/归档不入库
#    先靠 .gitignore，再加提交期判定（见 bfxh/trit 的 scripts/repo_admission.py 与 docs/ADMISSION.md）

# 5) 身份与归属：提交者必须是用户本人，禁止 AI 厂商账号/邮箱/trailer/remote
git log -1 --format='%an <%ae>'
git remote -v
```

| 面 | 规则 | 怎么判 |
|---|---|---|
| 密钥 | 仓库内不得出现 token/私钥；合成夹具要显式标注豁免 | `secret_scan.py` 退出码 0，豁免行带理由 |
| 依赖 | `Cargo.lock` 入库并锁定；升级要写原因 | `cargo audit` 0 漏洞；`cargo deny` 通过 |
| 产物 | `target/`、`dist/`、`*.zip`、权重、日志一律不入库 | `repo_admission.py`（BLOCK 档） |
| 身份 | 只允许用户本人账号出现在 author/committer/trailer/remote | `pre-push` 版权护栏 |
| 网络/子进程 | 新增外连、`Command::new`、`unsafe` 必须有注释与不变量说明 | 评审时逐条看；`hazard_gate` 同类做法可借鉴 |

## 3. 与 `bfxh/trit` 的分工（避免重复建设）

`bfxh/trit` 已经有可复用的成套门，直接搬口径，不要重新发明：

- `scripts/secret_scan.py`、`scripts/ai_vendor_scan.py`（身份/密钥）
- `scripts/repo_admission.py` + `docs/ADMISSION.md`（什么能进版本库）
- `scripts/hazard_gate.py`（unsafe / Relaxed / 裸分配）
- `docs/COLLAB.md §10`（多接手方防丢失：阶段完成即 push、不留 stash、删目录前归档）

## 4. 两个宿主的区分（别改错仓库）

- `bfxh/trit-kernel` 的 `crates/trit-host`：无头宿主（模型/工具循环、效果台账、JSONL、检查点）。
- `bfxh/trit-mc`：Minecraft 角色托管（零依赖纯 std，守护进程 + 游戏窗口控制）。

两者**不是同一个项目**；搜索/替换时先确认仓库根。
