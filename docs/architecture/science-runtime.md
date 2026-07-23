# Science runtime 合同

本文描述 v0.7.0 的稳定选择与身份合同。2026-07-13 的具体版本、错误 binary 事故与 E2E 证据见[日期化调查](../evidence/investigations/2026-07-13-science-runtime-and-skill-bridge.md)。

## 分离四个事实

1. **executable**：实际执行的 `claude-science` 文件；
2. **persistent data-dir**：`~/.csswitch/sandbox/home/.claude-science`；
3. **version runtime resources**：`<data-dir>/runtime/<version>/`；
4. **live identity**：canonical executable、data-dir、监听 PID、端口和受管启动记录的组合。

data-dir 持久化组织、项目、Skills 和 Science 自己的 runtime 数据，但不是 executable 版本 pin。

## 新启动选择顺序

1. 如果设置了 `SCIENCE_BIN`，它必须是绝对、非 symlink、可执行且能安全读取版本的开发 override；无效时 fail closed，不继续猜其他 binary。
2. 否则检查固定的 `~/.claude-science/bin/claude-science`。只有路径无 symlink、目录与文件由当前用户拥有且不可被 group/world 写入、文件是大小有界的 Mach-O、embedded identifier / Team ID 与当前 Science 身份相符，才把同一次安全打开读取到的字节原子提交到 CSSwitch 私有、只读、SHA-256 内容寻址的 runtime snapshot；snapshot 在隔离 HOME 下版本可确认时才选择为 `official_updated`。
3. 官方 updater runtime 不可用时，使用当前安装在 `/Applications/Claude Science.app` 中的官方 executable。
4. 只有以上来源都不可用、`<CSSwitch data-dir>/bin/claude-science` 可执行且版本可确认时，preflight 才返回 `cached_choice_required`；用户可授权 `cached_once`。
5. cache 授权只在本次启动的内存中生效，不写成偏好。未知版本或缺失 cache 不可启动。

`official_updated` 只读取并快照官方 updater 写入的固定单个 executable；snapshot 位于 `<CSSwitch data root>/runtime-snapshots/science/`，不在 Science data-dir 内。CSSwitch 不扫描、复制或读取真实 Science 账号、组织、配置、`conda`、`runtime` 或 `seed-assets`，不下载 Science、不调用 updater，也不覆盖 Science cache。检测到候选但本地校验失败时会显式报错，不静默回退旧 App。

当前上游 App seed 与 updater executable 的 `codesign --verify --strict` 都可能失败，因此 embedded identifier / Team ID 只作为格式与误选防护，不声称密码学证明文件来自 Anthropic。该路径沿用 CSSwitch 已有的“信任当前用户安装的本地 Science”边界；复制前后复核同一 source inode/metadata，snapshot 以完整 SHA-256 命名并进入 host-context fingerprint。启动、恢复、URL、status 和 stop 前 snapshot 内容变化均 fail closed；官方 updater 随后替换 source 不会改变已运行 daemon 的 executable 身份。为支持 CSSwitch 自身重启后的接管，恢复探测会在私有 snapshot 目录中重新验证已有的内容寻址 executable；历史 snapshot 只参与现有 daemon 的身份恢复，不改变 stopped-to-started 的新启动选择顺序。

## 启动与网络参数

新进程使用预检后的 binary 和固定 data-dir，并显式传入：

- `--host 127.0.0.1`；
- CSSwitch 选择的 UI port；
- 单独校验的 `--sandbox-port`；
- `--no-auto-update`。

Gateway 同样只监听 loopback。端口健康不等于身份健康；公共网络暴露不属于当前合同。

## 运行中身份与恢复

CSSwitch 在内存中记录实际 launch binary path、来源（`explicit`、`official_updated`、`installed_app` 或 `cached_once`）、版本和选择时文件指纹。启动、复用、恢复、生成受管 URL 与停止操作使用这份 runtime metadata，并在需要控制 daemon 的边界做强身份检查。

停止不能只信任 Science CLI 的退出码：部分版本会返回成功并删除 lockfile，但 daemon 仍在监听。CSSwitch 在调用 CLI 前后都要求 sandbox port 的唯一监听 PID 与已记录 executable 精确匹配；CLI 后端口仍存活时，只向这一个前后均匹配的 PID 发送 TERM，超时后再次复核同一身份才 KILL，并以端口实际关闭作为成功条件。监听身份变化时不发送信号并返回错误。

高频 UI `status` 是例外：它只对 sandbox port 做短超时 HTTP health，并把内存中的 path / source / version metadata 投影到诊断结果；它不反复调用 `claude-science status`，不重新核对监听 PID，也不能证明当前监听者就是已记录 runtime。

CSSwitch 自身重启后，只能在以下条件同时满足时接管已有 daemon：

- 监听 PID 的 canonical executable 与候选 binary 匹配；
- 候选 CLI 确认的是同一 data-dir daemon；
- 端口与受管状态一致。

单独的端口占用或 `status` 成功不足以证明身份。已健康 daemon 应复用，而不是只因 App 版本或可选 bridge 状态变化被强制重启。

## 升级合同

官方模式 updater 写入新 runtime 后，下一次 stopped-to-started 启动生成并选择对应内容 snapshot；如果没有 updater runtime 而用户更新了 Claude Science App，则下一次启动重新选择 App 内的 executable。两条路径都继续复用原 CSSwitch data-dir。使用 updater snapshot 的已健康 daemon 保持其不可变 executable，不因 source 出现新版本而强制重启；正常停止后下一次启动才切换。CSSwitch 不迁移或覆盖组织、项目和 Skill 数据。

每次上游 App 更新后，分别验证：

1. 实际 selected binary 与 `--version`；
2. data-dir 复用且没有读取真实 HOME runtime 资产；
3. live PID、executable、runtime directory、data-dir 与端口属于同一运行；
4. start / reopen / recovery / url / stop 的强身份一致，并单独确认 UI status 只表示 HTTP health；
5. 外部 Skill route、install / attach / load / restart / uninstall / detach；
6. bridge 不兼容仍只产生 warning，普通 Agent 可工作。

一次上游版本测试不能推出通用最低版本。观察接口变化时，应只关闭受影响 bridge 并如实报告，而不是替换或降级用户 App。

## 非目标

- 不把 `@` artifact / output 当成持久 Skill 注册；
- 不把 `<data-dir>/runtime/<version>/skills` 当外部 Skill 安装目标；
- 不通过 OAuth、私有 bearer、数据库写入或 binary patch 管理 Science；
- 不为 SSH、Skill 或 provider 失败扩大 runtime 权限。
