# 系统 SSH 配置复用

适用版本：CSSwitch v0.5.0。该功能让隔离 Science 在用户明确授权后，按系统 OpenSSH 语义复用真实 `~/.ssh/config`；它不是 SSH server、端口转发 UI 或公网暴露功能。

## 默认与 opt-in

`reuse_system_ssh` 默认关闭。关闭时，CSSwitch 不把真实系统 SSH 配置注入隔离 Science。

启用后，CSSwitch 在隔离环境 PATH 前放置一个窄 wrapper，最终执行：

```text
/usr/bin/ssh -F <real-home>/.ssh/config <原始参数...>
```

参数仍由调用方交给系统 `ssh`；wrapper 只固定配置文件入口，不实现 SSH 协议，也不读取或显示私钥内容。

## 授权的真实含义

这是一项行为授权，不只是“读一个 config 文件”。系统 OpenSSH 会按原生规则处理：

- `Include`
- `IdentityFile`
- `IdentityAgent`
- `ProxyCommand`
- `Match exec`

这些规则可能进一步访问其他文件、ssh-agent 或本机命令。用户启用前应理解现有 SSH 配置的信任边界。

## 不会做的事

- 不复制或 symlink 整个 `.ssh`；
- 不把 private key、config 内容或 ssh-agent 数据传到 CSSwitch UI；
- 不启动 `sshd`，不开启 macOS Remote Login；
- 不修改防火墙或建立 `0.0.0.0` listener；
- 不把 SSH 访问与 CSSwitch inference Gateway 混成同一服务；
- 不保证某个 host、key、agent、ProxyCommand 或网络一定可用。

## 失败边界

默认关闭时，SSH 不是普通 Science 启动的前置条件。用户启用该设置时，CSSwitch 先验证真实 `~/.ssh/config`；SSH 授权状态变化会先停止仍使用旧授权的隔离 Science，再保存新设置。

启用后的每次启动都会再次校验 config 与 packaged wrapper。config 缺失、wrapper 缺失或路径不安全时，启动 fail closed 并清理部分启动，不能以 warning 略过。只有 Science 已成功启动后的某次 `/usr/bin/ssh` 命令失败，才只影响该命令。

错误报告不得打印私钥路径、config 内容、ssh-agent 数据或其他敏感信息，也不得为了诊断读取真实 private key。

## 验证层

1. 配置默认关闭；
2. opt-in 保存时缺失 config 会被拒绝；
3. 启用后启动时 wrapper 内容、权限与 config 再次通过 fail-closed 校验；
4. 隔离 Science PATH 选择 wrapper；
5. wrapper 将参数转给 `/usr/bin/ssh -F`；
6. 没有 `.ssh` 复制、`sshd`、防火墙或公网 listener；
7. 特定真实 server 连通性只在单独授权后验证。

源码 / 合同测试不能替代第 7 层；第 7 层也不能泛化为所有用户配置可用。
