# 沙箱后端

`trit-host run --allow-shell` 需要 OS 沙箱，默认不提供“裸 shell”。三种来源：

| 方式 | 行为 |
| --- | --- |
| `--sandbox <program> [--sandbox-arg ...]` | 调用外部 wrapper（bwrap / sandbox-exec / 自定义），由它做写根约束 |
| `--sandbox builtin` | 内核构造 Linux `bwrap` 参数：只读系统 + 可写 workspace + `--unshare-all` |
| `--allow-unsandboxed-shell` | 显式降级；没有沙箱时执行 bare shell |

## 内置后端（Linux）

`SandboxCommand::builtin(root)` 生成：

```text
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
      --bind <root> <root> --chdir <root> \
      --unshare-all --die-with-parent -- sh -c <command>
```

- 系统仍需要安装 `bubblewrap`；内核只负责参数构造与 fail-closed。
- 非 Linux 平台直接返回错误 `builtin sandbox is only available on Linux`，不回退到无沙箱。
- workspace 以 `--bind` 方式可写；其余根文件系统只读。

## 内置后端（Windows AppContainer）

`SandboxCommand::builtin(root)` 在 Windows 返回：

```text
<trit-host.exe> appcontainer-run --workspace <root> -- cmd /C <command>
```

`appcontainer-run` 的步骤：

1. 按 workspace 路径 hash 创建/复用 AppContainer profile（`trit-sandbox-<hash>`）；
2. 给 `<root>` 授予该 AppContainer SID 的 `GENERIC_READ | GENERIC_WRITE |
   GENERIC_EXECUTE` ACE（继承到子目录）；
3. 用 `InitializeProcThreadAttributeList` +
   `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 创建子进程；
4. 等待退出并原样返回退出码。

边界：

- 只授予 workspace 的写权限；`C:\Windows\Temp`、用户目录等写入被拒绝（有回归测试）。
- profile 按 workspace 复用，不自动删除；需要清理时用系统工具删除。
- 失败 fail-closed：profile / ACL / 进程创建任一步失败 → 非零退出，不回退裸 shell。
- 不是网络隔离；`SECURITY_CAPABILITIES` 为空。

## 平台边界

- Linux：内置 `bwrap` 参数；macOS：内置 `sandbox-exec -p`；Windows：内置
  AppContainer（`appcontainer-run`）。
- “内置”指内核知道怎么构造隔离参数，不表示内核自己实现了 namespace/ACL；
  隔离强度取决于 `bwrap` / `sandbox-exec` / AppContainer 与操作系统的正确性。
- 它不是完备的安全边界：不防 `bwrap` / `sandbox-exec` / AppContainer 自身的缺陷，
  也不做网络隔离。
