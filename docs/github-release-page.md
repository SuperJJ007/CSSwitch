# GitHub 页面布置 + 首个 Release 设计

面向「公开全量 + 免责声明」（spec §12.1 已定）。这份是**页面怎么摆**的成品设计：仓库首页、About、
topics、社交预览、v0.1.0 Release 文案、以及公开推送的执行清单。措辞对普通用户友好，避免绝对断言。

---

## 1. 仓库首页（README 顶部）

已落地在 `README.md` 顶部：

- **横幅**：`docs/assets/social-preview.png`（1280×640，Claude 米白底 + 陶土图标 + 人话特性标签）。
- **徽章**：MIT License · macOS (Apple Silicon) · Tauri 2。
- 首屏一句话定位 + 免责声明紧随其后（已在 README）。

后续小节顺序（README 现状，无需重排）：定位 → 免责声明 → 背景 → 架构 → 铁律 → 快速开始 →
安装后自检 → 目录 → 测试 → **更新计划（Roadmap）** → 风险与边界 → 许可。

## 2. About（仓库右上「简介」栏，一句话）

```
让 Claude Science 的推理走自选的第三方 API（DeepSeek / 通义千问 / 任意 OpenAI 兼容端点），
保留工具调用·Skill·MCP·代码执行，含本地虚拟登录跳过。macOS 菜单栏 app。
```

Website 栏留空（无官网）。「Releases」「Packages」勾选显示。

## 3. Topics（标签，便于检索）

```
claude  claude-science  anthropic  deepseek  qwen  dashscope
openai-compatible  llm  proxy  mcp  tauri  menubar  macos
```

## 4. 社交预览图（Settings → Social preview）

上传 `docs/assets/social-preview.png`（GitHub 要求 1280×640，本图正好）。这决定别人分享链接时的卡片长相。

## 5. 首个 Release：v0.1.0

- **Tag**：`v0.1.0`；**标题**：`CSSwitch v0.1.0 — macOS 菜单栏 app`。
- **附件**：`CSSwitch_0.1.0_aarch64.dmg`（arm64）。
- **正文草稿**：

  ```markdown
  CSSwitch：让 Claude Science 的推理走你自选的第三方 API，Science 那套「AI Jupyter」体验照旧。

  ### 能做什么
  - 保留 Science 的**工具调用、代码执行、Skill、MCP**，模型换成自选的第三方 API。
  - **跳过登录**：本地自造虚拟 OAuth 门票，零真实 Anthropic 凭证。
  - **一个菜单栏 app** 管好一切：选 provider、填 key、起停代理、一键越登录、状态灯。
  - 支持 **DeepSeek、通义千问（Qwen）**，或任意 **OpenAI 兼容端点**（自定义 API）。

  ### 安装（macOS · Apple Silicon）
  1. 下载 `CSSwitch_0.1.0_aarch64.dmg`，拖进「应用程序」。
  2. 首次打开被 Gatekeeper 拦是正常的（本 app 做了 ad-hoc 签名但**未做 Apple 公证**）：
     **右键 →「打开」**，或系统设置 → 隐私与安全性 →「仍要打开」。
  3. 菜单栏出现开关图标，点开面板，填第三方 key 即可用。

  ### 已知限制
  - 产物仅 **arm64（Apple Silicon）**；Intel Mac 需自行 x86_64 / universal 构建。
  - **未公证**：企业/严格 Gatekeeper 环境可能仍拦；需 Apple Developer ID 才能公证（本项目不提供）。
  - **Qwen 流式**目前是「上游整段完成后再 SSE 回放」，首 token 延迟≈整段生成时间（DeepSeek 默认走原生透传，真流式）。

  ### 免责
  见仓库 README「免责声明」：个人研究用途；推理不经 Anthropic；逆向与「越过登录」可能触及服务条款
  与 DMCA §1201，使用者自负风险；与 Anthropic 无从属或背书关系；软件按现状提供、无担保。
  ```

## 6. 公开推送执行清单（公开前逐条过）

> 创建公开仓库 / 推送属对外发布动作，须用户明确点头；`gh` 需先 `gh auth refresh` 重新认证。

1. `gh auth refresh -h github.com`（用户自己做，Claude 不代登录）。
2. **再跑一遍 gitleaks**：工作树 + 暂存区 + 历史三处，确认 0 泄露（含新加的 `docs/assets/`、图标资源）。
3. `git remote add origin <用户确认的仓库>`，仓库名建议 `CSSwitch`（与本地一致）。
4. `git push -u origin main`。
5. `gh release create v0.1.0 <dmg 路径> --title ... --notes-file <第 5 节正文>`。
6. 仓库设置：填 About（第 2 节）+ Topics（第 3 节）+ 上传 Social preview（第 4 节）。
7. 过 spec §12.2 法律/条款 go-no-go（最后可反悔的闸），确认后才真正公开。

## 7. 待用户拍板

- 仓库名（默认 `CSSwitch`）、公开/私有（§12.1 已选公开全量）、GitHub 账号（`junjieashan`）。
- 是否随源码一并发 `.dmg`（附在 Release）。
- 是否现在就做 universal（Intel）构建（需再装 x86_64 工具链，国内网络较慢）。
