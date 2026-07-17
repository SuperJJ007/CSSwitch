# Codex 浏览器登录与 GPT-5.6 模型接入实施方案

> 历史方案说明（2026-07-17）：本文原有 Keychain 与稳定签名设计已被[当前 Codex 功能合同](codex-science-bridge.md)中的“无签名前置私有文件存储”替代。以下 Keychain/签名段落仅保留为实施历史，不代表当前源码、构建或安装要求。

状态：阶段 0–6 的源码、自动 Gate、构建变体一致性、独立 `$HOME/.csswitch-acceptance` 迁移 smoke 与新 Acceptance 安装已完成；Phase 6 前共享根 artifact 只保存在 `/private/tmp` 作为授权后恢复候选。新 artifact 的真实 OAuth、live 模型、Science 选择和实际推理待用户重新登录验收；正式 0.6.0 的真实配置状态仍待用户本人打开确认。

目标分支：`codex/codex-science-bridge`

基线：`785bf395d6a6f00daeae349bbe1088dd74a7efc6`

## 1. 本轮目标与非目标

本轮必须同时满足：

1. 修复 OpenAI Codex 模型目录的字段兼容错误。
2. CSSwitch 浏览器 OAuth 成功后，幂等地在“我的配置”中创建一条 Codex profile。
3. Science 的 “More models” 至少能选择账号目录中的 `GPT-5.6-Sol`、`GPT-5.6-Terra`、`GPT-5.6-Luna`。
4. CSSwitch 的用户登录路径只保留浏览器 OAuth，不再提供设备码登录。

本轮不做：

- 不读取、复用或修改原生 Codex 的 `~/.codex` OAuth；
- 不把 CSSwitch OAuth token 交给 Desktop、前端、Science 或普通配置文件；
- 不伪造账号目录中不存在的模型；
- 不因目录、刷新、401、断流或代理错误重发 Science 推理 POST；
- 不自动把新建 Codex profile 设为当前，以免无提示替换用户正在使用的其他 provider；
- 不修改正式 `/Applications/CSSwitch.app`。

## 2. 权威证据与调研结论

### 2.1 官方登录产品语义

OpenAI 当前 Codex 手册把本机 ChatGPT 登录描述为浏览器流程：`codex login` 默认打开浏览器，浏览器完成登录后把凭据返回 Codex。设备码被定位为 remote/headless 或 localhost callback 被网络阻断时的 beta 备用路径，而不是本机默认入口。

本轮因此把 CSSwitch 收口为浏览器-only 产品流程。实现继续固定并测试现有 PKCE、state、loopback callback、token exchange、Keychain 原子提交、取消和诊断合同；设备码不再属于 CSSwitch Desktop/Gateway 的可调用登录合同。

官方来源：

- <https://learn.chatgpt.com/docs/auth>
- <https://developers.openai.com/codex/codex-manual.md>
- OpenAI Codex source boundary：`openai/codex@cbc83d961e8132bfff4d340ab8342d181b79e95e`

### 2.2 当前 CSSwitch 登录全流程

```text
高级设置启用实验入口
  -> 前端 codex_auth_start()
  -> Desktop 短暂取得 lifecycle 锁
  -> 校验 Codex network route
  -> supervisor 预留 operation
  -> 只停止受管 Codex Science/Gateway
  -> 启动 csswitch-gateway codex-auth login-browser
  -> 释放 lifecycle 锁
  -> sidecar 绑定 127.0.0.1:1455/1457
  -> PKCE-S256 + 256-bit state
  -> 打开系统浏览器
  -> callback 校验 path/state/重放
  -> sidecar 通过统一 network factory 换 token
  -> cancel/commit CAS
  -> OAuth + thinking key + generation 原子提交到 CSSwitch Keychain/state
  -> bounded NDJSON terminal
  -> Desktop 在短 lifecycle 临界区原子、幂等 ensure Codex profile
  -> supervisor 保存 snapshot 并发 Tauri event
  -> 前端刷新 auth status + config，显示已就绪 profile
```

该链路现已闭环：登录只有在 OAuth 与 profile ensure 都完成后才发布 `succeeded`。若 OAuth 已提交但 profile 保存失败，终态为 `profile_ensure_failed/profile_ensure`；前端可用 `codex_ensure_profile` 补建，App 重启后也会通过脱敏本地 status 自动恢复该入口。普通“＋新建”向导仍可手工增加额外 Codex profile，但不再是首次登录的必需步骤。

### 2.3 当前模型目录全流程

```text
设为当前/查看模型
  -> Desktop 获取 Codex supervisor use lease
  -> scratch Gateway
  -> Gateway 从 Acceptance/CSSwitch Keychain 读取自己的 OAuth
  -> GET https://chatgpt.com/backend-api/codex/models?client_version=0.144.4
  -> 解析并只保留 visibility=list
  -> 保存无凭据 last-known-good cache
  -> /v1/models 暴露 claude-csswitch-codex-<raw-id>
  -> Science 0.1.18 的 More models 接受 claude-* alias
  -> /v1/messages 将 alias 反解为 raw id 后再发单次上游 POST
```

本机官方 Codex 的无凭据模型缓存当前确认包含：

| Raw id | Display name | Visibility | Science alias |
|---|---|---|---|
| `gpt-5.6-sol` | `GPT-5.6-Sol` | `list` | `claude-csswitch-codex-gpt-5.6-sol` |
| `gpt-5.6-terra` | `GPT-5.6-Terra` | `list` | `claude-csswitch-codex-gpt-5.6-terra` |
| `gpt-5.6-luna` | `GPT-5.6-Luna` | `list` | `claude-csswitch-codex-gpt-5.6-luna` |

OpenAI 当前公开稳定 Codex CLI release 仍为 `0.144.4`；本轮不因本机内部缓存标记 `0.144.5` 而切到未公开稳定版本。

### 2.4 已定位的字段错误

live 目录已经通过 HTTP success、JSON root 和 `models` array 检查，失败点是单个模型条目的 Serde 反序列化。

当前结构把：

```text
supports_reasoning_summary_parameter
```

声明为：

```text
supports_reasoning_summaries
```

的 alias。live 条目同时包含新旧两个布尔字段时，Serde 把它们视为同一字段的重复输入并拒绝整个条目。安全诊断因此返回：

```text
error_kind=protocol
message=Codex model catalog entry schema is incompatible
```

修复策略是分别解析两个 `Option<bool>`，不能简单删除旧字段兼容。字段身份与归一化表达式固定为：

```text
supports_reasoning_summaries                         # 当前 canonical 字段
    .or(supports_reasoning_summary_parameter)        # 旧兼容字段
    .unwrap_or(true)                                 # 保持当前缺省语义
```

即 canonical 字段在两者冲突时优先；测试必须覆盖 `true/false` 和 `false/true` 两种冲突，不能只用两者同值的 fixture。
字段缺失才进入兜底；显式 `null`、字符串或数字都属于协议错误，不得静默转换成缺失后默认 `true`。

## 3. 目标用户流程

```text
启用 Codex 实验入口
  -> 点击“浏览器登录 Codex”
  -> 浏览器完成 ChatGPT/OAuth 授权
  -> CSSwitch 原子保存自己的凭据
  -> Desktop 后端幂等确保一条 Codex profile 存在
  -> “我的配置”自动出现 Codex（实验）
  -> 用户点击“设为当前”
  -> 目录验证成功；只展示 live 账号目录实际返回的模型
  -> 若 live 返回 Sol/Terra/Luna，则分别显示 GPT-5.6-Sol/Terra/Luna
  -> 用户点击“一键开始”
  -> Science / More models 可选 live 目录对应的 Codex / … 条目
```

RM-36 的本轮真机目标仍是当前账号同时返回 Sol/Terra/Luna；若 live 目录不再返回其中任一项，该项验收必须据实标记未通过，不能靠 fixture、缓存外硬编码或 alias 补造模型。

登录成功不自动切换 active profile。若已经有一条或多条 Codex profile，登录终态不创建重复项、不改名、不删除历史项；只把“至少一条存在”视为 ready。

## 4. 阶段与任务

### 阶段 0：调研与合同冻结

任务：

- 复核当前分支、已安装 Acceptance 与隔离真机环境；
- 绘制浏览器/设备码、supervisor、Keychain、profile、目录和 Science alias 全链路；
- 核对 OpenAI 当前登录产品语义、公开稳定 client version 和三个 5.6 raw id；
- 固定本方案的目标流程、非目标、失败语义和验收证据。

门禁：

- 主线程只读核对；
- `git diff --check`；
- 一个 `fork_turns=none` 的干净上下文审查员检查缺口、错误假设、并发和安全边界；
- 审查问题写回本方案后才开始实现。

第一次独立审查已完成，写回的阻断项是：

- 明确定义 profile ensure 失败的错误协议、无须重新授权的 repair command 与 UI；
- 最终安装增加授权停线点、安装副本同源校验与 post-install smoke；
- 固定 reasoning-summary 双字段冲突时的 canonical 优先级；
- 增加 profile ensure 与手工创建、logout、关闭实验入口之间的真实并发测试。

### 阶段 1：模型目录兼容与 GPT-5.6 三模型

任务：

- 在 `desktop/gateway/src/codex_models.rs` 分开解析新旧 reasoning-summary 字段并明确优先级；
- 保持 raw id 内存/磁盘模型与 Science alias 边界不变；
- 增加 live-shape fixture：canonical 字段、旧字段、两者同时出现、两者缺失、错误类型；两种相反布尔值的冲突 fixture 都断言 canonical `supports_reasoning_summaries` 胜出；
- 增加 Sol/Terra/Luna 的目录排序、display name、Science alias 和反解测试；
- 保持 `visibility=hide` 不暴露，不能为缺失账号模型造假；
- 保持公开稳定 `client_version=0.144.4`。

最小测试：

- `cargo fmt --check --manifest-path desktop/gateway/Cargo.toml`
- `cargo clippy --manifest-path desktop/gateway/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path desktop/gateway/Cargo.toml codex_models`
- `cargo test --manifest-path desktop/gateway/Cargo.toml codex_models_response_exposes_science_aliases_and_cache_diagnostics`
- `git diff --check`

阶段审查：干净上下文审查员检查 parser 宽松度、alias 冲突、缓存兼容、模型伪造、推理不重发与测试缺口。

### 阶段 2：浏览器-only 登录合同

任务：

- UI 只保留一个“浏览器登录 Codex”按钮；删除设备码文案、事件显示和控件绑定；
- Desktop `codex_auth_start` 固定 browser，不再接收用户可选 method；
- supervisor snapshot 的 method 固定为 `browser`，保留 operation id/sequence/replay/cancel 合同；
- packaged sidecar CLI 只接受 `login-browser/status/logout`；
- 删除 Gateway production device dispatch、device HTTP/poll 和相应生产合同；
- 删除或改写 device-only tests、stage/error allowlist 和文档；
- 保留 callback 1455/1457、五分钟预算、PKCE、state、防重放、取消与统一 network factory。

最小测试：

- Gateway auth CLI 与 browser login 单测；
- Desktop supervisor、sidecar allowlist、timeout/cancel/commit CAS 测试；
- `node --check desktop/src/main.js`；
- 静态断言 packaged UI/API 不再出现设备码入口；
- 负向断言旧 `login-device` CLI 被拒绝，旧 `method=device` 参数不能选择或到达设备码 dispatch；
- `git diff --check`。

阶段审查：干净上下文审查员检查是否仍有可达 device 路径、浏览器回调安全、终态单一性、取消和非 Codex lifecycle 隔离。

### 阶段 3：登录成功后幂等创建 Codex profile

任务：

- 新增 `ensure_codex_profile_inner`，返回 `created/existing`，不自动 active；
- sidecar 成功提交 Keychain 后、supervisor 发出 succeeded 前，在短 lifecycle 临界区执行 ensure；
- 正常路径必须先 profile ready 再发登录成功终态，前端收到终态后 reload config；
- 已有 Codex profile 时不重复、不改名、不删除；
- `ensure_codex_profile_inner` 自身不得取得 lifecycle/supervisor 锁；它在同一次原子 config 读改写中，以 canonical `template_id == "codex"` 且 provider contract 为 Keychain OAuth 作为 existing 谓词；
- profile 写失败时登录 operation 以 `failed` 结束，固定 `code=profile_ensure_failed`、`stage=profile_ensure`、`retryable=true`，不得谎称完整 onboarding 成功；
- 前端收到该特定失败终态时仍刷新脱敏 `codex_auth_status`。若状态为 authenticated，显示“授权已保存，Codex 配置尚未创建”以及“补建 Codex 配置”按钮，不显示笼统的“登录失败”；
- 新增公开、幂等的 `codex_ensure_profile` Tauri command。它在短 lifecycle 临界区内按 `lifecycle -> supervisor mutation` 顺序排他执行，以 sidecar 的脱敏 local status 作为 authenticated 前置条件，再调用不读取凭据的 profile helper，返回 `{ disposition: "created" | "existing", profile_id }`；不访问网络、不把 token 带入 Desktop、不设置 active；
- repair 成功后前端 reload config 并隐藏按钮；repair 失败保留 authenticated 状态与可重试入口，不要求重新进行浏览器授权；
- 登录失败/取消不得创建 profile；
- 非 Codex provider 在浏览器等待期间保持可操作。

最小测试：

- 无 profile 成功登录创建一条；
- 已有一条/多条不重复；
- 登录失败、取消、pre-commit 取消不创建；
- profile 原子保存失败不回滚已提交 OAuth，但终态/文案必须准确；
- active 非 Codex profile 不变；
- 页面刷新/迟到事件不重复创建；
- terminal ensure 与 repair、手工 Codex create、logout、关闭实验入口并发时无死锁、最多一条自动创建的 canonical profile、active 不变；
- `profile_ensure_failed` 会刷新 auth status，repair command 的未登录/已登录、created/existing 与原子保存故障均覆盖；
- Desktop Rust + frontend syntax + `git diff --check`。

阶段审查：干净上下文审查员检查 lifecycle/supervisor 锁顺序、Keychain/config 双提交失败语义、幂等性和对其他 provider 的影响。

### 阶段 4：流程文案、模型展示与回归收口

任务：

- 登录成功文案明确“Codex 配置已创建/已存在”；若 Codex 不是当前配置，下一步是“设为当前”，若已是当前配置，下一步是“一键开始”；
- “查看模型”和激活校验显示账号目录来源与 Sol/Terra/Luna；
- 去掉“浏览器备用”“设备码优先”等历史文案；
- 更新 Codex 功能合同、真机验收矩阵和相关静态测试；
- 验证登录成功不自动启动 Gateway/Science、不自动替换 active provider。

最小测试：

- 前端静态/preview 检查；
- Desktop + Gateway 定向回归；
- `git diff --check`。

阶段审查：干净上下文审查员检查用户流程是否还有多余入口、错误下一步、隐藏状态或文档漂移。

### 阶段 5：全面测试、真机验收与实装

任务：

- 运行 `bash test/run_all.sh --require-release-ready`；
- 运行 Rust fmt/clippy/test、frontend check 和 `git diff --check`；
- 最终干净上下文审查员按四项要求做 requirement-by-requirement 全面审查；
- 从同一源码状态构建 `acceptance-build` app，并记录候选 app 的源码 HEAD、可执行文件 hash、bundle version、identity 与签名状态；
- 使用全新隔离 HOME/Acceptance Keychain/动态端口启动 build-tree 候选；
- 用户本人完成浏览器 OAuth；
- 真机验证 profile 自动出现、激活验证通过、Science 可选三个 5.6 模型、文本与工具调用；
- 核验真实 Science `8765`、正式 CSSwitch、原生 Codex OAuth 均未改变；
- 进入 `/Applications` 写入前设置明确停线点：确认本轮用户授权仍覆盖替换 Acceptance；没有授权则只交付候选路径，不复制；
- 获授权后只替换 `/Applications/CSSwitch Acceptance.app`，保留 `/Applications/CSSwitch.app` 0.6.0；
- 安装后比较 installed app 与候选的 bundle version、identity、可执行文件 hash 和签名状态，证明同源；
- 用同一隔离 HOME/Acceptance Keychain 启动 `/Applications` 中的 installed Acceptance，至少复验 sanitized auth status、profile、模型目录、正式 App/8765/原生 Codex guard，最后 `assert-stopped`；
- 若用户选择把真实 OAuth/Science 操作留到稍后，源码、测试、候选、安装态与真实验收必须分别标记，不能把未执行的真实登录/推理写成通过。

最终审查必须把以下证据层分开报告：源码/测试、构建 artifact、已安装 Acceptance、真实 OAuth、模型目录、Science 选择、实际推理、签名/公证。

### 阶段 6：双 App 的编译期数据根隔离

目标级审计发现，独立 bundle ID 与 Keychain service 仍不足以支持 Finder 下双 App 并存：原 Acceptance 构建的 Desktop 与 Gateway 都从真实 `$HOME/.csswitch` 派生配置、OAuth state、模型缓存、runtime、日志和 Science sandbox，可能把正式 0.6.0 的 v2 配置迁到 v3。

任务：

- `acceptance-build` 构建在 Desktop 与 Gateway 编译期固定 `$HOME/.csswitch-acceptance`；普通构建继续固定 `$HOME/.csswitch`；
- Gateway CLI auth state root 与 formal/scratch Codex provider root 使用同一构建常量；
- Desktop config、runtime、logs、Science sandbox 与 Skill manager 继续统一从 `config::default_dir()` 派生；
- 两种路径没有环境变量或 UI 运行时覆盖入口；
- 真机 guard 的 Acceptance fixture 改到 `.csswitch-acceptance`，并同时保护模拟真实 `.csswitch` 与 `.csswitch-acceptance` sentinel；
- Finder 启动的新 Acceptance 不读取或迁移正式配置，首次表现为独立空配置；此前旧 Acceptance 的 OAuth 操作不能计作新根的 live 验收。

门禁：

- default 与 `--features acceptance-build` 分别运行 Desktop/Gateway 路径合同测试；
- real-machine guard、browser-auth 静态合同、Rust fmt/clippy/test 与全量 release-ready Gate；
- 从当前源码重建并重新安装，候选/安装 executable hash 相同，正式 0.6.0 hash 不变；
- 干净上下文独立审查 Finder 双 App、Keychain/state/config/cache/runtime/Science sandbox 全链路无串根后才交给用户。

## 5. 失败语义与安全不变量

- 浏览器等待期间不持有全局 lifecycle 锁；
- 锁顺序保持 `lifecycle -> CodexAuthSupervisor`；
- token exchange、callback wait、slow header/body 与取消继续可中断；
- commit 开始后取消返回 `commit_in_progress`；
- OAuth commit 成功而 profile ensure 失败时，不删除已提交凭据、不要求用户重做浏览器授权；
- profile ensure 不能读取 token，只能消费“登录成功”这个控制事实；
- 模型解析错误不保存上游正文，不显示 token、email、account id、operation id 或完整 proxy URL；
- 目录 GET 可按既有合同重试，Science 推理 POST 仍最多一次；
- logout 只删除 CSSwitch 自有 Keychain 项，非法代理也不能阻止本地失效。

## 6. 四项完成判据

| 要求 | 完成证据 |
|---|---|
| 字段错误修复 | 双字段 live fixture 通过；隔离账号目录 GET 200，不再是 `protocol` 502 |
| 登录后自动有配置 | 新隔离 HOME 浏览器登录后无需打开“＋新建”，主列表出现且仅出现一条新 Codex profile |
| Science 可选三模型 | `/v1/models` 与 Science More models 同时显示 Sol/Terra/Luna alias；逐个选择时 Gateway 恢复对应 raw id |
| 只用浏览器登录 | UI、Tauri command、packaged sidecar 可达合同均无 device 登录入口；browser PKCE 真机成功 |

源码 Gate、构建变体身份、双 App 数据根隔离、安装同源和最终独立审查无未解决 P1 后，可在用户已授权的边界内替换 `/Applications/CSSwitch Acceptance.app`，供用户执行真实 OAuth 与 Science 验收。上表四项均取得当前 artifact/真机证据后，才可把 Codex browser-only live 功能标记为完成；安装验收包本身不等于这四项已经通过。
