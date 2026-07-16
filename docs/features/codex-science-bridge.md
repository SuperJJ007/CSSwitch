# Codex → Claude Science 实验桥接合同

状态：**阶段 0–6 的源码、自动 Gate、独立审查和隔离实机环境准备已完成**。Acceptance app 已停在打开真实 OAuth 浏览器之前；真实 Codex OAuth、live provider 与发布附件仍未验收。当前 artifact、哈希和隔离 root 见 [Acceptance 环境准备证据](../evidence/investigations/2026-07-16-codex-acceptance-preparation.md)。

本文冻结 CSSwitch 将用户自己的 Codex 登录接入隔离 Claude Science 的 v1 实施边界。它是非官方、实验性且默认关闭的本地能力，不代表 OpenAI 或 Anthropic 的官方集成。

## 目标与非目标

目标：用户通过 CSSwitch 独立完成 Codex OAuth 登录，由 Rust `csswitch-gateway` 将 Science 的 Anthropic Messages 请求翻译为 OpenAI Responses 请求，并让 Science 从动态目录中选择多个当前账号可用的模型。

v1 只支持：

- macOS；
- 一个全局 CSSwitch Codex 账号；
- loopback HTTP 与 SSE；
- 独立 CSSwitch OAuth 凭据；
- 动态模型发现；
- 文本、图片、reasoning、工具调用和工具结果。

v1 明确不做：

- 读取、复制、覆盖或删除原生 `~/.codex` 登录状态；
- 账号池、轮换、额度规避或多账号并发；
- device authorization flow；
- WebSocket transport；
- 将 token 交给 Tauri、前端、Science 或普通配置文件；
- 声称官方支持、稳定 API 或合规保证；
- 在 Codex 模型目录不满足多模型条件时伪造多个模型。

## 当前源码使用路径

此能力只存在于当前实验源码分支，默认关闭，不代表 `v0.6.0` 或任何既有发布附件已包含它。旧 UI 的入口收在「高级设置」：启用 Codex 实验能力 → 使用 CSSwitch 独立登录 → 新建 Codex 配置并设为当前 → 一键开始 → 在 Science 的 “More models” 中选择 `Codex / …` 动态模型。Codex 配置不填写 API Key、`base_url` 或固定模型。

登录由 CSSwitch 自己的 OAuth 与 Keychain 项完成，不读取、复用或修改原生 Codex 登录。关闭实验开关只停止受管 Codex 链路并隐藏新建/启动入口，不删除 CSSwitch 凭据；退出登录必须由用户单独确认。Doctor 只显示实验开关、Codex profile 数、authenticated / unauthenticated、expiry state 或稳定错误码，不显示账号指纹、邮箱、token、auth epoch / generation 或 Keychain 内容。

当前自动 Gate 已覆盖 mock OAuth、配置迁移、协议转换、动态目录、旧 UI 与生命周期。RM-35～RM-45 中需要真实账号或系统网络环境的项目仍必须等用户在隔离 Acceptance app 中亲自授权后完成，因此不能据源码测试声称真实账号、可用额度、两个以上 live 模型、真实 TUN/代理或发布包已验证。

## 参考实现边界

实现语义按以下优先级取证：

1. `openai/codex@cbc83d961e8132bfff4d340ab8342d181b79e95e`（Apache-2.0）：`codex-rs/login/src/{device_code_auth.rs,server.rs,pkce.rs,auth/manager.rs,auth/revoke.rs}` 的设备码、浏览器 OAuth、刷新与账号语义，以及同一固定 SHA 的模型目录 endpoint / 字段语义；
2. `raine/claude-code-proxy@1a2d700d7f900ef3c60d2e9e7c25f5e98ab8ff1f`（MIT）：Anthropic ↔ Responses 与 SSE 转换思路；
3. `anomalyco/opencode@4394b324c972c17952a3c890c608b71739b343c3`（MIT）：认证插件与 provider 分层；
4. `router-for-me/CLIProxyAPI@b6ce0beecd31dff389d3190f7db6d7a1d4ce0e7e`（MIT）：只借鉴测试矩阵。

第三方代码只有在许可证兼容且逐段审查后才能复用。许可证不清晰的 `icebear/codex-proxy` 只能作为行为线索，不能复制代码。

## 所有权与信任边界

| 数据 / 能力 | Source of truth | 所有者 |
|---|---|---|
| Codex OAuth access / refresh token | macOS Keychain service `com.csswitch.codex.oauth.v1`, account `default` | CSSwitch Gateway |
| auth epoch / generation | `~/.csswitch/codex-auth-state.v1.json`，不含账号或凭据 | CSSwitch Gateway |
| thinking HMAC key | macOS Keychain service `com.csswitch.codex.thinking.v1`, account `default` | CSSwitch Gateway |
| 登录、状态、退出和刷新 | `csswitch-gateway codex-auth ...` | CSSwitch Gateway |
| provider profile 和功能开关 | CSSwitch config v3 | CSSwitch Desktop backend |
| 模型目录缓存 | CSSwitch-owned、无凭据缓存 | CSSwitch Gateway |
| 原生 Codex 登录 | `~/.codex` 及原生 Codex Keychain 项 | 原生 Codex，CSSwitch 不接触 |
| Science 对话与项目 | 隔离 Science data-dir | Science |

上表是正常 CSSwitch 构建的 namespace。真机 Acceptance app 只能通过编译期 `acceptance-keychain` feature 构建，其 Gateway 改用 `com.csswitch.acceptance.codex.oauth.v1` / `com.csswitch.acceptance.codex.thinking.v1`；正常构建不包含该 feature，运行时也没有可覆盖 service 名的环境变量。

Tauri 只能调用结构化 auth 命令并读取脱敏状态，不能读取或传递 OAuth token。Science 和 scratch probe 只能得到 loopback gateway 地址与本地 path secret，不能得到 Keychain reference 或 OAuth 内容。

## OAuth 合同

同一个打包后的 `csswitch-gateway` 提供：

```text
csswitch-gateway codex-auth login-device
csswitch-gateway codex-auth login-browser
csswitch-gateway codex-auth status
csswitch-gateway codex-auth logout
```

`status` 与 `logout` 的 stdout 必须且只能是一行 UTF-8 JSON；登录使用 bounded NDJSON v2，每行最多 8 KiB、总计最多 64 KiB，只在阶段变化、取消确认和终态时输出。基础状态 schema 为：

```json
{
  "schema_version": 2,
  "ok": true,
  "command": "status | logout",
  "status": {
    "authenticated": true,
    "account_hash": "sha256-truncated-or-null",
    "expiry_state": "valid | expiring | expired | unknown | missing",
    "expires_at": 0,
    "auth_epoch": "128-bit-random-hex",
    "auth_generation": 1
  }
}
```

登录事件包含 `schema_version=2`、32 位小写十六进制 `operation_id`、`kind=progress|cancel_ack|terminal` 与固定 allowlist 字段；设备码只在 `verification_required` 阶段出现，进程退出后不持久化。stdin 只接受同一 operation ID 的单行 `cancel`。失败终态不转发上游 message 或正文，只允许 `code/stage/retryable/upstream_status/response_kind/challenge_detected/transport_kind`。

Desktop API 为 `codex_auth_start(method)->starting snapshot`、`codex_auth_cancel(operation_id)`、`codex_auth_operation_status()`；事件固定为 `codex-auth://operation`。snapshot 是唯一权威状态，`sequence` 单调递增，终态保留到下一次登录或 App 重启。前端必须先监听事件，再查询 snapshot，并按 `(operation_id, sequence)` 去重。

稳定退出码为：0 成功；2 参数 / schema；3 未登录；4 浏览器或用户终止；5 callback / device 超时；6 平台、Keychain、锁或本地状态不可用；7 OAuth 网络 / 协议 / 取消；8 内部错误。稳定错误码还包括 `oauth_unexpected_content_type`、`oauth_challenge_response`、`proxy_connect_failed`、`tls_failed`、`device_auth_unavailable` 和 `auth_cancelled`。输出不得包含邮箱、authorization code、token、PKCE verifier、state、nonce、完整上游 body、Cookie 或 Keychain payload。

设备码是默认入口。它固定 POST `/api/accounts/deviceauth/usercode`，显示 `https://auth.openai.com/codex/device`，按服务端 interval（限制为 1–30 秒）POST `/api/accounts/deviceauth/token`，最多等待 15 分钟，再以 `https://auth.openai.com/deviceauth/callback` 交换 token。初始化 404 表示该服务未启用设备码，UI 只建议用户手动改用浏览器；poll 阶段仅 JSON/空响应的 403/404 表示继续等待。`cf-mitigated: challenge` 优先分类为安全挑战，HTML 永远不能被当作 pending。

浏览器备用入口继续使用 Authorization Code + PKCE-S256 和至少 256-bit 随机 `state`。回调只绑定 `127.0.0.1`，按顺序尝试端口 1455、1457，最多等待五分钟；错误或重放 state 返回 400 并继续等待，首个有效 callback 是唯一终态。系统浏览器使用系统自身网络；CSSwitch 的 Codex route 只控制 sidecar/Gateway 的 HTTPS socket。

登录 request、poll、poll sleep、token exchange、慢 header、慢 body 与 browser callback wait 都可取消。cancel 与 `Running -> Committing` 竞争同一个原子屏障：cancel 胜出后不能写 Keychain；commit 胜出后返回 `commit_in_progress`。sidecar 明确回复 `accepted` 后两秒仍不退出，父进程可结束并回收；未收到确认时不得猜测并杀死可能正在提交的进程。

阶段 1 固定采用上述官方 Codex SHA 的当前参数：issuer `https://auth.openai.com`；client id `app_EMoamEEZ73f0CkXaXp7hrann`；authorize / token / revoke 分别为 `/oauth/authorize`、`/oauth/token`、`/oauth/revoke`；redirect URI 为 `http://localhost:{1455|1457}/auth/callback`；scope 精确为 `openid profile email offline_access api.connectors.read api.connectors.invoke`；另传 `id_token_add_organizations=true`、`codex_cli_simplified_flow=true`、`originator=codex_cli_rs`。这些是固定上游兼容参数，不表示 CSSwitch 获得官方合作身份；任何未来变更必须重新取证、审查和更新本文，不能静默漂移。

Keychain 不可用、拒绝或返回歧义项时 fail closed，不降级到文件。production build 只允许固定官方 endpoint 和 macOS Keychain；fake browser、fake OAuth、fake Keychain 只能通过 Rust 测试注入 trait 使用，不能通过生产环境变量注入 endpoint、token 或 Keychain payload。`logout` 先 best-effort revoke，再删除 CSSwitch 自有 Keychain 项；不得删除原生 Codex 凭据。非法代理时跳过无法建立的 revoke transport，仍推进 generation 并删除本地项，返回 `{code:"revoke_skipped",reason:"proxy_config_invalid"}`；合法 route 的 revoke 网络失败同样不阻止本地删除，只有 Keychain 删除失败才返回 logout 失败。

Keychain OAuth record 为 versioned JSON，包含 token、账号内部 id、expiry、`auth_epoch` 和 `auth_generation`；thinking record 同样包含 record version、epoch 和 generation。任何普通文件都不含这些秘密字段。非秘密 `codex-auth-state.v1.json` 持久保存随机 128-bit epoch、单调 `u64` generation 和本次身份已完整提交的 marker；首次创建 generation=0，成功 login / refresh / logout 各加一，logout 后不归零，因此旧模型缓存不能复活。状态文件及 lock 为 `0600`，父目录为 `0700`，拒绝 symlink / 非普通文件；写入使用同目录 temp、`fsync`、rename 和父目录 `fsync`。

login、refresh 和 logout 共用 `codex-auth.mutation.lock`：`O_NOFOLLOW` 打开、`flock(LOCK_EX|LOCK_NB)`、每 50ms 重试、五秒后 `auth_busy`；进程崩溃由内核释放锁，持久 lock 文件可以复用。login 只有 OAuth record、全新 thinking key record 和最后的 state commit marker 全部完成后才能返回成功：先保存旧 OAuth / thinking record 的内存快照，再写 generation+1 的 OAuth 与 thinking record，最后原子写 state；state 前任一步失败都恢复两份旧 record，state 写失败也同样恢复。覆盖旧 record 后若恢复本身失败，state 不提交，新 record 因 epoch / generation 不匹配而不可用，命令返回 fail-closed 错误。测试必须故障注入“OAuth 写成功但 thinking 写失败”和“thinking 写成功但 state 写失败”。refresh 不轮换 thinking key，只将其 generation 与 OAuth record 一并 CAS 更新后最后提交 state。logout 先把 state generation+1 持久化令旧 record 失效，再删除 OAuth 与 thinking Keychain 项；删除失败时旧 token 即使残留也因 generation 不匹配而不可用。status 不取 mutation lock，只有 OAuth、thinking 和 state 三者 version / epoch / generation / commit marker 全匹配才视为 authenticated。

Gateway 只在需要时刷新。serve 只在 refresh 临界区持有上述 mutation lock；网络调用前后重新读取并比较 epoch、generation 与 refresh-token 摘要，CAS 不匹配时丢弃新响应并返回 `auth_changed`，不能覆盖更新结果。401/403 令当前 generation 的模型缓存失效。刷新竞争的失败请求不重发推理 POST。

## provider 与配置合同

配置升级为 v3。profile 不再隐含“必须 API key、必须固定模型”，而是只持久化用户选择与受 catalog 约束的 credential / model policy：

```json
{
  "credential_source": "api_key | keychain_oauth | none",
  "credential_ref": "csswitch:codex:default | null",
  "model_policy": "required_fixed | optional_fixed | dynamic_catalog"
}
```

Config v3 另含带 `serde(default)` 的 `codex_network={mode:"auto|custom",proxy_url:""}`；旧 v3 文件与 v2→v3 都默认 `auto`，不升级到 v4。auto 只按 `HTTPS_PROXY`、`https_proxy`、`ALL_PROXY`、`all_proxy` 顺序选择，并应用 `NO_PROXY/no_proxy`；`HTTP_PROXY` 不用于 Codex HTTPS。custom 忽略 NO_PROXY。HTTP、HTTPS、SOCKS5、SOCKS5h URL 必须含 host 与显式端口，只允许根路径，拒绝 userinfo、代理认证、query、fragment、控制字符和超长值；不支持 PAC、自定义 CA、系统代理发现或 TUN 检测。

Desktop 与 Gateway 共用同一 Rust resolver，得到 `ResolvedCodexNetworkRoute{source,proxy_scheme,proxy_url,no_proxy,fingerprint}`。所有 Codex reqwest builder 先 `.no_proxy()`，再应用该规范化 route；OAuth、刷新、revoke、模型目录、推理、formal 与 scratch 不得隐式读取环境。route fingerprint 进入 formal Gateway 复用身份，变化后旧 Gateway 不能复用。UI 与 doctor 只显示 `direct|env_https|env_all|custom` 和 scheme；direct 的准确文案是“直接 socket，可能由系统 TUN 接管”，不得声称检测了 TUN，也不得显示完整代理 URL。

阶段 2 新建 typed `catalog/provider-contracts.v1.json`，作为 `auth_mode`、`model_discovery`、`transport` 等 provider 启动 capability 的唯一 source of truth；现有 `catalog/capabilities.v1.json` 继续只保存兼容性 evidence rules，schema 和 loader 不变。profile 不复制 capabilities。backend 将 provider contracts 与 profile 合并、校验后生成私有 `ResolvedLaunchPlan`，至少包含 adapter、endpoint、opaque credential handle、model policy、transport、超时和缓存策略，再投影成 `FormalGatewayPlan`、`ScratchPlan` 和 `PublicPlanView`。Gateway 同样直接编译并校验这份 catalog；Codex 的 model GET 与 inference transport 从中取得 connect、request、read-idle、cache TTL 和显式维护的上游 client compatibility version，桌面进程还会向受管 sidecar 注入 contract id 与完整 catalog SHA-256，Gateway 启动和 Tauri health 两侧均需匹配。只有 formal gateway 内部能把固定 `CodexDefault` handle 解析为 Keychain identity；scratch 只得到 `provider=codex`、临时 loopback endpoint / path secret，UI 只得到脱敏 public view。三条路径共用 resolver，但权限投影不同。

迁移在内存中按 v1 → canonical v2 → v3 执行，最后只原子提交一次 v3。v1 输入先保存原始 `config.json.v1.bak`，再把 canonical v2 保存为 `config.json.v2.bak`；v2 输入只需后者。目标 backup 已存在且字节相同时复用；内容不同时以 `config.json.vN.bak.<full-sha256>` 使用 `O_EXCL` 另存，绝不覆盖。备份发布使用同目录、由输入内容哈希确定的 hidden pending；publish 和首次目录 `fsync` 后必须删除 pending 并再次 `fsync`。若在该窗口崩溃，下一次相同迁移会先清理模块自有 pending hard link，再校验单链接不变量并继续。任一校验、backup、`fsync` 或 rename 失败都不改变当前 config。迁移的读取、版本备份与最终提交锚定同一个已打开目录句柄，不因目录路径替换漂移。迁移不改变现有 API-key profile、active profile、端口或设置，并为 Codex network 采用 auto 默认值。

`downgrade_to_v2` 对每个 Codex profile 都要求显式 `export_then_remove` 或 `remove`，不能只处理 active profile。export 只含 profile 元数据和模型选择，不含 token、账号 id、Keychain reference payload 或缓存；若移除的是 active Codex profile，必须把旧 v2 schema 的 `active_id` 写成 `""`，不能写 JSON null；非 Codex active 保持。`export_then_remove` 必须由调用方给出 CSSwitch 配置目录之外的目标：先原子落盘并完成目录 `fsync`，再使用 rolling backup、同目录 temp、`fsync`、rename 提交 v2。export 失败时 config 字节不变；export 已成功而后续 config 提交失败时，安全可重入结果是“原 v3 config + 已完成 export”，绝不能是“profile 已删除但 export 未完成”。用户 export 父目录权限不得被 CSSwitch 修改，失败回滚保留原目标文件的字节和 mode。降级保留所有 v2 可表达的 API-key profile、设置与端口，永远不读取、删除或修改 Keychain；v2 无法表达 Codex network，因此 RM-41 必须明确验证该字段被丢弃且其余设置不变。

## 请求与对话状态

Science 每次发送的完整消息历史是唯一请求上下文。Gateway 每次都发 `store=false`，不使用 `previous_response_id`，不保存 CSSwitch 对话状态。

转换器必须覆盖：

- system、user、assistant 文本；
- 图片输入；
- reasoning summary → Anthropic thinking；
- function call → `tool_use`；
- tool result → Responses function result；
- usage 与 stop reason；
- 多个并行工具调用和严格 tool id 关联。

需要跨轮传递的 Responses encrypted reasoning content 封装进 version 1 Anthropic signature。HMAC-SHA256 key 是独立 256-bit 随机值，只存 `com.csswitch.codex.thinking.v1/default`，不能复用 path secret 或 OAuth token；成功 login 生成 / 轮换，logout 删除，relogin 产生新 key。payload 绑定 `version=1`、`purpose=codex-reasoning`、auth epoch、account hash、response item id、可选 tool call id 和 encrypted content 摘要；篡改、版本未知、账号 / epoch 不同或错误 tool 关联均 fail closed，不把不可信内容送给上游。重启保留当前 key；logout 后的旧对话必须新开会话，不接受旧 signature。

推理 POST 一旦发送，CSSwitch 不做自动重发：401、尚无首字节、空 200、SSE 中断、下游断开都不重发当前请求。401 可以刷新凭据，但只供下一次用户请求使用。只有幂等模型目录 GET 可以使用有界网络重试。

## SSE 与取消

上游推理始终请求 `stream: true`。流式下游由独立 Codex SSE reducer 增量转换；非流式下游复用同一 reducer，在终态事件后有界归并为 JSON，不能另写一套语义。

reducer 必须处理文本 delta、reasoning delta、function call arguments、output item 生命周期、usage、completed、failed、incomplete 和协议错误。下游写失败或取消后立即停止读取和转换，不补发完成事件，不重试，不执行第二次工具调用。

数值边界固定为：HTTP request body 64 MiB；单个上游 SSE event 8 MiB；累计文本 32 MiB；累计 reasoning 16 MiB；单个 tool arguments 8 MiB；单个 signature 8 MiB；nonstream aggregate 64 MiB；每响应最多 256 个 tool call 和 1024 个 output item。请求体超限在发送上游前返回 HTTP 413；流式处理中超限发送一个脱敏 `error` SSE 后关闭；nonstream 超限返回 502 protocol error；三种情况都取消上游读取且不重试。

## 动态模型目录

模型列表来自官方 Codex 当前账号目录端点，不用硬编码列表冒充实时权限。缓存合同：

- 正常 TTL 5 分钟；
- key 为不可逆账号摘要 + auth generation，不含 token 或邮箱；
- 同 generation 网络失败时可用 last-known-good，最长 24 小时，并显式标注 stale；
- 登录、退出、账号变化、401/403 立即失效；
- 鉴权失败不能回落到旧账号目录。

目录 GET 最多三次：connect 10 秒、单次总超时 30 秒，失败后退避 250ms / 500ms；只重试网络错误、408、429 和 5xx，其他 4xx 不重试，401/403 还必须失效缓存。请求 query 不使用 Gateway crate 的占位版本 `0.0.0`，而使用 provider contract 固定并测试的 `upstream_client_version=0.144.2`（2026-07-16 本机官方 ChatGPT 内置 Codex 兼容基线）；升级该值必须重新核对官方模型目录字段与最低客户端要求。last-known-good 以 `0600`、无 symlink、原子写的 `codex-models-cache.v2.json` 跨 gateway 重启保存；v2 除 raw id 外还保存每个模型的 reasoning、summary 与 parallel-tool 能力，key 还包含 auth epoch。正式 gateway 与 scratch 共享 HOME 时，持久化 cache epoch 与短临界区文件锁共同保护缓存：目录请求开始时读取 epoch nonce，提交前在锁内做 CAS；每次 401/403 失效和每次成功 live 提交都轮换 nonce，恢复只把持久状态标为可用、绝不回到 `None`，因此旧在途 GET 不能利用 `None → nonce → None` 的 ABA 窗口重建缓存，进程重启也不能绕过。scratch 等待预算固定覆盖 5 秒 auth mutation lock、30 秒 refresh、三次 30 秒目录请求、750ms 退避及 5 秒本地余量。stale 通过 `/v1/models` 响应头 `x-csswitch-model-source: stale-cache`、`x-csswitch-model-age-seconds` 和脱敏诊断字段 `{source,stale,age_seconds}` 暴露；标准 model data 不混入伪造项。

2026-07-16 对本机 installed Science `0.1.18-dev.20260709.t211149.shab3f5130` 的隔离实验已验证：Science 会请求 `/v1/models?limit=1000`，但会过滤所有不以 `claude-` 开头的模型 id；同一响应中的 raw `gpt-5.3-codex` 不显示，而确定性的 `claude-csswitch-codex-gpt-5.3-codex` 会显示且可选。详细证据见 [Science 模型 ID 兼容实验](../evidence/investigations/2026-07-16-codex-science-model-compat.md)。

因此模型目录内部与磁盘缓存只保存官方 raw id；Science-facing `/v1/models` 暴露 `claude-csswitch-codex-<raw-id>`，显示名显式加 `Codex / ` 前缀，不冒充 Anthropic 内置型号。`/v1/messages` 只接受当前账号目录中可反解的该类 alias，校验后在发送上游前恢复 raw id；直接 raw id、未知 alias 和已失效目录项都返回确定性 400，不能发出推理 POST。至少两个真实账号模型的 live 选择与上游一致性保留为 RM-36，必须由用户亲自完成 OAuth 后验收，自动 Gate 不得伪造该结论。

## 生命周期与 UI

Codex 是现有 provider 架构中的一种 capability 组合，不新增第二套入口系统。UI 提供设备码默认登录、浏览器手动备用、取消、刷新可恢复的 operation 状态、独立网络路线、退出、动态模型列表和枚举化错误提示；新 UI 后续复用相同 backend contract。登录态不是 UI 专属守卫：正式 proxy、scratch / 模型发现和 Science 一键启动都在后端边界先取得 `CodexAuthSupervisor` 使用租约并调用受管 `codex-auth status`，只有 CSSwitch 自有状态明确为 authenticated 才允许继续；未登录或 sidecar 状态不可确认时，在启动任何 Codex gateway 或改变隔离 Science 状态前 fail closed。

独立 `CodexAuthSupervisor` 以使用租约阻止认证与 formal/scratch/model probe 的 check-then-start 竞态。锁顺序固定 `lifecycle -> supervisor`：登录 start 只在校验 route、预留 operation、停止受管 Codex Science/Gateway 和登记 sidecar 时短暂持有全局 lifecycle，随后释放并等待授权；其他 provider 不检查该 supervisor。第二次登录、logout、关闭开关和网络修改在认证中返回 `auth_busy`。`set_codex_network` 只停止受管 Codex 链路、原子保存且不自动重启；其他 provider 完全不动。

login/logout 前 Tauri 不能停止其他 provider 的 Science 或 gateway。若身份无法确认或 stop 失败，login/logout fail closed，不继续 auth mutation、不杀未知 PID。关闭实验开关只停止自有 Codex 链路，不删除凭据；停止失败时 UI 据实保留运行 / 错误状态。用户显式 logout 才删除 CSSwitch Codex 凭据。App 退出向当前 operation 发送取消并只回收登记的精确 sidecar PID，不持久化 operation 或设备码。

## 分阶段任务与 Gate

### 阶段 0：基线与合同

- 从干净 `main@0897e78f201e9e463be6a13e3d11888bde31f3b0` 创建独立 worktree；
- 记录五层总测试基线，见 [2026-07-16 日期化证据](../evidence/investigations/2026-07-16-codex-science-bridge-baseline.md)；
- 冻结本文全部边界。

Gate：原有 provider 五层全绿，工作区与其他 UI / Skill worktree 隔离。

### 阶段 1：OAuth 与 Keychain

- 实现 auth CLI、PKCE callback、Keychain store、mutation/refresh lock；
- fake browser、fake OAuth server、fake Keychain 全矩阵测试；
- 证明日志、argv、状态和配置无 token。

Gate：自动测试覆盖登录、state/PKCE、超时、端口冲突、刷新竞争、退出和原生凭据不接触。

### 阶段 2：配置与 provider 收口

- v3 schema、备份、迁移和安全降级；
- typed provider contracts catalog 与 `ResolvedLaunchPlan`；
- scratch / formal / UI 消除 API-key 与固定模型假设。

Gate：现有 v1/v2 迁移、全部 API-key provider 和 rollback 合同无回归。

### 阶段 3：Responses 与 SSE

- Anthropic Messages → Responses request translator；
- Codex SSE reducer 与 nonstream accumulator；
- thinking signature、工具调用、取消和零重试状态机。

Gate：golden、loopback、断流、401、空 200、工具去重、篡改 signature 和内存边界全部通过。

### 阶段 4：模型目录

- 官方动态目录 client、缓存与失效；
- raw id / shell alias Science 实验；
- installed Science 多模型选择兼容和 unknown-model 错误；产品 UI 接线在阶段 5 完成。

Gate：官方目录、缓存、失效与 alias → raw 映射自动测试通过；installed Science 对 raw / alias 的隔离双样本实验通过。真实账号至少两个可用模型的最终证明属于 RM-36，停在用户 OAuth 之前不能提前宣称完成。

### 阶段 5：产品接入

- 实验开关、auth 状态、登录/退出、诊断；
- 一键启动、切换、stop、logout 和 downgrade 编排；
- 文档和升级说明。

Gate：关闭开关、provider 切换和失败回滚不影响其他 provider、原生 Codex 或真实 Science。

### 阶段 6：全面验证与实机环境

- `bash test/run_all.sh` 五层回归；
- 独立安全审查、协议审查和最终差异审查；
- RM-35～RM-45；
- 构建独立 bundle ID 的 Acceptance app，生成临时 HOME、CSSwitch 根、Science data-dir 和动态端口。

Gate：自动化全绿；实机环境已准备但停在打开真实 OAuth 浏览器之前，由用户亲自完成授权。

## 真机矩阵增量

| ID | 证据层 | 场景 | 必须满足 |
|---|---|---|---|
| RM-35 | Acceptance artifact + 用户 OAuth 后 live provider | 独立 Codex 登录 | 只由脱敏 `codex-auth status` 证明 Acceptance namespace 凭据存在，不 dump Keychain；正式 CSSwitch 与原生 Codex 登录前后状态不变；无 token 证据泄漏 |
| RM-36 | 用户 OAuth 后 live provider | 动态多模型 | 当前单一账号至少两个真实可用模型可选，请求 id 与 gateway 脱敏观测一致 |
| RM-37 | 用户 OAuth 后 live provider | 流式文本与 reasoning | 增量顺序、thinking、usage 和终态正确；CSSwitch Gateway 不持久化对话，Science 自有项目 / 对话持久化不属于失败 |
| RM-38 | 自动 mock + 用户 OAuth 后 live provider | 工具调用 | tool id / result 严格闭环；真实最小工具成功；断流 / 取消不重复执行由 mock 故障注入证明 |
| RM-39 | 自动 mock | 刷新与失效 | fake OAuth / Keychain 强制 401 和 CAS；并发刷新单写者；401 只影响下一请求；不破坏真实 token |
| RM-40 | Acceptance artifact + 用户 OAuth 后 live provider | 退出与重登 | 只删除 Acceptance namespace 项；正式 CSSwitch、原生 Codex 与其他 provider 不变；只用脱敏 status 观测 |
| RM-41 | 自动 fixture + Acceptance artifact | v3 降级 | 每个 Codex profile 显式处理；API-key profiles、端口和设置完整；Codex network 字段按合同丢弃；Keychain 不变且不读取其内容 |
| RM-42 | Acceptance artifact | 隔离打包 | 独立 bundle ID、隔离目录；Gateway / Science 使用动态端口，OAuth callback 仍固定 1455 / 1457；8765 与已安装 App 不变；收尾无残留进程 |
| RM-43 | Acceptance artifact | Finder 无代理环境 + 系统 TUN | Finder 启动显示 `direct`，只说明“直接 socket，可能由系统 TUN 接管”；TUN 下设备码登录成功但不声称检测 TUN |
| RM-44 | Acceptance artifact + 本地 fixture | 显式代理 | HTTP CONNECT 与 SOCKS5h 分别完成设备码、模型目录与最小推理；SOCKS5h 证明域名在代理端解析；production 不注入自定义 CA |
| RM-45 | Acceptance artifact | 登录取消 | waiting、poll sleep、token exchange 取消在两秒内终态；pre-commit 取消后 generation 与 Acceptance Keychain 状态不变；committing 返回 `commit_in_progress` |
