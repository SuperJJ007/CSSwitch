# 架构文档

- [架构总览](overview.md)：CSSwitch 产品边界、所有权、依赖方向、网络与失败边界。
- [Desktop 控制面](desktop-control-plane.md)：Tauri command 注册、生产 frontend caller、event、DTO 与错误投影。
- [运行时状态与事务](runtime-state-transactions.md)：`AppState`、`Config`、`Lifecycle`、锁序、journal、recovery 与补偿。
- [Gateway 与 provider 路由](gateway-provider-routing.md)：正式/scratch Gateway、Codex、Skill stdio 与 runtime adapter。
- [Science runtime 适配合同](science-runtime.md)：只描述 CSSwitch-owned executable selection、隔离、identity 和 recovery contract。

这里仅维护 CSSwitch 自己的跨版本合同。Claude Science 是外部 runtime；其内部架构、私有接口、数据库、package layout 与逆向调查不属于公开文档。
