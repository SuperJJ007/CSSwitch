# Science runtime 适配合同

本文只描述 CSSwitch 如何对接外部 Claude Science runtime。Science 自身的内部架构、私有接口、数据模型、安装调查和版本证据不属于公开合同。

## 不透明外部 runtime

CSSwitch 把以下内容视为彼此不同的运行事实：

1. 本次选择的 executable；
2. CSSwitch 分配的隔离 HOME 与持久 data root；
3. CSSwitch 记录的 source、version 与文件指纹；
4. live listener、PID、端口和 managed launch receipt；
5. 外部 runtime 自己拥有的不透明内容和资源。

CSSwitch 只拥有前四项的选择、记录和校验逻辑。外部 runtime 内容即使位于 CSSwitch 分配的隔离目录中，也不会因此变成 CSSwitch-owned 数据。

## executable 选择

新启动按以下 CSSwitch 策略选择候选：

1. 显式开发 override；
2. 经本地文件安全与产品 identity 校验的用户级候选；
3. 已安装 App 中的候选；
4. 用户对历史 cache 的单次授权。

显式 override 无效时 fail closed。用户级候选被安全读取后，CSSwitch 可在自己的 private data root 中生成只读、内容寻址的 execution snapshot，避免选择后替换。历史 snapshot 只能用于确认既有受管 daemon 的身份，不能改变 stopped-to-started 的新启动顺序。

CSSwitch 不下载、升级、覆盖或降级 Science，也不把本地 metadata 校验描述成上游来源的密码学证明。具体安装路径、identifier、hash、版本和 package-static 观察不属于本文。

## 隔离与启动参数

CSSwitch 为第三方模式分配隔离 HOME、持久 data root、UI port 和 sandbox port，并要求所有受管 listener 绑定 loopback。CSSwitch 启动时禁用外部 runtime 的自动更新，以避免一次受管运行中发生未协调的 executable 切换。

端口健康只证明端点有响应，不证明 runtime identity。公共网络暴露不属于当前产品合同。

## recovery projection

CSSwitch 的启动事务只保护“CSSwitch 可能改写且失败时必须补偿”的显式管理投影。所有未列入 CSSwitch 管理合同的外部 runtime 目录和内容都视为 opaque：

- 不递归扫描、复制、同步、删除或回滚；
- 只在需要保护 CSSwitch transaction 时校验固定顶层入口的类型、属主、权限和身份；
- 未知顶层内容保持原位，除非它与 CSSwitch 管理入口冲突或根身份不安全。

recovery snapshot 在任何受保护写入前登记为 active recovery。只有事务成功、完整补偿或确认无需继续恢复时，才能以 compare-and-swap 转为 cleanup-only 并删除。进程崩溃、身份变化或补偿不确定时保留 manifest，不把部分写入态当成新基线。

runtime 启动前失败时，CSSwitch 可以恢复自己的管理投影。启动后外部 runtime 可能已修改不透明状态，因此 CSSwitch 只报告外部状态是否可确认，不声称回滚其内部环境。

## live identity

CSSwitch 在 `AppState` 中记录本次 launch 的 executable snapshot、source、version 和选择时文件指纹。需要复用、恢复或停止 daemon 时，adapter 将这些记录与 live listener、canonical executable、data root 和 managed receipt 组合校验。

停止不能只信任外部 CLI 的退出码。只有监听进程在停止前后都与 managed identity 一致时，CSSwitch 才能对其执行受限的 TERM/KILL fallback；身份变化时不得发送信号，并以端口真实关闭作为成功条件。

高频 UI `status` 是轻量投影：它只报告短超时 health 与已有 metadata，不能替代强 identity 检查。CSSwitch 自身重启后，也只有在 executable、data root、listener 和 receipt 一致时才能接管既有 daemon。

## 升级行为

下一次 stopped-to-started 启动会重新执行候选选择。已健康 daemon 继续使用启动时固定的 executable，不因外部候选出现变化而强制重启。CSSwitch 始终复用自己分配的持久 data root，但不迁移或覆盖外部 runtime 内容。

每次外部 App 更新后，应分别验证：

1. selected executable 与公开版本输出；
2. 隔离 HOME/data root 未越界到用户真实 runtime 数据；
3. live listener、executable、data root 与 receipt 属于同一运行；
4. start、reuse、recovery、URL 与 stop 的强身份一致性；
5. 可选 Skill/SSH bridge 的兼容性和降级行为。

一次版本验证不能推出通用最低版本。adapter 不兼容时，只关闭受影响的可选 bridge 并如实报告。

## 非目标

- 不公开或依赖 Science 私有 control plane、内部数据库或 package layout；
- 不读取、复制或修改用户真实 Science 登录数据；
- 不把外部 runtime 内容当作 CSSwitch 可重建缓存；
- 不为 Skill、SSH 或 provider 失败扩大 runtime 权限；
- 不把 runtime adapter 扩展成通用外部能力管理器。
