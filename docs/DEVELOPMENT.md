# CSSwitch 开发交接 / Phase 1 GUI 现状

面向「零上下文接手」的开发者或独立开发任务。读完这份就能继续把 Tauri 菜单栏 app 编译、测试、跑通。

## 铁律（最高优先级，任何改动不得违反）

见 [`../CLAUDE.md`](../CLAUDE.md) 第一节。核心四条：
1. 绝不复制/修改/删除真实 `~/.claude-science`（含 `.oauth-tokens`/`encryption.key`/`active-org.json`）。
2. 绝不把真实 OAuth token 复制进沙箱；沙箱只用本地自造的虚拟令牌。
3. 绝不用改过的环境去起真实实例；真实端口 8765，沙箱一律独立 HOME+端口+data-dir。
4. 测试默认不碰 Science；整链冒烟须用户明确同意（本轮用户已同意，见下「冒烟」）。

## 命名规范

项目名统一写作 **CSSwitch**（C-S-S，读作 CS Switch；CS = Claude **S**cience，呼应「CC Switch 之于 Claude Code」）。
- 仓库名 2026-07-04 已从 `CSswitch` 改为 `CSSwitch`（GitHub URL 大小写不敏感，旧链接 / 克隆 / dmg 下载 / 更新检查照常）。
- app productName、CHANGELOG、README、封面图、代码文案一律 `CSSwitch`；新增文案 / URL 都用 `CSSwitch`。
- 例外保持不动：bundle id `com.csswitch.*`（小写惯例）；本机真实克隆目录 `/Users/superjj/ccproj/CSswitch`（磁盘真实名，改它会断一批脚本 / 记忆路径）。

## 分层

CSSwitch = 翻译代理（Python）+ 虚拟登录伪造器（Node）+ 隔离脚本（shell）+ **菜单栏 app（Tauri，本阶段）**。
app 只是**进程管家**：Rust 后端起停子进程、注入环境变量、读写配置、探活；已验证的越权/翻译逻辑仍留在
`proxy/`、`scripts/` 里被当子进程调用。这样保住护栏与已验证行为，Rust 侧最小。

## 现状（截至 2026-07-02，分支 `phase1-gui`）

**已完成并提交：**
- Phase 0 加固（8 项 P1/P2）→ 已在 `main`。
- Phase 2 非 Rust 部分：`LICENSE`、`README.md`、`.gitleaks.toml`（历史+工作树扫描 0 泄露）、
  `scripts/doctor.sh`/`verify-proxy.sh`/`self-test.sh` + 12 测试（并入 `test/run_all.sh`，全绿）。
- Phase 1 **已编译通过 + `cargo test` 16/16 + 独立安全复查（修掉 1 Critical + 2 Important + 若干 Minor）
  + 绕过登录整链冒烟全过**（虚拟登录 → Science 经 path-secret 打代理 → 真实推理直连 DeepSeek 拿回补全）。

**Phase 1 源码（`desktop/`）：**
```
desktop/src/                     前端面板（原生 HTML/CSS/JS，来自已批准的 mockup 样式）
  index.html  styles.css  main.js
desktop/src-tauri/src/
  config.rs   ~/.csswitch/config.json 读写：dir 0700 / file 0600、lstat 拒符号链接、
              临时文件+原子 rename、key 掩码（只留末 4 位）。含 10 个内联单测。
  proc.rs     纯 std：TCP /health 探活（带 path-secret）、which、/dev/urandom 生成 secret、
              上游 TCP 可达性。含 5 个单测。
  lib.rs      托盘图标（左键切换面板显隐）、Accessory 激活策略（不占 Dock）、失焦即隐、
              Mutex<AppState>，10 个 command（见下）。
  tauri.conf.json   窗口配成 340×560 无边框置顶隐藏面板
  Cargo.toml        tauri（tray-icon 特性）+ serde
```

**前后端 command 契约（前端只调这些；key 完整值永不进前端，只回显掩码）：**
| command | 入参 | 返回 |
|---|---|---|
| `get_config` | — | `{provider, proxy_port, sandbox_port, keys:{deepseek,qwen}}`（keys 是掩码） |
| `set_config` | `{cfg:{provider,proxy_port,sandbox_port}}` | — |
| `save_provider_key` | `{provider, key}` | 掩码串 |
| `start_proxy` | — | `{port}` |
| `stop_all` | — | — |
| `one_click_login` | — | `{url}` |
| `status` | — | `{proxy,sandbox,upstream}`（各 green/amber/red） |
| `open_url` | — | —（开上次沙箱 URL） |
| `run_doctor` | — | doctor 文本输出 |
| `quit_app` | — | —（停代理、留沙箱、退出） |

## 进度

- [x] **编译**：`cd desktop/src-tauri && cargo build`（首次拉 Tauri 依赖走 rsproxy 镜像；见下）。盲写的 lib.rs 一次编过，Tauri v2 API 无需改。
- [x] **`cargo test`** 16/16：config/proc 的权限 0700/0600、符号链接拒绝、原子写、掩码、探活、which、secret、redact、update 全绿。
- [x] **独立安全复查**（子 agent，opus）：修掉 1 Critical（path-secret 漏进日志/前端）+ 2 Important（ensure_proxy 并发原子性、repo_root 攻击面）+ 若干 Minor（8765 守卫、gen_secret 失败关闭、config RMW 串行、doctor 占位 key、锁 poison 恢复）。
- [x] **绕过登录整链冒烟**：见文末「绕过登录冒烟」，全过；§12.3 主方案（base_url 带 path-secret）已验证可行。
- [ ] **发布 v1**：过 spec §12.2 法律/条款 go-no-go，用户拍板，公开推送前再跑一遍 gitleaks（工作树/暂存区/历史三处）。

### 曾经的 Tauri v2 API 关注点（现已验证 OK，留档）
`app.set_activation_policy(ActivationPolicy::Accessory)`、`tauri::tray::{TrayIconBuilder,TrayIconEvent,MouseButton,MouseButtonState}` 的 `Click{button,button_state,..}`、`WindowEvent::Focused(false)`、`get_webview_window("main")`、`default_window_icon().unwrap().clone()` —— 均按上述写法编译通过。

## 怎么装 Rust（国内网络，实测坑）

官方源 `static.rust-lang.org` 极慢。首选镜像：
```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn   # 或 https://mirrors.ustc.edu.cn/rust-static
rustup toolchain install stable --profile minimal
rustup default stable
```
若 rustup 自带下载器对大文件（`rustc` ~65MB）仍超时，直接 curl 拉三个组件 tarball 再本地装：
```bash
BASE=https://rsproxy.cn/dist/<日期>/    # 日期见 rustup 报的 "latest update on"
# 分别下 rustc / cargo / rust-std 的 <name>-<ver>-aarch64-apple-darwin.tar.xz
#（网络限单连接时用「分片并发 curl -r 范围 + 拼接」对抗限速），各自 install.sh --prefix=~/.rustlocal
```
crates.io 也走镜像，避免 `cargo build` 拉 ~400 依赖时再卡一次，写 `~/.cargo/config.toml`：
```toml
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```
（Xcode CLT、node 已具备。）

## 绕过登录冒烟（用户已同意，仍守铁律 2、3）

目的：验 `一键越过登录` 整链，且验掉 spec §12.3 的 path-secret 拼进 base_url 兼容性。
不必驱动 GUI，直接跑 app 编排的同一条链即可：
```bash
# 1. 起代理（带 path-secret，模拟 app 行为）
SEC=$(python3 -c "import secrets;print(secrets.token_hex(16))")
DEEPSEEK_API_KEY=<真实key> python3 proxy/csswitch_proxy.py --provider deepseek --port 18991 --auth-token "$SEC" &
# 2. 起沙箱，proxy-url 带 secret 前缀（= one_click_login 干的事）
scripts/launch-virtual-sandbox.sh --port 8990 --proxy-url "http://127.0.0.1:18991/$SEC"
# 3. 验：沙箱 /health、Science authenticated:true、（有 key 时）驱动一次推理确认走第三方
curl -s http://127.0.0.1:8990/health
# 4. 停
scripts/stop-science-sandbox.sh
```
关键观察点：Science 把推理打到 `ANTHROPIC_BASE_URL=.../$SEC` 后，是否正确在其后拼 `/v1/...`，
代理能否用 path-secret 认证放行（不通则退备用方案：`ANTHROPIC_CUSTOM_HEADERS` 注入 token，见 spec §7.2）。

## 离线回归

```bash
bash test/run_all.sh        # python 单元 + node 伪造器 + bash 脚本/运维三件套（不碰 Science、不联网）
cd desktop/src-tauri && cargo test   # Rust 后端纯逻辑单测
```
