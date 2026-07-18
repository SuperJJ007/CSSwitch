# v0.7.0 新 UI：UI / 打包审查

## 审查对象与方法

- 对象：模型连接、Skill、状态、设置四个主页面，920×600 与 760×520 响应式布局、深浅主题、键盘操作、Tauri 资源边界与验收窗口配置。
- 主审先用仓库现有视觉语言和当前静态入口进行浏览器截图与交互检查，再由 `gpt-5.6-sol`、`xhigh` 子 agent 三轮只读复审。
- 仅用本地静态 server 和 mock bridge 检查页面；没有启动正式 Tauri App。

## 主审发现

1. Skill 搜索每键重建输入框导致焦点丢失；详情抽屉缺 Escape、焦点圈和关闭后恢复。
2. 深色主题只改当前 DOM，刷新后恢复成浅色。
3. 官方模式与未知状态的颜色过于肯定；Codex 空 upstream 被错误聚合。
4. 生产 Tauri 窗口已改到 920×600/min 760×520，但 Acceptance 配置仍是旧的 420×700/min 380×600。
5. 新导航完成后仍有“我的配置”“高级设置”“＋ 新建”“启动代理”等模糊或已不存在的旧入口文案。
6. 原型 store/test 位于生产 `frontendDist`，资源边界不够清楚。

## 修改

- 搜索结果局部渲染；抽屉补全 dialog、`inert`、焦点圈、Escape 和焦点恢复。
- 主题写入独立 `csswitch-theme` 键并在初始化时恢复，不改变其他用户设置。
- 统一状态中性语义与 Codex 不适用层；官方打开只显示动作已发起。
- 生产与 Acceptance 窗口统一为 920×600/min 760×520。
- 当前入口文案统一为“一键开始”“设置 > Codex 账号与连接”“模型连接 > 配置方案”“新建配置”。
- 删除无真实能力支撑的 Skill/MCP 原型实现；生产源码目录只保留运行时模块，测试迁移到 `test/`。

## 子 agent 复审与终态

第一次复审覆盖源码、资源边界和页面可用性；第二次复审发现 Acceptance 窗口尺寸与“启动代理”残文两个 P2，修正后第三次复审确认 P0–P2 清零。动态模块均进入 Tauri 元数据，测试模块未进入。

## Acceptance 追补：一键开始 / 浏览器打开

首次覆盖安装后的 Acceptance 暴露出一个静态浏览器审查没有覆盖到的 P1：Science 的控制 URL 可能是 60 秒、一次性的，但“浏览器打开”复用了 one-click 已经消费过的内存 URL；同时 macOS `open` 退出 0 只代表 LaunchServices 接受请求，旧 UI 却把它表述成浏览器已经显示，手动打开成功路径也没有任何反馈。这不是此前静态预览 server 残留；安全 operation trace 显示 one-click 已实际完成 `action=reopened`。

第一次追补包仍有一项真实安装态漏审：为让 installed-provider 自动验收使用 fake opener，`acceptance-build` 直接通过 `PATH` 查找 `open`。手工 Acceptance 因而也落入了测试专用边界。用户在 13:40 再次点击时，operation trace 显示 proxy、Science 和 `open_browser` 阶段都成功，但 Chrome 在该时点没有新增标签，Safari 也没有本地 Science 页面；这证明不是浏览器残留，而是打开请求没有抵达可见浏览器。此前“修正后第三轮未发现 P0–P3”的结论没有覆盖到这一安装态差异，现已更正。

追补修正：

- 每次手动打开都在 lifecycle 串行区内确认当前隔离 Science runtime 身份与监听状态，再向该 runtime 重新取得 URL；新 URL 不返回前端、不写生产日志。
- 正式构建和未显式注入的手工 Acceptance 都固定使用 `/usr/bin/open`，不再从 `PATH` 查找。单测只能通过 `CSSWITCH_TEST_OPEN_BIN`、installed-provider controller 只能通过显式 `CSSWITCH_ACCEPTANCE_OPEN_BIN` 注入 fake opener；覆盖值必须是绝对、可访问、普通、非 symlink、可执行文件。
- 前端增加独立 `browserOpenInFlight` 门控、“打开中”以及成功/失败提示；全局 busy 的松开不会提前重新启用仍在途的浏览器按钮。
- one-click 和手动打开统一据实提示“已向默认浏览器发出请求”，不再把系统接单夸大成窗口已前置。

同规格 `gpt-5.6-sol`、`xhigh` 子 agent 对首次追补执行了三轮只读复审，但如上所述，第三轮仍漏掉了手工 Acceptance 与自动验收共用 PATH opener 的安装态差异。收到真实点击反馈后，主审重新核对 operation trace、Chrome/Safari 可见状态与已安装二进制，修正为显式注入；同规格子 agent 再审实际补丁后未发现新的 P0–P3。它仅记录了显式测试 override 在校验与执行之间的理论 P3 TOCTOU；该入口只存在于单测/Acceptance、当前 fixture 位于私有目录，暂不引入 fd-exec 抽象。

## 浏览器复证

- 920×600：模型连接、Skill、状态、设置均可见；760×520 无横向滚动。
- 搜索 `pdf` 后 active element 保持为搜索输入。
- 打开详情后 active element 为“关闭详情”，按 Escape 关闭并恢复到原详情按钮。
- 深色主题刷新前后均为 `dark`。
- 修正后截图：`/Users/superjj/.codex/visualizations/2026/07/17/019f7136-1520-7363-904f-b9006096a348/csswitch-v070-ui-audit/09-skill-detail-fixed-920x600.png`。

## 工程复证与边界

- `bash test/run-frontend.sh`：15/15；Python UI/runtime boundary：9/9；diagnostics：8/8。
- 两份 Tauri JSON 配置可解析，`cargo check`、本地 `cargo build --release` 与 `git diff --check` 通过。
- Acceptance 最终追补后：Python UI/runtime boundary 10/10、installed-provider controller 21/21、Acceptance-feature `cargo check`、system opener 单测、显式 fake Science/open nonce smoke 与完整 `bash test/run_all.sh --require-release-ready` 均通过；完整门禁中 Desktop 为 318 passed / 3 explicit ignored，`release-ready green: YES`。
- 已再次生成并覆盖安装 `/Applications/CSSwitch Acceptance.app`：bundle id `com.csswitch.acceptance`、版本 `0.7.0`、arm64、ad-hoc deep signature strict verify 通过；installed desktop SHA-256 `1edd60f3e4c52672ec888285c6752257b6ed714d11b106474fdd47f7ed89f1df`，gateway SHA-256 `53444d1712ab7bb798d1cb67acb2942bbdb288e8242b9ab972a1fb47127dc270`，与构建产物一致。
- `/Applications/CSSwitch.app` 未被启动或修改；desktop/gateway SHA-256 仍为 `d3175a787f86ad7ab656a903c02f5195e045189970ab87d2da5c8a534424ebeb` / `68f7ef50a35f6bfb251b0b8f0b50edf078f3055e3974628dca2642f5c209a360`。
- 旧 Acceptance 暂存为 `/private/tmp/CSSwitch Acceptance.before-browser-open-fix-20260718.app`，待修复包人工点击确认后可删除。
- 该证据不包含 DMG、Developer ID 签名、公证、Gatekeeper、正式 App 安装、真实账号/provider 或真实 Science 数据验收；最终修复包在本报告更新时尚未完成一次新的人工 Chrome 弹窗确认，因此只宣称构建、门禁和覆盖安装通过。
