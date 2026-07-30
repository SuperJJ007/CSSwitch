# CSSwitch 代码重构优先级

本文给出 CSSwitch 在 v0.8.4 源码基线上的重构顺序。它只审查 CSSwitch 自己的模块、接口、状态事务、Gateway、frontend 和测试边界；外部 runtime 始终视为不透明 adapter target。

## 结论

下一阶段不应从“大改架构”开始，而应按以下顺序收口：

1. 先消除一键失败阶段对错误文案的反向解析；
2. 再拆分一键事务协调与 recovery projection；
3. 随后拆分 Gateway server 的 HTTP、inference dispatch 与 Skill bridge；
4. 再拆 frontend feature controller；
5. 最后处理 config、Codex control 和安全敏感 store。

前三项直接决定故障能否被稳定定位，也覆盖当前职责密度和回归风险最高的生产路径。每一步都应保持现有 Tauri command、DTO、配置 schema、锁序、journal 顺序、Gateway wire behavior 与 runtime adapter contract 不变。

## 审查范围与证据

- 源码基线：`origin/main@37d5cfb6600a0022d5e1bbbe9a7e181a917b12dd`
- 公开架构文档基线：`5d46591b366fd5a4a727dbfac1c8a093bbb79892`
- 检查对象：
  - `desktop/src-tauri/src/`
  - `desktop/gateway/src/`
  - `desktop/src/`
  - 当前 Tauri command 注册、frontend caller 和公开架构文档
- 量化方法：
  - 统计文件总行数，并区分首个大型 `cfg(test)` module 之前的生产主体；
  - 检查顶层类型、函数、module graph、跨模块引用与测试落点；
  - 统计最近 100 个提交中的路径触达次数，作为变更热点参考。
- 未运行真实账号、真实 provider、已安装 App 或外部 runtime 验收。
- 未修改生产源码。

行数只用于发现候选，不单独决定优先级。测试与生产代码同文件时，必须先分清生产主体；只由 `cfg(test)` 引入的 E2E 文件不计作生产模块。

## 规模与变更热点

| 路径 | 生产主体约数 | 文件总行数 | 最近 100 个提交触达 | 判断 |
|---|---:|---:|---:|---|
| `runtime/sandbox_session.rs` | 6,203 | 9,711 | 28 | 生产职责和事务分支都过密 |
| `skill_manager/store.rs` | 3,149 | 4,405 | 低于前列 | 大但安全内聚，不能只按行数先拆 |
| `desktop/src/main.js` | 2,865 | 2,865 | 40 | 最大变更热点，preview 与多个产品 feature 共用全局状态 |
| `config.rs` | 2,449 | 3,707 | 13 | schema、迁移、安全 I/O、cleanup 与 downgrade 混合 |
| `gateway/codex_protocol.rs` | 2,445 | 3,517 | 低于前列 | 大但围绕协议转换和 reducer，当前相对内聚 |
| `runtime/science.rs` | 2,250 | 3,412 | 25 | runtime selection、identity、launch receipt 与 stop 的 CSSwitch adapter |
| `gateway/server.rs` | 2,215 | 3,977 | 15 | HTTP、provider/Codex dispatch、stream 和 Skill bridge 混合 |
| `commands/codex.rs` | 2,093 | 3,325 | 5 | Tauri command、sidecar protocol、operation 与设置混合 |
| `skill_manager/deployment.rs` | 1,617 | 2,896 | 低于前列 | 安全敏感的 deployment transaction |
| `commands/runtime.rs` | 984 | 7,484 | 40 | 总行数大主要因为同文件测试，不是第一拆分对象 |

`runtime/skill_install_bridge_e2e.rs` 虽有 2,857 行，但它只通过 `#[cfg(test)]` module 引入，不属于生产体积。

现有测试密度为后续机械拆分提供了基础：`config.rs` 约 58 个 test、`commands/runtime.rs` 约 48 个、`gateway/server.rs` 约 33 个、`runtime/science.rs` 约 31 个、`runtime/sandbox_session.rs` 约 28 个、`commands/codex.rs` 与 `gateway/codex_protocol.rs` 各约 27 个。

## P0：先稳定故障投影

### 现状

CSSwitch 已经有多种结构化错误和诊断基础：

- `runtime/operation.rs::OperationStage` 为 operation log 提供 typed stage；
- runtime status 的 `last_error` 有结构化 `type`；
- Codex auth command error 有 `code`、`reason`、`cause` 和 `retryable`；
- Gateway protocol error 有稳定 kind，HTTP 返回也有 typed error envelope。

但一键失败仍在 `commands/runtime.rs::science_failure_stage` 中按错误字符串包含关系推断 frontend stage。相同事务还分别使用：

- typed `OperationStage`；
- `RuntimeTransactionJournal.stage: String`；
- frontend coarse stage；
- auto-boot 的 `boot://failed` string。

这使文案改动可能改变故障归类，也让同一个失败在手动启动、auto-boot、operation log 和 recovery journal 中落到不同粒度。

### 目标边界

新增内部的 typed failure projection，优先复用现有 `OperationStage` 和已有错误种类，再映射到现有 frontend DTO 字段。不要新增外部 schema，不改变现有 DTO key，不改变 journal checkpoint，也不要把所有 subsystem 强行纳入一个全局 error enum。

### 验收不变量

- 修改用户文案不会改变 stage；
- 手动启动与 auto-boot 对同一 failure domain 的分类一致；
- Codex、Skill、SSH 的局部 typed error 保持局部所有权；
- 日志仍脱敏，不向 event 或 DTO 投影内部路径、凭证或不透明 runtime 状态。

## P0：拆分一键事务与 recovery projection

### 现状

`runtime/sandbox_session.rs` 的生产主体约 6,203 行。文件同时负责：

- CSSwitch recovery projection 的捕获、登记、恢复与 cleanup；
- pending cleanup manifest 和重试镜像；
- 一键开始的 preflight、Gateway catalog、runtime launch、health、commit 与补偿；
- managed runtime reuse、restart 与 stop proof；
- optional Skill / SSH reconcile；
- 最终 UI surface 打开和日志投影。

`one_click_login_with_options` 从约第 5,423 行延伸到首个大型 test module 前，接近 800 行。它需要同时维护 `Lifecycle`、`AppState`、config journal、recovery snapshot、Gateway 和 runtime adapter 的顺序，属于当前最高风险的变更汇合点。

### 目标边界

保留单一公开 orchestrator 和现有 `one_click_login` 入口，只做内部机械提取：

```text
sandbox_session facade
  -> recovery_projection
  -> pending_cleanup
  -> one_click_transaction
  -> runtime_reuse_and_restart
  -> optional_bridge_reconcile
```

`runtime/science.rs` 继续只封装 CSSwitch-owned executable selection、identity、managed receipt、probe 与 stop。当前只有一个外部 runtime adapter，不应先引入通用 plugin framework 或能力管理器。

### 验收不变量

- 锁序仍为 `Lifecycle -> AppState -> config::update`；
- recovery projection 必须先于受保护写入；
- journal checkpoint、binding commit 与 cleanup clear 的先后不变；
- stop 前后继续执行强 identity 校验，身份漂移时不发送信号；
- reuse 与 cold-start 两条分支的不同提交顺序不得被“统一”；
- optional bridge 失败继续只降级局部能力。

### 推荐提交粒度

1. 只移动 pending cleanup 类型和纯 helper；
2. 只移动 recovery projection 类型和文件操作；
3. 提取 reuse / restart decision；
4. 最后缩短一键 orchestrator；
5. 每个提交保持调用签名和测试语义不变。

不要在同一个提交中同时改事务语义、错误文案和模块位置。

## P0：拆分 Gateway server

### 现状

`desktop/gateway/src/server.rs` 的生产主体约 2,215 行，并承载三类不同故障域：

- HTTP head/body、response、chunk 与 SSE forwarding；
- provider/Codex inference dispatch、catalog、protocol error mapping；
- Skill install bridge 的 host lock、request spool、replay、heartbeat 与 terminal response。

粗略边界已经存在：

- HTTP 与 stream helper 主要位于文件前部；
- provider/Codex message dispatch 主要位于中部；
- Skill bridge 主要从约第 1,608 行开始；
- `serve` 位于文件尾部，适合作为稳定 façade。

### 目标边界

```text
server::serve
  -> http_codec
  -> inference_dispatch
     -> provider modules
     -> codex transport/protocol
  -> skill_bridge_host
```

`codex_protocol.rs` 虽大，但其 request translation、SSE decoder 与 Responses reducer 属于同一协议状态机。除非先发现明确的独立变更轴，不应仅因行数把它先拆碎。

### 验收不变量

- path-secret 检查与 raw `CONNECT` 分派顺序不变；
- body、event、tool、non-stream 等既有 size bound 不变；
- HTTP status、typed error envelope、SSE event 和 stream termination 不变；
- scratch 与正式 Gateway 仍共享 binary 但不共享状态承诺；
- Skill bridge request ID、replay window、terminal-once 与 host lock 语义不变；
- `server::serve(GatewayConfig)` 保持稳定入口。

## P1：拆 frontend feature controller

### 现状

`desktop/src/main.js` 为 2,865 行，也是最近 100 个提交中触达最多的路径之一。约前 370 行是 browser preview mock，之后同时管理：

- 全局 DOM 与 busy state；
- profile / model catalog editor；
- runtime start、stop、history recovery 与 status；
- Codex auth operation 和 network setting；
- diagnostics、update 与页面 wiring。

仓库已经提取了 `codex-auth-protocol.js`、`runtime-status-state.js`、`model-catalog-state.js` 和 Skill page 模块，说明按 feature 拆分已有可复用模式。

### 目标边界

优先提取：

1. `preview-adapter`：mock state、scenario 和 `mockInvoke`；
2. `ipc-client`：command name、参数 casing、typed rejection 与 event subscription；
3. runtime、profile/model、Codex 三个 feature controller；
4. 保留单一 bootstrap 负责 DOM 装配和跨 feature 导航。

### 验收不变量

- Tauri command 名称、顶层 camelCase 与 serde payload snake_case 不变；
- `boot://failed`、`boot://attention`、`codex-auth://operation` 的 listener 与冷启动补读不变；
- preview 不得成为生产 caller 证据；
- frontend 继续只接收掩码或脱敏状态。

## P1：拆 config 的模型、迁移与存储

### 现状

`desktop/src-tauri/src/config.rs` 的生产主体约 2,449 行，被约 23 个 Tauri 源文件直接引用。它同时拥有：

- `Profile`、`Config` 和 runtime journal 数据模型；
- v1 → v4 迁移；
- secure directory 与 atomic write；
- pending cleanup manifest；
- rolling / migration backup；
- downgrade transaction。

这是高 fan-in 文件，直接改 public function 会放大回归面。

### 目标边界

保留 `config` façade 的 `load_from`、`save_to`、`update` 和 `update_result`，内部再分为：

```text
config
  -> model
  -> migration
  -> store
  -> cleanup_manifest
  -> downgrade
```

该阶段只移动所有权，不增加配置版本，不改变 JSON schema，不改变原子提交、fsync、rename、CAS 或 backup 语义。

## P1：拆 Codex control，而不是拆认证语义

`commands/codex.rs` 的生产主体约 2,093 行，混合 Tauri command、sidecar spawn/wait、NDJSON envelope 校验、operation event、profile ensure、network setting 和 downgrade。建议保留 command façade，把以下实现移入 CSSwitch-owned Codex control 模块：

- sidecar process 与 protocol runner；
- login operation state / event projection；
- settings 与 downgrade orchestration。

`CodexAuthSupervisor` 的 mutation lease、cancel、sequence 和 last-known status 仍是控制面所有权；拆分不应创造第二套 auth state。

## P1：收拢 provider 扩展点

当前 Desktop 与 Gateway 都从 `catalog/provider-contracts.v1.json` 读取 provider contract，这是正确的扩展方向。但两侧仍各自定义 Rust contract 类型与校验逻辑，存在解释漂移风险。

在 Gateway server 拆分稳定后，可把共享的 catalog parsing、enum 和静态校验收拢到一个现有 workspace 可复用的 Rust library，Desktop 保留编辑与 launch-plan 校验，Gateway 保留 runtime endpoint 与 transport 校验。

新增 provider 应沿用：

```text
provider catalog
  -> Desktop launch plan
  -> Gateway transport module
```

不要在 frontend、runtime adapter 或 `server.rs` 中新增 provider 名称特判。

## P2：暂缓的拆分

### Skill store / deployment

`skill_manager/store.rs` 和 `deployment.rs` 很大，但主要围绕 no-follow 文件访问、ownership、quota、atomic commit、durability 与 reconciliation。它们是安全内聚模块。应先冻结文件系统 fault tests 和 transaction invariant，再按安全原语、store transaction、deployment transaction 分层；不适合作为第一批“为了减行数”拆分。

### Codex protocol

`gateway/codex_protocol.rs` 的生产主体约 2,445 行，但 request translation、tool history、SSE decoding、reasoning verification 与 Responses reduction共享协议状态。先保留；若未来变更长期只集中在 reducer 或 tool translation，再以状态机边界拆分。

### Runtime command tests

`commands/runtime.rs` 的 7,484 行中，生产主体约 984 行。优先把测试随被提取的实现移动到对应 module，而不是单独把测试拆成一个无法表达所有权的大文件。

## 故障定位目标

重构后的故障定位应能先回答“哪个 CSSwitch-owned domain 失败”，再查看 subsystem detail：

| Domain | 首要证据 | 不应混淆为 |
|---|---|---|
| Desktop / IPC | command typed result、event、frontend state | Gateway 或外部 runtime 内部失败 |
| runtime transaction | operation stage、journal、recovery status | 单纯 HTTP health |
| Gateway / provider | typed HTTP error、transport、catalog、stream termination | runtime identity |
| runtime adapter | CSSwitch selection、listener identity、managed receipt、stop proof | 外部 runtime 内部架构 |
| optional integration | Skill / SSH / Codex 局部 typed status | 全局 lifecycle 失败 |

现有 operation log、typed status、Codex error 和 Gateway error 应继续作为事实源。第一阶段只补齐它们之间的稳定映射，不增加新的 probe、lint、schema、gate 或 handoff 流水线。

## 建议实施顺序

1. 为现有 runtime failure 建立内部 typed projection，并冻结现有 DTO；
2. 从 `sandbox_session.rs` 机械提取 pending cleanup；
3. 机械提取 recovery projection；
4. 缩短一键 orchestrator，保持原锁序和提交顺序；
5. 从 `gateway/server.rs` 提取 Skill bridge host；
6. 提取 HTTP codec 与 inference dispatch，保留 `serve` façade；
7. 从 `main.js` 提取 preview adapter 和 IPC client，再按 feature 拆 controller；
8. 保持 façade 后拆 config 内部；
9. 拆 Codex control implementation；
10. 最后评估 provider shared library、Skill store 和 Codex protocol。

每一步的退出条件都是“行为不变、所有权更清楚、失败能定位到单一 CSSwitch domain”，而不是达到某个任意文件行数。
