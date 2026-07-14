# CSSwitch Agent 规则入口

本文件只负责索引，不保存版本快照、测试结果或架构正文。

## 阅读顺序

1. 所有任务先读[安全规则](.agents/rules/safety.md)和[Git / worktree 规则](.agents/rules/git-worktrees.md)。
2. 再按任务读取 [`.agents/rules/`](.agents/rules/) 下的测试、发布、Science runtime、外部 Skill 或系统 SSH 规则。
3. 涉及版本、分支、worktree、验证状态或已知问题时，先实时复核，再参考 [`.agents/context/`](.agents/context/) 的日期化快照。
4. 架构、运维、功能合同、历史证据和外部参考从[文档总入口](docs/README.md)进入。

## 信息权威顺序

目标 artifact / runtime > tag 与公开 release > 当前源码和测试 > `.agents/context/` > `docs/` > memory / handoff。

`.agents/rules/` 是 Agent 的稳定行为规则；`.agents/context/` 只是可更新的当前状态索引；临时交接不得替代两者。

## 工作边界

- 未提交内容按用户数据保护；只读任务只允许检查和报告。
- commit、push、tag、release、替换已安装 App或运行真实 provider 测试均需明确授权。
- 真实 API Key、OAuth token、Keychain、SSH 私钥和账号数据库始终不得读取或回显；真实 provider 测试只能消费用户显式提供给隔离进程的输入，授权测试不等于授权检查凭证内容。
- 临时 handoff 放入 `.agents/handoffs/`，长期事实应进入 rules、context 或 docs。
