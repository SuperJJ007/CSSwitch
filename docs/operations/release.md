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
```

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

```bash
shasum -a 256 CSSwitch_<version>_aarch64.dmg
codesign --verify --deep --strict --verbose=4 CSSwitch.app
codesign -dvvv CSSwitch.app
spctl --assess --type open --context context:primary-signature --verbose=4 CSSwitch_<version>_aarch64.dmg
xcrun stapler validate CSSwitch_<version>_aarch64.dmg
```

`codesign --verify` 通过只说明 seal / 签名结构有效。必须另记签名身份、TeamIdentifier、是否 Developer ID、notarization、stapled ticket 和 Gatekeeper 结果。

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
