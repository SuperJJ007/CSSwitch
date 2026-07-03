<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <img src="https://img.shields.io/badge/platform-macOS%20(Apple%20Silicon)-1d1d1f.svg" alt="macOS">
  <img src="https://img.shields.io/badge/built%20with-Tauri%202-C25A34.svg" alt="Tauri 2">
  <img src="https://img.shields.io/badge/WSL2%20%7C%20Docker-2496ED.svg?logo=docker" alt="WSL2 / Docker">
</p>

# CSSwitch

[Claude Science](https://claude.com) 是一套 **AI agent 原生的科研平台**：从查找、分析文献，到科研数据分析，再到图片与文章制作，全流程打通。

CSSwitch 让你**无需 Claude 订阅**也能用上它：填入你自选的第三方 API（DeepSeek、通义千问，或任意 OpenAI 兼容端点）即可。Science 那套 AI agent 科研体验照旧，底层模型换成你自己的。类比 CC Switch 之于 Claude Code。

## 背景

Claude Science 的登录只是**启动门票**：登录后推理请求打到哪，由环境变量 `ANTHROPIC_BASE_URL` 决定。CSSwitch 把它指向本地一个翻译代理，代理剥掉 Science 带来的 OAuth、换成你的第三方 key、按需翻译协议，最终打到你选的模型。登录门则在**隔离沙箱**里写一份本地自造的虚拟 OAuth 越过，全程不碰真实登录、零真实凭证。

```
Claude Science（沙箱 · 虚拟登录）
   │  ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>
   ▼
csswitch_proxy.py（本地翻译代理：剥离入站 Bearer、注入你的第三方 key）
   ▼
DeepSeek 原生 Anthropic 端点  /  通义千问等 OpenAI 兼容端点
```

## 特性（安全 · 易用）

**易用**

- **开箱即用**：一个 macOS 桌面 app 把一切串好。你只需填入自己的第三方 API key，点「一键开始」，浏览器自动打开已登录的 Science。**零 node 运行时依赖**：虚拟登录已是 Rust 原生实现，装了就能用，不再要求本机有 node。
- **自选模型**：DeepSeek、通义千问，或任意 OpenAI 兼容端点，面板里随时切换。
- **第三方 / 官方一键切换**：有 Claude 订阅、想走官方时，面板顶部切到「官方 Claude」即可干净交回你真实的 Science 与订阅（CSSwitch 不插手你的官方登录、也不起代理与沙箱）。
- **原生保真**：DeepSeek 走原生 Anthropic 端点，thinking 与工具调用不失真。

**安全（绝不影响你真实的 Claude 登录与订阅）**

- **零真实凭证**：登录用本地自造的虚拟 OAuth，绝不复制、不修改、不删除你真实的 `~/.claude-science`。
- **与真实实例隔离**：沙箱用独立 HOME、独立端口、独立 data-dir，真实实例（端口 8765）零影响；脚本对真实目录与 8765 做失败关闭护栏。
- **密钥只在本地**：0600 存 `~/.csswitch`，经环境变量注入子进程（绝不进命令行与日志），界面只回显末 4 位掩码；入站 `Authorization` / `x-api-key` 一律剥离不转发；代理只监听回环地址并做路径 secret 鉴权。

## 快速开始

**前置**：装好 [Claude Science](https://claude.com)，本机有 `python3`（虚拟登录已是 Rust 原生实现，**不再需要 node**）。

1. 下载最新 [Release](../../releases/latest) 里的 `CSSwitch_*.dmg`，拖进「应用程序」。首次打开**右键 →「打开」**（未公证，属正常，见下）。
2. 打开 CSSwitch，弹出一个正常窗口（可拖动 / 缩放 / 最小化）。保持顶部「**第三方模型**」，选 provider，**粘贴你自己的第三方 API key**（只存本地 `~/.csswitch/config.json`，0600）。
3. 点「**一键开始**」。它会自动起代理、写虚拟登录、起隔离沙箱、开浏览器打开已登录的 Science。完事，开始用。

> 你唯一要提供的就是**你自己的第三方 API key**（你付费的 key，无法内置到 app 里）。其余全自动。
>
> **首次打开被 Gatekeeper 拦是正常的**：本 app 做了 ad-hoc 签名但未做 Apple 公证。右键 →「打开」，或到系统设置 → 隐私与安全性 →「仍要打开」。目前仅 arm64（Apple Silicon）。

开发者的命令行用法（手动起代理与沙箱）、构建与测试，见 [`docs/DEVELOPMENT.md`](./docs/DEVELOPMENT.md) 与 [`desktop/README.md`](./desktop/README.md)。

**Windows / WSL2 用户**：可将 CSSwitch 翻译代理以 Docker 容器方式部署在 WSL2 中，供 Windows 上的 OpenAI/Anthropic 兼容客户端使用。见 [`docs/wsl2-container-deployment.md`](./docs/wsl2-container-deployment.md)（实验性）。

## 更新计划（Roadmap）

以下为规划方向，不代表时间承诺。欢迎以 issue / PR 参与共建。

**更广泛的模型与 API 支持**

- 内建更多第三方 provider：Kimi（Moonshot）、智谱 GLM、OpenRouter、本地 Ollama 等。
- 面板内直接配置任意 OpenAI 兼容端点（自定义 `base_url`、模型名、鉴权头），无需改代码。
- 每个 provider 的模型映射与选择器展示可在界面里编辑。

**多学科的 Skill / MCP 支持**

- 面向社会学、政治学、计算机科学等多学科，整理开箱即用的 Skill 与 MCP 服务器清单。
- 学科工具包一键装配：统计分析、文献抓取、数据可视化、问卷与量表处理等。
- 与 Science 的工具调用、代码执行打通，形成各学科的研究工作流模板。

**体验与工程**

- Qwen 走真流式翻译，降低首 token 延迟（DeepSeek 已是原生透传真流式）。
- 继续收敛运行时依赖：把翻译代理移到 Rust（axum）以拔掉 python（虚拟登录的 node 依赖已在 v0.1.4 拔除），最终做到零外部运行时。
- Intel（x86_64）与 universal 构建；可选的正式签名与 Apple 公证。
- 面板增加日志查看、用量统计、更快的 provider 切换入口。

## 反馈与报错

遇到问题或有想法，欢迎在 GitHub 提交（比私信更利于跟踪与复用）：

- **报 bug**：[新建 Bug 反馈](https://github.com/SuperJJ007/CSswitch/issues/new?template=bug_report.yml)，或面板右下角「反馈 / 报 bug」直接跳转。
- **提功能 / 想支持的 API**：[新建功能建议](https://github.com/SuperJJ007/CSswitch/issues/new?template=feature_request.yml)。
- **附日志更快定位**：面板「日志」链接会打开 `~/.csswitch/logs/`（`proxy.log`、`sandbox.log`）。**贴之前务必先删掉任何 API key / 令牌。**

隐私：本项目**不含任何自动遥测 / 崩溃上报**，不会在后台把你的数据发给任何人。所有反馈都由你手动提交、内容由你决定。

## 风险与免责声明

- 本项目仅供**个人学习与研究**用途，**使用者自负风险**。
- 推理请求经本地代理直连你自己付费的第三方模型，**不经过 Anthropic 服务端**做推理，用的是本地自造的虚拟登录，**零真实 Anthropic 凭证**。
- Science 在**启动阶段**仍会尝试访问其硬编码的 profile / account 接口（`api.anthropic.com` / `claude.ai`）；代理对这些请求即时短路（返回「未登录」），Science 以未登录态正常启动、不影响第三方推理。因此本项目**不宣称**「完全零 Anthropic 接触」这类绝对说法。
- 虚拟登录下，**Anthropic 托管的远程 MCP 服务**（如 pubmed / clinical-trials / chembl / biorxiv，位于 `*.mcp.claude.com`）不可用：它们需真实 Anthropic 授权，代理已将其短路，Science 会自动跳过（启动日志有 `load failed (skipped)` 属正常）。**本地内置的 bio-tools MCP 仍正常可用。**
- 对 Science 登录令牌加密格式的逆向、以及「越过登录」的实现，可能触及相关服务条款与版权法规（如美国 DMCA §1201 反规避条款）。是否适用、有无豁免需专业法律判断。
- 本项目与 Anthropic **无任何从属、合作或背书关系**；不偷取算力（推理走你自付第三方）、不泄露用户密钥、不含恶意代码。
- 软件按「现状」提供，**不提供任何形式的担保**。

## 许可

[MIT](./LICENSE)。
