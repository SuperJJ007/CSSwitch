# v0.7.0 新 UI 未提交版本总审查

## 结论

`codex/v070-ui-redesign@bd1f3ab` 的当前未提交工作树已完成五模块“主审 → `gpt-5.6-sol`/`xhigh` 复审 → 修改 → 再复审”。截至 2026-07-18，API/provider、Codex、Skill/MCP、runtime/安全、UI/打包均无 P0–P2 遗留阻塞；全量隔离门禁五层全绿。隔离 Acceptance 的真实点击先后暴露了手动打开复用一次性 URL 的 P1，以及手工 Acceptance 误用测试 PATH opener 的安装态漏审；两项均已修正并重新覆盖安装 Acceptance。最终包仍待一次新的人工 Chrome 弹窗确认，且不能据此宣称正式 App bundle、真实账号/provider、Developer ID 签名公证或公开发布已经验证。

## 模块结果

| 模块 | 修正重点 | 终态 |
| --- | --- | --- |
| [API / Provider](2026-07-18-v070-ui-redesign-api-provider-review.md) | provider 适用性、三态/四态聚合、官方模式不伪绿 | 无 P0–P3 |
| [Codex](2026-07-18-v070-ui-redesign-codex-review.md) | Codex upstream 不适用、动态目录说明、当前 IA | 无 P0–P3 |
| [Skill / MCP](2026-07-18-v070-ui-redesign-skill-mcp-review.md) | readiness、org race/在途请求、焦点、上限、marker | 无 P0–P2；1 项理论 P3 记录在案 |
| [runtime / 安全](2026-07-18-v070-ui-redesign-runtime-security-review.md) | fd 绑定、有界读取、完整快照丢弃、测试隔离 | 无 P0–P2；同一理论 P3 记录在案 |
| [UI / 打包](2026-07-18-v070-ui-redesign-ui-packaging-review.md) | 搜索/抽屉/主题、窗口合同、一次性 URL 重取、浏览器反馈、资源边界 | 无 P0–P3 |

这里的 P3 是同一项父目录逐段解析的理论 TOCTOU 边界，不是两个独立问题。当前最终对象已使用 `O_NOFOLLOW`、类型/权限/私有根检查并 fail closed；在没有不受信任方可写父目录的当前威胁模型下，暂不引入完整 fd-relative 路径框架。

## 关键产品判断

- 采用成熟 provider 管理产品的可解释状态分层，但不把 CSSwitch 扩成全功能 provider 市场；参照 [CC Switch](https://github.com/farion1231/cc-switch) 的 provider 编辑与路由合同。
- Codex 保留本仓库已有严格 OAuth、动态 catalog、Responses reducer 与 transport 边界；[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) 只用于核对成熟路由形态，没有替换现有协议栈。
- Skill/MCP UI 只展示真实已实现能力。调研的 [Anthropic Skills](https://github.com/anthropics/skills)、[superpowers](https://github.com/obra/superpowers)、[MCP Registry](https://github.com/modelcontextprotocol/registry) 和 [Claude Code plugins](https://github.com/anthropics/claude-code/blob/main/plugins/README.md) 支持“来源、组织/绑定、安装进度与能力边界分开表达”的方向；参考 servers 不等于生产可信源。
- 安全修正集中在真实攻击面和资源边界，没有为理论风险引入大而新的抽象层。

## 最终复证

最终一次 `bash test/run_all.sh --require-release-ready` 在允许隔离 loopback 的环境退出 0：

- offline：pass；
- loopback：82/82，pass；
- scripts：guards、doctor、verify-proxy、real-machine guard 均 pass；
- Rust：Desktop 318 passed / 3 explicit ignored，Gateway 228 library + 1 CLI passed；
- frontend：Codex 3/3、Skill 9/9、runtime status 3/3，pass；
- 汇总：`current-env clean: YES`、`release-ready green: YES`。

补充定向检查：Skill package 55 passed / 4 explicit ignored、system route 10/10、core listing 默认并发 9/9、Tauri Skill listing 4/4、provider 17/17、gateway provider 2/2、diagnostics 8/8、Codex browser auth 8/8、Python UI/runtime boundary 9/9、`cargo check`、本地 `cargo build --release`、格式检查与 `git diff --check` 均通过。

Acceptance 追补后再次运行同一完整门禁，仍为五层全绿、`release-ready green: YES`。另有 Python UI/runtime boundary 10/10、installed-provider controller 21/21、Acceptance-feature `cargo check` 和显式 fake Science/open nonce smoke 通过。该显式 smoke 保持默认 ignored，不混入自动门禁计数，但本轮单独执行并退出 0。

最终修复后的 `/Applications/CSSwitch Acceptance.app` 已覆盖安装并完成 bundle id/版本、arm64、ad-hoc strict codesign 与构建/安装二进制哈希一致性检查；desktop SHA-256 为 `1edd60f3e4c52672ec888285c6752257b6ed714d11b106474fdd47f7ed89f1df`，gateway 为 `53444d1712ab7bb798d1cb67acb2942bbdb288e8242b9ab972a1fb47127dc270`。正式 `/Applications/CSSwitch.app` 两个哈希保持不变。覆盖后尚未完成新包的人工 Chrome 弹窗确认，因此不把安装副本写成已经完成点击/runtime 验收；上一版 Acceptance 暂存在 `/private/tmp/CSSwitch Acceptance.before-browser-open-fix-20260718.app` 供确认前回滚。

## 明确未验证

- 未启动或修改正式 App，未生成/安装/打开正式 `.app` 或 DMG；只生成并覆盖安装了隔离 bundle id/data root 的 Acceptance `.app`。
- 未读取、写入或导出真实凭据、Keychain、真实 `~/.csswitch/skills` 或 Science 数据。
- 未做真实 Codex OAuth/推理、真实 provider 调用、真实 MCP 服务或第三方 bundle 安装。
- 未验证 Developer ID 签名、公证、Gatekeeper、发布附件与公开 release；Acceptance 仅验证了 ad-hoc strict codesign。
- ignored 的隔离 runtime/外部 smoke 仍保持 ignored，不计入自动通过。

## 总审

同规格 `gpt-5.6-sol`、`xhigh` 子 agent 对 exact dirty tree、五份模块报告和本总报告做了最终交叉核对。它确认代码与测试没有 P0–P2，末轮六项修正均闭环；同时发现两组报告措辞把当前信息架构写反、把修前琥珀色语义误写成红色。上述报告文字已按真实 UI 与修前代码更正。首次 Acceptance 后，同规格子 agent 又对“一键开始 / 浏览器打开”执行三轮复审，修正了一次性 URL 复用、fake opener 隔离和 UI 在途门控；但第三轮仍漏掉了手工 Acceptance 与自动验收共用 PATH opener 的安装态差异。收到真实点击反馈后，主审用 operation trace 与浏览器可见状态复现并改为显式测试注入，同规格子 agent 再审实际补丁未发现新的 P0–P3。除前述同 UID 扩大威胁模型下的路径 TOCTOU 外，另记录一个仅在测试/Acceptance 显式 opener override 校验与执行间存在的理论 P3 TOCTOU；当前私有 fixture 威胁模型下不再扩张实现。

## 后续门槛

1. 先人工审阅当前 dirty diff，确认新 UI 与安全修正属于同一候选范围。
2. 若要形成正式发布候选，仍需另做 Developer ID 签名、公证、Gatekeeper、DMG 与正式 bundle 身份验证；本轮 Acceptance ad-hoc bundle 不能替代。
3. 真实 OAuth/provider/Skill/MCP 只能在明确授权、可回滚、无真实数据污染的 Acceptance 环境执行，并形成独立证据，不能回填为本报告已经证明的事实。
