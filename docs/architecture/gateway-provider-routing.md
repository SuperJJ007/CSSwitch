# Gateway 与 provider 路由

本文描述 CSSwitch Gateway 的公开进程入口、正式与 scratch 路由、协议边界和故障定位。Desktop IPC 见 [Desktop 控制面](desktop-control-plane.md)，生命周期提交见 [运行时状态与事务](runtime-state-transactions.md)。

## 构建与进程边界

Desktop build 会构建并打包同变体的 Rust Gateway sidecar。当前生产运行没有 Python proxy fallback；旧 listener 清理只属于 lifecycle compatibility。

同一个 `csswitch-gateway` binary 承载多个 CSSwitch-owned 入口：

| 入口 | 职责 |
|---|---|
| HTTP server | 正式或 scratch provider/Codex Gateway |
| Codex auth CLI | browser-only OAuth、refresh、logout 与 status |
| Skill stdio MCP | 外部 Skill install、uninstall 与 poll 的窄 connector |
| runtime bridge adapter | 可选外部 runtime bridge 的 best-effort reconcile |

这些入口共享 binary，但不共享同一状态承诺。runtime bridge adapter 封装外部 runtime 差异；其目标 runtime 的内部接口不属于本文合同。

## 正式 Gateway

Tauri 从 active profile 和 provider contract 生成 launch plan。saved-model profile 生成 static model catalog；Codex adapter 合并独立 network route。Skill compatibility catalog 不参与正式 launch plan。

正式 Gateway 使用持久化的 loopback port 与 path secret，每次 spawn 生成新的 launch ID。普通 API-key provider 按 contract 注入 credential environment；Codex 使用 CSSwitch 私有 auth state 和独立 network route。

正式 server：

- 接受外部 runtime 使用的 Anthropic-compatible endpoint；
- 按 provider contract 路由 Anthropic Messages、OpenAI Chat/Responses、SSE、tools 和 tool results；
- 提供 `/v1/models` 与 runtime selector compatibility；
- 为 Codex profile 提供动态 model catalog 和 Responses transport；
- 提供 loopback 可达的 raw `CONNECT` transport。

API key 会通过 Tauri command 进入进程，再进入正式 Gateway 的 credential environment 和 launch context；它不进入 argv 或普通日志，出站配置 DTO 只返回掩码。

## scratch Gateway

scratch 使用同一 binary 与 provider contract，但拥有独立的：

- 临时 loopback port；
- 临时 secret；
- discovery 或 message-probe intent；
- child guard 与 timeout budget；
- 候选 credential、base URL 和 model environment。

scratch 结果只验证当前候选请求。它不提交正式 `AppState` identity、runtime binding 或产品 live 结论；loopback mock 也不能写成真实 provider 验证。

## Codex 路由

Codex 是默认关闭的独立边界：

```text
Desktop Codex command
  -> Gateway auth control / CSSwitch private files
  -> dynamic account model catalog
  -> Gateway Responses transport
  -> Codex upstream
```

auth mutation 使用私有文件锁。动态 model catalog 使用进程内 mutex 与 cache file lock。inference 读取 auth epoch/generation 一致的 snapshot，但不取得 auth mutation lock，也没有独立 inference lock。

Codex network route 是独立的 `Config.codex_network` 快照，在 launch 时解析并注入 Gateway；它不写入 auth record generation。修改该配置仍取得 Desktop supervisor 的 mutation lease，以串行化 CSSwitch 控制面操作。Codex 失败不改变其他 provider 的认证状态。

## Skill stdio MCP

`skill-install-mcp` 是 CSSwitch 外部 Skill 工作流专用 connector：

- 只接受受限 bridge directory、token 与 request；
- host side 完成 archive 下载、验证和提交；
- install、uninstall 与 poll 共享 request ID 和 terminal response；
- package commit 后仍需由 runtime adapter 确认可见性。

它不是通用 MCP 管理器、Directory connector、hosted MCP 或用户自定义 MCP UI。

## runtime bridge adapter

可选 bridge reconcile 是 best-effort 局部操作：

- CSSwitch 只提交自己拥有的 bridge state；
- 外部 runtime 不可用或版本不兼容时，普通 inference 仍可继续；
- 已完成的局部步骤不伪装成原子事务；
- drift 检查区分 CSSwitch 本地配置不匹配与运行中 adapter state 过期；
- 所有目标 runtime 特有逻辑必须封装在 adapter 内，不能扩散到 provider、Desktop DTO 或 Config 的通用模型。

公开文档只承诺这个 CSSwitch-owned 适配边界，不承诺或描述外部 runtime 的内部 control plane。

## 网络与协议边界

- Gateway 与外部 runtime UI 只绑定 loopback；
- path secret 认证 inference HTTP 路径；
- raw `CONNECT` 在 HTTP path-secret 认证前分派，只受目标策略约束；它是本机可达的 TCP tunnel，不证明 HTTP/SSE/Streamable HTTP MCP；
- raw `CONNECT` 的 DNS、dial 和已建立 session budget 与普通 HTTP request 不同，故障定位必须分开；
- runtime app proxy、sandbox network、package mirror 与 Gateway provider egress 是不同网络面；
- Codex 有独立 network route，不能代表外部 runtime 的网络设置；
- provider body、HTML challenge、timeout 与 protocol mismatch 必须按稳定 code/category 分开投影。

## 失败与证据

| 失败面 | 影响 |
|---|---|
| 正式 Gateway spawn、health 或 catalog | 可以阻断一键开始 |
| scratch candidate failure | 只影响候选验证或候选保存状态，不提交正式 runtime |
| Codex auth、catalog 或 transport | 通常只阻断对应 Codex 操作；当前启动依赖 Codex proof 时可阻断该次启动 |
| Skill stdio 或 runtime bridge adapter | 降级可选能力，可产生 warning |
| raw `CONNECT` target | 只说明 TCP transport 失败，不证明 MCP 产品结论 |

source/unit、scratch、local mock、真实 provider、final artifact 与 installed/live 证据必须分别记录。
