# 任务：Windows AppContainer 内置沙箱后端

## 级别与当量

**S**。预计 3–5 天，不可与其它批次并行：涉及 Win32 安全 API、ACL 修改、进程创建与
真实隔离验证，失败模式是安全边界，不能用“看起来能跑”验收。

## 起点

- 仓库：`bfxh/trit-agent-kernel`
- 分支：`feat/gate-chain`（或从 `main` 新开）
- 起点 SHA：以派发当天的 `main`/分支 HEAD 为准，写进 PR 描述
- 环境：Windows 10/11 x64，Rust 1.88+，普通用户（不需要管理员）
- 必读：
  - `docs/SANDBOX.md`（现有 Linux/macOS 后端与边界）
  - `crates/trit-host/src/tools.rs`（`SandboxCommand`、`bash`、`append_shell`）
  - `crates/trit-host/Cargo.toml`（`windows-sys` features）
  - `crates/trit-host/src/bin/trit-host.rs`（`--sandbox` 解析与 `run` 分支）

## 目标（每条可被命令判真假）

- **G1** `SandboxCommand::builtin(root)` 在 Windows 返回 AppContainer 后端：
  `program` 指向当前 `trit-host` 可执行文件，`args` 以 `appcontainer-run --workspace
  <root> --` 开头；不再返回 “only available on Linux/macOS”。
- **G2** 新增 `trit-host appcontainer-run --workspace <dir> -- <program> [args...]`：
  1. 创建或复用 AppContainer profile（名字固定前缀 + 用户 SID 派生）；
  2. 给 `<dir>` 授予该 AppContainer SID 的读/写/执行 ACE（保留原 DACL 条目）；
  3. 用 `InitializeProcThreadAttributeList` +
     `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 创建子进程；
  4. 等待退出并把子进程退出码原样返回；
  5. 失败时恢复原 DACL，不执行子进程。
- **G3** fail-closed：profile 创建、ACL 授予、进程创建任一步失败 → 非零退出，
  **不允许**回退到无沙箱 `cmd /C`。
- **G4** 测试与文档：
  - `#[cfg(windows)]` 单测覆盖 ACL 构造、属性列表失败路径、退出码转发；
  - 端到端：workspace 内写文件成功；workspace 外（例如 `%USERPROFILE%`）写文件失败；
  - 更新 `docs/SANDBOX.md` 的 Windows 段落。

## 非目标

- 不改 Linux `bwrap` 与 macOS `sandbox-exec` 分支。
- 不做网络隔离（AppContainer 的 capability 先留空，只做文件系统约束）。
- 不修改用户目录、系统目录的 ACL；只改 `--workspace` 指向的目录。
- 不做“管理员模式”或绕过 UAC。

## 验收（可复算）

```powershell
# 1) 单测
cargo test -p trit-host --lib tools::tests::windows_appcontainer

# 2) 隔离验证（workspace 内）
trit-host run --task t --workspace . --allow-shell --sandbox builtin `
  --script .\script.json
# 期望：exit 0；脚本写入 .\out.txt 成功

# 3) 隔离验证（workspace 外）
# script 尝试写入 %USERPROFILE%\trit-escape.txt
# 期望：非零退出或写入失败；文件不存在

# 4) ACL 审计
icacls . | Select-String "AppContainer"
# 期望：包含本次 profile 的 SID 或应用容器名对应的 ACE

# 5) 进程身份
# 在 appcontainer-run 内执行 `whoami /groups`
# 期望：包含 AppContainer 标记的完整性级别（Low/Untrusted）
```

AI 断言：`exit code`、文件存在性、`icacls` 输出、`whoami /groups` 输出；不接受
“看起来被隔离了”。

## 独立 oracle

- 用 `icacls` 与 `whoami /groups` 作为第二实现，验证 ACL 与进程身份；
- 用 `--allow-unsandboxed-shell` 作为对照组：同一脚本在裸 shell 下应成功写出
  workspace 外文件，在 `--sandbox builtin` 下必须失败。

## 性质测试 / 不变量

- 同一 workspace 重复运行：profile 可复用，ACL 不重复叠加（ACE 幂等）。
- 子进程异常退出（非零 / 被终止）：宿主返回同一退出码，profile 仍可清理。
- ACL 授予失败：原 DACL 必须逐字节恢复（可用 `icacls` 保存前后快照比对）。
- 无管理员权限：仍可创建 per-user AppContainer profile；若系统策略拒绝，明确报错。

## 性能预算

- `--sandbox builtin` 相对外部 wrapper 的启动延迟 ≤ 2x（`Measure-Command` 各跑 5 次取中位数）；
- 不要求吞吐指标，但不得在每次 `bash` 调用里重复创建 profile（应复用）。

## 失败负结果（必须入册）

- 如果 `CreateAppContainerProfile` 在某些 Windows 版本/策略下不可用，写清错误码、
  复现命令与替代方案（继续用外部 wrapper），不得静默回退。
- 如果 ACL 修改需要管理员权限，写清判定方法与拒绝路径。

## 禁区

- 不把 `--sandbox builtin` 的失败静默降级为 `--allow-unsandboxed-shell`。
- 不修改 `crates/trit-host/src/tools.rs` 里 Linux/macOS 的 bwrap/sandbox-exec 参数。
- 不把 AppContainer profile 名字写成全局固定值（多用户/多 workspace 会串）。
- 不在测试里写入真实用户目录；用临时目录并在结束后恢复 ACL。

## 回退

- 删除本批次创建的 AppContainer profile（`DeleteAppContainerProfile`）；
- 用保存的 DACL 恢复 workspace 权限；
- 代码 revert 到 `--sandbox builtin` 在 Windows 拒绝的行为。

## 交付物

- 代码：`SandboxCommand::builtin` Windows 分支、`appcontainer-run` 子命令、ACL 模块；
- 测试：单测 + 端到端（Windows-only）；
- 文档：`docs/SANDBOX.md` Windows 段落、README 工具边界条目；
- PR 描述含：`cargo test` 读数、隔离验证命令与输出、`icacls` 前后快照、性能读数、
  失败负结果。
