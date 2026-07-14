# 发布规则

- 发布工作必须固定到干净 commit、版本、架构和预期附件名。
- 从同一 commit 构建 app 与 sidecar，并核对版本、资源、注册命令和 executable identity。
- 未经授权不覆盖 `/Applications/CSSwitch.app`；候选安装优先使用隔离位置。
- 分别核验源码、artifact、临时安装、runtime、provider、分发和公开发布层。
- 独立记录签名完整性、签名身份、notarization、Gatekeeper、hash 与上传附件；ad-hoc 签名不是 Developer ID 签名或公证。
- README、CHANGELOG、升级说明、限制、tag、release 页面和最终附件必须一致，才可称发布闭环。
- 构建或测试成功不授权 commit、push、tag、release、删除远端分支或替换 App。
- 具体步骤使用[发布流程](../../docs/operations/release.md)，每个版本的结果写入 dated release evidence。
