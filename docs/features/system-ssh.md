# 系统 SSH 配置复用

该功能自 v0.5.0 起提供，v0.8.1 补充了隔离 Science 的 SSH 前置校验桥接。它让隔离 Science 在用户明确授权后，发现系统 SSH Host alias，并按系统 OpenSSH 语义复用真实 `~/.ssh/config`；它不是 SSH server、端口转发 UI 或公网暴露功能。

## 默认与 opt-in

`reuse_system_ssh` 默认关闭。关闭时，CSSwitch 不把真实系统 SSH 配置注入隔离 Science。

启用时，CSSwitch 会在隔离 HOME 的 `.ssh/config` 创建一个 `0600` 普通入口文件。该文件先包含一条指向真实 `~/.ssh/config` 的绝对 `Include`，再包含真实顶层 config 中经过安全过滤、去重和限额的显式 `Host` alias。字面 Host 行用于兼容不会展开 `Include` 的 Science Host 枚举器；实际连接仍由下述 wrapper 固定使用真实配置。

投影不会复制 `HostName`、`User`、`Port`、`IdentityFile`、`ProxyCommand`、`Match` 或其他连接参数，也不会投影通配符和否定 pattern。真实 config 通过 `Include` 引入的其他文件仍由 OpenSSH 在连接时解析；其中的 alias 不会被 CSSwitch 递归投影，必要时仍可在 Science 中手工输入。

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

- 不复制或 symlink 整个 `.ssh`，也不复制真实 config 的连接参数；
- 隔离 config 不是指向真实文件的 symlink，避免 Science 写穿真实配置；
- 不把 private key、连接参数或 ssh-agent 数据传到 CSSwitch UI；隔离 Science 只看到用户已授权投影的 Host alias；
- 不启动 `sshd`，不开启 macOS Remote Login；
- 不修改防火墙或建立 `0.0.0.0` listener；
- 不把 SSH 访问与 CSSwitch inference Gateway 混成同一服务；
- 不保证某个 host、key、agent、ProxyCommand 或网络一定可用。

## 失败边界

默认关闭时，SSH 不是普通 Science 启动的前置条件。用户启用该设置时，CSSwitch 先验证真实 `~/.ssh/config`；SSH 授权状态变化会先停止仍使用旧授权的隔离 Science，再保存新设置。关闭授权会撤销 CSSwitch 管理的隔离 config；若该位置是外来文件、symlink 或特殊文件，CSSwitch 会拒绝覆盖或删除并据实报错。

启用后的每次启动都会再次校验 config 与 packaged wrapper，并原子刷新 alias 投影。config 缺失、wrapper 缺失、alias 提取失败、投影超限或路径不安全时，启动 fail closed 并清理部分启动，不能以 warning 略过。旧 v1 Include-only stub 会在下一次 opt-in 启动时升级；关闭授权仍可撤销 v1/v2。只有 Science 已成功启动后的某次 `/usr/bin/ssh` 命令失败，才只影响该命令。

错误报告不得打印私钥路径、config 内容、ssh-agent 数据或其他敏感信息，也不得为了诊断读取真实 private key。

## 验证层

1. 配置默认关闭；
2. opt-in 保存时缺失 config 会被拒绝；
3. 启用后启动时 wrapper 内容、权限与 config 再次通过 fail-closed 校验；
4. 隔离 config 只出现安全的字面 Host alias，不出现连接参数；
5. 隔离 Science PATH 选择 wrapper；
6. wrapper 将参数转给 `/usr/bin/ssh -F`；
7. 没有 `.ssh` 复制、`sshd`、防火墙或公网 listener；
8. 特定真实 server 连通性只在单独授权后验证。

其中第 1～7 项可由本地 fixture 和系统 OpenSSH 自动验证；Claude Science 是否枚举字面 `Host` 行仍必须使用当前安装版本做一次隔离 UI 验收，不能由 `/usr/bin/ssh -G` 的结果代替。

源码 / 合同测试不能替代第 8 层；第 8 层也不能泛化为所有用户配置可用。
