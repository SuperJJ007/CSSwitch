<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <a href="https://github.com/SuperJJ007/CSSwitch/releases/tag/v0.7.0"><img src="https://img.shields.io/badge/release-v0.7.0-2ea44f.svg" alt="CSSwitch v0.7.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20Apple%20Silicon-1d1d1f.svg" alt="macOS Apple Silicon">
  <img src="https://img.shields.io/badge/built%20with-Tauri%202-C25A34.svg" alt="Tauri 2">
</p>

<p align="center">
  <a href="./README.md">简体中文</a> ·
  <a href="./README.en.md">English</a>
</p>

# CSSwitch

CSSwitch 是一个给 Claude Science 使用的本地配置转换器。它把 Science 的推理请求转换并接入你自己的模型 API，可配置 DeepSeek、通义千问、Kimi、MiniMax、GLM、OpenRouter、中转站或自定义兼容端点；也可以通过 CSSwitch 独立的 Codex 浏览器登录，把当前账号可用的 Codex 模型接入 Science。

它面向的不只是开发者：准备 Claude Science，再选择第三方 API Key 或 Codex 登录，在桌面面板里设为当前并点击「一键开始」即可。

> 当前版本主要支持 macOS Apple Silicon。首次打开未公证的 `.dmg` 应用时，macOS 可能需要你右键选择「打开」。

[下载最新版](https://github.com/SuperJJ007/CSSwitch/releases/latest) · [项目文档](./docs/README.md) · [更新日志](./CHANGELOG.md) · [报告问题](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml) · [功能建议](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)

> **0.7.0：** 新增默认关闭的 Codex 实验接入：CSSwitch 独立完成浏览器 OAuth，动态读取当前账号模型目录，并通过 Rust Gateway 将 Codex Responses/SSE 转换给 Science。登录、模型获取、推理和退出共用统一网络路由，不读取或修改原生 Codex 登录；发布包继续采用 ad-hoc 签名，不要求 Apple 开发者账号。新版同时启用 1 号橙色圆角图标，并修复部分 Codex SSE 响应缺少 `Content-Type` 时被误报为 Claude 暂时不可用的问题。详见 [Codex → Claude Science 合同](./docs/features/codex-science-bridge.md)。

## 目录

- [为什么需要 CSSwitch](#为什么需要-csswitch)
- [可以做什么](#可以做什么)
- [快速开始](#快速开始)
- [使用 Codex](#使用-codex)
- [安装和卸载外部 Skill](#安装和卸载外部-skill)
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
  -> DeepSeek / Qwen / Kimi / MiniMax / GLM / OpenRouter / Codex / custom endpoint
```

## 可以做什么

**给普通用户**

- 用桌面面板管理多套模型配置，不需要手改环境变量。
- 同一家 provider 可以保存多套配置，例如不同 Key、不同模型、不同中转地址。
- 点击「设为当前」前会先验证 Key；失败不会悄悄切换到坏配置。
- 点击「一键开始」会自动启动代理、准备隔离环境、打开 Science。
- Science 顶部模型选择器会显示你选择的真实模型名，而不是笼统的 `claude` 或 `opus`。
- 可使用 CSSwitch 独立 Codex 浏览器登录，并在 Science 的 “More models” 中选择当前账号动态返回的多个 `Codex / …` 模型。
- 可以一键切回「官方 Claude」模式，不干扰你的真实 Claude 登录。
- 复用 Science 的持久化 data-dir；Skill 状态不阻塞 CSSwitch 启动。可从准确的公开 GitHub URL 或本地 `.zip` / `.skill` 安装单 Skill/bundle；bundle 卸载必须整包确认并只隔离 CSSwitch 自己的导入。
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
- 一个可用的第三方模型 API Key，或一个可完成浏览器授权的 Codex 账号
- CSSwitch 已内置 Rust inference gateway，无需另外安装 Python 运行时

1. 从 [GitHub Releases](https://github.com/SuperJJ007/CSSwitch/releases/latest) 下载最新的 `CSSwitch_*.dmg`。
2. 将 CSSwitch 拖入「应用程序」。
3. 第一次打开如果被 Gatekeeper 拦截，请右键应用并选择「打开」。
4. 保持顶部模式为「第三方模型」。
5. 点击「+ 新建」，选择 provider，填写 API Key、模型和必要的 `base_url`。
6. 点击「创建」，再在配置列表中点击「设为当前」。
7. 验证通过后点击「一键开始」。
8. CSSwitch 会启动隔离 Science，并在浏览器中打开入口。

CSSwitch 不替你选择 Science 版本。正常启动使用当前安装的 Claude Science App；如果 App 缺失，面板会据实显示可确认的历史缓存版本并要求你明确选择“仅本次使用”，或打开官方下载页安装 / 更新。缓存选择不会写入配置；以后检测到 App 时会自动恢复使用 App。

如果你想使用 Science 的官方服务配置，切到「官方 Claude」模式即可。CSSwitch 会停止第三方代理链路，再打开真实 Science。

## 使用 Codex

Codex 接入在 0.7.0 中仍是需要手动开启的实验能力。它使用 CSSwitch 自己的浏览器 OAuth 和认证文件，不读取、复用或修改原生 `~/.codex` 登录，也不要求 Apple Developer 或 Developer ID 签名。

1. 打开「高级设置」，启用 Codex 实验能力。
2. 选择 Codex 网络：`auto` 会使用可用的 `HTTPS_PROXY` / `ALL_PROXY` 与 `NO_PROXY`；从 Finder 启动通常没有终端代理变量，此时显示 `direct`，系统 TUN 仍可能接管直接 socket。也可以填写无认证的 HTTP、HTTPS、SOCKS5 或 SOCKS5h 代理。
3. 点击 Codex 浏览器登录，在 OpenAI 页面完成账号验证和授权。浏览器最终回到 `localhost` 并显示完成后，返回 CSSwitch。
4. 登录成功会自动确保一条 Codex 配置存在，但不会擅自切换当前 provider。把该配置设为当前，再点击「一键开始」。
5. 在 Science 顶部模型选择器的 “More models” 中选择 `Codex / …`。列表来自当前账号的动态模型目录，不是 CSSwitch 硬编码的固定模型表。

当前边界是 macOS Apple Silicon、单个 CSSwitch Codex 账号和浏览器登录。暂不支持多账号、设备码、代理认证、PAC、自定义 CA、系统代理自动发现或自动判断 TUN。系统浏览器使用系统自身网络；CSSwitch 的 Codex 网络设置只控制 OAuth token exchange、模型目录和推理等 Gateway HTTPS 请求。

## 安装和卸载外部 Skill

CSSwitch 提供 GitHub 和本地文件两条安装路线。两条路线使用相同的路径、大小、符号链接和保留文件校验，并在安装后把 Skill 原生绑定到 Science 的默认 Agent。安装器不会替你登录 GitHub，也不会读取或接管 Science 凭证。

**从公开 GitHub URL 安装**

1. 在 CSSwitch 中完成「一键开始」，然后打开一个新的 Science 对话。
2. 把准确的公开 GitHub tree URL 发给 Agent，并明确要求使用 CSSwitch 外部 Skill 安装器。例如：

   ```text
   请使用 CSSwitch 外部 Skill 安装器安装这个固定提交，只发起一次请求，不要自动重试：
   https://github.com/<owner>/<repo>/tree/<commit>/<path>
   ```

3. Science 请求访问 CSSwitch bridge 目录时，核对路径后批准本次读写授权。
4. 下载期间等待同一个请求继续推进，不要重复发送安装指令。成功后，单 Skill 会要求在当前会话做一次加载验证；bundle 会报告完整成员数量和绑定结果。

推荐使用固定 commit URL，便于复用和核对。公开仓库的根目录 bundle 也可以使用 `.../tree/<commit>`。私有仓库、名称搜索、覆盖更新和 Skill 发布不在当前范围内。

**从本地文件安装**

1. 保持隔离 Science 正在运行且状态正常。
2. 在 CSSwitch 主面板点击「导入 Skill 包」。
3. 选择 `.zip` 或 `.skill` 文件。支持 archive 根目录直接包含 `SKILL.md`、唯一外层 Skill 目录，以及包含多个直接 Skill 子目录和可选 `_shared` 的 bundle。
4. CSSwitch 校验、原子写入并绑定后，在新的 Science 对话中加载对应 Skill，确认它能真实执行。

完全相同的 GitHub 固定提交或本地 archive 再次安装时会校验并快速复用，不会重新下载或生成重复 Skill。同名但来源或内容不同的包会返回冲突，不会覆盖现有安装。

**卸载与 bundle 确认**

在 Science 中要求 CSSwitch 卸载器卸载某个 Skill。单 Skill 只会处理带有 CSSwitch 导入标记的目录，并在隔离后完成 Agent 解绑。若目标属于 bundle，首次调用只返回 bundle 名称、完整受影响 Skill 列表和确认 ID，不会删除文件；只有用户明确确认后才整包解绑和隔离。取消时不应再次调用卸载工具，当前版本也不支持只物理删除一个 bundle 成员。

更完整的状态、限制、恢复行为和故障排查见[外部 Skill bridge](./docs/features/external-skill-bridge.md)。

## 从旧版升级

0.7.0 会把 v1/v2 配置原子迁移为 v3，并在提交前保留不可覆盖的版本备份；现有 API provider、active profile、端口、未知字段和 `~/.csswitch/sandbox/home/.claude-science` 会继续保留。v3 新增 Codex profile 与网络设置；回滚到 0.6.0 前必须在 0.7.0 中执行“导出并降级到 v2”，或停掉所有 CSSwitch 进程后恢复迁移生成的 v2 备份，单纯删除 Codex profile 不会改变 schema。旧 Skill store/inventory 仍原样保留但不参与启动，外部 `~/.claude/skills` 也不会自动同步到 Science。

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
| Codex（实验，默认关闭） | CSSwitch 独立 OAuth + Responses/SSE | 动态读取当前账号模型；不复用原生 Codex 登录；单账号、浏览器登录 |

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
- Codex 实验凭据只保存在 CSSwitch 自有 data root 的私有文件中：父目录权限 `0700`、认证文件权限 `0600`，拒绝符号链接并使用原子提交与 generation/CAS。CSSwitch 不使用 macOS Keychain，也不读取、复制、覆盖或删除原生 `~/.codex` 登录。
- Key 不显示在应用日志中，本地网关只监听回环地址。
- 「允许隔离 Science 使用系统 SSH 配置」默认关闭。启用后只让 Science 调用系统 OpenSSH 时读取真实 `~/.ssh/config`；CSSwitch 不复制或链接整个 `.ssh`，不启动 `sshd`、不修改防火墙，也不提供 `0.0.0.0` 公网监听。OpenSSH 配置中的 `Include`、密钥、Agent 和命令规则仍按其原生语义生效，因此这是一项显式信任授权。
- 隔离 Science 新启动会优先使用本机官方 Claude Science App 的 binary；App 不可用且旧沙箱副本版本可确认时，只会在用户明确授权后单次回退，选择不会持久化。CSSwitch 不下载 Science，仍使用 `--no-auto-update`。
- 官方 Claude 模式会拆掉第三方代理链路，再把你交回真实 Science。

## 哪些能力暂时用不了

CSSwitch 不是 Claude 官方服务，第三方模型模式也不会获得 Anthropic 官方账号权限。以下限制是当前架构边界：

- Anthropic 托管的远程 MCP 服务不可用，例如 `pubmed`、`clinical-trials`、`chembl`、`biorxiv` 等 `*.mcp.claude.com` 服务。
- 依赖真实 Claude 账号授权的目录连接器、远程插件、云端能力可能会显示 session expired、unavailable 或 skipped。
- Science 原生 GitHub 导入、新 Skill 发布和草稿删除仍可能查询 Anthropic 账号 catalog。CSSwitch 不伪造 OAuth 或 catalog；外部 Skill bridge 只支持准确公开 GitHub URL 或用户选择的本地 `.zip` / `.skill`，并对 CSSwitch 导入做整包隔离回收，不提供确定性的名称搜索、覆盖、成员级物理删除、永久删除/恢复界面、私有仓库或发布。
- 第三方模型对工具调用、长上下文、thinking、图片和流式输出的兼容程度不同；原生 Anthropic 端点通常比 OpenAI 翻译路径更稳。
- 当前 macOS 包尚未 Apple 公证，首次启动需要手动放行。
- inference gateway 已是随应用打包的 Rust sidecar；不再提供运行时 Python fallback。
- Codex 仍是默认关闭的实验能力，依赖 OpenAI 当前账号权限、模型目录和上游协议；单账号与上述网络限制不应被理解为官方支持或稳定性承诺。

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
