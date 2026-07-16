# CSSwitch 0.6.0 升级与回滚 / Upgrade and rollback

本说明适用于 macOS Apple Silicon 的 CSSwitch 0.6.0，并补充当前未发布 Codex 实验源码的配置升级边界。0.6.0 继续复用 Science 持久化 data-dir，把 v0.5.0 外部 Skill 路由迁移为支持 GitHub bundle、本地 ZIP 和整包确认卸载的合并 connector，并保留用户 MCP 配置、未知字段和精确的旧 proxy 清理。

This guide applies to CSSwitch 0.6.0 for macOS Apple Silicon and also records the configuration boundary of the current unreleased Codex experimental source. Version 0.6.0 keeps reusing Science's persistent data-dir, migrates the v0.5.0 external-Skill route to a combined connector for GitHub bundles, local ZIPs, and confirmed whole-bundle removal, preserves user MCP entries and unknown fields, and retains exact legacy-proxy cleanup.

## 升级前 / Before upgrading

1. 在 CSSwitch 中停止当前第三方链路，然后退出 CSSwitch。
2. 备份整个 `~/.csswitch/`，包括配置、日志和 Skill Manager store/inventory。
3. 不要删除 `~/.csswitch/sandbox/`。覆盖安装 app 不应删除该目录，但手工删除会影响隔离 Science 状态与历史数据。
4. 确认下载文件名和目标版本是 `CSSwitch_0.6.0_aarch64.dmg` / `0.6.0`。

1. Stop the active third-party path in CSSwitch, then quit CSSwitch.
2. Back up all of `~/.csswitch/`, including configuration, logs, and Skill Manager store/inventory.
3. Do not delete `~/.csswitch/sandbox/`. Replacing the app should not remove it, but manual deletion can remove isolated Science state and history.
4. Confirm that the download and target version are `CSSwitch_0.6.0_aarch64.dmg` / `0.6.0`.

## 覆盖安装 / In-place install

1. 打开 DMG，把 CSSwitch 拖入「应用程序」并选择替换旧版。
2. 首次打开如果被 macOS 阻止，在 Finder 中右键 CSSwitch，选择「打开」。0.6.0 当前为 ad-hoc 签名且未公证；这不等于 Developer ID、notarization 或 Gatekeeper 已验证。
3. 打开 CSSwitch，确认已有 profile 仍存在，再执行一次「设为当前」。
4. 先用最小请求验证常用 provider，再恢复日常工作。

1. Open the DMG, drag CSSwitch into Applications, and replace the older copy.
2. If macOS blocks the first launch, right-click CSSwitch in Finder and choose “Open.” The current 0.6.0 package is ad-hoc signed and not notarized; this is not Developer ID, notarization, or Gatekeeper verification.
3. Open CSSwitch, confirm that existing profiles remain, then run “Set active” once.
4. Send one minimal request through your usual provider before resuming normal work.

## 回滚 / Rollback

发布版 0.6.0 保持 v2 配置 schema。当前 Codex 实验源码会一次性把 v1/v2 安全迁移为 v3，并保留不可覆盖的版本备份；这不表示 0.6.0 发布附件已经使用 v3。回滚前仍应先备份整个 `~/.csswitch/`。退出 CSSwitch，确认 app 与 `csswitch-gateway` 均已停止，然后用上一稳定版 `.dmg` 覆盖 `/Applications/CSSwitch.app`。不要同时运行两个版本，也不要把新 sidecar 单独复制进旧版 app。回滚不会删除 Science data-dir、已安装 Skill、bundle manifest、隔离回收内容或旧 Skill Manager 数据。

The released 0.6.0 build keeps the v2 configuration schema. The current Codex experimental source performs a one-time safe v1/v2-to-v3 migration with non-overwriting version backups; this does not mean the 0.6.0 release artifact uses v3. Back up all of `~/.csswitch/` before rollback. Quit CSSwitch, confirm that both the app and `csswitch-gateway` have stopped, then replace `/Applications/CSSwitch.app` using the previous stable DMG. Do not run two versions at once or copy a newer sidecar into an older app. Rollback does not delete the Science data-dir, installed Skills, bundle manifests, quarantined content, or legacy Skill Manager data.

回滚只替换应用程序，不自动回退或删除 `~/.csswitch` 数据。若旧版无法读取升级后的配置，请退出旧版，把备份的 `config.json` 恢复到原位并保持文件权限为 `0600`。不要在 CSSwitch 或 Science 运行时修改配置文件。

Rollback replaces only the app; it does not automatically revert or delete `~/.csswitch` data. If the older app cannot read the post-upgrade config, quit it, restore the backed-up `config.json`, and keep permissions at `0600`. Do not edit the config while CSSwitch or Science is running.

### Codex 实验源码降级 / Downgrading the Codex experimental source

旧版无法表达 Codex profile。当前源码在「高级设置」提供“导出并降级到 v2”：它先预览全部 Codex profile，要求用户在四秒内再次点击确认，再选择导出文件；后端为**每一个**当前 Codex profile 应用 `export_then_remove`，停止全部受管链路，先原子导出再提交 v2，在同一配置锁内设置进程终态 latch，随后直接退出。latch 后所有配置读取、写入与状态轮询均失败关闭，不能触发常规 v2 → v3 自动迁移；终态退出也不再走会重新读取配置的通用 stop 路径。如果当前生效项是 Codex，降级后的 `active_id` 为空。导出只含 profile 元数据，不含 token、账号 ID、credential payload 或模型缓存。降级不会读取、注销或删除 CSSwitch Keychain OAuth；如要删除该凭据，应在降级前另行点击“退出登录”。API-key profiles、端口和 v2 可表达设置保持不变。取消文件选择、profile 列表在确认后变化或停止/导出失败时不提交 v2；若导出成功而随后配置提交失败，安全结果是“原 v3 + 已完成导出”，可重新执行。

Older builds cannot represent Codex profiles. The current source exposes “Export and downgrade to v2” under Advanced Settings: it previews every Codex profile, requires a second click within four seconds, then asks for an export destination. The backend applies `export_then_remove` to **every** current Codex profile, stops all managed paths, atomically exports before committing v2, sets a process-terminal latch under the same config lock, then exits directly. After the latch, every config read/write and status poll fails closed and cannot trigger the normal v2-to-v3 migration; terminal exit also bypasses the generic stop path that may reload config. If Codex is active, the downgraded `active_id` is empty. Exports contain profile metadata only—never tokens, account IDs, credential payloads, or model caches. Downgrade neither reads nor logs out nor deletes the CSSwitch Keychain OAuth record; use the separate Logout action first if removal is intended. API-key profiles, ports, and all v2-expressible settings remain intact. Cancelling the picker, a changed profile set, or stop/export failure does not commit v2; if export succeeds but the later config commit fails, the safe result is the original v3 config plus a completed export, and the operation can be retried.

## 证据边界 / Evidence boundary

本说明描述安全操作步骤，不证明某个具体下载附件已经通过 hash、签名、公证、Gatekeeper、真实账号或 live provider 验证。每个发布附件都应在对应 release evidence 中单独记录。

This guide describes safe operational steps. It does not prove that a particular download passed hash, signing, notarization, Gatekeeper, real-account, or live-provider verification. Each release artifact needs its own release evidence.
