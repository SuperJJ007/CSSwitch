# 系统 SSH 规则

- 系统 SSH 配置复用默认关闭，必须由用户明确 opt-in。
- bridge 的执行面只能让系统 OpenSSH 读取真实 `~/.ssh/config`；不得复制或链接整个 `.ssh`，不得暴露私钥内容。为兼容不展开 `Include` 的 Science Host 枚举器，opt-in 时可在隔离 HOME 写一个 CSSwitch 管理的 `0600` 普通 config stub：先写指向真实 config 的绝对 `Include`，再只投影真实顶层 config 中满足安全 token 语法的显式 Host alias。
- config stub 不得是 symlink，不得复制 HostName、User、Port、IdentityFile、ProxyCommand、Match 或其他真实连接配置。Host alias 投影必须过滤通配符/否定 pattern、稳定去重并有数量/大小上限；提取失败必须阻断 opt-in 启动，不能静默退回 Include-only。
- 启用时遇到外来文件、伪造 marker、symlink 或特殊文件必须 fail closed；v1 Include-only stub 可原子迁移，关闭授权时只删除能严格识别为 CSSwitch 生成的 v1/v2 stub。
- 启用设置时必须先验证真实 config；后续启动必须再次验证 config 与 packaged wrapper，任何一项缺失或不安全都应 fail closed。Science 启动后的单次 `ssh` 命令失败才只影响该命令。
- 不启动 `sshd`、不开启 Remote Login、不改防火墙，也不增加公网监听。
- 这是行为授权而非单文件授权：OpenSSH 的 `Include`、`IdentityFile`、`IdentityAgent`、`ProxyCommand` 和 `Match exec` 可能按原生语义访问其他文件、Agent 或命令。
- SSH 能力必须与 inference Gateway 暴露、真实账号凭证处理分离。
- 特定真实服务器或用户 SSH 配置的连通性需要独立授权和证据，不能由 wrapper 测试推断。
- 错误与证据不得打印真实 Host alias、私钥路径、config 连接内容、agent 数据或其他敏感信息。
