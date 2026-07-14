# CSSwitch 架构总览

本文是 v0.5.0 当前架构合同，只保留产品边界、所有权、数据流和失败边界。版本固定的调查结果见[证据目录](../evidence/README.md)。

## 产品边界

CSSwitch 是 Claude Science 的 provider 配置转换器、本地 inference gateway 和隔离启动器。它负责：

- 将当前 profile 转换为 Science 使用的 Anthropic 兼容本地端点；
- 管理 Rust CSSwitch Gateway 生命周期；
- 准备隔离登录状态并复用持久 Science data-dir；
- 选择、启动、复用、打开和停止正确的 Science runtime；
- 提供两个窄范围可选 bridge：公开 GitHub 外部 Skill 安装 / 隔离卸载，以及显式授权的系统 SSH 配置复用。

Science 仍拥有项目、组织、原生 Skills、runtime 资源、Agent 绑定与升级。CSSwitch 不模拟 Anthropic OAuth / catalog，也不扩展成 Skill Manager、Science 下载器或远程访问服务。外部 Skill bridge 失败不阻断一键启动；系统 SSH 默认关闭，但用户一旦启用，其 config / wrapper 安全校验就是 fail-closed 启动条件。

## 主数据流

```text
CSSwitch profile
  -> Rust CSSwitch Gateway (loopback)
  -> 隔离登录状态
  -> 持久 Science data-dir
  -> 选择并启动 / 复用 Science executable
  -> Science UI
```

普通一键启动不经过外部 Skill store、inventory、catalog、reconcile 或 deploy。外部 Skill 的 Agent 控制面在 Science 健康后 best-effort 配置；系统 SSH wrapper 则在用户 opt-in 时于 Science 启动前完成安全校验。

## 所有权与 source of truth

| 数据 / 能力 | Source of truth | 所有者 |
|---|---|---|
| provider profiles 与 CSSwitch settings | `~/.csswitch/` 配置 | CSSwitch |
| Gateway 生命周期与本地路由 | CSSwitch runtime state | CSSwitch |
| 已安装 Science executable | `/Applications/Claude Science.app/.../claude-science` | 用户 / Science installer |
| 持久 Science 状态 | `~/.csswitch/sandbox/home/.claude-science` | Science |
| 版本 runtime 资源 | `<data-dir>/runtime/<version>/` | Science |
| 组织与 Skills | `<data-dir>/orgs/<active-org>/...` | Science 组织 |
| provider capability 元数据 | `catalog/capabilities.v1.json` | CSSwitch |
| v0.4.2 / v0.4.3 legacy Skill store / inventory | 原样保留但不参与当前 runtime | 非当前运行路径 |

持久 data-dir 提供状态连续性，不固定 executable 版本。正常新启动优先用户当前安装的 App；历史缓存只有在 App 不可用、版本可读且用户仅本次授权时才可使用。详见 [Science runtime 合同](science-runtime.md)。

## 组件边界

### Desktop / Tauri backend

管理配置、端口、Science runtime metadata、强身份边界、UI health 状态和可选 bridge 编排。关闭设置窗口只隐藏窗口；明确退出 CSSwitch 才按受管顺序停止 Science 与 Gateway。

### Rust Gateway

作为随 app 打包的 sidecar 处理推理协议与本地认证，也承载限界的外部 Skill host worker / MCP 子命令。运行时没有 Python proxy fallback。

### Science runtime

新启动显式传入 data-dir、`--host 127.0.0.1`、独立 `--sandbox-port` 和 `--no-auto-update`。CSSwitch 记录实际 binary path、来源和版本；启动、复用、恢复与停止边界另结合监听 PID、canonical executable 和 data-dir CLI 结果做强身份判断。高频 UI `status` 只做 HTTP health，并返回内存中的 runtime metadata，不能凭它证明端口属于本沙箱。

### 外部 Skill bridge

一个 bundled routing Skill 将安装 / 卸载请求送到合并 connector；host worker 只把用户批准的公开 GitHub Skill 写入 active org，或隔离带 CSSwitch ownership marker 的导入。Science 原生 `attach_skill` / `detach_skill` 和 `skill()` 决定 Agent 可用性。详见[功能合同](../features/external-skill-bridge.md)。

### 系统 SSH bridge

默认关闭。启用后通过窄 wrapper 执行 `/usr/bin/ssh -F <real-home>/.ssh/config`，不复制 `.ssh`、不启动服务、不开放监听；config 或 wrapper 校验失败会 fail closed。详见[系统 SSH 文档](../features/system-ssh.md)。

## 网络与安全边界

- Gateway 与 Science UI 均绑定 loopback；产品没有 `0.0.0.0` 开关。
- Science preview port 由 CSSwitch 显式分配并检查冲突、保留端口和溢出。
- 原始 Science `serve` 输出可能含 data-dir 或一次性 URL，因此不直接进入 CSSwitch 日志。
- 一次性 Science URL、nonce 与 CSRF 状态只在 backend 内存和限界控制流中使用，不序列化到普通 Tauri status。
- 第三方模式不读取或复制真实 Claude 登录数据。

## 失败边界

provider 配置、Gateway、隔离登录准备、runtime preflight、端口所有权、Science launch 与 health / identity 可以令一键启动失败。

以下项目只能降级外部 Skill 可选功能，不能阻断或强制重启普通 Science：

- legacy Skill store / inventory 内容；
- 外部 `~/.claude/skills`；
- Anthropic catalog 不可用；
- route / MCP / Agent 控制面配置失败。

系统 SSH 是不同边界：默认关闭时完全不参与启动；启用后 config / wrapper 缺失或不安全会令 Science 启动 fail closed。Science 已启动后的单次 SSH 连接失败不影响 provider Gateway。

App 缺失且没有合格的一次性 cache 授权属于 runtime preflight 结果，不应伪装成 provider 或 Skill 错误。较新 binary 已尝试打开持久 data-dir 后，也不能盲目用较旧 binary 回退。
