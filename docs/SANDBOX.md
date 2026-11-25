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

## 平台边界

- **Windows AppContainer 未实现**：`--sandbox builtin` 在 Windows 上拒绝；后续 S 级
  批次见 [AGENT-TASKS/S-WINDOWS-APPCONTAINER.md](AGENT-TASKS/S-WINDOWS-APPCONTAINER.md)。
- macOS 内置后端使用 `sandbox-exec -p`；profile 只放行 workspace、`/tmp` 与
  `/private/tmp` 的写入，其余按 `deny default`。
- “内置”指内核知道怎么构造隔离参数，不表示内核自己实现了 namespace/ACL；
  隔离强度取决于 `bwrap` / `sandbox-exec` / AppContainer 与操作系统的正确性。
- 它不是完备的安全边界：不防 `bwrap` / `sandbox-exec` 自身的缺陷，也不做网络隔离。
