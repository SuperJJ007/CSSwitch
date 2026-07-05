# 更新日志 / Changelog

本项目所有值得记录的变更都写在这里。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

> **约定**：已修问题从 [`docs/known-issues.md`](docs/known-issues.md)「毕业」到这里（发布即定稿）；未修/进行中留在 known-issues；硬 bug 的根因证据链存在 [`findings/`](findings/)。

## [0.3.3] — 2026-07-05

> 主题：**跨平台桌面构建 + 远程 Linux Helper**。这一版把 CSSwitch 从 Apple Silicon 单平台包推进到 Windows x64 / Windows arm64 / macOS arm64 三类桌面安装包，并把远程 Linux helper 做成 x86_64 / aarch64 两个静态产物，方便在 Windows 桌面里一键准备远程服务器上的 Science 沙箱。版本同时补齐 GitHub Actions 发版矩阵和 macOS 编译回归，后续可以通过 `v*` tag 自动构建并发布。

### 新增 Added
- **Windows 桌面包**：CI 现在产出 `CSSwitch-Windows-x64` 和 `CSSwitch-Windows-arm64` 两个 NSIS 安装器；x64 面向常见 Intel/AMD Windows，arm64 面向 Windows on Arm。
- **macOS Apple Silicon DMG**：保留现有 macOS arm64 DMG 方案，不额外引入 Intel / Universal 构建，避免扩大未签名分发范围。
- **远程 Linux helper 双架构**：CI 产出 `csswitch-helper-linux-x86_64` 与 `csswitch-helper-linux-aarch64`，桌面包会内置 helper 资产，用于自动上传到远程 Linux 主机并管理远程沙箱 / 代理。
- **远程一键启动闭环**：补齐 SSH 登录认证、helper 上传 / 自愈、固定远程端口、返回远程沙箱访问链接、远程进程状态检测等路径，让 Windows 本机也能管理 Linux 远程运行环境。
- **可追溯发布矩阵**：GitHub Actions 拆成 helper、Windows、macOS、测试、Release 五段；main 分支先验证产物，`v*` tag 再自动创建 Release 并挂载安装包 / 二进制资产。

### 变更 Changed
- **桌面包自包含策略**：Tauri 资源继续打入 `proxy/`、`scripts/` 和 `helper-assets/`，所以安装后的 Windows / macOS app 不需要旁边再放仓库源码。这里包含的 Python 文件是 CSSwitch 本地代理运行所需脚本，不是第三方项目二进制。
- **平台产物定位明确**：Windows 发布 x64 + arm64 两个安装器；macOS 按现有方案只发 Apple Silicon arm64 DMG；Linux 不发布桌面 app，只发布给远程模式使用的 helper。
- **远程 helper 内置代理脚本**：helper 自身也编译进代理脚本兜底，远程主机缺文件或安装目录被清理时可以重新落地，减少“装过一次后漂移”的问题。

### 修复 Fixed
- **macOS CI 编译失败**：修复 Tauri 回调和停沙箱路径里的 `AppHandle` 作用域问题，并加 UI contract 回归测试覆盖 `_app` / `app` 这类跨平台编译差异。
- **helper / 桌面构建参数不一致**：统一 helper 的无桌面特性构建、Tauri 桌面 target 和资源下载路径，避免某个平台构建成功但包内缺远程资产。
- **远程沙箱进程管理健壮性**：加强端口避让、用户级路径兼容、状态识别和停止逻辑，减少远程一键启动后残留进程或误判。

### 说明 Notes
- 如果要让安装包内部版本号和 Release tag 完全一致，请使用 `v0.3.3` tag 触发 Actions 重新构建；不建议把 `0.3.2` 版本号的旧 artifacts 挂到 `v0.3.3`。
- 与 CC Switch 的关系：CSSwitch 的命名和产品形态受 [CC Switch](https://github.com/farion1231/cc-switch) 启发，也借鉴了“provider/profile 切换工具”的思路；CC Switch 使用 MIT 许可。CSSwitch 是面向 Claude Science 的独立项目，不隶属、不同步发布、也不受 CC Switch 或 Anthropic 背书；当前代理、桌面、远程 helper 和沙箱登录实现均为本仓库实现，没有打包 CC Switch 二进制。
- 发布产物说明：Windows 下载 `.exe` 安装器；macOS Apple Silicon 下载 `.dmg`；`csswitch-helper-linux-*` 是远程服务器用的 helper，不是普通用户桌面安装包。
- 分发限制：当前 Windows 未做代码签名，可能触发 SmartScreen；macOS 为 ad-hoc 签名、未 Apple notarization，首次打开需右键“打开”或在隐私与安全性里放行。
- 安全边界仍不变：只操作 CSSwitch 的隔离沙箱和 `~/.csswitch` 配置，真实 Science 数据目录与 8765 端口仍按铁律保护。

## [0.3.2] — 2026-07-04

> 主题：**Science 顶部显示真实模型名 + 新增 Kimi / MiniMax**。修复 relay 家在 Science 模型选择器里笼统显示「claude / opus」的问题（#11），让每个服务商都能选择或自填模型、并在 Science 里显示真实模型名；新增 Kimi（Moonshot）与 MiniMax；各家内置模型更新到官方主流版本。

### 新增 Added
- **Kimi（Moonshot）与 MiniMax 两家服务商**：均走原生 Anthropic 兼容端点（`api.moonshot.cn/anthropic` / `api.minimaxi.com/anthropic`），零协议转换。thinking 按各家要求注入（Kimi 强制 enabled、MiniMax 走 adaptive）。
- **为每个服务商选择或自填模型**：模型输入从下拉改为「下拉精选 + 自填」——下拉里是我们维护的各家主流模型，也可以直接填写任意模型名。自定义端点终于有地方填模型名了。

### 修复 Fixed
- **relay 家在 Science 顶部选择器显示笼统的「claude / opus」（#11，#12 显示部分）**：根因是 Science 的模型面板二进制写死只认 `claude-` 开头的 id。现让 relay 家复用 DeepSeek 已验证的「借壳」做法——代理向 Science 返回一个 `claude-opus-4-8` 外壳、显示名写成你选择的真实模型名，实际推理仍走你选的模型。真机验证：Science 顶部正确显示 `glm-5.2` 等真实名。
- **中转 / 自定义端点可以保存空的连接地址或空模型**：清空 `base_url` 或不选模型也能「保存成功」、激活时才失败。现在保存前就拦下，前端与后端各有一道守卫，绝不谎报已保存。
- **各家内置模型过时**：上官方来源逐一核对，更新到当前主流版本（GLM 旗舰 `glm-5.2`、MiniMax 旗舰 `MiniMax-M3`、硅基 `DeepSeek-V4` 系、Kimi `kimi-k2.7-code` 等）。

### 说明 Notes
- 全 relay 家统一为「选一个模型」：GLM / OpenRouter 从「默认跟随 Science」改为需要指定一个模型。升级时，旧的未指定模型的配置会自动补上该服务商的默认模型，并给出一次提示。
- 硅基流动的 Anthropic `/v1/messages` 兼容性经真机确认（返回 200，无需协议转换）。
- 全绿：cargo test 122 / clippy 0 / fmt clean；代理单测 40；真机在隔离沙箱 Science 里验证模型选择器显示。铁律全程守住（真实 `~/.claude-science` 与 8765 端口未碰）。

## [0.3.1] — 2026-07-04

> 主题：**内置预设支持自定义 base_url**。修用户反馈的小米 MiMo「token plan」401。

### 修复 Fixed
- **内置预设 `base_url` 只读，导致小米 MiMo「token plan」报 401**：小米 MiMo 的「token plan」套餐走独立域名 `token-plan-cn.xiaomimimo.com/anthropic`，与内置的 `api.xiaomimimo.com/anthropic` 不是同一 host；旧版预设地址锁死改不了，套餐 key 打到内置域名被上游 401。现将四家 relay 预设（智谱 GLM / 小米 MiMo / 硅基流动 / OpenRouter）的 `base_url` 改为**可编辑的默认值**：预填官方地址，允许改到 token 套餐 / 区域镜像 / 自建反代（新建向导与「编辑连接」两处都可改）。DeepSeek / 通义千问为原生 adapter（上游地址在代理内固定，运行时不吃自定义地址），保持只读以免「能填但不生效」的假象。「自定义」来源行为不变（空地址、可编辑）。

### 说明 Notes
- 纯前端 + 模板注册表改动，不改运行语义与鉴权。cargo test 114 全绿 / clippy 0 / fmt clean；前端预览实测（新建向导 + 编辑连接均可改、原生仍锁、自定义仍空、改到 token-plan 地址生效）。
- 顺带修 `config.rs` 一处历史 fmt 漂移。

## [0.3.0] — 2026-07-04

> 主题：**多 API 支持 + UI 改版**。从只支持 DeepSeek / 通义千问两家，扩展到 7 家 provider + 自定义端点的 cc-switch 式**多 profile 管理**；面板重做为配置列表 + chip 网格 + 三能力模型呈现。真机验收通过，正式毕业为**稳定版**（取代 v0.2.1 成为 Latest）。`0.3.0-beta.1/beta.2` 大预览版的内容在此定稿。

### 新增 Added
- **多 API / 多 provider 支持**：内置 7 家模板（DeepSeek、通义千问、智谱 GLM、OpenRouter、小米 MiMo、硅基流动）+ **自定义 OpenAI / Anthropic 兼容端点**（自填 `base_url` / 模型 / Key）。
- **cc-switch 式多 profile 配置管理**：把「固定槽」升级为用户自管的命名配置列表 + 当前生效指针；同一家可存多套、命名、增删、一键切换。切换是**事务式**的（先探活候选、健康才提交、失败回滚、全程不停沙箱）。配置用 JSON 存储并硬化（原子写 + schema 版本 + 覆盖前留 `.bak`），v1→v2 迁移不丢数据。
- **中转站 relay provider**：填 `base_url` + `token` 即可接任意 Anthropic 兼容中转站；`/v1/models` 回源自动铺该站真实模型到选择器。
- **DSML 工具调用兜底 shim（默认 `off`）**：DeepSeek 偶发把工具调用泄漏成纯文本 DSML 标记致 Science 卡死（issue #8）；由环境变量 `CSSWITCH_TOOLUSE_SHIM` 选 `off`（默认字节透传）/ `detect`（透传 + 遥测）/ `rewrite`（还原成真正的 `tool_use`）。

### 变更 Changed
- **面板 UI 改版**：重做为 profile 配置列表；来源选择改 **chip 网格**（键盘可达 / `aria-pressed`）；模型字段按**三能力**呈现（native 内置映射 / relay 跟随 Science / relay 固定）；新建 / 编辑走独立视图（隐藏运行区、保留反馈区）；文案瘦身 + a11y（label 关联 / `:focus-visible`）。折入四轮外审反馈。
- **反馈栏空闲不占位**：去掉常驻「就绪。」，只有真实反馈（错误 / 结果 / 自检输出）时才显示。

### 修复 Fixed
- **配置列表长 key 掩码横向溢出**：掩码原来一个字符一个圆点，长 key 在 WKWebView 里不换行、撑出横向滚动条、裁掉行与按钮。改为**定长** `••••` + 末 4 位（任何长度都短、不泄漏 key 长度）。
- **自检（doctor）对非 deepseek/qwen 来源误报**：`doctor.sh` 停留在单 provider 时代、按 provider 名写死并去 shell 环境找 key，导致 glm/xiaomi 等生效时误报「未知 provider」。改为多 profile 感知（据 adapter + key 有无报告，key 存 `config.json`）。
- 承接 beta.1/beta.2 的多 profile / DSML 修复（无效 native key 拦截、切换回滚健壮性、SSE 末帧、布尔校验、rewrite 无泄漏不逐字等，详见下方 beta 条目）。

### 说明 Notes
- **真机验收已完成**：多 profile / relay 的真机行为经用户在场实测确认（beta 阶段的 RM 待办已消解）；DSML 仍默认 `off`，`rewrite` 需显式开启。
- **铁律零回退**：全程只碰隔离沙箱，绝不触碰真实 `~/.claude-science` 与端口 8765；真机测试由用户在场完成、Claude 不代登录。
- **验收闸门**：cargo test 113 全绿 / node / bash 语法通过。
- **拔 node / python（治本）仍在 roadmap**：app 运行时零 node（虚拟登录 Rust 原生，v0.1.4 起）；翻译代理仍 Python，收敛到 Rust 单二进制待后续。

## [0.3.0-beta.2] — 2026-07-04

> 主题：**大预览版（Big Preview）**。一次把三块尚未进正式版的功能合在一起供实机试用：① cc-switch 式**多 profile 配置管理** + **中转站 relay provider**；② DeepSeek 工具调用泄漏**兜底 shim**（默认 `off`）。
>
> **⚠️ 这是预览/测试版（prerelease），不是稳定版。** 稳定版仍是 **v0.2.1**。多 profile 与 relay 的真机行为仍待复测（见「说明」），DSML 默认关闭仅供显式验证。本版**取代并撤回**同日名不副实的 `v0.3.0-beta.1`（那版 CHANGELOG 只写了 DSML，却把桌面侧改动一并打包却未如实说明）。

### 新增 Added
- **多 profile 配置管理（cc-switch 式）**：把原来的「固定槽」（每家一份）升级为**用户自管的命名配置列表 + 当前生效指针**：同一家（如 GLM）可存多套、命名、增删、一键切换。切换是**事务式**的：先探活候选、健康才提交、失败回滚、全程不停沙箱。内置 7 家 provider 模板。配置继续用 **JSON** 存储并硬化（原子写 + schema 版本字段 + 覆盖前留 `.bak`），SQLite 缓议。
- **中转站 relay provider**：只需填 `base_url` + `token` 即可接**任意 Anthropic 兼容中转站**；`/v1/models` 回源自动把该站真实模型铺进选择器；双鉴权头兼容各家。
- **DSML 兜底 shim（默认 `off`）**：DeepSeek 偶发把工具调用泄漏成纯文本 DSML 标记（`<｜｜DSML｜｜tool_calls>…`），Science 当普通文本、工具无回执 → **卡死**（issue #8）。shim 端到端接进 `_handle_anthropic`，由环境变量 `CSSWITCH_TOOLUSE_SHIM` 选模式：`off`（默认、字节透传、零回归）/ `detect`（透传 + 遥测）/ `rewrite`（把泄漏还原成真正的 `tool_use`）。新增 `test/test_proxy_dsml_e2e.py` 端到端证明。

### 修复 Fixed
- **（多 profile）无效 native key 被误报「已切到」**：`deepseek`/`qwen` 等 native adapter 此前跳过上游校验、只探本地 `/health`（恒回 200 不验 key），坏 key 会被提交为当前生效、UI 谎报成功、直到首个真实推理才 401。现 native 也走隔离探测打 `/v1/messages` 触上游，坏 key 拦下不提交、active 与旧代理不动。
- **（多 profile）切换/编辑健壮性**：切换写盘失败回滚进程；停沙箱失败即返错且端口不变（不再谎称已重置）；非 active 连接编辑 truthful-save（只拦明确 4xx、其余据实标「未校验，激活再验」）；真机护栏拒软链 + `canonicalize` 拒落真实 HOME 内 + 写盘先删软链再写新文件；若干前端如实提示。
- **（DSML）SSE 末帧丢失**：`_drain_frames` 的 `flush_tail` 形参此前未被使用，EOF 突然时吞掉 `message_stop`（实测 `b''`）。现 `finalize()` 补吐末帧。
- **（DSML）非法布尔臆断成 `false`**：`_coerce_param` 此前把 `maybe` 之类都当 `False` 并过校验，可能合成参数错误的真实工具调用。现只认 `true/1/yes`、`false/0/no`，其余整块作废。
- **（DSML）rewrite 非流式无泄漏时不逐字 + 遥测误报**（清洁 agent 实机验证发现）：`rewrite_nonstream_body` 此前无条件 `json.dumps` 再序列化，干净响应也被改字节且误报「已改写」。现无改动时原样返回原字节。

### 说明 Notes
- **⚠️ 多 profile / relay 仍待真机复测**：RM-04/06/13（非 active native 编辑即时校验、无效 native key 必被拦、端口占用报错措辞）代码 + 单测已覆盖，但**真机行为需在场实测确认**（铁律 4，Claude 不代登录）。这正是本版为 prerelease 的原因；请勿当稳定版依赖。
- **DSML 默认关闭**：普通用户安装后 DSML 行为与 0.2.1 一致；`rewrite` 需 `CSSWITCH_TOOLUSE_SHIM=rewrite` 显式开启（`detect` 只统计不改写）。把 rewrite 设为默认留待其余闸门（如把合法 DSML 示例误判为调用的边界）关闭后的后续版本。
- **铁律零回退**：全程只碰隔离沙箱，绝不触碰真实 `~/.claude-science` 与端口 8765；真机测试须用户在场、Claude 不代登录。
- **验收闸门**：cargo test 113 / clippy -D warnings 0 / `run_all.sh` ALL GREEN / gitleaks 0。
- **拔 node/python（治本）未在本版**：proxy 仍是 Python、伪造器仍是 Node，收敛到 Rust 单二进制仍在 roadmap。

## [0.2.1] — 2026-07-03

> 主题：热修「开了 CSSwitch 仍被要求登录」。0.2.0 有两个会导致「流程走完仍落登录页」的缺陷，本版各修一个并各补一条离线回归测试。链路方案本身没坏（代理此前成功处理过真实聊天、虚拟 OAuth 结构自洽），坏的是「重开 / 取入口 URL」路径。

### 修复 Fixed
- **入口 URL 解析错误（直接命中用户现象）**：Science 的 `url` 命令输出多行（第一行是真 URL，第二行是「single-use…」说明）。`sandbox_url()` 旧代码把**整段 stdout** 当 URL 交给 `open`，参数带上换行与说明文字 → 打开错误入口、单次性 nonce 未被正确消费 → 最终落到 `/login`。新增纯函数 `first_http_url()`，**只取第一条合法 `http(s)://` URL**；找不到才退回裸端口。回归测试 `first_http_url_takes_only_first_valid_url` 钉死多行/尾随说明/无 URL 各情形。
- **健康快捷路径绕过登录修复（自愈缺口）**：0.2.0 只要 `8990` daemon 活着就「连 auth 文件都不读」直接重开窗口，于是**旧版遗留 / 凭证损坏 / 已落登录页**的健康 daemon，重复点「一键开始」也永不自愈。现在健康分支先做**只读**校验 `login_intact()`（复用既有 `read_intact_login` 的从严自洽判定，绝不写文件）：自洽 → 只重开（保持 0.2.0 行为，org 不动、旧对话不丢）；**健康但登录失效 → 停沙箱，落到「修复保 org + 重启」路径自愈**（`ensure_virtual_login` 幂等，绝不静默换 org）。回归测试 `login_intact_true_for_fresh_false_when_damaged_and_readonly` 钉死「自洽判真 / 缺 .enc 或过期判假 / 全程只读不写」。

### 说明 Notes
- **受影响用户临时绕法**：先点「停止代理与沙箱」，确认 `8990` 已停，再点「一键开始」——绕开健康快捷路径、生成新入口；不会删除历史组织数据。装了 0.2.1 后无需此绕法。
- **铁律零回退**：全程只碰隔离沙箱（`~/.csswitch/sandbox`），绝不触碰真实 `8765` 与 `~/.claude-science`；`login_intact` 只读且带同一套真实目录护栏。
- 现有测试之所以全绿仍漏掉这两处，是因为没覆盖「CLI 多行 URL」与「daemon 健康但登录态失效」两个场景；本版补上。

## [0.2.0] — 2026-07-03

> 主题：一键**幂等化**——修 #3（运行中再点的分派）与 #6 的「复发」部分（更新后对话「不见」），并随三轮外部复审全面加固代理与进程路径。

### 修复 Fixed
- **#3 一键幂等分派 + #6 对话不再被孤儿化（阻止复发）**：虚拟 OAuth 伪造由「每次铸新」改为**幂等**。`one_click_login` 先按 **data-dir 强身份**探沙箱（非裸端口）——已在跑就只重新打开、**绝不重伪造、连登录文件都不读**；没起才 `ensure_virtual_login`（完整自洽 → 原样复用 / 部分损坏 → 修复但**保住 org** / 真首次 → 铸新）。**核心不变式：`org_uuid` 只在真正首次铸一次、此后复用与修复都粘住它**，根治「每点一键换新 org、把旧对话留在旧 org 目录里界面看不到」。org 来源优先级 `active-org.json → 可解 token → orgs/ 目录`；发现多个历史组织却无法确定活动者时**报恢复错误、绝不静默新铸**。**注**：本版只「阻止复发」，**已经产生的孤儿历史对话恢复**（把旧 org 指回 active-org）留待后续（#6b），故 #6 未完全毕业。
- **提示据实**：一键结果分「已在运行，已重新打开 / 已用新配置重启代理，Science 沿用不变 / 沿用原有对话 / 已启动」四态；浏览器打开失败改提示手动打开，不再谎报「已重新打开」。
- **停止 / 切换官方模式不再虚报成功**：定位不到停止脚本时如实报错（沙箱可能仍在跑）；切「官方 Claude」改为**先拆第三方链路成功、再落盘 official**，杜绝「磁盘=官方 / UI=第三方 / 进程已停」的状态分裂。
- **代理健壮性**：畸形请求体（顶层非对象 / `messages` 非数组）规范回 **400** 而非击穿线程；上游 **401/403/429 原样透传**（DeepSeek 原生透传与 Qwen 翻译**两条路径都修**），存 key 时能准确提示「key 无效」。

### 安全 / 健壮性（三轮外部复审加固）
- **OAuth 复用校验从严**：仅当 `account_uuid` 是合法 UUID、`provider=claude_ai`、`access_token` 非空、`token_expires_at` 未过期、读路径非符号链接时才判「可复用」；否则降级修复（安全方向）。`encryption.key` 的 `OAUTH_ENCRYPTION_KEY` 非法 base64 时自愈重造。
- **孤儿代理清理收紧**：`pkill` 匹配收紧到**本安装的绝对脚本路径 + 端口**（正则元字符转义），不误杀另一 checkout / 用户手启的同名代理。
- **沙箱身份确认**：一键启动后与状态灯改用 `claude-science status --data-dir`（按 data-dir 的强身份）确认，替代裸端口 `/health`，防端口被冒名服务占用时误判「已启动 / 绿灯」。

### 变更 Changed
- 全仓 Rust 统一格式（`cargo fmt`，`--check` 通过）。

## [0.1.5] — 2026-07-03

### 变更 Changed
- **主按钮及提示文案脱敏**：用户可见的「一键越过登录」全部改为中性的「一键开始」（`desktop/src/index.html` 主按钮 + `desktop/src/main.js` 各 `setMsg` 提示 + `desktop/src-tauri/src/lib.rs` 回传给用户的错误串 + `README.md` / `desktop/README.md` 里展示的按钮名）。README 开场白「绕过 Claude 登录」改为「无需 Claude 订阅也能用上它」。技术内部文档、历史记录与 DMCA 免责里的机制描述按边界保留。

## [0.1.4] — 2026-07-03

> 自 0.1.2 起合并数波修复发布：node-ectomy（拔掉 node 运行时依赖）、沙箱卡「Switching organization」实机修（CONNECT 403→401）、面板改正常窗口去托盘、第三方 / 官方模式一键切换，及两轮外部复审加固。

### 重大：开箱即用（拔掉 node 运行时依赖）
- **🎯 node-ectomy —— 虚拟 OAuth 伪造器移进 Rust 原生实现，app 彻底不再需要 node。** 那 220 行 `make-virtual-oauth.mjs`（HKDF-SHA256 + AES-256-GCM 的 v2 加密令牌）用 RustCrypto（`aes-gcm`/`hkdf`/`sha2`）1:1 重写为 `src/oauth_forge.rs`，编进二进制；一键流程在进程内伪造，启动脚本带 `--skip-oauth-forge` 跳过 node 步。**与 `.mjs` 的 v2 GCM 格式字节兼容**，由 node↔rust 双向解密对拍单测钉死（`oauth_forge::tests::crosscompat_*`）。这是 #2「缺 node」的**治本解**：不管用户装没装 node（含 B 类「根本没装」的研究者用户），app 都能用。`.mjs` 保留作 dev/独立脚本 + 对拍预言机。`doctor` 里 node 从「必需」降为「仅 dev」。

### 新增 Added
- **第三方 / 官方 模式一键切换**：面板顶部加分段切换。**第三方模型**（默认）= 原有一键越过登录 → 走 DeepSeek/Qwen；**官方 Claude** = 面板收起 provider/key/沙箱那套，主按钮改为「打开官方 Claude Science」，用 `open`（LaunchServices 正常启动，显式抹掉任何 `ANTHROPIC_*`）把用户交回自己真实的 Science 与订阅——**CSSwitch 不插手官方登录、不起代理/沙箱、不碰任何真实凭证**（铁律 1/2/3）。切到官方是**真正的切换**：后端先停掉第三方链路（沙箱 + 代理、清 secret），既不留后台空跑，也避免 macOS 单实例下 `open` 误聚焦还活着的沙箱实例。给「有 Claude 订阅、想正常走官方 OAuth」的用户用。模式存 `~/.csswitch/config.json` 的 `mode` 字段，下次启动记住。

### 修复 Fixed
- **#2 已装 node 仍报「缺少依赖 node」**（GUI 从访达启动只拿到最小 PATH，查不到装在 `/usr/local/bin`、Homebrew、nvm 等处的 node）。两层解决：**① 治本** = 上面的 node-ectomy（app 不再需要 node）；**② 止血兜底**（仍用于定位 python3）= `which()` PATH 未命中时扫常见安装目录 + 登录 shell `zsh -lic 'command -v'` 解析真实 PATH。*多位客户 + 开发者第二台机器复现确认。*
- **#3 沙箱卡在「Switching organization」**（启动时对 `claude.ai/api/oauth/profile` 的阻塞请求在到不了 claude.ai 的网络上挂住）。代理新增 `do_CONNECT`：对 `claude.ai / *.claude.com / *.anthropic.com` 的 CONNECT 立即 **401（未登录）** fast-fail，其余域名隧道透传；`launch-virtual-sandbox.sh` 注入 `https_proxy` 指向代理 + `no_proxy=127.0.0.1`（本地推理仍直连）。**实机整链验证并修正**：初版回 403（禁止）会让 operon 当组织问题反复重试、仍卡住；改回 **401** 才触发 operon `treating as logged-out` 秒过（operon 日志实测：403→卡、401→过，唯一变量就是状态码）。根因与实机证据见 [`findings/switching-organization-hang.md`](findings/switching-organization-hang.md)。
- **#1 面板无法鼠标拖动**：随 #4 消失（原生标题栏自带拖动）。

### 变更 Changed
- **#4 面板从「菜单栏 accessory」改为正常 Dock 窗口**：`decorations`（标题栏关闭/最小化/缩放三键）+ `visible`（启动即显示）+ `center`（居中）+ `resizable`（min 320×520）；**移除菜单栏托盘图标**（纯正常窗口）；从红叉关窗与「退出」按钮一致——停代理、清 secret、保留沙箱（避免留孤儿代理）。
- 依赖收敛：app 运行时依赖从 node + python3 减为 **仅 python3**（下一步同法移 axum 拔 python，见 [`docs/dependency-analysis.md`](docs/dependency-analysis.md)）。

### 安全 / 健壮性（外部复审修复）
- **[P1] 伪造器符号链接重定向**：去掉旧 `.sandbox/` 字符串启发式后，仅「==真实凭证目录」拦不住把 auth_dir 预置成指向其它目录的符号链接 → 恢复「沙箱根内」约束（`forge` 收 `sandbox_root=sandbox_home()`，解析后路径必须落其下），在写任何文件前拒绝逃逸。加回归测试 `forge_rejects_symlink_escaping_sandbox_root`。
- **[P2] 旧 `.enc` 删除失败被静默忽略**：原 `let _ = remove_file`，删不掉会残留多个 `.enc`（Science 预期恰好一个 → 显示启动成功却登录不上）→ 改为显式失败。
- **[P2] `http_proxy` 设了却不支持普通 HTTP 代理**：普通 HTTP 的 MCP/下载会撞代理拿 404 → 启动脚本改为**只设 `https_proxy`**（fast-fail 目标 `claude.ai/api/oauth/profile` 是 HTTPS），普通 HTTP 直连或走用户自己的代理。
- **[P3] 登录 shell 探测超时后泄漏子进程**：改 spawn + 轮询 + 超时 kill，病态 rc 卡死时终止 zsh。
- **[P2·评估后保留] CONNECT 不校验 path-secret**：代理只绑回环 + 隧道是裸 TCP 转发（不注入 key、不经推理端点），path-secret 守护的边界未被削弱，风险面小；已在代码注释记录该判断与「`Proxy-Authorization` 收紧」路径，待整链联调验证 operon 行为再定。

### 安全 / 健壮性（第二轮外部复审修复）
- **[P1] 模式切换只改配置、不切换进程** → `set_mode` 切到「官方」时**真正拆掉第三方链路**（停沙箱 Science + 杀代理 + 清 secret），做成名副其实的「切换」而非「选择模式」；也堵住 macOS 单实例下 `open` 误聚焦到还带着 `ANTHROPIC_*` 环境的沙箱实例。全程绝不碰真实 8765。
- **[P1] 伪造器信任根仍可被父级软链接带偏**：若 `~/.csswitch/sandbox` 被预置成指向真实 `~/.claude-science` 子树的符号链接，沙箱根自身会解析进真实树，令「沙箱根内」检查失效。**新增护栏 0（铁律最高优先，先于沙箱根检查）**：解析后的写入根绝不落在真实 Science 目录之内或本身，任何异常布局都绝不触碰真实目录。加回归测试 `forge_rejects_symlink_into_real_science_tree`（验真实目录零改动）。
- **[P2] 切换失败时 UI 谎报成功**：原「先翻 UI 再写配置、失败不回滚、错误提示又被后续普通提示覆盖」→ 改为**先落盘成功再翻 UI**，失败保持旧模式并如实报错、不被覆盖，切换期间禁用按钮，切成后刷新状态灯。
- **[P2] 关键 OAuth 实现未纳入 Git**：`oauth_forge.rs`（及对拍脚本 `test/decrypt-oauth.mjs`、CONNECT 测试 `test/test_proxy_connect.py`）此前是未跟踪文件，干净检出会因缺 `mod oauth_forge` 而构建失败 → 本次一并纳入版本控制。

## [0.1.2] — 2026-07-03

### 修复 Fixed
- 面板拖拽与端口相关修复。

### 新增 Added
- 存 API key 后用一条最小请求**真实验证一次 key**（200 可用 / 401·403 被拒）。
- 稳定复用的 path-secret（一次性鉴权令牌持久化，沙箱重连不失效）。
- 反馈 / 报 bug 基建（一键跳 GitHub issue 模板、打开日志目录自查）。

## [0.1.1] — 2026-07-03

### 新增 Added
- 一键越过登录面板（填 key → 一键起代理 + 沙箱 + 打开浏览器）。
- 检查更新（跳 GitHub Releases）。
- terracotta 暖橘品牌主题与图标集。

## [0.1.0] — 2026-07-03

### 新增 Added
- 首个公开版本。
- Tauri 菜单栏 app（进程管家）：管代理与沙箱两个子进程、读写 `~/.csswitch/config.json`、key 只注入环境变量、探活。
- provider 可切代理 `csswitch_proxy.py`：DeepSeek 原生 Anthropic 透传（默认）/ 通义千问 DashScope 翻译。
- 虚拟 OAuth 伪造器（本地假凭证越过 Science 的登录门票，零真实凭证）。
- 运维三件套 `doctor` / `verify-proxy` / `self-test` + 离线回归套件。
- 每日维护巡检（launchd 09:00/21:00，只读 + 规划，守铁律）。
