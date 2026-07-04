# CSSwitch

让 Claude Science 的模型推理走第三方 API（DeepSeek / 阿里通义千问 / 任意 OpenAI 兼容端点），保留 Science 那套 AI agent 科研体验，模型换成你自己的。类比 CC Switch 之于 Claude Code。

> 本文件只留**铁律 + 架构 + 速查指针**。详细内容分散在：
> 逆向与已验证事实 → [`docs/verified-facts.md`](docs/verified-facts.md)；
> 已知问题/待修队列 → [`docs/known-issues.md`](docs/known-issues.md)；
> 命令与构建 → [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) / [`desktop/README.md`](desktop/README.md)；
> 关键环境/进度事实 → 项目记忆 `memory/`（每次会话自动载入 `MEMORY.md`）。

## 一、铁律（最高优先级，任何会话都不得违反）

1. **绝不影响用户真实的订阅与登录状态。** 真实 Claude Science 的数据目录是 `~/.claude-science`，登录凭证在 `~/.claude-science/.oauth-tokens`、`active-org.json`、`encryption.key`、`orgs/`、`.key-backups/`。这些文件**只读都要谨慎，绝不复制、绝不修改、绝不删除**。
2. **绝不把真实 OAuth token 复制进任何沙箱。** 复制后两个实例共享同一 token，刷新时 Anthropic 可能轮换刷新令牌，导致用户真实实例被登出。要给沙箱登录，只能在沙箱里**全新独立登录**（另一套会话 token，对真实登录零影响），且由用户手动完成，Claude 不代做登录。
3. **绝不用改过的环境变量去启动用户的真实实例。** 真实实例跑在端口 8765。所有实验用的沙箱必须用**独立 data-dir + 独立端口 + 独立 HOME**，与 8765 完全隔开。
4. **测试默认不碰 Science。** 能用「代理↔上游」单独验证的，就不启动 Science（见 `test/`）。只有到最终整链联调、且用户明确同意时，才启动沙箱 Science，并且仍然遵守第 2、3 条。
5. 动任何有状态的东西前，先确认它不在铁律清单里；拿不准就停下来问用户。

## 二、架构

```
Claude Science（沙箱 · 虚拟登录；登录仅当启动门票，推理不走 Anthropic）
   │  ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>
   ▼
翻译代理 proxy/csswitch_proxy.py（默认 deepseek 原生透传 / qwen 翻译，可切）
   │  剥离入站 OAuth Bearer，注入第三方 key，按需 Anthropic ↔ OpenAI 互转，path-secret 鉴权
   ▼
DeepSeek 原生 Anthropic 端点 / 阿里 DashScope（千问）/ 其它 OpenAI 兼容端点
```

关键点：Claude 登录只是**启动 Science 的门票**，推理被 `ANTHROPIC_BASE_URL` 导去本地代理后，Anthropic 服务端不经手推理。门票用**本地伪造的虚拟 OAuth** 越过（零真实凭证、无需任何 Anthropic 账号）。

## 三、速查（详情见 docs/ 与 memory/）

- **必知三条**：① `ANTHROPIC_BASE_URL` 无条件生效；② 手动填 API key 被 operon 写死拒绝，**必须有 OAuth 门票**；③ 门票用本地伪造虚拟 OAuth 越过。完整证据与格式 → `docs/verified-facts.md`。
- **发布态 / 待办**：已公开于 github.com/SuperJJ007/CSSwitch；桌面 app 在 `desktop/`（Tauri **正常窗口**进程管家，已去托盘）。**当前 Latest 版本以 GitHub Releases / [`CHANGELOG.md`](CHANGELOG.md) 为准**；**待办队列与排期** → [`docs/known-issues.md`](docs/known-issues.md)。（此处不写具体版本号与「当前最优先」等易变状态，那些归 known-issues 与项目记忆 `memory/`。）
- **对外文案（脱敏）**：用户**可见**文案不露骨，别直说「越过 / 绕过登录」；主按钮用「一键开始」类中性说法。技术**内部**文档描述机制时可仍用「越过门票」。详见 `docs/known-issues.md` 第 1 条。
- **上游/模型**：默认 DeepSeek（原生 Anthropic 透传），可 `--provider qwen`（翻译）。模型映射与选择器广告 id 见 `csswitch_proxy.py` 的 `PROVIDERS`。
- **每日维护巡检**：launchd 每天 09:00/21:00（Asia/Shanghai）跑受限 `claude -p`，**只读仓库 + 抓公开网页 + 只往 `findings/auto-maint/` 写规划报告**（白名单工具、硬禁 commit/push/rm、禁读写 `~/.claude-science`、不启动 Science）。装卸看：`scripts/install-maintenance.sh {install|uninstall|status|run}`。

## 四、目录与常用命令

```
proxy/csswitch_proxy.py           【主】provider 可切代理（deepseek 默认 / qwen）
proxy/qwen_proxy.py               早期单 provider(千问)版，已被上者取代
scripts/make-virtual-oauth.mjs    虚拟 OAuth 伪造器（Node，字节级一致；只写沙箱，护栏拒真实目录）
scripts/launch-virtual-sandbox.sh 起沙箱 Science + 写虚拟登录 + 指向代理（推荐入口）
scripts/stop-science-sandbox.sh   停沙箱（按 data-dir，绝不影响真实 8765）
scripts/doctor.sh                 只读环境诊断（依赖/key 有无/端口/权限/铁律自检）
scripts/verify-proxy.sh           校验运行中的代理（/health + /v1/models，零上游花费）
scripts/self-test.sh              离线回归套件（test/run_all.sh 包装）
scripts/*maintenance*             每日巡检 wrapper/提示词/launchd/安装器
desktop/                          Tauri 桌面 app（正常窗口，构建见 desktop/README.md）
test/                             隔离回归测试（只打代理，不碰 Science）
findings/                         证据、二进制分析、诊断记录；auto-maint/ 是巡检输出（git 忽略）
.sandbox/                         沙箱 Science 独立 HOME/data-dir（git 忽略）
```

常用命令（更多见 `docs/DEVELOPMENT.md`）：
- 起代理（默认 DeepSeek）：`DEEPSEEK_API_KEY=... python3 proxy/csswitch_proxy.py --provider deepseek --port 18991`（切千问用 `--provider qwen` + `DASHSCOPE_API_KEY`；也支持 `--env-file`）
- 离线回归：见 `test/`（自动起代理、打完停掉）。
- 整链（虚拟登录，推荐）：先起代理，再 `scripts/launch-virtual-sandbox.sh --port 8990 --proxy-url http://127.0.0.1:18991/<secret>`；停：`scripts/stop-science-sandbox.sh`。

## 五、环境备忘

- 真实 Science 数据目录 `~/.claude-science`、端口 8765，绝对不碰。
- 上游 key 在用户 shell 环境：`DEEPSEEK_API_KEY` / `DASHSCOPE_API_KEY`（值不显示、不入库）。DashScope 兼容端点 `https://dashscope.aliyuncs.com/compatible-mode/v1`。
- **代理端口坑（gh/git/operon 都踩，反复中招）**：小写 `https_proxy`/`http_proxy`/`ALL_PROXY`→`127.0.0.1:7890`（**活**）；大写 `HTTPS_PROXY`/`HTTP_PROXY`→`127.0.0.1:8001`（**死，连接被拒**）。gh（Go）读大写 → 连不上 GitHub，**还会把 token 误报 `invalid`（假阴性，别当成要重新登录）**。**跑任何 gh/git 前先**：`export HTTPS_PROXY=http://127.0.0.1:7890 HTTP_PROXY=http://127.0.0.1:7890 ALL_PROXY=http://127.0.0.1:7890`。operon 认小写、本就走 7890。详见记忆 `github-repo`。
- Python 用 conda 环境（见用户全局记忆），避免系统 3.9。
