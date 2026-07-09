# 已知问题 / 待修队列

本文件记待修队列与近期排期。按用户反馈与自测记录，附根因与方向。

> **✅ 当前发布线（2026-07-09）：CHANGELOG 已到 `v0.3.6`。** 历史 `v0.3.0`（2026-07-04）发布了「多 API 支持 + UI 改版」，随后 `v0.3.2`–`v0.3.6` 继续补上模型选择器、自定义 OpenAI / Responses 等发布切片。发布事实以 [`../CHANGELOG.md`](../CHANGELOG.md) 与 GitHub Releases 为准。**长期剩余待办**：① issue #3 原生入口（打开 app 即自动起 Science + 后台常驻，spec 已写、待做）；② 翻译代理移 Rust/axum 拔 python；③ DSML 阶段二 rewrite 前 3 闸门（rewrite 仍默认 off）。

> **2026-07-09 重构收口注记（当前源码工作树）：** main 已在 #37-#40 后继续补强 packaging gate、architecture boundary、runtime/status/provider/science diagnostics、set-current vs discovery/pin 边界、doctor 默认不探真实 HOME、frontend config snapshot 命名和文档边界。已验证的是源码单元/脚本/代理契约与隔离 mock 路径；这**不等于**真实账号态、真实 `~/.claude-science`、Science GUI E2E、live provider E2E、published DMG、签名或公证验证。Phase 7/release 前仍需按 `test/run_all.sh --require-release-ready`、打包资源 smoke、必要真机手测分开确认。

> **排期（历史）**：**v0.2.1**（2026-07-03 登录热修：修 0.2.0 两处「一键开始走完仍落登录页」缺陷，见 #7 与 CHANGELOG）。此前 v0.2.0（#3 幂等 forge 毕业 + 三轮外审 + 阻止 #6 复发）。#6 待 #6b 做完历史恢复才真正修复毕业。

> **约定**：已修问题发布后从本文件「毕业」到 [`../CHANGELOG.md`](../CHANGELOG.md)。
> **v0.3.x 发布线**：`v0.3.0`（2026-07-04）发布多 API 支持 + UI 改版，后续 `v0.3.2`–`v0.3.6` 继续发布模型选择器、自定义 OpenAI / Responses 等切片；具体版本以 CHANGELOG 为准。此前 **v0.2.1**（2026-07-03）：登录热修（`sandbox_url` 多行 URL 只取首条 + 健康快捷路径先只读校验登录态），见 #7 / CHANGELOG `[0.2.1]`。此前 **v0.2.0**：#3 运行中再点主按钮幂等分派（幂等 forge：org_uuid 只在真首启铸一次、此后 sticky，健康沙箱不重伪造）+ 三轮 GPT 外审 + 阻止 #6 复发。更早 v0.1.5（#1 文案脱敏）、v0.1.4（node-ectomy、卡死实机修 403→401、正常窗口去托盘）见 CHANGELOG。

## 1. ✅ 对外文案脱敏（已随 v0.1.5 发布，已毕业）

> 主按钮及提示文案「一键越过登录」→「一键开始」，已随 **v0.1.5**（2026-07-03，当时发布线）发布。详情见 [`../CHANGELOG.md`](../CHANGELOG.md) 的 `[0.1.5]`。编号保留占位，避免与 #2–#5 混淆。
>
> **长期约定**：用户可见文案保持中性，别再引入「越过 / 绕过登录」这类字眼；技术内部文档描述机制时可仍用「越过门票 / 虚拟登录」。

## 2. DeepSeek 在 Science 里自检 `request_host_access` ❌ 被拒（路径不存在）

> **状态：新记录，待查（信息不全，需复现细节）。**

- **现象**：用户让 DeepSeek 在 Science 里跑自检，`request_host_access` 返回 **❌ 被拒（路径不存在）**。
- **背景推测（待验证，勿当定论）**：`request_host_access` 应是 Science 的一个能力/工具（申请访问宿主机某路径）。「路径不存在」说明它请求的路径在**沙箱环境**里不存在。候选方向：
  1. 沙箱 HOME 是 `~/.csswitch/sandbox/home`（独立于真实 HOME），Science 预期的工作目录 / 项目路径在沙箱里没被创建 → 申请的路径不存在。
  2. 该能力可能依赖真实授权 / 官方后端，虚拟登录下受限（类似远程 MCP 被 fast-fail 那种「虚拟登录用不了官方托管能力」）。
  3. DeepSeek 走**原生 Anthropic 透传**（非翻译），所以**不是翻译层的锅**，更可能是环境 / 路径 / 授权。
- **待收集（下次复现时）**：① 完整报错文本；② 它请求的**具体路径**是什么；③ 是否只有 DeepSeek 触发、还是任何 provider 都这样；④ 该自检具体是什么（Science 自带的环境自检？哪个 agent 发起？）。
- **不改代码**，先记录待查。

## 2a. Claude Science `0.1.15-dev` native 基线漂移（需隔离复核）

> **状态：只读基线已更新，未做真实账号态 / 真实 HOME / GUI E2E。**

- **当前本机安装版**：`/Applications/Claude Science.app` 仍是 `0.1.0-dev.20260630.t212931.sha2bc1ac8`。
- **本地缓存证据版**：`.science-binaries/README.md` 记录过 `0.1.15-dev.20260701.t220242.shaaa553de`；该缓存只作为本地证据，不随公开包发布。本轮未读取、未复制、未修改真实 `~/.claude-science`。
- **route diff 新增面**：`/api/auth/nonce`、`/api/auth/` 可能影响虚拟 OAuth / auth status；`/api/conda/conda-remote`、`/api/pypi/pypi-remote/simple`、conda mirror preference/probe 可能影响沙箱包源代理；`/api/frames/:id/token-series`、`/api/skills/:name/resync` 可能影响诊断或技能同步行为。
- **2026-07-09 追加（#15 auth refresh / logged-out）：** catalog/status diagnostics 已新增 `science.auth.refresh-hardcoded-0_1_15` 边界，用于解释 `0.1.15-dev.20260701.t220242.shaaa553de` 可能通过硬编码 `api.anthropic.com/oauth/token` 刷新 OAuth、从而把虚拟 OAuth 判为 logged-out 的版本风险。`status().science` 只暴露沙箱健康、虚拟 OAuth 边界和“status 轮询不跑二进制版本探测”的说明；这不是修复真实账号态，也不等于真实 HOME、GUI E2E、published DMG 或 live provider E2E 验证。
- **下一步**：在 `0.1.15-dev` 二进制可控时，用隔离 HOME + 非 `8765` 端口复跑虚拟 OAuth、沙箱启动、包源代理相关最小路径；不要从当前 `0.1.0-dev` 实测外推兼容结论。

## 3. ✅ 运行中再点主按钮的幂等分派（已随 v0.2.0 发布，已毕业）

> **状态：已随 v0.2.0（2026-07-03，当时发布线）发布并毕业到 [`../CHANGELOG.md`](../CHANGELOG.md) 的 `[0.2.0]`。** 幂等 forge 落地（org_uuid 只在真首启铸一次、此后 sticky，健康沙箱只重开不重伪造），实机 T1 已验证「会话不会丢」；同时阻止 #6 复发。详细设计与实现计划见本地开发文档（不入库）。原设计如下（历史留存）：
>
> 主按钮在「已在运行」时再点，行为要更合理。同时修 #6（更新后对话孤儿化，下游用户反馈）。

- **现状**：`one_click_login` 会 ① `ensure_proxy`（幂等：端口/provider/key 指纹一致且健康就复用）② **无条件重新伪造 OAuth**（往沙箱目录重写，哪怕 operon 正跑）③ 起沙箱脚本检测 daemon 已在跑 → 复用 ④ 再开一次浏览器。=> 能用，但往活着的沙箱重写登录文件多余且有风险，消息还像干了新活。
- **目标（按当前状态分派）**：

  | 当前状态 | 再点应做什么 |
  |---|---|
  | 代理 + 沙箱都健康、key 没变 | 不重启、不重伪造，只**重新打开 Science**，提示「已在运行，已重新打开」 |
  | key / provider 变了 | **只重启代理**（带新 key），operon 不动，提示「已用新 key 重启代理」 |
  | 单边挂（代理或沙箱） | 只补起挂掉的那个 |
  | 都没起（首次） | 走完整流程 |

- **两条核心**：① 沙箱已健康就**跳过重新伪造**；② 消息**说实话**（区分新起 vs 只是重新打开）。防连点已由前端 `setBusy` 覆盖。

## 4. 更广的 provider / API 支持（调研已封存，慢慢做）

> **状态：调研进行中被用户叫停、就地封存。** 已成文的部分见 [`provider-support.md`](provider-support.md)：核心分型（Anthropic 原生透传 vs OpenAI 兼容翻译）、Science 工具调用格式与翻译坑、CC Switch 参考（70 预设、65 直连 Anthropic）、国际 provider 表已并入。**缺口**：国产各家的 OpenAI 端点 / 模型细节大表（深挖 agent 中断）未并入，留待续做。

## 5. 用户反馈「request 有但 console 报错」（信息不全，沟通中）

- **现象**：console 显示 4 条报错。逐条分析多为**浏览器扩展噪声**（Adobe Acrobat 扩展 `efaidnbmnnnibpcajpcglclefindmkaj` 的慢网络/字体干预、PWA 横幅、扩展消息通道 `message channel closed`），**非 CSSwitch/Science 自身错误**。唯一有意义信号是「Slow network」，印证网络因素。
- **要问清**：① "request 有" 具体指什么、模型有没有正常回复；② 用无痕 / 禁扩展或按 `localhost:<port>` 过滤 console 复看，看有没有真正来自 Science 页面的错误。
- **状态**：信息不全，记录 + 沟通中，不改。

## 6. 更新 app 后 Science 对话「不见了」（高优先，下游用户反馈；已查证）

> **状态：「阻止复发」已随 v0.2.0（2026-07-03）发布（随 #3 落地：幂等 forge，从此不再新增孤儿 org，P1a 保证；实机 T1 已验证会话不丢）。但现存用户已孤儿化的旧对话本轮不找回 → #6 不毕业，待 #6b 做完历史恢复才算真正修复。**
>
> 原查证结论（历史留存）：根因已查证（只读追踪，未实机验证复原）。数据不丢，是活动组织漂移；根因与 #3 同源，修 #3 即修本条的「复发」部分。

- **现象**：用户更新 CSSwitch（v0.1.4 → v0.1.5）后重进 Science，之前的对话看不到了。
- **查证结论**：
  - **更新本身不删数据**。沙箱对话库在稳定用户目录 `~/.csswitch/sandbox/home/.claude-science/orgs/<org_uuid>/operon-cli.db`，路径只依赖 `$HOME`（`config.rs:86-91`、`lib.rs:108-110`），与 app 版本 / 安装路径无关；换 `.app` 不碰它。启动/停止脚本无 data-dir 级删除；虚拟 OAuth 重伪造只写 `encryption.key` / `active-org.json` / `.oauth-tokens/*.enc`，**绝不碰 `orgs/` 对话库**（`oauth_forge.rs:294-315`）。
  - **真正原因：每次点「一键开始」都无条件重伪造一个全新随机 org**（`oauth_forge.rs:270-271` 每次新 `org_uuid`，覆盖 `active-org.json` 见 `:326`；对应 #3 的「② 无条件重新伪造」）。活动组织一换，Science 打开的是新空组织，**旧对话被孤儿化**（仍在磁盘旧 `org_uuid` 目录下，界面看不到）。磁盘实证：沙箱里已累积 7 个 org 目录 / 7 个独立 DB，`active-org.json` 只指其一。
  - 与「更新」无因果：**每次一键都会发生**，只是用户更新后必然重新一键，主观体验成「更新后对话没了」。
- **修法 = 落地 #3**：#3 幂等分派设计里「沙箱已健康就跳过重新伪造」正是此病的解——沙箱活着就不重伪造、不换 org，旧对话一直在。**故 #3 从「顺延 UX」升级为兼修本高优先项。**
- **临时绕过（可告知用户）**：① 别点「一键开始」，若沙箱 daemon 还活着，直接浏览器开 `http://127.0.0.1:<沙箱端口>`，活动组织不变、旧对话都在；② 数据没丢，都在 `~/.csswitch/sandbox/home/.claude-science/orgs/`。
- **未验证（待实机）**：把 `active-org.json` 指回旧 org、并令牌 org_uuid 匹配，Science 是否就能重新加载旧历史（复原路径，超出只读范围，需实测）。

## 6b. 历史会话恢复（把某历史 org 指回 active-org 令旧对话重现）

> **状态：待设计，需实机。** #6 本轮只做了「阻止复发」（不再新增孤儿）；本条负责把**现存**用户已孤儿化的旧对话找回。#6 要做完 6b 才算真正修复、才毕业到 CHANGELOG。

- **背景**：#6 修复（幂等 forge）之前，每次一键都换 org，磁盘上已累积多个孤儿 org（沙箱实证 7 个）。`active-org.json` 只指其一，其余 org 里的旧对话界面看不到。
- **数据都在**：`~/.csswitch/sandbox/home/.claude-science/orgs/<org_uuid>/operon-cli.db`，未丢。
- **恢复思路（待实机验证）**：把想要的历史 `org_uuid` 写回 `active-org.json`，并令 `.oauth-tokens/*.enc` 里的 `org_uuid` 与之一致，看 Science 是否据此重载旧历史（= #6「未验证（待实机）」那条）。可能需要一个「历史会话」选择 UI（列出 `orgs/` 各库、让用户切换活动组织）。
- **本轮已埋雏形**：`ensure_virtual_login` 遇「多历史组织但无法定位活动者」会**报恢复错误**并提示用户手动把 `org_uuid` 写回 `active-org.json`——这就是 6b 的人工版最小形态。
- **优先级**：低于「下个主线」，但它是 #6 毕业的前置。

## 7. ✅「开了 CSSwitch 仍要登录」登录热修（已随 v0.2.1 发布，已毕业）

> **状态：已随 v0.2.1（2026-07-03，当时发布线）发布并毕业到 [`../CHANGELOG.md`](../CHANGELOG.md) 的 `[0.2.1]`。** 下游 0.2.0 用户反馈「开了 switch、一键开始走完仍要登录」。GPT 两轮诊断 + 对代码逐条核实，锁定 0.2.0 两处缺陷（走 systematic-debugging，test-first 红→绿修复）：
>
> - **Bug1 入口 URL 解析**：`claude-science url` 现输出多行（第一行真 URL + 第二行「single-use…」说明），`sandbox_url()`（`lib.rs`）把整段 stdout 当 URL 交 `open` → 换行/说明污染参数、单次性 nonce 未被正确消费 → 落 `/login`（**这条才是用户直接症状**）。修：新增纯函数 `first_http_url()` 只取第一条合法 `http(s)://` URL。
> - **Bug2 健康快捷路径绕过登录修复**：0.2.0 只要沙箱 daemon 活着就「连 auth 文件都不读」直接重开，导致旧版遗留 / 凭证损坏 / 已落登录页的健康 daemon 永不自愈（0.2.0 引入的分支）。修：健康分支先做**只读**校验 `login_intact`（复用既有 `read_intact_login` 自洽判定）；自洽→只重开（org 不动、旧对话不丢），失效→停沙箱、走 `ensure_virtual_login` 修复保 org + 重启。
>
> 各补一条离线回归测试（`first_http_url_*` / `login_intact_*`），`cargo test` 49 全绿、fmt CLEAN、`run_all.sh` ALL GREEN。**更新安全**已用全仓删除路径审计证明「升级不删会话」：无任何生产代码删除 `orgs/`（唯一生产删除=原子写临时文件 + 多余登录 `.enc`；Bug2 修法走保 org 的 `ensure_virtual_login`；stop/launch 脚本零删除）。符合 cc-switch「更新只换 app、不动 `~/.csswitch` 用户数据」的原则。

## 8. 「API 支持」重架方向：cc-switch 式多 profile 配置 + 代理移 Rust（2026-07-03 定，主线）

> **✅ 状态（2026-07-04）：已随 `v0.3.0` 正式版毕业。** 轨道 1（多 profile 配置管理）实现 + 四轮外审全修 + 真机验收（用户在场）通过 → 连同 relay + DSML + Slice A UX **merge 进 `main`（`c6e23bc`）并发进 `v0.3.0` 正式版（当时取代 v0.2.1）**。7 家 provider + 自定义端点已交付，原「② 面板内自定义 OpenAI 端点」并入本轨。**剩余唯一 = 轨道 2（翻译代理移 Rust/axum 拔 python），未做。** 过程记录（beta.1 撤回、beta.2 大预览版、逐轮外审、外部 relay PR #4 / WSL2 PR #7 待议）见 `findings/2026-07-04-codex-review-dsml-wiring-and-beta-status.md`。下方技术细节为历史留存。
>
> **GPT 二轮外审 + 修复（2026-07-03，改动只在 `desktop/src-tauri/src/lib.rs` + `desktop/src/main.js`，未 commit）**：复审多 profile 更新链 `45f3304..fdad9ea`。第一轮 5 项：P1-a `start_proxy`/`verify_key` 未过串行器（属实但两命令前端零调用=死命令、竞态不可达，已仍包串行器）、P1-b `set_mode` 与一键并发、P1-c 改代理端口后复用旧沙箱指向死端口、P2-d 非 active 连接编辑不校验、P2-e 回滚失败仍谎称已回滚，**无一触碰铁律**（最坏=孤儿第三方沙箱），全修。第二轮增量 2 项：P1 端口拆链 `let _ = stop_sandbox_inner` 丢结果+先落盘 → 改为**停成功才落盘/停失败返 Err 且端口不变**+前端如实提示；P2 非 active 校验用户选 **truthful-save**（只拦明确 401/403/400/404/422，native/429/5xx/无响应仍保存但据实标「未校验，激活再验」，`update_profile_connection` 回传 `{validated}`）。新增纯函数 `settings_change_needs_teardown`/`rollback_status_clause`/`nonactive_probe_verdict`（均 TDD）。**cargo test 104→107、clippy 0、fmt、node --check、run_all ALL GREEN、gitleaks 0。**
>
> **GPT 真机验收 + P1/P2 修复（2026-07-03，改动只在 `desktop/src-tauri/src/{lib.rs,scratch.rs}`，未 commit）**：GPT 用独立 HOME / 独立 `~/.csswitch` / 独立 Science data-dir / 测试端口跑当前工作树整链 18 条矩阵（真实 8765 仅 `lsof` 比对 PID、全程不变、不读改 `~/.claude-science`；护栏 = `test/real_machine_guard.sh` + `test/REAL_MACHINE_TEST.md` + `test/tauri.real-machine.conf.json`，独立 bundle id `com.csswitch.acceptance` 防误测旧窗口），报告 `findings/2026-07-03-real-machine-acceptance.md`。发现 1 个发布阻断 **P1**：deepseek/qwen 归为 native adapter 后，`set_active_profile_txn` 对 native 直接 `scratch_ok=true` 跳过上游校验，而 `real_healthy` 只探本地代理 `/health`（`csswitch_proxy.py` 的 `/health` 恒回 200、绝不验 key），于是无效 native key 也被提交为 active、UI 谎报「已切到」、旧可用代理已被换掉，首个真实推理才 401，违背「上游健康才提交 / 失败回滚」核心承诺（连带 P2-d 的「激活再验」承诺落空）。**修复**：native 也走 scratch 上游探测（`--provider deepseek/qwen` + 原生 `*_API_KEY` env，`scratch_probe` 由 relay-only 泛化为 `ScratchTarget`）；native 的 `/v1/models` 是静态列表探不出坏 key，故 native 一律用 **Message 探测**打 `/v1/messages` 触上游（坏 key→401→拦下不提交、不换代理、active 不动）；只有显式 `skip_verify` 才跳过。附带修 **P2**（次要）：本地探活超时报错不再含糊说「或 key 无效」，按日志区分端口占用 vs 依赖/脚本异常。新增纯函数 `skip_scratch_verify` / `should_scratch_candidate` / `probe_kind_for` / `scratch::scratch_env` / `health_timeout_reason`（均 TDD）。**cargo test 107→113、clippy 0、fmt、node --check、run_all ALL GREEN。**
>
> **GPT 三轮外审 + 3 项修复（2026-07-03，改动在 `test/real_machine_guard.sh` + `desktop/src/main.js`，未 commit）**：① **P1 护栏隔离**：真机护栏若隔离目录/文件被预置软链指向真实目标，会经软链覆写真实文件（如 `~/.csswitch/config.json`）。修法分两轮：(a) `preflight`/`prepare-legacy` 加 `reject_symlinks`（拒绝掌控目录 `TEST_ROOT`/`TEST_HOME`/`STATE_DIR`/`.csswitch` 被预置成软链）+ `assert_isolated_from_real_home`（canonicalize 后拒绝落在真实 HOME 内）；(b) 四轮外审又指出仅拒目录不够（`config.json` 文件本身、以及 `STATE_DIR` 里的 `port-8765.{pids,now}` 若是软链，`>` 仍会跟随覆写真实文件），改为**三处写盘全部走 `write_fresh`**（先 `rm -f` 删掉软链本身、再从 stdin 写全新普通文件，杜绝跟随预置软链）。**表述订正**：这消除的是「跟随预置软链」，非原子、不宣称消除检查与写入间的并发竞态。自验时另发现并修掉一处 `$canon（` 紧贴全角括号触发 `set -u` unbound 的 shell 坑（改 `${canon}`）。软链拒绝与「不覆写真实 config/sacred 文件」已用伪 HOME 场景实测通过。② **P2 前端矛盾提示**：回滚失败时后端已说「代理当前已停」，前端却盲目追加「仍在用原配置运行」自相矛盾，改为只显示后端如实文案。③ **P2 `persistPorts` 未置忙**：开头 `if (busy) return` 只挡「已忙时进入」，挡不住本函数在途时其它操作启动，补 `setBusy(true)` + `finally setBusy(false)`。**node --check、git diff --check、bash -n、cargo test 113、run_all ALL GREEN。** 护栏软链拒绝已用伪 HOME 场景实测（软链/落在真实 HOME 内都 die、正常外部目录放行）。下一步 = 用户真机复测 RM-06（无效 native key 必须被拦、active 与旧代理不变）/ RM-04（非 active native 编辑即时校验）/ RM-13（端口占用报错措辞）→ 再谈提交与 merge。
>
> **轨道 1 已落地**（`feat/relay-presets` 分支，HEAD `fdad9ea`，同分支重构掉固定槽）：spec v2（折进 Codex 外审 P1×6+P2）→ 计划 → subagent-driven 实现：**MP-1 后端**（`ProviderCfg`→`Profile`；`relay_presets.rs`→`templates.rs` 7 家 `template_id`→adapter 注册表，deepseek 保留模型映射/relay 透传双鉴权/qwen 翻译；v1→v2 迁移不丢数据 + `.v1.bak` 失败即中止 + `schema>2` 拒启 + 幂等；profile CRUD 命令面）+ **MP-2 后端**（生命周期串行器 + 切换事务：scratch 校验→起正式代理→探活健康**才**提交→失败杀候选恢复旧代理→**不停沙箱**→generation token；连接编辑 validate-before-persist）+ **前端 Phase C**（面板重写为 profile 列表 UI，对接新命令面）。whole-feature 终审 Approved（M1 `get_config` 补 notes、M2 切换写盘失败回滚进程 已修）。**全绿**：cargo test 104、clippy -D warnings 0、fmt、node --check、`run_all.sh` ALL GREEN、gitleaks 0。durable 账本见 `.superpowers/sdd/progress.md`「多 profile 重架」段（含累积 Minor triage）。**坑**：本项目 Tauri(2.6.3, 无 rename_all) 顶层多词命令参数是 **camelCase**（templateId/skipVerify），serde struct 包装参内字段才蛇形。

- **要做什么（用户 2026-07-03）**：像 cc-switch 那样能**存多套命名配置（profile）**、列出来、一键切当前生效的那套（同一家可存多套、可命名、可增删）。现在是**固定槽**（deepseek/qwen/relay-glm/…每家一份），做不到多套 → 数据模型从「固定槽」升级为「用户自管的 profile 列表 + 当前生效指针」。
- **配置存储：先不换 SQLite（用户要「保持稳定」）。** 多 profile 只是数据模型（JSON 存一个数组即可），跟存储后端解耦。这版继续 JSON、把它硬化（原子写已有 + schema 版本字段 + 覆盖前留 `.bak` + 修面板回显可见性缺口）；SQLite 留到确有扩展需求（多窗口并发 / 大量记录 / 历史）时再迁，届时 JSON→SQLite 迁移很简单。SQLite 价值在可扩展+并发，不在「更稳」。
- **代理移 Rust（独立轨道）**：翻译代理从 python 移到 Rust（axum），**vendor cc-switch 的 MIT `transform*.rs`** 拿广覆盖（4 种 apiFormat），加我们的 path-secret + 虚拟 OAuth 剥离。cc-switch 代理**不能当 sidecar 直接复用**（焊死它的 SQLite DB，见 `verified-facts.md` 事实 6），复用 = 移植它的翻译模块。这条同时完成 python-ectomy 治本，与「多 profile 配置」解耦。
- **relay-presets 分支现状**：`feat/relay-presets`（Task1-13 全实现 + opus 终审过）实现了 relay provider + 预设 + 面板选模型，但用**固定槽 + python 代理**，**降为参考/回退，不作为发布基座**（重架后被多 profile + Rust 代理取代）。HEAD `2a5084f`（f148eb2 + P3 hygiene 修）**clippy-green**。GPT 外审逐条核实：P1「保存写错槽+覆盖」= 尖端已修（STALE）；P1b「保存非原子」+ P2a「自愈忽略停沙箱失败 `lib.rs:967`」= 真 Important、折进重架设计（P2a 应像 `set_mode` 停失败即中止）；P2b「python 代理 OSError 全当占用」= 真但代理要换掉；P3「clippy 2 处 + 版本不一致」= 已修（`2a5084f`）。**教训**：验收闸门要含 `cargo clippy --all-targets -D warnings`（比 `cargo test` 的 rustc 警告更严；账本旧称「0 warnings」漏了 clippy）。
- **隔离层四家实测证据**：`findings/2026-07-03-pr4-relay-provider-testing.md`（GLM/小米/硅基流动/OpenRouter 真机实测，含工具，守铁律4 未启 Science），已随 `c8b9cfe` 提交。
- **下一步**：① **用户在场做真机整链手测**（构建 + 启动 app，逐项过：迁移后旧对话在不在 / 新建 / 切换 / 一键开始 / 连接编辑 / 清 key / 删除；起沙箱 Science + 登录那步由用户手动，守沙箱隔离、凭证隔离和端口隔离）；② 过了再谈 merge（现未 push、main 未动）+ 更新 `CHANGELOG.md`（**故意还没写**，避免仓库里提前显「已完成」）；③ 轨道 2 = 代理移 Rust（另起 spec）。spec/计划在 `docs/superpowers/`（gitignore）。

---

**下个主线（用户 2026-07-03 定，两条）**：
- **① 打开 app 即自动起 Science + 后台常驻（issue #3 原生入口 + 配置可见）**：用户 0.2.1 实机时又主动提「一键开始要不要加自动拉起 Science 的逻辑」——正是 issue #3。澄清：`一键开始` 内部**已经**在起沙箱 Science，用户真正要的是「**开 app 就自动起、退后台**」这个触发方式（不再手动点）。设计已成文并锁定：**本地开发文档**（gitignore）`docs/superpowers/specs/2026-07-03-native-entry-and-config-visibility-design.md`，**两轮 GPT 外审已折进、四决策已锁定**（⌘, 菜单 / 关窗≠退出 / 已保存条+打勾 / 存时验启动只查非空）+ 第二轮外审（清 key 按运行中 provider 判、`validate_and_save` 事务化、生命周期串行器 + generation token、清除确认）。**尚未写实现计划、尚未写代码。** 切三片：**A** 配置可见 + 清 key 运行态撤销 + `validate_and_save` 事务化 + 串行器最小核心；**B** boot 协调器 + 后台常驻 + 单实例 + ⌘, 菜单 + 关窗≠退出 + `visible=false`（自动起就在这片）；**C** WKWebView spike 门控的 app 窗口。轻量版（启动钩子后台调 `one_click_login`）可先尝鲜。**下一步 = 用户复审 spec → `superpowers:writing-plans`（A/B/C 一份计划、A→B→C 执行验收）。**
- **② 「API 支持」重架（已升级为 #8，2026-07-03）**：原「面板内自定义 OpenAI 端点」升级为 cc-switch 式**多 profile 配置管理**（存多套命名配置 + 一键切）+ 代理移 Rust（vendor cc-switch MIT 翻译模块）。配置层先 JSON 硬化、SQLite 缓议。详见 **#8**。

**其它 roadmap（非 bug）**：
- **python-ectomy**：翻译代理移到 Rust（axum），拔掉 python（node 已在 v0.1.4 拔除），最终零外部运行时。**落地方式（2026-07-03 定）= vendor cc-switch 的 MIT `transform*.rs` 拿广覆盖**（见 #8、`verified-facts.md` 事实 6）。
- Intel（x86_64）/ universal 构建；可选正式签名 + Apple 公证。

---

## 9. 模型选择器显示 claude / opus（用户 2026-07-04 批次，⭐高优先：先修再发 v0.3.2）

> **状态：已实现 + 真机验证通过 + 已随 v0.3.2 发布。** 真机证据：沙箱 Science 顶部选择器显示真实模型名 `glm-5.2`（非 claude/opus），代理 `/v1/models` 在 force 时返回单壳 `{id:claude-opus-4-8, display_name:真实名}`；铁律全程守住（8765 与真实目录未碰）。对应用户 bug 清单 #11（智谱等显示 claude）+ #12 显示部分（自定义显示 opus）。**#12 的校验部分（自定义 scratch 探测误判 Ambiguous）仍待复现，未在本轮解决。**
>
> **2026-07-08 追加（#26 自定义 OpenAI / Responses 选择器空列表）：** 当前 main 已有本地隔离 proxy/mock 证据，正式 `openai-custom` 与 `openai-responses` profile 在已配置模型时，`/{secret}/v1/models` 只返回单壳 `claude-opus-4-8`，真实模型名放 `display_name`，且 force 模式不回源暴露第三方原始模型 id；见 [`findings/2026-07-08-issue-26-custom-selector-evidence.md`](../findings/2026-07-08-issue-26-custom-selector-evidence.md)。随后又用隔离 Science UI + mock/custom OpenAI 上游验证 `openai-custom` selector：菜单非空并显示/选中 `glm-4.5`；见 [`findings/2026-07-08-issue-26-science-ui-evidence.md`](../findings/2026-07-08-issue-26-science-ui-evidence.md)。这仍不是真实账号态、真实 HOME、published DMG、签名/公证或 live provider E2E 验证；关闭 #26 时应按“当前 main 源码代理契约 + 隔离 mock/UI 证据”表述。
>
> **修复摘要（对齐 deepseek 借壳 + cc-switch 自填范式，spec/plan 见本地 `docs/superpowers/`，git 忽略）**：① 层二＝代理 `build_models_response` 在 `RELAY_FORCE_MODEL` 设时返回单壳（`claude-opus-4-8` + `display_name`＝真实模型名），出站由 `resolve_model` 的 force 分支覆盖、零改动；② 层一＝全 relay 统一 FIXED（GLM/OpenRouter 也 `requires_model_override=true`），模型控件从 `<select>` 换成 `<input list>+<datalist>`（下拉精选＋自填兜底，`custom` 终于能填模型名）；③ 各家 `builtin_models` 官方核定（GLM→`glm-5.2`、MiniMax→`MiniMax-M3`、硅基→`DeepSeek-V4-Pro` 等，硅基 Anthropic `/v1/messages` 真机 200）；④ 后端 `relay_missing_model` 守卫接 create/edit/activate（不变量不可绕过）；⑤ 存量空 model 的 relay 加载时回填模板默认＋一次性提示（甲迁移）；⑥ 模型名 trim 规范化。原设计如下（历史留存）：

- **现象**：① 智谱 GLM 等 relay 家在 Science 顶部模型选择器里「显示的是 claude」；② 自定义（填 claude 中转 API）「显示的是 opus」；用户说「自定义的模型也有问题」。
- **根因（`proxy/csswitch_proxy.py` PROVIDERS 注释已证实，逆向标记 s0/ZjO/XjO/hB_）**：Science 模型面板有**二进制写死的两道硬规则**——① 可选模型 id **必须 `claude-` 开头**；② 只有 `claude-{opus|sonnet|haiku}-<纯数字版本>` 进**主列表**（每 family 留一个），其余进「More models」折叠。**CSSwitch 改不了 Science 这个 UI**，只能控制代理 `/v1/models` 返回什么。
  - **deepseek（native）**：`models` **借壳** `claude-opus-4-8`/`claude-haiku-4-5` + `display_name`「DeepSeek V4 Pro/Flash」，`model_map` 出站还原真实 id → Science 显示真实名，正常。
  - **relay + `requires_model_override=false`（GLM/openrouter）**：`passthrough=True`，`fetch_relay_models` 回源拉**真实 id**（`glm-4.6` 非 claude-）→ **被 Science 硬规则过滤** → 选择器回退显示 claude 默认。= **#11 根因**。
  - **relay + `requires_model_override=true`（小米/硅基/kimi/minimax/自定义）**：面板选了模型→`RELAY_FORCE_MODEL` override 实际走选中模型，但 Science 选择器**仍显示 claude**；自定义 `default_model="claude-opus-4-8"` → **#12「显示 opus」**。
- **修复方案（待定，倾向 A+B 结合，对齐 deepseek 成熟做法）**：让 relay 也**借壳**——`fetch_relay_models`/`build_models_response` 给回源真实模型分配 `claude-{family}-<数字>` 壳 id（进主列表）+ `display_name=真实模型名`，出站用动态 `model_map`（壳→真实 id）还原。
  - **A**：全 relay 回源模型借壳；主列表每 family 限 1（最多 3 个真实名进主列表，其余 More models）——多模型的 GLM/openrouter 需要。
  - **B**：`requires_model_override=true` 的家只借**选中的那一个**模型壳（简单，kimi/minimax/小米/硅基/自定义适用）。
  - **C（治标）**：不改协议，只在 CSSwitch app 文案说明「Science 顶部显示 claude 是外壳，实际走你选的模型」。
  - **坑**：壳 id 要稳定、每 family 分配、动态生成；现 `passthrough` 不映射，借壳后出站 `model_map` 还原要与 passthrough 协同。需一份 spec；代理 `/v1/models` 返回可不启 Science 单验，整链看 Science 显示须用户在场。

## 10. Kimi / MiniMax 需不需要像千问/DS 那样「翻译」？→ 不需要，已做（原生透传）

> **结论（可直接回用户）**：**不需要翻译，已经做了。**

- 「翻译」（Anthropic ↔ OpenAI 互转，`_handle_openai` + `anthropic_to_openai`）**只对 `mode:"openai"` 的千问（qwen）**：DashScope 只提供 OpenAI 兼容端点，故必须转。
- **deepseek** 是 `mode:"anthropic"` 原生透传、不翻译。
- **Kimi / MiniMax 是 relay = `mode:"anthropic"`**，官方提供 `/anthropic` 兼容端点，走原生 Anthropic 透传（`_handle_anthropic`），**零翻译**。真机已证：两家 `/v1/messages` 直接透传 200（thinking 各自策略见 v0.3.2 实现：Kimi=enabled / MiniMax=adaptive）。

## 11. 用户反馈 bug 清单（2026-07-04 批次，源 `~/Desktop/已知bug/已知bug.md`，12 条分诊）

> **本文件（`docs/known-issues.md`）即「你我都可改的共享追踪文档」**（git 跟踪，你我都能编辑）。以下把用户新清单 12 条映射到根因/已有条目，后续在此更新处置。

| # | 现象 | 分诊 / 归属 |
|---|---|---|
| 1 | Claude 不可用重试；用 ds api，客户说不开梯子就报错 | 待辨：纯文本推理是否也需梯子（ds 国内直连，Science 静态资源/校验可能仍触外网）。需复现 |
| 2 | **最严重**：websearch 拉下来没法输出就中断 | 模型端 DSML 泄漏（tool_use 吐成文本）→ 见 DSML shim 轨道；ds 透传把工具调用漏成 `<｜｜DSML｜｜>` 卡死 |
| 3 | 国内反代中转站，回不了；"session no longer valid" | 架构边界：claude.ai 服务端功能被 401 隔离（虚拟登录）。非代理 bug |
| 4 | 挂 csswitch 能登进去，但无输出/一晃而过（多人报告） | 疑可修代理 bug，需 proxy.log 复现 |
| 5 | nature skills / GitHub 第三方插件装不了 | 架构边界：claude.ai 托管能力被 401 隔离 |
| 6 | DSML tool_calls 泄漏成文本（截图） | 同 #2，模型端 DSML |
| 7 | Artifact failed（截图） | 待辨（信息不全） |
| 8 | Directory connectors unavailable / session expired | 架构边界：目录连接器是 claude.ai 服务端功能 |
| 9 | 弹 sign in 但点一下直接进（没真登录） | 架构边界/预期：虚拟登录表现为"未登录门票" |
| 10 | 截图（待补） | 待辨 |
| **11** | **模型选择器显示不正确**（小米/智谱/openrouter/硅基/kimi/minimax 都有） | **✅ 已修（见 #9，真机验证 `glm-5.2` 正确显示，已随 v0.3.2 发布）** |
| **12** | **自定义 API「无法确认(网络/上游繁忙)，未切换，可重试或跳过验证」**；用户说 curl 没问题 | 显示 opus 部分 **✅ 已修（→ #9：custom 现可自填模型名 + 借壳显示真实名）**；校验部分（自定义 relay 的 scratch 探测把可用端点误判为 Ambiguous）**仍需复现**：拿用户的自定义 base_url/流程，看 scratch Message/Models 探测状态码为何非 200；curl 通但代理探测不通 = 探测路径/鉴权头/thinking 注入差异 |

- **三大根因归类**（沿用 2026-07-03 分诊，`findings/2026-07-03-user-reported-bugs-triage.md`）：① 架构边界（claude.ai 服务端功能被 401 隔离）=#3/#5/#8/#9；② 模型端（DeepSeek 透传把 tool_use 吐成文本卡死）=#2/#6 最严重；③ 疑可修代理 bug=#4（输出一晃而过）/#12（自定义校验）/#1（梯子）。
- **下一步优先级（2026-07-09 更新）**：~~先修 #9 模型选择器~~ **✅ #9 已实现 + 真机验证 + 已随 v0.3.2 发布**。剩：① #12 校验部分（自定义 scratch 误判）待复现；② DSML 阶段二 rewrite；③ 原生入口 / 后台常驻与 python-ectomy 仍按 roadmap 另线推进。
