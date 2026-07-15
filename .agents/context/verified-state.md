# 已验证状态快照

最后复核：2026-07-16。

## v0.6.0 已发布

- 外部 Skill 安装支持准确公开 GitHub URL 与主面板本地 `.zip` / `.skill`；共享 package core 对单 Skill 和 bundle 执行一致的安全校验与原子提交。
- GitHub 安装使用单请求协议，提供下载进度、终态响应、无自动重试、`.processing` 清理与 gateway 重启中断恢复。
- bundle 从任意成员卸载会先返回完整受影响列表和整包确认；取消不改动，确认后整包批量解绑与隔离，不提供部分物理删除。
- v0.5.0 旧 route / split connector 会迁移到合并 connector，并保留用户 MCP 项和未知字段；停止、重启恢复与重复安装均有自动化或定向验证。
- `32aa062` 上 `bash test/run_all.sh --require-release-ready` 退出 0；五层全部 pass，Tauri 232 passed / 3 explicit ignored，Gateway 102 passed；其源码树与最终 `70950f1` 相同。
- 发布者报告大部分 v0.6.0 真机验收成功并授权合并发布；没有逐项结构化日志，因此只记作部分人工验收。
- PR #58 已合并；公开 peeled `v0.6.0`、发布时 `origin/main` 与最终构建 source 都是 `70950f1d1e8a18e8bd43db9b9a45e4e22ef2bcb4`。
- 最终 DMG 为 9,701,904 bytes，SHA-256 `4a67973ef090e6d0a0dbc8e85d456c81d87733831555f6ea84cecb75e563dfb4`；GitHub asset 元数据相同，重新下载后逐字节一致且 `hdiutil verify` 通过。
- 包内 app 为 0.6.0 arm64，desktop / gateway 齐全、旧 proxy 不存在；签名结构有效但身份为 ad-hoc、无 TeamIdentifier，Gatekeeper 拒绝且没有 stapled ticket。
- `/Applications/CSSwitch Acceptance.app` 已从最终 `70950f1` 重新构建并安全替换，独立 bundle id 为 `com.csswitch.acceptance`；安装后二进制与 Acceptance 构建产物一致，UI 已重新打开且未自动启动 Gateway。正式 CSSwitch.app 未修改。

## v0.5.0 历史直接复核

- 文档基线为 `v0.5.0 / main@557a01f`；本轮开始时维护 worktree 干净。
- `main`、`origin/main` 与本地 `v0.5.0^{}` 指向同一 commit。
- GitHub `v0.5.0` Release 已公开；附件名称、大小和 digest 已查询。
- 本地同名最终 DMG 的大小和 SHA-256 与 GitHub 附件元数据完全一致。
- 挂载后的 app 为 `0.5.0`，包含 Rust `csswitch-gateway` sidecar 与 `Resources/scripts`，不含旧 `Resources/proxy`。
- `codesign --verify --deep --strict` 通过，但签名身份为 ad-hoc、无 TeamIdentifier；DMG 被 Gatekeeper 以 `no usable signature` 拒绝，且无 stapled ticket。
- v0.5.0 源码重新核对了 installed-App 优先、显式 `SCIENCE_BIN` fail-closed、one-shot cache、loopback / sandbox port、系统 SSH opt-in fail-closed 与外部 Skill 工具合同。
- 独立审查后进一步对齐：高频 UI status 仅是 HTTP health；Skill Agent 控制面会管理固定 route / connector、旧 connector、`customize` 与受管 prompt，且顺序失败不自动回滚已完成步骤。
- bundled routing Skill 的工具名为 `install_external_skill` / `uninstall_external_skill`，并明确要求原生 attach / detach；与新功能文档一致。
- 独立审查修正后，`bash test/run_all.sh` 在具备 loopback / process inspection 权限的环境再次退出 0：offline、loopback、scripts、rust、frontend 五层全部 pass；输出同时为 `current-env clean: YES` 与 `release-ready green: YES`。
- 52 份当前可追踪 Markdown 的仓库内相对链接检查通过；`test/run-live.sh` 退出 0 并正确指向新真机 runbook。

## 文档维护收尾检查

- `git diff --check` 与 Markdown 尾随空白检查通过。
- 当前正文不再链接旧文档路径；残留的 Python proxy / `Resources/proxy` 只出现在 CHANGELOG 历史、迁移账本或明确的“不再存在”断言中。
- 新 Agent / docs 文件可追踪；`.agents/handoffs/` 只保留 policy，临时文件继续被忽略。
- 最终只读复核确认：15 项开发区、6 项 acceptance 区与 detached recovery 实验区仍保持各自原有脏状态；本轮只有维护 worktree 出现规则 / 文档改动。

版本固定的 Science / Skill E2E 证据已迁入[日期化调查](../../docs/evidence/investigations/2026-07-13-science-runtime-and-skill-bridge.md)，不能自动外推到其他 Science 版本。
