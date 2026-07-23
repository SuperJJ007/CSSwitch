# Science runtime 规则

- Science executable、持久 data-dir、版本 runtime 资源、组织数据和监听进程是不同事实。
- 新启动按“显式开发 override → 通过固定路径、本地身份与文件安全校验后生成的 updater executable 内容寻址 snapshot → 已安装 App executable → 隔离 cache one-shot”选择，并始终复用 CSSwitch 隔离 data-dir。
- `SCIENCE_BIN` 仅是显式开发 override；无效时 fail closed。历史缓存绝不能隐式回退。
- 只允许读取真实 `~/.claude-science` 下固定 executable；通过属主、权限、Mach-O、embedded identifier / Team ID 校验后，将同一次安全打开读取的字节原子提交到 CSSwitch 私有、只读、SHA-256 内容寻址 snapshot，并从 snapshot 执行。检测到候选但校验失败时不得静默回退旧 App。这些本地 metadata 不是官方来源的密码学证明。不得扫描、复制或读取其他账号、组织、配置、runtime 资源或凭证。CSSwitch 不下载或升级 Science，并保持 `--no-auto-update`。
- Science 与 CSSwitch Gateway 均绑定 loopback；引入或暗示 `0.0.0.0` 需要单独的安全和产品决策。
- 端口占用或 `status` 成功不能单独证明 runtime 身份；需结合 executable、选择时文件指纹、data-dir、监听 PID 和受管启动身份。选择后 executable 被替换必须 fail closed。
- 停止成功必须以 sandbox 端口真实关闭为准，不能只信任 Science CLI 的 0 退出码。CLI 假成功时只允许终止停止前后均保持唯一监听且 canonical executable 精确匹配的 PID；身份变化时不得发送信号。
- 已健康 daemon 不因版本探测或可选功能漂移而强制重启。
- 外部 Skill route / connector 配置失败只降级该可选功能，不阻断普通 Science 启动。
- 系统 SSH 默认关闭；一旦用户启用，真实 config 与 packaged wrapper 的安全校验属于 fail-closed 启动条件，不能当作 warning 略过。
