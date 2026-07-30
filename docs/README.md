# CSSwitch 文档总入口

公开产品概览从根目录 [README 中文版](../README.md) / [English](../README.en.md) 进入。本轮维护的 architecture、features 与 operations 当前合同只描述 CSSwitch 自己的产品边界和运维方法；`docs/evidence/` 继续保留仓库历史上已经公开的日期化证据。本轮新增的外部 runtime 私有调查材料不因主题或后继关系自动进入公开文档。

## 架构

- [架构索引](architecture/README.md)：按稳定问题选择当前架构正文。
- [架构总览](architecture/overview.md)：产品边界、所有权、依赖方向和高层故障定位。
- [Desktop 控制面](architecture/desktop-control-plane.md)：Tauri command/event/DTO 与 frontend caller。
- [运行时状态与事务](architecture/runtime-state-transactions.md)：CSSwitch 状态 ownership、锁序、journal、recovery 与补偿。
- [Gateway 与 provider 路由](architecture/gateway-provider-routing.md)：正式/scratch Gateway、Codex、Skill stdio 与 runtime adapter 边界。
- [Science runtime 适配合同](architecture/science-runtime.md)：只描述 CSSwitch-owned executable、隔离、identity 与 recovery contract。

## 运维

- [开发](operations/development.md)
- [自动测试](operations/testing.md)
- [真机验收](operations/real-machine-acceptance.md)
- [发布](operations/release.md)
- [升级与回滚](operations/upgrade-and-rollback.md)
- [Quality kernel](operations/quality-kernel.md)：机器事实源、source-green 与证据层边界。

## 功能合同

- [功能合同索引](features/README.md)
- [外部 Skill bridge](features/external-skill-bridge.md)
- [系统 SSH 配置复用](features/system-ssh.md)
- [UI 信息架构](features/ui-information-architecture.md)

## 证据

- [证据索引](evidence/README.md)
- [发布证据](evidence/releases/README.md)
- [日期化调查](evidence/investigations/README.md)

日期化证据只证明所绑定的源码、artifact、runtime 和环境；不能覆盖当前架构合同。

## 外部参考

- [外部参考索引](references/README.md)
- [外部项目参考](references/external/README.md)

外部参考记录 reviewed source 和可借鉴边界，不作为 CSSwitch 代码或外部 runtime 内部实现的权威来源。

## 维护约定

- 当前合同写在 architecture、operations 与 features；索引只负责路由。
- 当前版本、worktree 与已知问题放在 [`.agents/context/`](../.agents/context/)，不复制进架构正文。
- Agent 强制行为放在 [`.agents/rules/`](../.agents/rules/)，文档只链接。
- 外部 runtime 始终作为 adapter target；私有接口、内部数据库、package-static 观察、精确路径/hash/版本调查不得进入公开架构正文。
