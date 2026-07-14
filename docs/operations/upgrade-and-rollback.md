# CSSwitch 0.5.0 升级与回滚 / Upgrade and rollback

本说明适用于 macOS Apple Silicon 的 CSSwitch 0.5.0。该版本继续复用 Science 持久 data-dir，增加外部 Skill 安装 / 隔离卸载 bridge 和可选系统 SSH 配置授权，runtime 使用 Rust `csswitch-gateway` sidecar。

This guide applies to CSSwitch 0.5.0 for macOS Apple Silicon. It keeps reusing Science's persistent data-dir, adds the external Skill install/quarantine bridge and optional system SSH configuration grant, and uses the Rust `csswitch-gateway` runtime sidecar.

## 升级前 / Before upgrading

1. 在 CSSwitch 中停止第三方链路并退出 CSSwitch。
2. 备份整个 `~/.csswitch/`，包括配置、日志、持久 Science data-dir 和保留的 legacy Skill Manager store / inventory。
3. 不要删除 `~/.csswitch/sandbox/`；覆盖 App 不应删除它，手工删除会丢失隔离 Science 状态。
4. 确认下载目标为 `CSSwitch_0.5.0_aarch64.dmg`；可与 [v0.5.0 release evidence](../evidence/releases/v0.5.0.md) 的大小和 hash 对照。

1. Stop the third-party path and quit CSSwitch.
2. Back up all of `~/.csswitch/`, including config, logs, the persistent Science data-dir, and retained legacy Skill Manager data.
3. Do not delete `~/.csswitch/sandbox/`; replacing the app should preserve it, while manual deletion removes isolated Science state.
4. Confirm the download is `CSSwitch_0.5.0_aarch64.dmg`; compare its size and hash with the [v0.5.0 release evidence](../evidence/releases/v0.5.0.md).

## 覆盖安装 / In-place install

1. 打开 DMG，把 CSSwitch 拖入“应用程序”并替换旧版。
2. v0.5.0 最终附件为 ad-hoc 签名、未公证且 Gatekeeper 不接受；首次打开若被阻止，在 Finder 中右键 CSSwitch 并选择“打开”。
3. 确认已有 profiles 仍在，再执行一次“设为当前”。
4. 用最小请求验证常用 provider；需要外部 Skill 或 SSH 时再分别验证可选功能。

1. Open the DMG, drag CSSwitch into Applications, and replace the older copy.
2. The final v0.5.0 asset is ad-hoc signed, not notarized, and not Gatekeeper-accepted. If blocked, right-click CSSwitch in Finder and choose Open.
3. Confirm existing profiles, then run Set active once.
4. Send a minimal request through the usual provider; verify optional Skill or SSH features separately when needed.

## 数据边界 / Data boundary

- v0.5.0 保持 v2 配置 schema，并继续复用 `~/.csswitch/sandbox/home/.claude-science`。
- 组织、项目和原生 / 导入 Skills 由 Science data-dir 持有；覆盖 App 不迁移或删除它们。
- legacy `~/.csswitch/` Skill store / inventory 原样保留，但不参与当前一键启动。
- 更新 Claude Science App 会让下一次干净启动选择新 App executable，不改持久 data-dir。
- 外部 Skill 卸载会把 CSSwitch-owned 导入移入 quarantine，而不是永久删除。

## 回滚 / Rollback

退出 CSSwitch，确认 app 与 `csswitch-gateway` 已停止，然后用上一稳定版 DMG 替换 `/Applications/CSSwitch.app`。不要并行运行两个版本，也不要把 v0.5.0 sidecar 单独复制到旧 app。

Quit CSSwitch, confirm the app and `csswitch-gateway` are stopped, then replace `/Applications/CSSwitch.app` with the previous stable DMG. Do not run two versions together or copy the v0.5.0 sidecar into an older app.

回滚只替换 App，不自动回退或删除 `~/.csswitch`、Science data-dir、Skills、quarantine 或 legacy store。若旧版无法读取升级后的配置，先退出所有相关进程，再恢复备份的 `config.json` 并保持权限 `0600`；不要在 CSSwitch 或 Science 运行时编辑配置。

Rollback replaces only the app. It does not revert or delete `~/.csswitch`, the Science data-dir, Skills, quarantine, or legacy store. If an older app cannot read the newer config, stop all related processes, restore the backed-up `config.json`, and keep mode `0600`.

## 证据边界 / Evidence boundary

本说明描述操作，不证明某一下载副本的 hash、签名、公证、Gatekeeper、installed runtime 或 live provider。每个附件必须使用对应 release evidence 独立核对。

This guide describes operations, not proof for a particular downloaded copy. Verify hash, signing, notarization, Gatekeeper, installed runtime, and live provider evidence independently.
