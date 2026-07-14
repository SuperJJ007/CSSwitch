# Science runtime 规则

- Science executable、持久 data-dir、版本 runtime 资源、组织数据和监听进程是不同事实。
- 新启动通常使用用户当前安装的官方 Claude Science App executable，并复用 CSSwitch 隔离 data-dir。
- `SCIENCE_BIN` 仅是显式开发 override；无效时 fail closed。历史缓存绝不能隐式回退。
- 不从真实 `~/.claude-science` 复制 runtime 资产，不下载或升级 Science；保持 `--no-auto-update`，除非产品合同另行批准。
- Science 与 CSSwitch Gateway 均绑定 loopback；引入或暗示 `0.0.0.0` 需要单独的安全和产品决策。
- 端口占用或 `status` 成功不能单独证明 runtime 身份；需结合 executable、data-dir、监听 PID 和受管启动身份。
- 已健康 daemon 不因版本探测或可选功能漂移而强制重启。
- 外部 Skill route / connector 配置失败只降级该可选功能，不阻断普通 Science 启动。
- 系统 SSH 默认关闭；一旦用户启用，真实 config 与 packaged wrapper 的安全校验属于 fail-closed 启动条件，不能当作 warning 略过。
