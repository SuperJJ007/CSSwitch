# 2026-07-17 Codex browser-only Acceptance 安装与 live 验收证据

> 本页按时间保留多个 artifact。每组 hash 只证明对应候选，不能外推到最终 v0.7.0 发布附件；下方旧 Keychain 与共享数据根记录均明确标为历史证据。
>
> 后续状态：最终 v0.7.0 的 source、DMG identity 与公开分发已单独建立，见 [v0.7.0 发布证据](../releases/v0.7.0.md)；最终 DMG 仍未重跑本页的 live OAuth / 推理。本页的恢复候选和停线步骤均为历史记录，不是当前操作指引。

## 当前 no-signing 私有文件 Acceptance 与 live 推理

2026-07-17 在用户明确拒绝正式签名后，当前候选改为 CSSwitch data root 下的私有文件 OAuth，不再调用 macOS Keychain，也不要求 Apple Development、Developer ID 或 Team ID。随后重建并替换 `/Applications/CSSwitch Acceptance.app`；正式 `/Applications/CSSwitch.app` 未替换。

| 项目 | 当前值 |
|---|---|
| bundle | `CSSwitch Acceptance.app` |
| artifact version | `0.6.0`（功能分支升版前的 Acceptance，不是最终 v0.7.0 DMG） |
| bundle id | `com.csswitch.acceptance` |
| desktop SHA-256 | `44a7044f0d8d22bf0dc0adad832796813ee199e0debbe9c24f7415f15fcb6b89` |
| gateway SHA-256 | `cedbfad3383f31954d4e50c060aa5dfa7f3dd1e36873528adff33984c26671e4` |

候选 bundle 与已安装 Acceptance 的两个 executable hash 逐项相同。用户在该安装态完成独立浏览器 OAuth；CSSwitch 自动确保 Codex profile，动态模型目录可见，并在 Science 中选择 `Codex / GPT-5.6-Sol`。直接 Gateway 最小请求返回 `pong`，Science UI 最小文本请求返回“端到端成功”。同一当前 Science PID 的日志区间没有 `Invalid content type`、`Claude is temporarily unavailable` 或 `temporarily unavailable`；更早日志中的同类错误属于修复前进程。

该轮最终修复把 Codex inference `User-Agent` 对齐为 `codex_cli_rs`，并在上游成功响应缺少 `Content-Type` 时只做有界 SSE 前缀识别：HTML、JSON、challenge、empty 和 other 仍拒绝，已消费字节会接回 reducer，每个 Science 推理请求仍最多一个上游 POST。定向结果为 Codex transport 6/6、Gateway 228 lib + 1 CLI、fmt/clippy 与 `git diff --check` 通过。

验收期间只使用 status、模型名、进程身份、脱敏日志枚举与请求结果判断；未读取、打印或比较 CSSwitch OAuth 文件内容，也未读取原生 `~/.codex`。

## 当前 Phase 6 artifact

2026-07-17 从当前未提交源码重新构建并安装了双 App 数据根隔离后的 Acceptance。Git HEAD 仍为 `785bf395d6a6f00daeae349bbe1088dd74a7efc6`，分支为 `codex/codex-science-bridge`；实际 artifact 还绑定本轮未提交的 browser-only、自动 profile、模型目录、打包一致性与 `$HOME/.csswitch-acceptance` 增量，不能仅凭 HEAD 复现。

- `bash test/run_all.sh --require-release-ready` 在允许 loopback 的本机环境退出 0；offline、loopback、scripts、Rust、frontend 五层均通过，`release-ready green: YES`。
- build script 对普通和 Acceptance 构建都禁止 `CSSWITCH_SKIP_GATEWAY_STAGE`；默认 staged Gateway 只含正式 `.csswitch` / Keychain identity，并对 Acceptance expected service 返回 identity mismatch 及退出码 8。
- Acceptance 候选内 Desktop 固定 `.csswitch-acceptance` 与 Acceptance expected service；Gateway 固定 `HOME.csswitch-acceptance` 和 `com.csswitch.acceptance.codex.*`，对正式 expected service 在进入 auth/Keychain 前返回 identity mismatch 及退出码 8。

| 项目 | 当前值 |
|---|---|
| bundle | `CSSwitch Acceptance.app` |
| version | `0.6.0` |
| bundle id | `com.csswitch.acceptance` |
| architecture | arm64 |
| desktop SHA-256 | `359b1118cb1230e5ffb781fefb430c59a15f2680a2790d5ed7ceccf937fc1098` |
| gateway SHA-256 | `8cb391f431eaf8197b46f9a50539b0680567c98be67c76815dcdf869f3ba7696` |

候选与 `/Applications/CSSwitch Acceptance.app` 的两个 executable hash 逐项相同；`codesign --verify --deep --strict` 通过，身份仍是 ad-hoc、无 Team ID，未提供 notarization 凭据。正式 `/Applications/CSSwitch.app` 替换前后 hash 均保持 desktop `d5a044de2d173a195db5f6c378688595539c748aa27f235107d019047fffaf07`、Gateway `e161f2b0ebcd5abea3bfc093365214f45e41c867a66d5a5cd8ed4dd2a8889883`。`/Applications` 只保留正式 app 与当前 Acceptance。

编译期根的实际迁移 smoke 在同一个临时 HOME 中同时放置两个 v2 fixture：候选和安装态 Acceptance 启动后，正式 `.csswitch/config.json` 仍为 v2，SHA-256 保持 `97deea942763a3007532c93a03bf7effd84c511ec853d39cc085daaae3e29004`；只有 `.csswitch-acceptance/config.json` 迁移为 v3 并生成自己的 `.v2.bak`。候选与安装态测试进程随后均已停止。

Phase 6 前共享根 artifact 当时曾在仓库外保留为临时恢复候选，未在 `/Applications` 并存；该候选不属于当前发布或验收流程。

真实 OAuth、当前账号 live 模型目录、Science 模型选择与实际推理仍未在这个新 artifact 上验收；旧共享根登录不能迁移为该结论，用户应预期重新登录。

## Phase 6 前 artifact（Superseded / 已停线）

以下 `33768…` / `67dda…` artifact 早于双 App 数据根隔离，Finder 启动仍使用正式 `$HOME/.csswitch`。它不是当前安装包，也不能作为隔离 Acceptance、live 模型或 Science 验收依据。

本证据只绑定 Phase 6 前的未提交源码快照，不把它写成公开 release、干净 commit 或当前工作树。Git HEAD 为 `785bf395d6a6f00daeae349bbe1088dd74a7efc6`，目标分支为 `codex/codex-science-bridge`；同一 HEAD 之后又增加了编译期 `$HOME/.csswitch-acceptance` 隔离，因此仅凭 HEAD 不能复现本页 artifact。

### 当时的自动 Gate 与审查

- `bash test/run_all.sh --require-release-ready` 在允许 loopback 的本机环境退出 0；offline、loopback、scripts、Rust、frontend 五层均为 pass，`release-ready green: YES`。
- Desktop 与 Gateway 的 `cargo fmt --check`、全 target/all feature Clippy `-D warnings` 通过；前端语法、新 browser-auth 静态合同与 `git diff --check` 通过。
- 模型、browser-only、profile/UI 四个阶段分别做了干净上下文复审；最终 requirement-by-requirement 只读审查未发现 P1/P2，允许构建 Acceptance。

### 历史候选与安装同源

构建命令使用 `acceptance-build` feature 和 `test/tauri.real-machine.conf.json`，没有复用 2026-07-16 的旧 artifact。

| 项目 | 值 |
|---|---|
| bundle | `CSSwitch Acceptance.app` |
| version | `0.6.0` |
| bundle id | `com.csswitch.acceptance` |
| architecture | arm64 |
| desktop SHA-256 | `33768c8c0e3443d08b3a738a66eb259e4fa5241036087256242f080796e932f1` |
| gateway SHA-256 | `67dda50f8d9681beac6c574a2db42d6ed576e3cb5e7288cc9e68a8df1558ebb1` |

候选与 `/Applications/CSSwitch Acceptance.app` 的两个 executable hash 逐项相同。`codesign --verify --deep --strict` 对候选、安装 staging 与最终安装 app 均通过；身份为 ad-hoc、无 Team ID，未提供 notarization 凭据，因此不声称 Developer ID、公证、stapled ticket 或 Gatekeeper 通过。

对已安装 Gateway 使用隔离 HOME、正确 Acceptance service handshake 调用旧 `codex-auth login-device`，进程在任何认证或 Keychain 工作前无输出退出 2；安装包不提供设备码登录合同。

正式 `/Applications/CSSwitch.app` 保持版本 `0.6.0`，安装前后 executable hash 未变：desktop `d5a044de2d173a195db5f6c378688595539c748aa27f235107d019047fffaf07`，Gateway `e161f2b0ebcd5abea3bfc093365214f45e41c867a66d5a5cd8ed4dd2a8889883`。`/Applications` 中 CSSwitch 系列只保留正式 app 与 Acceptance app。

### 当时执行的外层 HOME smoke（不证明 Finder 数据根隔离）

使用全新的 `/private/tmp/csswitch-codex-acceptance-20260717-final` 外层 HOME、独立空默认 Keychain、空 v3 config 和动态端口 `57885/57889`。安装态 Acceptance 从 `/Applications` 启动成功；Codex 实验入口保持默认关闭，随后只停止本轮自有进程。guard 与 `assert-stopped` 通过，测试 Gateway/Science 均停止，真实 Science `8765` 的监听 PID 保持不变。该 smoke 靠显式临时 HOME 隔离，不能证明用户从 Finder 启动时不会读取正式 `$HOME/.csswitch`。

### Phase 6 当时的风险与恢复停线

- 当前正式 `/Applications/CSSwitch.app` executable hash 当时未变，只证明 app 文件未替换，不证明真实配置仍是 v2；正式配置状态保持 **unverified**。
- 当时的停线要求是不自动读取、修改或降级真实 `$HOME/.csswitch`；正式 0.6.0 只能由用户本人打开验证，报告版本过新后才可另行授权恢复。
- 当时替换新 Acceptance 前保留了一份仓库外恢复候选，并禁止在 `/Applications` 留第三个 app；该步骤已归档，不属于当前 runbook。
- 新 Phase 6 Acceptance 使用 `$HOME/.csswitch-acceptance`，不会把本页共享根登录视为新 artifact 的登录证据，用户应预期重新登录。

## 该候选当时仍未验收

此前用户在 Phase 6 前 Acceptance 上完成浏览器登录并遇到模型校验失败；旧包的 desktop/Gateway hash 与当前安装包不同，因此该操作不算新 artifact 的 live 证据。本轮没有读取、dump 或比较真实 Keychain/OAuth 内容，也没有读取原生 `~/.codex`。

以下项目在该候选时点仍需用户实际操作后分别记录：

- 浏览器 OAuth 与自动出现 Codex profile；
- 当前账号 live 目录是否实际返回 Sol/Terra/Luna；
- 激活验证与一键开始；
- Science “More models” 选择；
- 文本、reasoning、工具调用与推理 POST 单发语义。
