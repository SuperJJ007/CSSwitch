# 发布流程

发布是逐层建立证据，不是一次 `build` 或一次 `gh release`。Agent 的授权禁止项见[发布规则](../../.agents/rules/release.md)。

## 1. 固定发布输入

- 目标版本、分支与 exact commit；
- 目标架构与预期 app / DMG 文件名；
- package.json / lock、Cargo.toml / lock 与 Tauri 配置中的版本一致；
- README、CHANGELOG、升级说明和 known limitations 已准备；
- 工作树干净，受保护 worktree 不参与发布。

把 source commit 记入该版本的 `docs/evidence/releases/<version>.md`。

## 2. 源码门禁

```bash
bash test/run_all.sh --require-release-ready
git diff --check
```

如环境层被阻塞，就换到具备相应能力的发布环境复跑；不能把 `current-env clean` 改写成 `release-ready green`。

## 3. 构建 artifact

```bash
cd desktop
npm ci
npm run tauri build
cd ..
```

Codex OAuth 与 Gateway 不要求 Apple Developer 身份、Developer ID、Team ID 或正式签名。需要公开分发时，维护者可以在独立发布流水线中选择 Developer ID / notarization；不得把该可选分发步骤写成源码构建或 Codex 登录前置。

从目标 commit 构建后核对：

- `.app`、DMG 与 `CFBundleShortVersionString`；
- `Contents/MacOS/desktop` 与 `Contents/MacOS/csswitch-gateway`；
- `Contents/Resources/scripts`；
- 不存在旧 Python `Resources/proxy` runtime；
- gateway executable identity、Tauri externalBin / resources 和注册命令与源码一致。

计算最终 DMG 的大小和 SHA-256，之后任何重建都视为新 artifact，需重跑后续层。

## 4. 临时安装与 runtime

只读挂载 DMG，把 app 复制到隔离位置或使用独立 bundle ID；未经授权不覆盖 `/Applications/CSSwitch.app`。

使用临时 HOME / data-dir、动态端口、假凭证验证：

- Gateway ownership、启动 / 停止；
- installed App 优先、无效 `SCIENCE_BIN` fail-closed、cache one-shot；
- Science start / reopen / recovery / url / stop 的强 runtime identity，并确认高频 UI status 只报告 HTTP health 与已记 metadata；
- 外部 Skill route、install / attach / load / restart / uninstall / detach；
- 外部 Skill bridge 失败只 warning；系统 SSH 默认关闭不影响启动，但 opt-in 后缺失 / 不安全 config 或 wrapper 必须 fail closed。

真实 provider、真实账号和真实 SSH server 只在单独授权后验证，并与 loopback 结果分开写。

## 5. 分发检查

分别执行并记录：

至少记录最终附件 SHA-256、包内 Desktop/Gateway hash、版本和安装/runtime 结果。如果维护者选择公开 macOS 分发签名，再单独记录 Developer ID、notarization、stapled ticket 与 Gatekeeper 结果；本项目不提供或强制一个固定 Team ID，也不把这些分发证据当作 Codex 功能本身的前置。

## 6. 发布与回读

在明确授权后创建 tag / push / GitHub Release / 上传附件。发布后重新查询：

- tag peeled commit 与目标 commit；
- release 非 draft / prerelease 状态与发布时间；
- 最终附件名称、大小和 digest；
- 重新下载或用独立同字节 artifact 计算 hash；
- README 下载入口、CHANGELOG、升级说明和 known limitations 一致。

只有公开页面和最终附件都回读一致，才写“已发布”。未取得的 installed runtime、live provider、签名或公证层必须明确写“未建立”。

## 7. 收尾

- 将版本结果写入 `docs/evidence/releases/`；
- 刷新 `.agents/context/current-release.md`、verified-state 与 known-issues；
- 再检查所有 worktree，确认没有误改用户工作区；
- commit、push、tag、release 和清理分支分别报告，不合并授权。
