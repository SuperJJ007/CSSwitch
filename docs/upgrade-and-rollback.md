# CSSwitch 0.4.0 升级与回滚 / Upgrade and rollback

本说明只适用于 macOS Apple Silicon 的 CSSwitch 0.4.0 本地构建或发布包。0.4.0 使用随 app 打包的 Rust inference gateway，不再携带生产 Python proxy，也没有运行时 backend selector。

This guide applies to CSSwitch 0.4.0 local or released builds for macOS Apple Silicon. Version 0.4.0 uses the Rust inference gateway bundled with the app; it does not ship a production Python proxy or a runtime backend selector.

## 升级前 / Before upgrading

1. 在 CSSwitch 中停止当前第三方链路，然后退出 CSSwitch。
2. 备份 `~/.csswitch/config.json`；如需保留诊断上下文，可另行备份 `~/.csswitch/logs/`。
3. 不要删除 `~/.csswitch/sandbox/`。覆盖安装 app 不应删除该目录，但手工删除会影响隔离 Science 状态与历史数据。
4. 确认下载文件名和目标版本是 `CSSwitch_0.4.0_aarch64.dmg` / `0.4.0`。

1. Stop the active third-party path in CSSwitch, then quit CSSwitch.
2. Back up `~/.csswitch/config.json`. Back up `~/.csswitch/logs/` separately only if you need diagnostic history.
3. Do not delete `~/.csswitch/sandbox/`. Replacing the app should not remove it, but manual deletion can remove isolated Science state and history.
4. Confirm that the download and target version are `CSSwitch_0.4.0_aarch64.dmg` / `0.4.0`.

## 覆盖安装 / In-place install

1. 打开 DMG，把 CSSwitch 拖入「应用程序」并选择替换旧版。
2. 首次打开如果被 macOS 阻止，在 Finder 中右键 CSSwitch，选择「打开」。0.4.0 当前为 ad-hoc 签名且未公证；这不等于 Developer ID、notarization 或 Gatekeeper 已验证。
3. 打开 CSSwitch，确认已有 profile 仍存在，再执行一次「设为当前」。
4. 先用最小请求验证常用 provider，再恢复日常工作。

1. Open the DMG, drag CSSwitch into Applications, and replace the older copy.
2. If macOS blocks the first launch, right-click CSSwitch in Finder and choose “Open.” The current 0.4.0 package is ad-hoc signed and not notarized; this is not Developer ID, notarization, or Gatekeeper verification.
3. Open CSSwitch, confirm that existing profiles remain, then run “Set active” once.
4. Send one minimal request through your usual provider before resuming normal work.

## 回滚 / Rollback

0.4.0 保持 v2 配置 schema，但回滚仍应先备份当前配置。退出 CSSwitch，确认 app 与 `csswitch-gateway` 均已停止，然后用上一稳定版 `.dmg` 覆盖 `/Applications/CSSwitch.app`。不要同时运行两个版本，也不要把 0.4.0 的 sidecar 单独复制进旧版 app。

Version 0.4.0 keeps the v2 configuration schema, but back up the current config before rolling back. Quit CSSwitch, confirm that both the app and `csswitch-gateway` have stopped, then replace `/Applications/CSSwitch.app` using the previous stable DMG. Do not run two versions at once or copy the 0.4.0 sidecar into an older app.

回滚只替换应用程序，不自动回退或删除 `~/.csswitch` 数据。若旧版无法读取升级后的配置，请退出旧版，把备份的 `config.json` 恢复到原位并保持文件权限为 `0600`。不要在 CSSwitch 或 Science 运行时修改配置文件。

Rollback replaces only the app; it does not automatically revert or delete `~/.csswitch` data. If the older app cannot read the post-upgrade config, quit it, restore the backed-up `config.json`, and keep permissions at `0600`. Do not edit the config while CSSwitch or Science is running.

## 证据边界 / Evidence boundary

本说明描述安全操作步骤，不证明某个具体下载附件已经通过 hash、签名、公证、Gatekeeper、真实账号或 live provider 验证。每个发布附件都应在对应 release evidence 中单独记录。

This guide describes safe operational steps. It does not prove that a particular download passed hash, signing, notarization, Gatekeeper, real-account, or live-provider verification. Each release artifact needs its own release evidence.
