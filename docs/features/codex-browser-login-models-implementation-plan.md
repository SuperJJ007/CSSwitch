# Codex 浏览器登录与模型接入实施方案（历史归档）

状态：**已归档，不是当前产品合同或待办清单。** 本路径仅为旧链接保留兼容入口；完整阶段方案仍可从 Git 历史查看。

该方案形成于 v0.7.0 开发期间，曾包含 Keychain、稳定签名、共享正式数据根和设备码等设计。最终实现已经改为：

- CSSwitch 自有 browser-only OAuth；
- CSSwitch data root 下的私有文件认证，不使用 macOS Keychain；
- 普通构建固定 `$HOME/.csswitch`，Acceptance 构建固定 `$HOME/.csswitch-acceptance`；
- 不读取、复用、修改或删除原生 `~/.codex` 登录；
- 登录、模型目录、刷新、退出与推理使用同一 Codex 网络路由；
- 不要求 Apple Development、Developer ID、Team ID 或正式签名。

当前权威入口：

- 稳定行为与安全边界：[Codex → Claude Science 实验桥接](codex-science-bridge.md)
- v0.7.0 source、DMG 与分发：[v0.7.0 发布证据](../evidence/releases/v0.7.0.md)
- Acceptance live 行为及其边界：[2026-07-17 browser-only Acceptance](../evidence/investigations/2026-07-17-codex-browser-only-acceptance.md)
- 当前缺口：[已知问题](../../.agents/context/known-issues.md)

Acceptance 候选曾完成浏览器 OAuth、动态模型目录和最小文本推理，但它显示 0.6.0 且不是最终公开 v0.7.0 DMG。最终 DMG 的 source、hash、包内身份与分发已建立；从最终 DMG 安装后重跑真实 OAuth / 模型 / 推理仍未建立。
