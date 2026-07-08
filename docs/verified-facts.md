# 已验证的事实与逆向记录

从 CLAUDE.md 拆分出来的详细技术记录（CLAUDE.md 只留铁律 + 架构 + 指针）。这些是**有证据、别重复推导**的结论，来自对二进制 `/Applications/Claude Science.app/Contents/Resources/bin/claude-science`（内部代号 operon）的静态分析 + 实测。证据文件在 `findings/`。

## 一、启动与鉴权

1. **base_url 无条件生效**：`LJ()` 直接读 `process.env.ANTHROPIC_BASE_URL`，登录后推理请求打到它。实测登录态下 Science 向本地代理发出 `GET /v1/models`、`POST /v1/messages`。
2. **手动 API key 被写死拒绝**：凭证解析器 `HLO.resolve()` 只认 OAuth（`_tryOauthToken`），`_tryManualApiKey()` 恒返回 `null`；还有守卫把「等于环境变量的凭证」置空。所以**完全不登录 + 只填 key 走不通**，必须有 OAuth 门票。早期「隔离 HOME 后 mock 收到 0 请求」是假阴性：隔离把登录也隔离了，Science 在发 HTTP 前就因无 OAuth 终止。
3. **虚拟 OAuth（本地自造令牌，零 Anthropic、零真实凭证）已跑通整链**（2026-07-02，证据 `findings/e2e-virtual-oauth-fullchain-proof.log`）：
   - 登录门票**不必真登录**：在沙箱 auth_dir 伪造一份加密令牌即可让 Science 认为已登录。
   - 令牌文件 `.oauth-tokens/<account_uuid>.enc`，格式 `"v2:"+base64(IV(12)‖AES-256-GCM‖tag(16))`；派生 `hkdfSync("sha256", base64(OAUTH_ENCRYPTION_KEY), Buffer.alloc(0), "operon:aes-256-gcm:oauth", 32)`，AAD=`v2:oauth`，明文是 token blob JSON。目录里须**恰好一个** `.enc`。
   - `encryption.key` 是换行分隔 `KEY=VALUE`：`OAUTH_ENCRYPTION_KEY`/`ANTHROPIC_API_KEY_ENCRYPTION_KEY`/`USER_SECRET_ENCRYPTION_KEY`(base64≥16B) + `JWT_SIGNING_SECRET`(≥16 字符)。keychain 镜像账号按**路径 SHA256** 派生（`encryption.key-<hash12>`），沙箱与真实天然隔离；本机实测 keychain 写入超时被跳过，纯用文件。
   - 关键坑：`token_expires_at` 必须设远期 ISO 串（如 `2099-01-01T...Z`），否则 `qP()` 判过期 → `_refreshToken` 联网打 `platform.claude.com` → 失败即无凭证。`provider="claude_ai"`，scopes=`user:inference user:file_upload user:profile user:mcp_servers user:plugins`。
   - `subscription_type` 由令牌自填、启动/鉴权阶段**不做服务端付费校验**（profile/account 走硬编码 api.anthropic.com，失败无害）。**无需任何 Anthropic 账号**。
   - 工具：`scripts/make-virtual-oauth.mjs`(Node，字节级一致) + `scripts/launch-virtual-sandbox.sh`。
   - Science 自身 `GET /api/auth/status` 返回 `authenticated:true, email:virtual@localhost.invalid`。
   - 沙箱守护 API：身份取自磁盘令牌；写操作需 `Origin: http://localhost:<port>` + 双提交 CSRF（cookie `operon_csrf` 回显头 `x-operon-csrf`）；建会话 `POST /api/frames {project_id}`，发消息 `POST /api/frames/:id/message {input_data:{request:"..."}, model}`（**用户文本键是 `request`，不是 text**）。
   - **沙箱钥匙串弹窗（已修 2026-07-02）**：Science 会把 `encryption.key` 镜像进 macOS 钥匙串；沙箱独立 HOME 下无钥匙串 → securityd 反复弹「找不到钥匙串」。修法：`launch-virtual-sandbox.sh` 在沙箱 HOME 内建一个独立、空密码、不自动锁的 `login.keychain-db`，只在 `HOME=$SANDBOX_HOME` 上下文里操作。核对前后**真实** `~/Library/Keychains` 逐字节不变。
4. **Claude Science native 基线有版本漂移（2026-07-08 只读复核）**：
   - 本机 `/Applications/Claude Science.app` 的 `Info.plist` 与 `claude-science --version` 均为 `0.1.0-dev.20260630.t212931.sha2bc1ac8 (release, public)`。
   - 仓库本地缓存证据 `.science-binaries/README.md` 记录过 `0.1.15-dev.20260701.t220242.shaaa553de`，但该目录为本地证据缓存、未入公开包；本次未读取、未复制、未修改真实 `~/.claude-science`。
   - 已有 route diff 记录 `0.1.15-dev` 相比 `0.1.0-dev` 新增 `/api/auth/nonce`、`/api/auth/`、`/api/conda/conda-remote`、`/api/credentials/openalex/validate`、`/api/frames/:id/token-series`、`/api/preferences/conda-mirror`、`/api/preferences/conda-mirror/probe`、`/api/pypi/pypi-remote/simple`、`/api/skills/:name/resync`，未见删除路由。
   - 公开 Anthropic 发布页只确认 Claude Science beta 可用于 macOS/Linux，未公开版本号或 changelog；版本判断仍以本机 plist/二进制与本地缓存证据为准。

## 二、代理与整链

5. **翻译代理 ↔ 真实通义千问整条链路已跑通**（`proxy/qwen_proxy.py`，隔离环境，未碰 Science/OAuth/CC Switch）：`/v1/models`、非流式、流式 SSE、tool_use 发起、tool_result 回喂后接着作答全部通过；入站 OAuth Bearer 逐条确认被剥离。证据 `findings/e2e-proxy-qwen-proof.log`。
6. **CC Switch 的代理是完整翻译器，但不能当独立 sidecar 复用**（2026-07-03 读 v3.16.5 源码复核，farion1231/cc-switch，**MIT**）：
   - 早期二进制观察（留存）：含 `/v1/messages`、`/v1/chat/completions`、`cc_switch_transform_error`、两套协议字段与 SSE 桥接，内建模型目录含 DeepSeek/Qwen/Kimi；端口默认 `127.0.0.1:15721`。
   - **无 headless / CLI / 独立二进制**：代理只在它 Tauri GUI 进程内跑，构造即绑死它的 SQLite `Database`（`ProxyServer::new(config, Arc<Database>, Option<tauri::AppHandle>)`），每个请求都查该 DB 选 provider。**没有可 spawn 的 sidecar**，`ANTHROPIC_BASE_URL` 无处可指，除非把它整个 app 一起打包。
   - **翻译契合度极高**：`forwarder.rs` 对入站 `authorization/x-api-key/x-goog-api-key` 一律丢弃、换成 adapter 提供的上游鉴权头（`AuthStrategy`：Anthropic→x-api-key / Bearer / Google→x-goog-api-key / OAuth），正是我们「丢弃 Science 虚拟 OAuth、注入第三方 key」所需；`providers/transform*.rs` 双向 Anthropic↔OpenAI/Responses/Gemini，含 SSE + tool_use/tool_result。
   - **两个缺口**：① 入站**无鉴权**（无 path-secret，仅靠 bind localhost）→ 复用要自己加门；② 配置**存 SQLite、非配置文件**（provider 行 + `apiFormat` 字段 `anthropic|openai_chat|openai_responses|gemini_native`，由 GUI/IPC 灌），不是我们能直接写的文件。
   - **结论**：复用 = 把它的 MIT `transform*.rs` 等翻译模块**移植/vendor 进我们自己的（Rust）代理**当参考实现，不是插它的二进制。license MIT（署名即可），但仓库周更（v3.16.5、~2050 commits），fork 有持续跟进成本。证据：本会话 general-purpose 研究 agent（引用 `src-tauri/src/proxy/*`）。
7. **DeepSeek 接入（默认上游，2026-07-02）**：主代理 `proxy/csswitch_proxy.py`，provider 可切（`--provider deepseek|qwen`）。
   - DeepSeek 走**原生 Anthropic 端点** `https://api.deepseek.com/anthropic/v1/messages`，鉴权头 `x-api-key`，代理只「改模型名 + 换鉴权 + 归一化 thinking + 夹 max_tokens + 重试」，**不翻译协议** → thinking/tool_use 原生保真。
   - 模型：`claude-opus-4-8→deepseek-v4-pro`、`claude-haiku/sonnet→deepseek-v4-flash`。
   - **模型选择器主列表机制（逆向 `s0`/`ZjO`/`XjO`/`hB_`）**：① `s0`：id 必须以 `claude-` 开头否则不显示。② 只有 `ZjO(id)<3`（opus=0/sonnet=1/haiku=2/其它=3）且 `XjO(id)` 命中 `^claude-(opus|sonnet|haiku)-<纯数字版本>$` 的 id 才进【主列表】，每 family 一个；其余折叠进「More models」。所以要让第三方模型平铺，就挂在 `claude-opus-4-8`/`claude-haiku-4-5` 这类主列表 id 上、显示名照写第三方。
   - **两处透传坑（已修）**：① Science 发 `thinking.type:"auto"` → 归一化 `adaptive`；② 强制 `tool_choice` 时 DeepSeek 不允许 thinking，且 flash 默认 thinking 开 → 强制工具时无条件置 `thinking:{type:disabled}`。
   - 健壮性：连接 + 完整读体都重试（覆盖 IncompleteRead、SSL EOF、503 too-busy）。
   - 实测：主推理(v4-pro 流式+thinking)、标题(v4-flash 强制工具)、工具循环全通；v4-pro 跟随 OPERON 协议明显比 qwen-max 稳。

## 三、agent 特性覆盖

- **多轮工具循环已验证**：`tool_use(python) → 人工批准门 → 内核执行 → tool_result 回喂 → 继续作答`。
  - 坑1：模型写**裸表达式**（`result` 而非 `print(result)`）时 tool_result 只含 stdout（空），模型会瞎猜 → 让模型用 `print()`。
  - 坑2：代码执行是**人工批准门**（`output_data.pending_input_requests`）。无头驱动：`POST /api/frames/:id/resolve-input`，body `{responses:[{requestId,tool_id,approved:true,action:"allow",scope:"conversation|project|always"}]}`；`--dangerously-skip-approvals` 在此 ant build 被禁用。
- **并行工具调用已验证**：一轮 assistant 里 `bash+read_file+edit_file+bash` 4 个 tool_use/tool_result 经代理完整往返；Auditor/verifier 子 agent 循环也走代理。
- **全量工具清单**：主 agent 26 个（`web_search bash python r repl save_artifacts read_file edit_file manage_environments manage_packages fetch_article_fulltext list_compute compute_details ask_about_compute skill ask_user search_skills boundary summary_query request_network_access list_host_grants request_host_access delete_host_files update_step_status wait_for_notification generate_plan`）+ 标题 agent 1 个。抓取法：代理设 `PROXY_DUMP_TOOLS=<dir>` 落盘请求里的 tools 数组。代理对工具名不特判。
- 已验：tool_use/tool_result/并行调用/verifier 子 agent；已修 max_tokens 夹取、SSE 回放、上游重试。**仍待验**：思考块、cache_control、多模态图像块。
- **两个非代理问题**：(a) Science 里 `bash` 与 `read_file/edit_file` 文件视图不互通（bash 写 /tmp 的文件 read_file 读不到）；(b) qwen-max 在 OPERON 复杂协议里跟随力不稳（多步/并行/带 verifier 会跑偏）—— 模型质量问题，非代理/虚拟登录问题，换 v4-pro/deepseek 改善。

## 四、其它

- Qwen(DashScope) 为 OpenAI-兼容备选（`--provider qwen`，翻译路径）；`proxy/qwen_proxy.py` 是其早期单 provider 版，已被 `csswitch_proxy.py` 取代。
- **已决（2026-07-03）**：CC Switch 代理不能当 sidecar 直接复用（见事实 6）。方向 = 自研代理移 Rust（axum）+ vendor CC Switch 的 MIT 翻译模块拿广覆盖（治本 python-ectomy）。这条独立于「配置层多 profile 化」，各走节奏。见 `known-issues.md` #8。
- DashScope 兼容端点：`https://dashscope.aliyuncs.com/compatible-mode/v1`。DashScope 偶发连接抖动（SSL EOF `_ssl.c:1129`/握手超时），代理已加连接级重试（4 次退避，仅重试连接错误）。
