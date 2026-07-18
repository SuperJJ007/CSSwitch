# v0.7.0 新 UI：runtime / 安全审查

## 审查对象与方法

- 对象：Science 控制面、外部 Skill route、Skill listing/install、状态读取、临时文件和响应体边界。
- 主审按输入边界、路径/对象身份、竞态、资源上限、secret 暴露和生命周期逐项检查；随后由 `gpt-5.6-sol`、`xhigh` 子 agent 两轮只读复审。
- 所有动态验证都使用临时 HOME、动态 loopback 端口和 fixture；保留端口 8765 只作只读基线检查。

## 主审发现

1. 固定 Science 临时路径只在路径层 chmod，未把权限修改绑定到已经验证的目录文件描述符，存在符号链接/替换窗口。
2. Science HTTP 响应在完整 `.bytes()` 后才校验上限，无法阻止超限主体先占用内存。
3. active org 在长列表过程中改变时，旧组织结果可能被发布；route 没有完整贯穿 expected org。
4. marker/active-org 文件需要拒绝 symlink、FIFO 等非普通对象，并限制读取量。
5. 父目录逐段解析仍有理论 TOCTOU 面，但当前目录由隔离 fixture 或私有应用根控制。

## 修改

- 固定临时路径拒绝 symlink 和非目录对象，使用 `O_DIRECTORY|O_NOFOLLOW` 打开并对已验证 fd 执行权限收紧。
- HTTP 主体改为 `read_limited` 流式累计，声明长度和实际流均受上限约束。
- marker 与 active-org 使用 `O_NOFOLLOW`、类型检查和有界读取；listing 校验 marker tuple。
- Skill listing 在前后两次复核 active org，变化即丢弃；外部 route 把 expected org 传入并核验。
- system route 的 current/legacy body 与 marker 同样使用有界、非跟随、非阻塞的打开后读取；组织变化会丢弃完整旧快照，包括 warning。
- 相应失败均保持结构化、无 secret、无真实路径回显。

## 子 agent 复审与终态

第一次复审确认上述资源上限与竞态是实质问题。交叉复审又定位到 system route 二次鉴别无界读取、前端在途旧请求、旧 warning 与测试并行隔离四项缺口；修正后最终复审没有 P0–P2。唯一保留项是 P3 理论父目录替换窗口：若未来数据根允许不受信任方写父目录，应升级为逐段 `openat`/fd-relative 遍历；在当前私有根与最终对象 fail-closed 检查下，不把它扩成复杂框架。

## 复证

- Skill package：55 passed / 4 explicit ignored。
- system route：10/10；core listing 默认并发：9/9；Tauri Skill listing：4/4。
- Desktop Rust 全量：317 passed / 3 explicit ignored。
- Gateway Rust：228 library + 1 CLI passed。
- loopback：82/82。
- bash guards、doctor、verify-proxy、real-machine guard 全部通过。
- `bash test/run_all.sh --require-release-ready` 最终为五层 `pass`、`release-ready green: YES`。

## 边界

没有启动正式 App、没有信号或接管非自有进程、没有读取真实 HOME 凭据，也没有写入真实 `~/.csswitch/skills` 或 Science 数据。两项 ignored runtime smoke 仍须显式隔离执行，未计为自动通过。
