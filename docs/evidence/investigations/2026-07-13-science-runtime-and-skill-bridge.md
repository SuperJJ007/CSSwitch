# 2026-07-13 Science runtime 与外部 Skill bridge 调查

状态：**日期化、版本固定的历史证据**。稳定合同见 [Science runtime](../../architecture/science-runtime.md) 与[外部 Skill bridge](../../features/external-skill-bridge.md)。

## 调查环境

- Claude Science：`0.1.18-dev.20260709.t211149.shab3f5130-release`（`b3f5130a`）；
- 对照 cache：观察到 `0.1.15`；
- CSSwitch：v0.5.0 实现线，最终进入 `main@557a01f`；
- 隔离临时 HOME / data-dir、动态非 8765 端口、假 `security` / API 材料、local mock provider；
- 外部公开内容：`anthropics/skills` 的 `internal-comms`；
- 未读取真实 OAuth、Keychain、SSH 或真实 Science 组织数据。

本调查不建立 Science 的通用最低版本，也不证明真实 provider、真实账号或任意未来 Science 版本兼容。

## 错误 binary 选择事故

当时已安装 App 为 0.1.18，但 CSSwitch 仍启动了 `<data-dir>/bin/claude-science` 0.1.15。改为选择 App 后：

- `lsof` 显示 8990 监听进程执行 App binary；
- 同一持久 CSSwitch data-dir 加载 0.1.18 runtime；
- `claude-science status` 报告该端口健康。

这证明当时曾选错 executable，并支持“App executable 与持久 data-dir 分离”的修复方向；不能由此推导“0.1.18 是最低支持版本”。

额外对照显示，0.1.15 与 0.1.18 CLI 都可能对同一 0.1.18 daemon 返回 status / stop 结果，因此端口或 CLI 响应不能单独证明 runtime identity。

## runtime 观察

- 新临时 data-dir 可由 selected App 初始化，无需读取真实 `~/.claude-science` 的 `bin`、`conda`、`runtime` 或 `seed-assets`。
- App 更新后可复用同一 data-dir 中的组织、项目和 Skills。
- 0.1.15 / 0.1.18 都暴露 `--host`；CSSwitch 显式使用 `127.0.0.1`。
- 两版隐式 preview port 行为不同，因此 v0.5.0 对新启动显式传 `--sandbox-port`。
- CSSwitch 保持 `--no-auto-update`，未下载、升级或降级 Science。
- healthy daemon 可跨候选 CLI 被读取 / 停止，不应仅因版本漂移强制重启。

## 原生 Skill / catalog 观察

- organization Skill 位于 `<data-dir>/orgs/<active-org>/skills/<name>/`；runtime 版本目录不是安装目标。
- 标准 Skill 可包含 `SKILL.md`、scripts、references 等多文件资源。
- 默认 `OPERON` Agent 持久化受限 `skill_names`；目录可被 list 到，不代表 `skill()` 可加载，仍需 attach。
- `host.agents.attach_skill("OPERON", name)` 可校验目录、持久绑定并刷新 registry；`detach_skill` 可不经 catalog 移除绑定。
- `host.skills` SDK 没有本地 install / import；Settings 的 GitHub importer 使用另一套 marketplace / catalog 路径。
- `local-mcp.json` 只观察到 `name`、`command`、`args`、`env` 和可选 `description`，没有 unsandboxed / trusted-host 开关。

在 account fetch 401 与 `[skillCatalog] provider list() degraded` 状态下，原生 GitHub preview / Agent authoring 路径不可用。一次真实 UI 对话还把卸载错误路由到 `customize` / catalog-gated delete，继而尝试错误的可见目录，说明仅有 connector 描述不足以保证自然语言路由。

## 外部 Skill bridge E2E

隔离 E2E 逐层确认：

1. CSSwitch 原子创建固定 `csswitch-external-skill-tools` route，并通过 Science loopback nonce / cookie / CSRF 控制流顺序完成：attach route、attach 合并 connector、清理受管旧 uninstaller connector、detach `customize`、写入并回读带 marker 的 OPERON custom prompt；
2. 对话选择 `mcp-csswitch-skill-installer`，经 `request_host_access` 授权专用 bridge directory；
3. Agent 用 `host.mcp`、`edit_file` 与 `read_file` 提交 / 读取限界请求，没有使用 `host.skills.edit/publish`；
4. host worker 拉取完整 `internal-comms`，写入 v1 `.import-origin`，原子提交到 active org；未写 `.catalog_stamp`；
5. Agent 调用 native `attach_skill`，当前对话 `skill("internal-comms")` 返回完整导入说明；
6. 用同一临时 data-dir 重启 Science，新对话仍能加载该 Skill；
7. 仅输入 `请卸载 internal-comms` 时，route 选择同一 connector 的 `uninstall_external_skill`；
8. host 将带 marker 的完整目录移入 quarantine，Agent native detach 后确认不再 load；
9. 卸载没有调用 `host.skills.delete`、`skills.deleteDraft`、bash 或手工文件删除；
10. 所有写入避开 `<data-dir>/runtime/<version>/`，catalog degraded 仍不影响该 E2E。

聚焦测试另覆盖了 URL / ref 解析、大小与路径限制、symlink / FIFO、HMAC / expiry / replay、launch-bound key rotation、crash 后 lock 恢复、no-replace、marker ownership、connector merge / migration、route idempotence、运行中只读检查 / `RESTART_REQUIRED`、版本化 route-state 和 warning-only 失败边界。

Agent 控制面由多个顺序 HTTP 请求组成，不是原子事务；测试证明失败会降级成 warning 且不写成功 route-state，但不证明会回滚此前已经成功的 attach / detach / prompt 步骤。该部分配置边界必须与 Skill 目录自身的原子 commit / quarantine 分开理解。

## 未证明

- 最终已安装 CSSwitch App 的人工 UI 复核；
- 真实 provider 推理或真实账号迁移；
- 任一安装 Skill 的脚本、资产、网络、包依赖或领域功能；
- 仅名称请求的 provider 搜索质量；
- 私有仓库、更新 / 覆盖、永久删除或 quarantine 恢复 UI；
- 特定用户系统 SSH server 连通；
- 未来 Science 版本仍保持相同 local MCP、nonce / CSRF、`.import-origin` 或 attach / detach 合同。

因此 E2E 应准确表述为“在该隔离环境中完成 fetch、commit、attach、load、restart、quarantine 和 detach”，不能缩写成对所有 Skill / Science 版本的通用成功声明。
