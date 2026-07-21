# 安全规则

本规则适用于仓库内所有任务。

- 严格遵守“只读”“只出报告”“不改代码”等范围限制。
- API Key、OAuth token、Keychain、SSH 私钥、账号数据库和私人日志不得读取、打印、复制、修改或删除。
- 不枚举、读取或修改用户真实 `~/.claude-science` 数据。唯一的 runtime 例外是按产品合同只读校验并执行精确文件 `~/.claude-science/bin/claude-science`；只能检查该文件的路径边界、metadata、可执行性和 `--version`，启动时必须注入 CSSwitch 独立 HOME/data-dir，不能访问同目录中的账号、OAuth、项目、会话、设置或数据库。诊断时也不改真实 `~/.csswitch/skills` 或其他用户 Skill 数据。
- 自动 runtime 检查必须使用临时外层 `HOME`、隔离 data-dir、假密钥 / 假 `security`、动态端口和可精确归属的进程。
- `8765` 视为用户真实 Science 的保留端口；只有护栏需要确认监听身份时才只读观察。
- 未经明确授权，不把 `/Applications/CSSwitch.app` 当开发测试目标，也不替换它。
- 日志、截图、报告和 issue 文本必须脱敏：凭证、path secret、一次性 nonce、邮箱、私有 URL 和用户数据均不得暴露。
- 安全隔离环境无法建立的结论，必须写成“未验证”，不能自行扩大访问范围。
