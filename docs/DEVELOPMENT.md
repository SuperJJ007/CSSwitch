# CSSwitch 开发交接 / 现状

面向「零上下文接手」的开发者或独立开发任务。读完这份就能继续把 Tauri app 编译、测试、跑通、发版。

## 铁律（最高优先级，任何改动不得违反）

见 [`../CLAUDE.md`](../CLAUDE.md) 第一节。核心四条：
1. 绝不复制/修改/删除真实 `~/.claude-science`（含 `.oauth-tokens`/`encryption.key`/`active-org.json`）。
2. 绝不把真实 OAuth token 复制进沙箱；沙箱只用本地自造的虚拟令牌。
3. 绝不用改过的环境去起真实实例；真实端口 8765，沙箱一律独立 HOME+端口+data-dir。
4. 测试默认不碰 Science；整链冒烟须用户明确同意、且在场（碰 Science 的手测）。

## 命名规范

项目名统一写作 **CSSwitch**（C-S-S，读作 CS Switch；CS = Claude **S**cience，呼应「CC Switch 之于 Claude Code」）。
- 仓库名 2026-07-04 从 `CSswitch` 改为 `CSSwitch`（GitHub URL 大小写不敏感，旧链接照常）。
- app productName、CHANGELOG、README、封面图、代码文案一律 `CSSwitch`。
- 例外保持不动：bundle id `com.csswitch.*`（小写惯例）；本机真实克隆目录 `/Users/superjj/ccproj/CSswitch`（磁盘真实名，改它会断脚本 / 记忆路径）。
- **对外文案脱敏**：用户可见文案不直说「越过 / 绕过登录」，主按钮用「一键开始」类中性说法；技术内部文档描述机制时可仍用「越过门票」。

## 分层

CSSwitch = 翻译代理（Python）+ 虚拟登录伪造器（**Rust 原生**）+ 隔离脚本（shell）+ **正常窗口 app（Tauri）**。

app 是**进程管家**：Rust 后端起停子进程、注入环境变量、读写配置、探活、跑切换事务；已验证的越权/翻译逻辑仍留在 `proxy/`、`scripts/` 里被当子进程调用（保住护栏与已验证行为，Rust 侧最小）。虚拟 OAuth 伪造已移进 Rust（`src-tauri/src/oauth_forge.rs`，字节级一致、护栏拒真实目录），**app 运行不需要 Node.js**；`scripts/make-virtual-oauth.mjs` 是等价的 Node 独立版，仅命令行单独用时才需要 node。

## 现状（截至 2026-07-04，`main`，最新发布 v0.3.2）

**已发布**：v0.3.2（Latest）—— 见 [`../CHANGELOG.md`](../CHANGELOG.md) 与 GitHub Releases（此处不复述版本号，以那两处为准）。

**能力面**：
- **多 profile 配置管理**（cc-switch 式）：7 家 provider 模板（DeepSeek / 通义千问 / 智谱 GLM / Kimi / MiniMax / 小米 MiMo / 硅基流动 / OpenRouter）+ 自定义端点；同一家可保存多套（不同 key / 模型）；JSON 存储 `~/.csswitch/config.json`（schema v2，v1→v2 一次性迁移），key 明文 0600、只回掩码。
- **provider 分型**：native（deepseek/qwen，`--provider` 走各自固定端点）vs relay（其余，anthropic 兼容透传、双鉴权、带 `base_url`）。qwen 是唯一走 OpenAI↔Anthropic 翻译的（DashScope 只 OpenAI 端点）；deepseek/relay 都原生 anthropic 透传。
- **模型选择（#9，v0.3.2）**：全 relay 家「选一个模型 → force」；模型控件是「下拉精选 + 自填」；代理 force 时 `/v1/models` 回单壳 `claude-opus-4-8`+真实 `display_name`，Science 顶部选择器显示真实模型名。
- **UI**：正常窗口 420×700（`decorations:true`，已去托盘/菜单栏），配置列表 + chip 网格 + 三能力模型呈现（native/relay）。
- **切换事务**：`set_active`/连接编辑经串行器走「scratch 校验候选 → 起正式代理探活 → 健康才提交 active_id」，失败回滚不停沙箱。

**待办**（详见 [`known-issues.md`](known-issues.md)）：#12 自定义校验 scratch 误判（需复现）；#2/#6 DeepSeek DSML tool_use 泄漏兜底（shim 已成形、默认关）；轨道 2 代理移 Rust（axum）拔 python；Intel/Universal 构建 + 公证。

## 源码结构（`desktop/`）

```
desktop/src/                     前端面板（原生 HTML/CSS/JS，无框架，逻辑内联 main.js）
  index.html  styles.css  main.js
desktop/src-tauri/src/
  lib.rs           tauri command + 切换事务 + launcher（起代理/沙箱、注入 env）+ 状态灯
  config.rs        ~/.csswitch/config.json 读写：dir 0700 / file 0600、lstat 拒符号链接、
                   原子写、key 掩码、schema v2、v1→v2 迁移、relay 空 model 回填（甲）
  config_legacy.rs v1（旧固定槽）结构，仅迁移用
  templates.rs     provider 模板注册表（单一来源）：adapter/base_url/是否必选模型/内置模型/thinking 策略
  lifecycle.rs     串行器（切换事务加锁）+ generation token
  scratch.rs       候选连接的临时代理探测（Models/Message），起完即杀，绝不碰正式链路
  oauth_forge.rs   Rust 原生虚拟 OAuth 伪造（护栏拒真实目录）
  proc.rs          纯 std：TCP /health 探活（带 path-secret）、which、/dev/urandom secret、上游可达性
  main.rs          入口
  tauri.conf.json  正常窗口 420×700；bundle.resources 打包运行所需 proxy/scripts allowlist；qwen_proxy.py 为 legacy/compat 资源
```

## 前后端 command 契约

前端只调 Rust command；key 完整值永不进前端，只回显掩码。**坑：Tauri 顶层多词命令参数用 camelCase**（`templateId`/`baseUrl`/`skipVerify` 等），serde 结构体入参（如 `req`）内部字段仍 snake_case。

- **配置读写**：`get_config`（→ `{profiles, templates, active_id, proxy_port, sandbox_port, mode, pending_notice}`，key 掩码）、`create_profile`、`update_profile_connection`（改 base_url/model/key）、`update_profile_metadata`（改名/备注）、`delete_profile`、`clear_profile_key`、`set_active_profile`（设为当前，经切换事务）、`list_templates`、`set_settings`、`set_mode`（proxy/official）。
- **模型**：`fetch_models`（起临时代理探 `/v1/models`，回真实 id + 内置合并）、`verify_key`。
- **运行控制**：`start_proxy`、`stop_all`、`one_click_login`（→ `{url}`：起代理→写虚拟登录→起沙箱→返回 Science URL）、`status`（→ 代理/沙箱/上游 三灯 green/amber/red）。
- **打开/工具**：`open_url`、`open_logs`、`open_official`、`open_release_page`、`report_bug`、`run_doctor`、`app_version`、`quit_app`。

## 命令与构建

```bash
# 起代理（默认 DeepSeek）
DEEPSEEK_API_KEY=... python3 proxy/csswitch_proxy.py --provider deepseek --port 18991
# 切千问：--provider qwen + DASHSCOPE_API_KEY；relay 家：--provider relay + CSSWITCH_RELAY_BASE_URL/KEY/MODEL
# 也支持 --env-file

# 编译 / 跑 / 打包（desktop/，需 node 装 @tauri-apps/cli；产物含 .app / .dmg）
cd desktop && npm install         # 首次装 tauri CLI
npm run tauri dev                 # 开发跑
npm run tauri build               # 打包 → src-tauri/target/release/bundle/dmg/CSSwitch_<ver>_aarch64.dmg
```

## 离线回归

```bash
bash test/run_all.sh                        # python 单元 + node 伪造器 + bash 脚本三件套（不碰 Science、不联网）
python3 -m pytest test/test_proxy_units.py  # 代理纯逻辑单测（40）
cd desktop/src-tauri && cargo test          # Rust 后端单测（122）；配 cargo clippy --all-targets -- -D warnings + cargo fmt --check
node --check desktop/src/main.js            # 前端语法（不加 node 测试依赖，前端逻辑预览手验）
```

## 整链冒烟（碰 Science，须用户同意 + 在场，守铁律 2/3/4）

不必驱动 GUI，直接跑 app 编排的同一条链（独立 HOME + 端口 + data-dir，绝不碰 8765）：
```bash
# 1. 起代理（带 path-secret，模拟 app）；relay 家用 CSSWITCH_RELAY_* env
SEC=$(python3 -c "import secrets;print(secrets.token_hex(16))")
CSSWITCH_RELAY_BASE_URL=https://open.bigmodel.cn/api/anthropic CSSWITCH_RELAY_KEY=<key> CSSWITCH_RELAY_MODEL=glm-5.2 \
  python3 proxy/csswitch_proxy.py --provider relay --port 18996 --auth-token "$SEC" &
# 2. 起沙箱，proxy-url 带 secret 前缀（= one_click_login 干的事；护栏用 --dry-run 走干跑）
scripts/launch-virtual-sandbox.sh --port 8990 --proxy-url "http://127.0.0.1:18996/$SEC"
# 3. 验：沙箱 /health；取带票 URL（daemon 重启后浏览器旧 session 失效，重取）
curl -s http://127.0.0.1:8990/health
HOME=<沙箱HOME> '/Applications/Claude Science.app/Contents/Resources/bin/claude-science' url --data-dir <沙箱data-dir>
# 4. 停
scripts/stop-science-sandbox.sh
```
关键观察点：Science 把推理打到 `ANTHROPIC_BASE_URL=.../$SEC` 后能否被 path-secret 认证放行；relay force 时顶部选择器显示真实模型名。**注意 `--dry-run` 是 flag（不是 `DRY_RUN=1` 环境变量），用错会真启动沙箱。**

## 怎么装 Rust（国内网络，实测坑）

官方源 `static.rust-lang.org` 极慢。首选镜像：
```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn   # 或 https://mirrors.ustc.edu.cn/rust-static
rustup toolchain install stable --profile minimal && rustup default stable
```
crates.io 也走镜像，写 `~/.cargo/config.toml`：
```toml
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```

## 发版流程（v0.3.2 实操记录）

```bash
# 0. 功能分支合 main（真机验证过后），跑全绿：cargo test / clippy / fmt / pytest / node --check
# 1. 版本号 bump（5 处一致）：desktop/package.json + package-lock.json（根 + packages[""]）
#    + src-tauri/Cargo.toml + Cargo.lock（desktop 包）+ tauri.conf.json
# 2. CHANGELOG.md 加条目（已修问题从 known-issues「毕业」到这里）；README 若有能力面变化一并改
# 3. gh/git 前先： export HTTPS_PROXY=http://127.0.0.1:7890 HTTP_PROXY=http://127.0.0.1:7890 ALL_PROXY=http://127.0.0.1:7890
#    （大写默认 8001 是死的，gh 会误报 token invalid）
git push origin main
# 4. 打包 dmg：cd desktop && npm run tauri build
# 5. tag + Release
git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z
gh release create vX.Y.Z --title "..." --notes-file <notes> <dmg 路径>
# 6. 发布前建议 gitleaks 扫（工作树/暂存/历史三处）
```
当前 dmg 未 Apple 公证（无 APPLE_* 凭证），首次启动右键「打开」；仅 Apple Silicon（arm64）。
