# CSSwitch 0.7.0 升级与回滚 / Upgrade and rollback

本说明适用于 macOS Apple Silicon 的 CSSwitch 0.7.0。0.7.0 继续复用 Science 持久化 data-dir 与外部 Skill bridge，并把 v1/v2 配置安全迁移为支持 Codex profile 和网络路由的 v3；现有 API provider、用户 MCP 配置、未知字段和精确的旧 proxy 清理保持不变。

This guide applies to CSSwitch 0.7.0 for macOS Apple Silicon. Version 0.7.0 keeps reusing Science's persistent data-dir and external Skill bridge, then safely migrates v1/v2 configuration to v3 for Codex profiles and network routing. Existing API providers, user MCP entries, unknown fields, and exact legacy-proxy cleanup remain intact.

## 升级前 / Before upgrading

1. 在 CSSwitch 中停止当前第三方链路，然后退出 CSSwitch。
2. 备份整个 `~/.csswitch/`，包括配置、日志和 Skill Manager store/inventory。
3. 不要删除 `~/.csswitch/sandbox/`。覆盖安装 app 不应删除该目录，但手工删除会影响隔离 Science 状态与历史数据。
4. 确认下载文件名和目标版本是 `CSSwitch_0.7.0_aarch64.dmg` / `0.7.0`。

1. Stop the active third-party path in CSSwitch, then quit CSSwitch.
2. Back up all of `~/.csswitch/`, including configuration, logs, and Skill Manager store/inventory.
3. Do not delete `~/.csswitch/sandbox/`. Replacing the app should not remove it, but manual deletion can remove isolated Science state and history.
4. Confirm that the download and target version are `CSSwitch_0.7.0_aarch64.dmg` / `0.7.0`.

## 覆盖安装 / In-place install

1. 打开 DMG，把 CSSwitch 拖入「应用程序」并选择替换旧版。
2. 首次打开如果被 macOS 阻止，在 Finder 中右键 CSSwitch，选择「打开」。0.7.0 为 ad-hoc 签名且未公证；这不等于 Developer ID、notarization 或 Gatekeeper 已验证。
3. 打开 CSSwitch，确认已有 profile 仍存在，再执行一次「设为当前」。
4. 先用最小请求验证常用 provider，再恢复日常工作。

1. Open the DMG, drag CSSwitch into Applications, and replace the older copy.
2. If macOS blocks the first launch, right-click CSSwitch in Finder and choose “Open.” Version 0.7.0 is ad-hoc signed and not notarized; this is not Developer ID, notarization, or Gatekeeper verification.
3. Open CSSwitch, confirm that existing profiles remain, then run “Set active” once.
4. Send one minimal request through your usual provider before resuming normal work.

## 回滚 / Rollback

0.7.0 使用 v3 配置 schema，并在迁移 v1/v2 时保留不可覆盖的版本备份。0.6.0 只能读取 v2，因此不要直接拿旧 app 打开 v3 配置：应先在 0.7.0「高级设置」中使用“导出并降级到 v2”，或在所有进程退出后恢复迁移生成的 v2 备份。随后再用 0.6.0 `.dmg` 覆盖 `/Applications/CSSwitch.app`。不要同时运行两个版本，也不要把新 sidecar 单独复制进旧版 app。回滚不会删除 Science data-dir、已安装 Skill、bundle manifest、隔离回收内容或旧 Skill Manager 数据。

Version 0.7.0 uses config schema v3 and preserves non-overwriting versioned backups while migrating v1/v2. Version 0.6.0 can read only v2, so do not open v3 configuration directly with the old app. First use “Export and downgrade to v2” under 0.7.0 Advanced Settings, or restore the migration-created v2 backup after every CSSwitch process has stopped. Then replace `/Applications/CSSwitch.app` with the 0.6.0 DMG. Do not run two versions at once or copy a newer sidecar into an older app. Rollback does not delete the Science data-dir, installed Skills, bundle manifests, quarantined content, or legacy Skill Manager data.

回滚只替换应用程序，不自动回退或删除 `~/.csswitch` 数据。若旧版无法读取升级后的配置，请退出旧版，把备份的 `config.json` 恢复到原位并保持文件权限为 `0600`。不要在 CSSwitch 或 Science 运行时修改配置文件。

Rollback replaces only the app; it does not automatically revert or delete `~/.csswitch` data. If the older app cannot read the post-upgrade config, quit it, restore the backed-up `config.json`, and keep permissions at `0600`. Do not edit the config while CSSwitch or Science is running.

### Codex 配置降级 / Downgrading Codex configuration

旧版无法表达 Codex profile 或 Codex network route。0.7.0 在「高级设置」提供“导出并降级到 v2”：它先预览全部 Codex profile，要求用户在四秒内再次点击确认，再选择导出文件；后端为**每一个**当前 Codex profile 应用 `export_then_remove`，停止全部受管链路，先原子导出再提交 v2，在同一配置锁内设置进程终态 latch，随后直接退出。latch 后所有配置读取、写入与状态轮询均失败关闭，不能触发常规 v2 → v3 自动迁移；终态退出也不再走会重新读取配置的通用 stop 路径。如果当前生效项是 Codex，降级后的 `active_id` 为空。导出只含 profile 元数据，不含 token、账号 ID、credential payload、模型缓存或代理 URL。降级不会读取、注销或删除 CSSwitch 私有 OAuth 文件；如要删除该凭据，应在降级前另行点击“退出登录”。API-key profiles、端口和 v2 可表达设置保持不变；Codex network 字段按合同丢弃。取消文件选择、profile 列表在确认后变化或停止/导出失败时不提交 v2；若导出成功而随后 config 提交失败，安全结果是“原 v3 + 已完成导出”，可重新执行。

Older builds cannot represent Codex profiles or the Codex network route. Version 0.7.0 exposes “Export and downgrade to v2” under Advanced Settings: it previews every Codex profile, requires a second click within four seconds, then asks for an export destination. The backend applies `export_then_remove` to **every** current Codex profile, stops all managed paths, atomically exports before committing v2, sets a process-terminal latch under the same config lock, then exits directly. After the latch, every config read/write and status poll fails closed and cannot trigger the normal v2-to-v3 migration; terminal exit also bypasses the generic stop path that may reload config. If Codex is active, the downgraded `active_id` is empty. Exports contain profile metadata only—never tokens, account IDs, credential payloads, model caches, or proxy URLs. Downgrade neither reads nor logs out nor deletes the CSSwitch private OAuth files; use the separate Logout action first if removal is intended. API-key profiles, ports, and all v2-expressible settings remain intact; the Codex network field is deliberately discarded because v2 cannot represent it. Cancelling the picker, a changed profile set, or stop/export failure does not commit v2; if export succeeds but the later config commit fails, the safe result is the original v3 config plus a completed export, and the operation can be retried.

## 证据边界 / Evidence boundary

本说明描述安全操作步骤，不证明某个具体下载附件已经通过 hash、签名、公证、Gatekeeper、真实账号或 live provider 验证。每个发布附件都应在对应 release evidence 中单独记录。

This guide describes safe operational steps. It does not prove that a particular download passed hash, signing, notarization, Gatekeeper, real-account, or live-provider verification. Each release artifact needs its own release evidence.
