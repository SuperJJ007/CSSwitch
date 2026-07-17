# 已验证状态快照

最后复核：2026-07-17。当前维护基线为 v0.7.0；历史版本的固定证据不在本文件重复。

## v0.7.0 已发布

- 公开 peeled `v0.7.0`、发布时 `origin/main` 与 clean build source 均为 `b8ed8d8a818c38e5b1823c11e357a7fdbda81b85`。
- 功能树发布前的五层 Gate、Rust fmt / clippy、前端检查与 `git diff --check` 已记录为通过；这是源码层证据，不自动证明最终 DMG 的真实 provider 行为。
- 最终 DMG 的大小、SHA-256、重新下载一致性、只读挂载内容与 arm64 executable identity 已建立；完整数值见 [v0.7.0 发布证据](../../docs/evidence/releases/v0.7.0.md)。
- 独立 Acceptance 候选完成过 CSSwitch 浏览器 OAuth、动态模型目录、`Codex / GPT-5.6-Sol` 与 Science 最小文本 live 验收；该候选显示 0.6.0 且 hash 不同，不能外推为最终 v0.7.0 DMG 已重跑 live OAuth / 推理。
- Codex 认证保存在 CSSwitch 自有 data root 私有文件中，不使用 macOS Keychain，也不读取、复用、修改或删除原生 `~/.codex` 登录。
- 最终 app 只有 ad-hoc seal、无 TeamIdentifier、无 notarization 或 stapled ticket，Gatekeeper 不接受；这不影响源码构建或 Codex 登录合同，但属于公开分发限制。

## 证据入口

- 当前发布与附件：[v0.7.0 release evidence](../../docs/evidence/releases/v0.7.0.md)
- Acceptance 候选：[2026-07-17 browser-only Acceptance](../../docs/evidence/investigations/2026-07-17-codex-browser-only-acceptance.md)
- 历史版本：[发布证据索引](../../docs/evidence/releases/README.md)

本文件不保存本机 worktree 路径、临时 artifact 位置或可漂移的工作区数量；这些状态必须实时查询。
