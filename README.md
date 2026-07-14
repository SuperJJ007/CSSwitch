<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <a href="https://github.com/SuperJJ007/CSSwitch/releases/tag/v0.5.0"><img src="https://img.shields.io/badge/release-v0.5.0-2ea44f.svg" alt="CSSwitch v0.5.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20Apple%20Silicon-1d1d1f.svg" alt="macOS Apple Silicon">
  <img src="https://img.shields.io/badge/built%20with-Tauri%202-C25A34.svg" alt="Tauri 2">
</p>

<p align="center">
  <a href="./README.md">简体中文</a> ·
  <a href="./README.en.md">English</a>
</p>

# CSSwitch

CSSwitch 是一个给 Claude Science 使用的本地配置转换器。它把 Science 的推理请求转换并接入你自己的模型 API，可配置 DeepSeek、通义千问、Kimi、MiniMax、GLM、OpenRouter、中转站或自定义兼容端点。

它面向的不只是开发者：你只需要准备 Claude Science、一个第三方 API Key，然后在桌面面板里新建配置、设为当前、点击「一键开始」。

> 当前版本主要支持 macOS Apple Silicon。首次打开未公证的 `.dmg` 应用时，macOS 可能需要你右键选择「打开」。

[下载最新版](../../releases/latest) · [项目文档](./docs/README.md) · [更新日志](./CHANGELOG.md) · [报告问题](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml) · [功能建议](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)

> **0.5.0：** CSSwitch 可在第三方模型模式下通过一个合并后的本地 connector 安装或卸载外部 Skill。安装要求准确的公开 GitHub 目录 URL，并在 Science 审批后复制、原生绑定和验证；卸载只隔离 CSSwitch 自己导入的目录，再使用 Science 原生解绑。版本还优化了重复「一键开始」的 Science 版本探测与 Skill 路由同步，并可由用户显式授权隔离 Science 使用系统 `~/.ssh/config`。详见[外部 Skill bridge](./docs/features/external-skill-bridge.md)、[系统 SSH](./docs/features/system-ssh.md)和[架构说明](./docs/architecture/overview.md)。

## 目录

- [为什么需要 CSSwitch](#为什么需要-csswitch)
- [可以做什么](#可以做什么)
- [快速开始](#快速开始)
- [从旧版升级](#从旧版升级)
- [支持的模型来源](#支持的模型来源)
- [状态诊断与能力 catalog](#状态诊断与能力-catalog)
- [它如何保护你的真实账号](#它如何保护你的真实账号)
- [哪些能力暂时用不了](#哪些能力暂时用不了)
- [多语言](#多语言)
- [开发与构建](#开发与构建)
- [风险与免责声明](#风险与免责声明)

## 为什么需要 CSSwitch

Claude Science 是 Anthropic 面向科研与分析场景的 AI Agent 应用，可以做文献分析、数据处理、代码执行、图表生成和论文写作等工作。但 Science 默认依赖 Claude 登录和 Anthropic 推理服务。

CSSwitch 做的是本地运行控制：

- 在隔离环境里启动 Claude Science。
- 在独立的本地工作区中运行第三方模型模式，不接管你的真实 Claude 账号。
- 把 Science 的模型请求转发到你选择的第三方 provider。
- 在需要时把 Anthropic Messages API 和 OpenAI 兼容接口互相转换。
- 保留「官方 Claude」模式，可随时切回 Science 的官方服务配置。

简单理解：CSSwitch 之于 Claude Science，类似 CC Switch 之于 Claude Code，并额外管理桌面应用、隔离工作区与本地网关。

```text
Claude Science sandbox
  -> CSSwitch local proxy
  -> DeepSeek / Qwen / Kimi / MiniMax / GLM / OpenRouter / custom endpoint
```

## 可以做什么

**给普通用户**

- 用桌面面板管理多套模型配置，不需要手改环境变量。
- 同一家 provider 可以保存多套配置，例如不同 Key、不同模型、不同中转地址。
- 点击「设为当前」前会先验证 Key；失败不会悄悄切换到坏配置。
- 点击「一键开始」会自动启动代理、准备隔离环境、打开 Science。
- Science 顶部模型选择器会显示你选择的真实模型名，而不是笼统的 `claude` 或 `opus`。
- 可以一键切回「官方 Claude」模式，不干扰你的真实 Claude 登录。
- 复用 Science 的持久化 data-dir；Skill 状态不阻塞 CSSwitch 启动。0.5.0 可通过对话从准确的公开 GitHub URL 安装 Skill，并通过同一 connector 安全隔离卸载 CSSwitch 自己的导入。
- CSSwitch 默认继承 `/Applications/Claude Science.app` 中用户当前安装的 Science，不比较、固定、升级或降级版本；更新 App 后，下次启动继续复用原 data-dir 并使用更新后的 App executable。
- 如果 Science App 缺失，CSSwitch 不会静默启动 data-dir 中的旧缓存。只有缓存可执行且版本可确认时，UI 才允许“仅本次使用缓存版本”；该选择不保存。否则只能打开 [Claude 官方下载页](https://claude.com/download) 安装 / 更新或取消。

**给进阶用户**

- 支持原生 Anthropic 兼容端点、OpenAI Chat Completions 兼容端点、OpenAI Responses 兼容端点。
- 支持自定义 `base_url`、模型名和中转站。
- DeepSeek、Kimi、MiniMax 等原生 Anthropic 端点优先透传，尽量保留工具调用、thinking 和流式响应。
- Qwen 与自定义 OpenAI 端点通过本地代理做协议转换。
- 配置和日志都保存在本机，便于自查和反馈。

## 快速开始

开始之前，请确认你已经安装：

- [Claude Science（Claude 官方下载页）](https://claude.com/download)
- macOS Apple Silicon 设备
- 一个可用的第三方模型 API Key
- CSSwitch 已内置 Rust inference gateway，无需另外安装 Python 运行时

1. 从 [GitHub Releases](../../releases/latest) 下载最新的 `CSSwitch_*.dmg`。
2. 将 CSSwitch 拖入「应用程序」。
3. 第一次打开如果被 Gatekeeper 拦截，请右键应用并选择「打开」。
4. 保持顶部模式为「第三方模型」。
5. 点击「+ 新建」，选择 provider，填写 API Key、模型和必要的 `base_url`。
6. 点击「创建」，再在配置列表中点击「设为当前」。
7. 验证通过后点击「一键开始」。
8. CSSwitch 会启动隔离 Science，并在浏览器中打开入口。

CSSwitch 不替你选择 Science 版本。正常启动使用当前安装的 Claude Science App；如果 App 缺失，面板会据实显示可确认的历史缓存版本并要求你明确选择“仅本次使用”，或打开官方下载页安装 / 更新。缓存选择不会写入配置；以后检测到 App 时会自动恢复使用 App。

如果你想使用 Science 的官方服务配置，切到「官方 Claude」模式即可。CSSwitch 会停止第三方代理链路，再打开真实 Science。

## 从旧版升级

0.5.0 保留现有 v2 配置格式并继续复用 `~/.csswitch/sandbox/home/.claude-science`，因此已有 Science 组织、项目和 Skill 不会被迁移或覆盖。旧 `~/.csswitch/` Skill store/inventory 会原样保留但不再参与启动；外部 `~/.claude/skills` 也不会自动同步到 Science。

完整步骤、备份位置和回滚边界见[升级与回滚说明](./docs/operations/upgrade-and-rollback.md)。

## 支持的模型来源

| 来源 | 接入方式 | 说明 |
|---|---|---|
| DeepSeek | 原生 Anthropic 端点 | 默认来源，尽量保留 thinking、工具调用和流式能力 |
| 通义千问 Qwen | OpenAI Chat Completions 兼容端点 | 由 CSSwitch 代理转换为 Science 需要的 Anthropic 格式 |
| 智谱 GLM | Anthropic 兼容端点 | 可编辑官方默认地址，可选择或自填模型 |
| 小米 MiMo | Anthropic 兼容端点 | 支持改到套餐或区域端点 |
| 硅基流动 | Anthropic 兼容端点 | 可选择或自填模型 |
| Kimi / Moonshot | Anthropic 兼容端点 | 可编辑官方默认地址，支持 Kimi 系列模型 |
| MiniMax | Anthropic 兼容端点 | 可编辑官方默认地址，支持 MiniMax 系列模型 |
| OpenRouter | Anthropic 兼容聚合入口 | 可选择或自填模型 |
| 自定义 Anthropic | 自填兼容端点 | 适合私有网关、Claude 兼容中转站、本地适配器 |
| 自定义 OpenAI | 自填 OpenAI Chat Completions base root | 代理自动补 `/chat/completions` 与 `/models` |
| 自定义 OpenAI Responses | 自填 OpenAI Responses base root | 代理自动补 `/responses` 与 `/models` |

> 如果你的地址是 `/anthropic` 端点，请选择「自定义 Anthropic」。如果选择「自定义 OpenAI」，请填写 OpenAI 兼容的 base root，例如 `https://example.com/v1`，不要填 Anthropic 端点。

## 状态诊断与能力 catalog

CSSwitch 内置了只读的 capability catalog，用来把 provider、工具调用和 transport 的已知兼容性边界显式化。运行时诊断会返回当前 profile 命中的规则，便于定位当前配置的处理方式。

这个 catalog 是诊断与可观测性入口，不代表所有外部 provider、官方托管能力、签名或公证状态都已验证。

状态灯也只表示当前可观测的本地状态：例如沙箱灯是本地 HTTP health，不等于已证明该端口一定属于本沙箱 Science。`自检` 默认不会读取真实 `~/.claude-science`；只有显式设置 `CSSWITCH_DOCTOR_CHECK_REAL_HOME=1` 才会做真实 HOME 存在性检查。

## 它如何保护你的真实账号

CSSwitch 的核心边界是：第三方模型模式只把凭证、数据目录和网络代理放在隔离环境里，不接管你的真实 Claude 账号。

- 不复制、读取或修改真实 Claude 登录凭证、OAuth token、账号状态或用户数据。
- 隔离 Science 使用独立 HOME、独立端口和独立数据目录。
- 第三方 API Key 保存在 `~/.csswitch/config.json`，文件权限为 `0600`。
- Key 不显示在应用日志中，本地网关只监听回环地址。
- 「允许隔离 Science 使用系统 SSH 配置」默认关闭。启用后只让 Science 调用系统 OpenSSH 时读取真实 `~/.ssh/config`；CSSwitch 不复制或链接整个 `.ssh`，不启动 `sshd`、不修改防火墙，也不提供 `0.0.0.0` 公网监听。OpenSSH 配置中的 `Include`、密钥、Agent 和命令规则仍按其原生语义生效，因此这是一项显式信任授权。
- 隔离 Science 新启动会优先使用本机官方 Claude Science App 的 binary；App 不可用且旧沙箱副本版本可确认时，只会在用户明确授权后单次回退，选择不会持久化。CSSwitch 不下载 Science，仍使用 `--no-auto-update`。
- 官方 Claude 模式会拆掉第三方代理链路，再把你交回真实 Science。

## 哪些能力暂时用不了

CSSwitch 不是 Claude 官方服务，第三方模型模式也不会获得 Anthropic 官方账号权限。以下限制是当前架构边界：

- Anthropic 托管的远程 MCP 服务不可用，例如 `pubmed`、`clinical-trials`、`chembl`、`biorxiv` 等 `*.mcp.claude.com` 服务。
- 依赖真实 Claude 账号授权的目录连接器、远程插件、云端能力可能会显示 session expired、unavailable 或 skipped。
- Science 原生 GitHub 导入、新 Skill 发布和草稿删除仍可能查询 Anthropic 账号 catalog。CSSwitch 不伪造 OAuth 或 catalog；0.5.0 仅增加准确公开 GitHub URL 的本地安装与隔离回收，不提供确定性的名称搜索、更新、覆盖、永久删除/恢复界面、私有仓库或发布。
- 第三方模型对工具调用、长上下文、thinking、图片和流式输出的兼容程度不同；原生 Anthropic 端点通常比 OpenAI 翻译路径更稳。
- 当前 macOS 包尚未 Apple 公证，首次启动需要手动放行。
- inference gateway 已是随应用打包的 Rust sidecar；不再提供运行时 Python fallback。

遇到问题请通过 [GitHub Issues](https://github.com/SuperJJ007/CSSwitch/issues) 反馈。

## 多语言

README 目前提供：

| 语言 | 文件 |
|---|---|
| 简体中文 | [README.md](./README.md) |
| English | [README.en.md](./README.en.md) |

应用界面当前以中文为主。README 多语言不代表桌面应用 UI 已经完成多语言切换；后续如果应用内 i18n 落地，会在这里单独说明。

## 反馈与社区

遇到问题时，建议先说明：

- CSSwitch 版本
- macOS 版本与芯片架构
- 使用的 provider 和模型
- 操作步骤
- `~/.csswitch/logs/` 中相关日志

提交日志前请删除 API Key、令牌、邮箱、私有 URL 和任何敏感数据。

- [报告 Bug](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml)
- [提出功能建议](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)
- [查看更新日志](./CHANGELOG.md)

<p align="center">
  <img src="docs/assets/wechat-group.jpg" alt="CSSwitch 微信群" width="420">
</p>

## 开发与构建

用户不需要从源码运行。以下内容只给想调试或参与开发的人。

维护者可从[项目文档总入口](./docs/README.md)查看架构、测试、发布、功能合同和版本证据。

```bash
cd desktop
npm install
npm run tauri dev
```

常用检查：

```bash
bash test/run_all.sh
bash test/run_all.sh --require-release-ready

(cd desktop/gateway && cargo test)
(cd desktop/src-tauri && cargo test)
python3 -m unittest discover -s test -p 'test_*.py' -v
node --check desktop/src/main.js
```

## 风险与免责声明

- 本项目仅供个人学习与研究使用，使用风险由用户自行承担。
- CSSwitch 与 Anthropic 不存在从属、合作或背书关系。
- 推理请求会发送到你自行配置并付费的第三方模型服务。
- 第三方模型模式不授予 Anthropic 官方账号权限；部分官方托管能力仍可能不可用。
- 软件按「现状」提供，不作任何形式的担保。

## 致谢

CSSwitch 的名字和产品形态参考了 [CC Switch](https://github.com/farion1231/cc-switch)。两个项目彼此独立，不存在从属或背书关系。

## 许可

[MIT](./LICENSE)
