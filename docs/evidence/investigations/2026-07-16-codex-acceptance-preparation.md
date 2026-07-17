# 2026-07-16 Codex Acceptance 环境准备证据

> **归档说明：** 本页记录私有文件认证方案落地前的准备过程，其中 Keychain 步骤与后续动作均已被 v0.7.0 当前合同取代。它只用于解释历史候选，不是当前验收 runbook；当前流程见[真机验收](../../operations/real-machine-acceptance.md)。

## 结论

Codex → Claude Science 的源码与自动 Gate 已进入可供用户真机验收的状态。本轮成功构建独立 `CSSwitch Acceptance.app`，并生成了独立 HOME、空 v3 配置和动态端口；全过程停在启动 app 与打开 OAuth 浏览器之前。

这份证据**不证明**真实 Codex OAuth、live 模型、额度、推理、工具调用或发布签名已经通过。RM-35～RM-38、RM-40 仍需用户亲自在该隔离环境中完成。

## 绑定状态

- worktree：`/private/tmp/csswitch-codex-science-bridge`
- branch：`codex/codex-science-bridge`
- 基线 commit：`0897e78f201e9e463be6a13e3d11888bde31f3b0`
- 构建内容：基线 commit 加当前未提交 Codex bridge diff
- artifact：`desktop/src-tauri/target/release/bundle/macos/CSSwitch Acceptance.app`
- 版本：`0.6.0`
- bundle ID：`com.csswitch.acceptance`
- 临时验收 root：`/private/tmp/csswitch-codex-acceptance-20260716-091441`

## 自动 Gate

`PATH="<stable-rust-toolchain-bin>:$PATH" bash test/run_all.sh --require-release-ready`：

- offline：pass
- loopback：pass
- scripts：pass，包含纯隔离的 `test_real_machine_guard.sh`
- Rust：pass；Gateway 203 unit + 2 integration pass，Tauri 271 pass / 3 ignored
- frontend：pass
- `current-env clean`：YES
- `release-ready green`：YES

3 个 ignored 均为需要显式启动 fake / installed Science 的隔离 runtime 场景，不是失败；本轮未借此声称真实 OAuth 或 live provider 已验证。

## Artifact 检查

- app 大小：24668 KiB
- `Contents/MacOS/desktop` SHA-256：`e565fd37849d2e8c345605fa6b30f5274fc9d5dc59880768b527f088d9cd9990`
- `Contents/MacOS/csswitch-gateway` SHA-256：`56f7f9001b111316d5a5c3f1c367bbe5fea2b68ba240cb523c0d585bc224cbcc`
- 包含 desktop、Rust Gateway 与 4 个运行脚本；未发现旧 `Resources/proxy`
- 两个可执行文件均为 arm64 Mach-O；Acceptance Gateway 包含 `com.csswitch.acceptance.codex.*`，不包含生产 `com.csswitch.codex.*` namespace
- Tauri → Gateway 编译期身份握手通过：匹配 Acceptance service 的非法命令在认证访问前返回 exit 2；伪装生产 service 在认证访问前返回 exit 8；临时 HOME 未产生状态文件
- `codesign --verify --deep --strict`：通过
- 签名身份：ad-hoc，`TeamIdentifier=not set`
- `spctl --assess`：rejected
- stapler：无 ticket

因此它只用于本机源码验收，不是 Developer ID、已公证或可发布 artifact。

## 隔离环境

`real_machine_guard.sh preflight` 与 `prepare-codex` 的结果：

- HOME：`/private/tmp/csswitch-codex-acceptance-20260716-091441/home`
- Gateway 端口：`58279`
- Science sandbox 端口：`58280`
- 两端口均空闲，且避开 `8765`、`1455`、`1457`
- OAuth callback `1455` / `1457` 至少一个空闲；guard 未停止任何占位进程
- 真实 Science `8765` listener PID 基线前后一致
- config schema：v3
- profiles：0；`active_id`：空
- `experimental_codex_enabled`：false
- config 未包含 `token` 或 `credential_ref`
- root / HOME / `.csswitch` / state：`0700`；config 与动态端口状态：`0600`；目录树无 symlink
- `assert-stopped`：通过；Gateway 与 Science sandbox 均未启动

本轮准备时尚未发现：当前 Security Framework 调用会按隔离 `HOME` 查找默认钥匙串；若该 HOME 没有默认钥匙串，真实 OAuth 会在 `keychain_commit` 阶段失败并触发 macOS 恢复提示。后续真机验收已据此修订 guard：在临时 HOME 内创建空的独立默认钥匙串，同时继续使用 Acceptance 编译期 service namespace。本轮没有启动 app，没有调用登录 / 状态 / 退出命令，也没有打开 OAuth 浏览器，因此当时没有读取或修改 CSSwitch 与原生 Codex 的任何 Keychain / OAuth 状态。

## 下一停线点

用户开始真机验收前，按 [真机验收矩阵](../../operations/real-machine-acceptance.md)导出 guard 环境并启动 Acceptance app。先完成 RM-42 的 UI 默认关闭、bundle ID、端口与 8765 检查；随后由用户决定是否打开 Codex 实验开关并进入 RM-35。任何真实凭据证据都只保留脱敏状态，不 dump Keychain 或 `~/.codex`。
