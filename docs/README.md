# CSSwitch 文档总入口

当前维护基线为 v0.5.0。公开产品概览从根目录 [README 中文版](../README.md) / [English](../README.en.md) 进入；本目录按内容寿命和证据类型分类。

## 架构

- [架构总览](architecture/overview.md)：边界、所有权、数据流和失败边界。
- [Science runtime](architecture/science-runtime.md)：稳定的 binary、data-dir、身份和网络合同。

## 运维

- [开发](operations/development.md)
- [自动测试](operations/testing.md)
- [真机验收](operations/real-machine-acceptance.md)
- [发布](operations/release.md)
- [升级与回滚](operations/upgrade-and-rollback.md)

## 功能合同

- [外部 Skill bridge](features/external-skill-bridge.md)
- [系统 SSH 配置复用](features/system-ssh.md)

## 证据

- [发布证据](evidence/releases/README.md)：按版本记录最终 artifact 与分发结果。
- [日期化调查](evidence/investigations/README.md)：只证明特定日期、runtime 和环境。

## 外部参考

- [外部项目参考](references/external/README.md)：记录 reviewed commit 与可借鉴边界，不作为代码来源。

## 维护约定

- 当前合同写在 architecture / operations / features；日期化结果不能覆盖稳定合同。
- 当前版本、worktree 与已知问题放在 [`.agents/context/`](../.agents/context/)，不复制到每份文档。
- Agent 行为规则放在 [`.agents/rules/`](../.agents/rules/)，文档只链接，不重复禁止项。
- 旧公开路径在一次发布周期内保留兼容指针；指针不是第二份正文。
