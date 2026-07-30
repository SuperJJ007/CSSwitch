# Desktop 控制面

本文回答 WebView、Tauri command/event、DTO 与 CSSwitch backend 怎样连接。状态提交与补偿见 [运行时状态与事务](runtime-state-transactions.md)，Gateway 协议见 [Gateway 与 provider 路由](gateway-provider-routing.md)。

## 可达层定义

| 层 | 定义 |
|---|---|
| `compiled` | 进入当前 Rust module graph 或 frontend bundle |
| `registered` | compiled 后进入 Tauri invoke handler 或 event 面 |
| `product-reachable` | 当前生产 frontend、已启用的条件 auto-boot 或受管 runtime 存在调用路径 |
| `test-only` | 只从 `cfg(test)`、ignored E2E、preview/mock 或测试 helper 到达 |
| `legacy/orphan` | 文件存在，但没有进入当前 module、invoke 或 frontend 产品图 |

`production-source` 只是变更影响分类，不能替代可达性判断。

## 边界与依赖方向

```text
WebView
  -> invoke(command, DTO)
  -> Tauri command
  -> command-specific boundary
     -> Lifecycle serializer, or
     -> operation-local transaction and runtime-context recheck
  -> Config / AppState / Gateway / runtime adapter

Tauri backend
  -> event(payload)
  -> WebView listener
```

`desktop/src-tauri/src/lib.rs::run` 是 command 注册入口。生产 frontend caller 既可能位于 `desktop/src/main.js`，也可能位于其动态导入模块。preview 的 `mockInvoke` 只模拟 DTO，不构成生产 caller。

## command 面

注册 command 按 CSSwitch 职责分组：

| 组 | 职责 |
|---|---|
| profile / settings | 配置读取、创建、编辑、删除、当前选择、模式及端口/SSH/Codex 设置 |
| runtime | 一键开始、恢复选择、停止、preflight、状态、URL 与退出 |
| model / provider | 模型发现与连接验证 |
| Codex | 开关、网络、auth operation、profile、logout 与 downgrade export |
| Skill | 本地包安装与已安装 Skill 列表 |
| diagnostics | doctor、版本、release、issue 与 log 入口 |

大多数 runtime、profile 和 mode 的复合 mutation 进入 `Lifecycle`。这不是所有 Desktop 写操作的统一锁：本地 Skill 安装使用文件选择前后的 runtime-context 复核，再进入 package commit 和 runtime adapter 的局部提交。第二次复核不是互斥锁，其后仍可能与 stop 或 switch 交错。

以下 command 已注册，但当前生产 frontend 没有 caller：

- `list_templates`
- `validate_profile_catalog_model`
- `preview_profile_preset_sync`
- `apply_profile_preset_sync`
- `start_proxy`

它们属于 dormant registered surface；注册本身不构成产品能力。

## event 面

| Event | Payload | 当前边界 |
|---|---|---|
| `boot://failed` | string | 只保留 message，丢失一键 DTO 的结构化阶段与恢复字段 |
| `boot://attention` | JSON value | 保留需要用户处理的 attention 对象 |
| `codex-auth://operation` | typed operation snapshot | 保留 sequence、state 和 error 等结构化字段 |

frontend 启动时同时读取 boot result command 并监听对应 event，用于覆盖 listener 注册前已发生的结果。

## 一键 DTO 与错误投影

手动一键开始存在四类结果：

1. 成功或复用：resolved object；
2. 需要用户选择：resolved `status=attention` object；
3. 普通失败：resolved `status=error` object，包含公开的 stage 与恢复状态；
4. typed command error：invoke rejection，由 frontend `catch` 处理。

内部 operation trace、持久 journal 与 frontend stage 不是同一枚举。当前公共 stage 仍有一部分由错误字符串分类，文案变化可能改变投影；auto-boot 又只转发 message。故障分析不能把 frontend stage 当成精确 transaction checkpoint。

## 选择、应用与诊断语义

- `set_active_profile` 只持久化当前选择；运行中的 Gateway/runtime 不立即切换，下一次一键开始才应用。
- `status` 是轻量状态投影；HTTP health 不证明完整 runtime identity。
- `run_doctor` 会在 `Lifecycle` 边界内尝试修复 CSSwitch-owned optional bridge state，因此不是纯只读诊断；它不会仅为诊断启动 Gateway 或外部 runtime。
- 关闭窗口只隐藏；显式退出才按受管顺序停止 runtime 和 Gateway。

## 条件入口

- auto-boot 只在 `CSSWITCH_AUTO_BOOT_ON_LAUNCH=1` 时进入。
- 内嵌 runtime WebView 只在 `CSSWITCH_SCIENCE_WEBVIEW_SPIKE=1` 时尝试。

仓内没有生产 UI 或 packaging 为这两个变量赋值的 producer，因此它们是条件源码路径，不代表普通安装默认启用。

## 维护规则

- 新增或删除 command 时，同时检查注册、所有生产 caller、preview mock 和 DTO。
- 新增 event 时，明确 payload schema、冷启动补读策略和敏感字段。
- “有 command”“有 mock”“有测试”不得写成“UI 可达”。
- 公共错误阶段应来自结构化字段；在实现统一前，文档必须保留字符串分类缺口。
