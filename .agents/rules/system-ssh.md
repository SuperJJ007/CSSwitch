# 系统 SSH 规则

- 系统 SSH 配置复用默认关闭，必须由用户明确 opt-in。
- bridge 只能让系统 OpenSSH 读取真实 `~/.ssh/config`；不得复制或链接整个 `.ssh`，不得暴露私钥内容。
- 启用设置时必须先验证真实 config；后续启动必须再次验证 config 与 packaged wrapper，任何一项缺失或不安全都应 fail closed。Science 启动后的单次 `ssh` 命令失败才只影响该命令。
- 不启动 `sshd`、不开启 Remote Login、不改防火墙，也不增加公网监听。
- 这是行为授权而非单文件授权：OpenSSH 的 `Include`、`IdentityFile`、`IdentityAgent`、`ProxyCommand` 和 `Match exec` 可能按原生语义访问其他文件、Agent 或命令。
- SSH 能力必须与 inference Gateway 暴露、真实账号凭证处理分离。
- 特定真实服务器或用户 SSH 配置的连通性需要独立授权和证据，不能由 wrapper 测试推断。
- 错误与证据不得打印私钥路径、config 内容、agent 数据或其他敏感信息。
