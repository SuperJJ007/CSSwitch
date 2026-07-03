# 已知问题 / 待修队列

本文件记待修队列与近期排期。按用户反馈与自测记录，附根因与方向。

> **排期（用户 2026-07-03）**：**v0.2.1 已发布**（Latest，登录热修：修 0.2.0 两处「一键开始走完仍落登录页」缺陷，见 #7 与 CHANGELOG）。此前 v0.2.0（#3 幂等 forge 毕业 + 三轮外审 + 阻止 #6 复发）。#6 待 #6b 做完历史恢复才真正修复毕业。**下个主线（两条，见文末）** = ① 打开 app 即自动起 Science + 后台常驻（issue #3 原生入口，spec 已写、两轮外审已折入、待复审）；② 面板内自定义 OpenAI 兼容端点。

> **约定**：已修问题发布后从本文件「毕业」到 [`../CHANGELOG.md`](../CHANGELOG.md)。
> **v0.2.1 已发布**（2026-07-03，Latest）：登录热修（`sandbox_url` 多行 URL 只取首条 + 健康快捷路径先只读校验登录态），见 #7 / CHANGELOG `[0.2.1]`。此前 **v0.2.0**：#3 运行中再点主按钮幂等分派（幂等 forge：org_uuid 只在真首启铸一次、此后 sticky，健康沙箱不重伪造）+ 三轮 GPT 外审 + 阻止 #6 复发。更早 v0.1.5（#1 文案脱敏）、v0.1.4（node-ectomy、卡死实机修 403→401、正常窗口去托盘）见 CHANGELOG。

## 1. ✅ 对外文案脱敏（已随 v0.1.5 发布，已毕业）

> 主按钮及提示文案「一键越过登录」→「一键开始」，已随 **v0.1.5**（2026-07-03，Latest）发布。详情见 [`../CHANGELOG.md`](../CHANGELOG.md) 的 `[0.1.5]`。编号保留占位，避免与 #2–#5 混淆。
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

## 3. ✅ 运行中再点主按钮的幂等分派（已随 v0.2.0 发布，已毕业）

> **状态：已随 v0.2.0（2026-07-03，Latest）发布并毕业到 [`../CHANGELOG.md`](../CHANGELOG.md) 的 `[0.2.0]`。** 幂等 forge 落地（org_uuid 只在真首启铸一次、此后 sticky，健康沙箱只重开不重伪造），实机 T1 已验证「会话不会丢」；同时阻止 #6 复发。详细设计与实现计划见本地开发文档（不入库）。原设计如下（历史留存）：
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

> **状态：已随 v0.2.1（2026-07-03，Latest）发布并毕业到 [`../CHANGELOG.md`](../CHANGELOG.md) 的 `[0.2.1]`。** 下游 0.2.0 用户反馈「开了 switch、一键开始走完仍要登录」。GPT 两轮诊断 + 对代码逐条核实，锁定 0.2.0 两处缺陷（走 systematic-debugging，test-first 红→绿修复）：
>
> - **Bug1 入口 URL 解析**：`claude-science url` 现输出多行（第一行真 URL + 第二行「single-use…」说明），`sandbox_url()`（`lib.rs`）把整段 stdout 当 URL 交 `open` → 换行/说明污染参数、单次性 nonce 未被正确消费 → 落 `/login`（**这条才是用户直接症状**）。修：新增纯函数 `first_http_url()` 只取第一条合法 `http(s)://` URL。
> - **Bug2 健康快捷路径绕过登录修复**：0.2.0 只要沙箱 daemon 活着就「连 auth 文件都不读」直接重开，导致旧版遗留 / 凭证损坏 / 已落登录页的健康 daemon 永不自愈（0.2.0 引入的分支）。修：健康分支先做**只读**校验 `login_intact`（复用既有 `read_intact_login` 自洽判定）；自洽→只重开（org 不动、旧对话不丢），失效→停沙箱、走 `ensure_virtual_login` 修复保 org + 重启。
>
> 各补一条离线回归测试（`first_http_url_*` / `login_intact_*`），`cargo test` 49 全绿、fmt CLEAN、`run_all.sh` ALL GREEN。**更新安全**已用全仓删除路径审计证明「升级不删会话」：无任何生产代码删除 `orgs/`（唯一生产删除=原子写临时文件 + 多余登录 `.enc`；Bug2 修法走保 org 的 `ensure_virtual_login`；stop/launch 脚本零删除）。符合 cc-switch「更新只换 app、不动 `~/.csswitch` 用户数据」的原则。

---

**下个主线（用户 2026-07-03 定，两条）**：
- **① 打开 app 即自动起 Science + 后台常驻（issue #3 原生入口 + 配置可见）**：用户 0.2.1 实机时又主动提「一键开始要不要加自动拉起 Science 的逻辑」——正是 issue #3。澄清：`一键开始` 内部**已经**在起沙箱 Science，用户真正要的是「**开 app 就自动起、退后台**」这个触发方式（不再手动点）。设计已成文并锁定：**本地开发文档**（gitignore）`docs/superpowers/specs/2026-07-03-native-entry-and-config-visibility-design.md`，**两轮 GPT 外审已折进、四决策已锁定**（⌘, 菜单 / 关窗≠退出 / 已保存条+打勾 / 存时验启动只查非空）+ 第二轮外审（清 key 按运行中 provider 判、`validate_and_save` 事务化、生命周期串行器 + generation token、清除确认）。**尚未写实现计划、尚未写代码。** 切三片：**A** 配置可见 + 清 key 运行态撤销 + `validate_and_save` 事务化 + 串行器最小核心；**B** boot 协调器 + 后台常驻 + 单实例 + ⌘, 菜单 + 关窗≠退出 + `visible=false`（自动起就在这片）；**C** WKWebView spike 门控的 app 窗口。轻量版（启动钩子后台调 `one_click_login`）可先尝鲜。**下一步 = 用户复审 spec → `superpowers:writing-plans`（A/B/C 一份计划、A→B→C 执行验收）。**
- **② 面板内自定义 OpenAI 兼容端点**：面板里配 base_url / 模型名 / 鉴权头，无需改代码即接任意 OpenAI 兼容上游。它是第 4 条 provider 研究（[`provider-support.md`](provider-support.md)）的**可落地产品化切片**。开工前先走一轮 brainstorming + 设计。

**其它 roadmap（非 bug）**：
- **python-ectomy**：翻译代理移到 Rust（axum），拔掉 python（node 已在 v0.1.4 拔除），最终零外部运行时。
- Intel（x86_64）/ universal 构建；可选正式签名 + Apple 公证。
