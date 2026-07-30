# CSSwitch 架构总览

本文描述 CSSwitch 自己的稳定模块边界、依赖方向和故障定位。Claude Science 在本文中始终被视为外部 runtime；其内部实现、私有接口和数据模型不属于 CSSwitch 的公开架构合同。

## 产品边界

CSSwitch 是本地 provider 配置转换器、inference Gateway 和外部 runtime 适配器。它负责：

- 把当前 profile 转换为外部 runtime 可使用的 Anthropic-compatible loopback endpoint；
- 管理 Rust Gateway 与隔离 runtime 的启动、停止、身份校验和失败补偿；
- 持久化 CSSwitch 自己的 profile、端口、运行模式和 runtime binding；
- 提供默认关闭的 Codex browser-only OAuth、动态模型目录与 Responses bridge；
- 提供范围受限的外部 Skill 和系统 SSH 适配入口。

外部 runtime 拥有自己的 UI、工作区、用户内容和内部生命周期。CSSwitch 不复制这些能力，也不把适配器扩展成通用 Skill、MCP、Plugin 或远程运行管理器。

## 主依赖方向

```text
Desktop WebView
  -> Tauri command / event
     -> command-specific mutation boundary
        -> Lifecycle serializer
        -> Config + AppState
        -> runtime adapter
        -> Rust Gateway
           -> provider / Codex upstream

External runtime
  -> Anthropic-compatible loopback endpoint
  -> CSSwitch Gateway

Optional integrations
  -> CSSwitch-owned Skill / SSH adapter
  -> external runtime or user-owned system tool
```

依赖方向的重点是：Desktop 编排 CSSwitch 状态，Gateway 处理 inference transport，runtime adapter 封装外部 runtime 差异。其他模块不应直接依赖外部 runtime 的内部存储或私有接口。

## 组件边界

### Desktop / Tauri

Desktop 管理配置、端口、运行模式、`AppState`、runtime lifecycle、UI 状态和可选适配器编排。command、event、DTO 与 frontend caller 见 [Desktop 控制面](desktop-control-plane.md)。

### 状态与事务

`AppState` 保存进程内 Gateway child、runtime identity、boot references 和清理重试镜像；`Config` 持久化 profile、端口、mode、binding 与 transaction journal；`Lifecycle` 串行化需要跨多个状态源提交的复合变更。局部 Skill、SSH 和 Codex 操作拥有各自的事务边界，不应被描述为共享一把全局锁。详见 [运行时状态与事务](runtime-state-transactions.md)。

### Rust Gateway

Gateway sidecar 处理 provider 与 Codex inference、模型目录、协议转换、SSE 和 tool 映射。同一个 Rust binary 还承载受限的 scratch、Codex auth 和外部 Skill 入口。当前生产路径没有 Python proxy fallback。详见 [Gateway 与 provider 路由](gateway-provider-routing.md)。

### 外部 runtime adapter

CSSwitch 为外部 Science runtime 提供隔离目录、loopback 端口、可执行文件选择、运行身份校验和受限恢复。所有外部 runtime 数据默认视为不透明；CSSwitch 只操作自己明确管理的投影和记录。详见 [Science runtime 适配合同](science-runtime.md)。

## CSSwitch 所有权

| 数据 / 能力 | Source of truth | 所有者 |
|---|---|---|
| profile、provider contract、model preset、mode、端口 | CSSwitch config / catalog | CSSwitch |
| Gateway child、launch identity 与 route state | CSSwitch `AppState` / Gateway | CSSwitch |
| runtime transaction journal 与 last healthy binding | CSSwitch config | CSSwitch |
| CSSwitch runtime snapshot、receipt 与 cleanup record | CSSwitch private data root | CSSwitch |
| Codex auth、model generation 与 network route | CSSwitch private state / config | CSSwitch |
| 外部 Skill marker、bundle journal 与 bridge state | CSSwitch private state | CSSwitch |
| 外部 runtime 的内容、内部状态和执行语义 | external runtime | 外部 runtime |
| SSH config、key、agent、network 与 server | user / OpenSSH / external system | 用户 / 外部系统 |

任何单一端口、文件或内存字段都不能独立证明完整运行身份。

## 网络与安全

- Gateway 和外部 runtime UI 只绑定 loopback；产品没有公开监听开关。
- runtime UI port 与 sandbox port 分开校验。
- 一次性 URL、nonce、CSRF、凭证和 path secret 不进入普通 status 或日志。
- 第三方模式不读取或复制用户真实 runtime 登录数据。
- Gateway 的 raw `CONNECT` 是 loopback 可达的通用 TCP transport，不等于 Remote MCP，也不应被描述成受 inference path secret 保护的产品 API。
- runtime app proxy、sandbox network、package mirror、Codex route 与 provider egress 是不同网络面。
- SSH 是用户 opt-in 的外部系统适配；CSSwitch 不复制 `.ssh`、不启动 `sshd`、不开放监听。

## 高层失败边界

以下失败可以阻断一键开始：

- profile 或 provider contract 无效；
- Gateway spawn、health 或 model catalog 无法建立；
- CSSwitch 的 recovery snapshot、runtime preflight 或端口身份校验失败；
- 外部 runtime launch、health 或 managed identity 无法确认；
- 当前启动需要的 Codex auth proof 不成立；
- 用户启用 SSH 后，CSSwitch-owned preflight 失败。

以下失败只降级局部能力：

- 外部 Skill bridge 的配置或单次操作；
- 单次 SSH domain operation；
- 与当前 runtime 无关的 Codex auth、catalog 或 transport 操作。

故障定位时先判断失败属于 Desktop 编排、CSSwitch 状态事务、Gateway/provider、runtime adapter 还是外部系统；不要用外部 runtime 的内部实现解释 CSSwitch-owned failure。
