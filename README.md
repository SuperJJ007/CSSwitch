<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <a href="https://github.com/SuperJJ007/CSSwitch/releases/tag/v0.4.4"><img src="https://img.shields.io/badge/release-v0.4.4-2ea44f.svg" alt="CSSwitch v0.4.4"></a>
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

[下载最新版](../../releases/latest) · [更新日志](./CHANGELOG.md) · [报告问题](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml) · [功能建议](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)

> **0.4.4：** CSSwitch 一键启动不再扫描外部 Skill、读取旧 store/inventory 或执行 reconcile。升级继续复用 `~/.csswitch/sandbox/home/.claude-science`，不迁移、删除或覆盖现有 Science 数据。现行边界见 [架构说明](./docs/ARCHITECTURE.md)。

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
- 复用 Science 的持久化 data-dir；Skill 状态不再阻塞 CSSwitch 启动。第三方模式下，依赖 Anthropic 账号 catalog 的 Science 原生 Skill 导入/发布入口可能不可用。

**给进阶用户**

- 支持原生 Anthropic 兼容端点、OpenAI Chat Completions 兼容端点、OpenAI Responses 兼容端点。
- 支持自定义 `base_url`、模型名和中转站。
- DeepSeek、Kimi、MiniMax 等原生 Anthropic 端点优先透传，尽量保留工具调用、thinking 和流式响应。
- Qwen 与自定义 OpenAI 端点通过本地代理做协议转换。
- 配置和日志都保存在本机，便于自查和反馈。

## 快速开始

开始之前，请确认你已经安装：

- [Claude Science](https://claude.com)
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

如果你想使用 Science 的官方服务配置，切到「官方 Claude」模式即可。CSSwitch 会停止第三方代理链路，再打开真实 Science。

## 从旧版升级

0.4.4 保留现有 v2 配置格式并继续复用 `~/.csswitch/sandbox/home/.claude-science`，因此已有 Science 组织、项目和 Skill 不会因解除 Skill Manager 而被迁移或覆盖。旧 `~/.csswitch/` Skill store/inventory 会原样保留但不再参与启动；外部 `~/.claude/skills` 也不再自动同步到 Science。

完整步骤、备份位置和回滚边界见 [升级与回滚说明](./docs/upgrade-and-rollback.md)。

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
- 远程访问只生成用户自行运行的 SSH 客户端命令：转发 Science 与预览端口，不转发持有上游凭证的 Gateway，也不会替你启用远程登录或修改 `sshd`；一次性 Science token 只在访问端终端生成，不返回 CSSwitch 前端。
- 隔离 Science 新启动会优先使用本机官方 Claude Science App 的 binary；官方候选不可用时才回退旧沙箱副本。CSSwitch 不下载 Science，仍使用 `--no-auto-update`。
- 官方 Claude 模式会拆掉第三方代理链路，再把你交回真实 Science。

## 哪些能力暂时用不了

CSSwitch 不是 Claude 官方服务，第三方模型模式也不会获得 Anthropic 官方账号权限。以下限制是当前架构边界：

- Anthropic 托管的远程 MCP 服务不可用，例如 `pubmed`、`clinical-trials`、`chembl`、`biorxiv` 等 `*.mcp.claude.com` 服务。
- 依赖真实 Claude 账号授权的目录连接器、远程插件、云端能力可能会显示 session expired、unavailable 或 skipped。
- Science 当前的外部 Skill GitHub 导入和新 Skill 发布会查询 Anthropic 账号 catalog；第三方模型模式没有真实 Anthropic 登录时可能失败。CSSwitch 0.4.4 不伪造 OAuth 或 catalog，也尚未提供自然语言本地安装桥。
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
