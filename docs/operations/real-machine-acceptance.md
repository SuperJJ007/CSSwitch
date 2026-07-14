# CSSwitch v0.5.0 真机验收

本矩阵描述应如何验收，不表示各项已经通过。每次执行必须记录目标 commit / artifact、环境和结果；发布附件的既有结果见对应 [release evidence](../evidence/releases/README.md)。

## 1. 安全护栏

- 使用独立 `HOME`、独立 `~/.csswitch`、独立 Science data-dir 和测试端口。
- 不读取、修改或删除真实 `~/.claude-science`、真实 Keychain / OAuth / SSH 私钥或真实 `~/.csswitch`。
- 真实 Science 的 `8765` 端口只用 `lsof` 观察基线 PID，不停止或接管。
- 已安装 CSSwitch 正在运行时，不强退用户实例；构建独立 bundle ID 的 Acceptance app。
- 真实 provider、真实 Claude 登录和真实 SSH server 测试必须单独获得授权。
- 截图与日志只保留端口、PID、状态码、profile 名称和脱敏摘要，不含 key、path secret 或 nonce。

## 2. 自动化基线

```bash
bash test/run_all.sh
```

记录五层状态和 `current-env clean` / `release-ready green`，不要记录过期的固定 pass 数。构建发布候选前另跑 `--require-release-ready`。Python 仅供测试与 mock 使用；v0.5.0 runtime proxy 是 Rust sidecar。

## 3. 先在开发 HOME 构建

```bash
PROJECT_ROOT="$PWD"
DEV_HOME="$HOME"
(
  cd desktop
  PATH="$DEV_HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
    npm run tauri build -- --config ../test/tauri.real-machine.conf.json --bundles app
)
```

目标为 `desktop/src-tauri/target/release/bundle/macos/CSSwitch Acceptance.app`。必须在导出隔离 `HOME` **之前**构建；否则 `$HOME/.rustup` 会指向空的测试 HOME。

## 4. 隔离准备与启动

```bash
bash test/real_machine_guard.sh preflight
```

只有验证 RM-01 v1 -> v2 迁移时才准备 legacy fixture。该步骤要求两个非空变量；使用明确的假值，不要读取或写入真实 provider key：

```bash
DEEPSEEK_API_KEY='csswitch-migration-fixture-deepseek' \
DASHSCOPE_API_KEY='csswitch-migration-fixture-qwen' \
  bash test/real_machine_guard.sh prepare-legacy
```

随后才把当前 shell 切到 guard 生成的隔离运行环境：

```bash
eval "$(bash test/real_machine_guard.sh env)"
```

验证 SSH opt-in 时，在这个隔离 HOME 内创建空的普通 config fixture；它只用于 wrapper / fail-closed 合同，不证明真实服务器连通：

```bash
install -d -m 700 "$HOME/.ssh"
install -m 600 /dev/null "$HOME/.ssh/config"
```

启动独立 Acceptance app：

```bash
HOME="$HOME" CSSWITCH_REPO="$CSSWITCH_REPO" \
  "$CSSWITCH_REPO/desktop/src-tauri/target/release/bundle/macos/CSSwitch Acceptance.app/Contents/MacOS/desktop"
```

正式 DMG 验收应从只读挂载的 app 复制到隔离位置，并且不设置 `CSSWITCH_REPO`；不能拿源码 build 代替最终 artifact。

`preflight` 应记录 8765 基线、创建隔离 HOME 并确认测试端口可用。每次改变运行态后执行：

```bash
bash test/real_machine_guard.sh guard
```

若 8765 PID 变化，或真实用户目录被碰触，立即停止并把该次验收记为失败 / 证据污染。

## 5. 当前验收矩阵

原有 RM-01～RM-18 保持历史语义；v0.5 新增场景从 RM-19 继续编号，避免源码注释和旧证据错指。

| ID | 场景 | 操作 | 必须满足 |
|---|---|---|---|
| RM-01 | v1 -> v2 迁移 | 用假 key fixture 首次启动 | DeepSeek / Qwen profile 与 active 正确；`config.json.v1.bak` 为 `0600`；key 只显示掩码 |
| RM-02 | 新建 profile | 新建后分别取消 / 完成 | 取消不落盘；完成新增且不自动生效；同模板可多条 |
| RM-03 | 元数据编辑 | 改名和备注后重启 | 名称 / 备注持久；连接字段与 key 不变 |
| RM-04 | non-active 连接编辑 | 正确 key、错误 key、5xx / 断网 | 2xx 标已验证；明确 4xx 拒绝且不落盘；含糊态保存但标未验证 |
| RM-05 | 激活切换 | DeepSeek ↔ Qwen | scratch 与正式 Gateway 健康后才提交 active；Gateway PID / adapter 变化；Science 不重启 |
| RM-06 | 激活失败回滚 | 候选使用错误 key / model | `active_id` 不变；旧 Gateway 恢复；UI 不谎称成功；Science 不停止 |
| RM-07 | active 连接编辑 | 修改当前连接为有效 / 无效值 | 有效值提交并换 Gateway；无效值不落盘且恢复旧链 |
| RM-08 | 一键开始 | 连续点击两次 | 首次启动 Gateway + Science；再次幂等复用并 reopen；UI status 只按 health 解释 |
| RM-09 | 整链推理 | 经授权发送 minimal text 与 tool request | 实际 provider / model / tool 结果分栏；日志无 path-secret / key；8765 PID 不变 |
| RM-10 | 清 key | 对 active / non-active 各清一次 | active 撤销链路并清 active；non-active 不影响当前链；backup 不可恢复旧 key |
| RM-11 | 删除 profile | 删除 non-active；尝试删除 active | non-active 消失且链不变；active 不留下悬空 `active_id` |
| RM-12 | 端口变更 | 运行中修改 Gateway / Science port | 先停受管链再保存；旧端口释放；下次按新端口启动 |
| RM-13 | 端口冲突 | 预占候选端口 | 明确报占用；不误报 key；不杀未知占位进程 |
| RM-14 | 官方模式 | 第三方链运行时切换 | 只停测试 Gateway / Science；真实 8765 不变；切回不自启 |
| RM-15 | 全部停止 / 退出 | UI 停止后退出 | 据实报告；测试端口释放；无残留受管 desktop / gateway 子进程 |
| RM-16 | 重启恢复 | 同一隔离 HOME 重开 | profiles / active / notes / ports 持久；不自动启动；恢复不能仅凭端口冒认 runtime |
| RM-17 | 包资源 | 从 `.app` 与挂载 DMG 启动 | `Contents/MacOS/{desktop,csswitch-gateway}` 与 `Contents/Resources/scripts` 齐全；无旧 `Resources/proxy`；正式包无需 `CSSWITCH_REPO` |
| RM-18 | 发布安全 | hash、codesign、spctl、stapler | 签名完整性、身份、公证、ticket、Gatekeeper 分栏；不把 ad-hoc 写成已公证 |
| RM-19 | installed App 优先 | App 与 stale cache 同时存在 | 选择 App executable，复用原 data-dir，cache 不被改写 |
| RM-20 | explicit / cache preflight | 合法 / 非法 `SCIENCE_BIN`，App 缺失与 cache 组合 | override 无效 fail closed；cache 仅版本可读时提供 one-shot；选择不持久化 |
| RM-21 | Science 升级与强身份 | 替换测试 App 后 stopped-to-started；再恢复 / stop | 新 executable + 原 data-dir；启动 / 恢复 / stop 核对 PID、binary、data-dir、port；UI status 仍只代表 HTTP health |
| RM-22 | Skill Agent 控制面 | 首次配置、重复启动、注入中途失败 | 管理固定 route / connector / `customize` / prompt；成功 marker 后跳过重复；失败 warning 且如实报告可能的部分配置 |
| RM-23 | 外部 Skill 安装 | 精确公开 GitHub URL | connector -> host approval -> commit -> native attach -> `skill()` load，各阶段分开记录 |
| RM-24 | Skill 重启 / 卸载 | 同 data-dir 重启，再卸载 | 重启仍 load；只 quarantine 有 marker 的导入；native detach；不走 catalog / shell |
| RM-25 | 运行中 Skill 配置漂移 | Science 运行时改变 MCP / route 预期 | 只读检查并返回 `RESTART_REQUIRED`；不并发改写；普通 Science 继续 |
| RM-26 | 系统 SSH 默认 / opt-in | 无 fixture、创建 fixture、再移除 fixture | 默认关闭不阻断；启用时 wrapper 使用 `/usr/bin/ssh -F`；启用后 config / wrapper 缺失必须 fail closed |
| RM-27 | SSH 非目标 | 检查文件与监听状态 | 不复制 `.ssh`、不启动 `sshd`、不改防火墙、不监听 `0.0.0.0`；真实 server 另行授权 |

## 6. Skill 证据词汇

外部 Skill 至少分为：content fetched、目录 committed、Science discovered、Agent attached、`skill()` loaded / triggered、领域功能完成、重启持久化、quarantine、detached。不能用一个“安装成功”覆盖所有层。

bundled route 必须使用 `mcp-csswitch-skill-installer` 的 `install_external_skill` / `uninstall_external_skill`，不得回退到 `customize`、`host.skills.*`、shell 或手工文件删除。

## 7. Artifact 检查

对最终候选分别记录：版本、大小、SHA-256、包内 executable / resources、`codesign --verify`、签名身份、`spctl`、stapler。ad-hoc seal 验证通过不能写成 Developer ID、notarization 或 Gatekeeper 通过。

## 8. 收尾

在 UI 停止链路并退出验收 app 后运行：

```bash
bash test/real_machine_guard.sh assert-stopped
```

确认测试端口释放、8765 PID 不变、真实用户目录未改、已安装用户 app 未被替换。每项如实标为通过、失败、环境阻塞、未执行或需人工判断。
