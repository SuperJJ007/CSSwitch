# v0.7.0 多模型目录收紧范围总报告

日期：2026-07-18

实施范围仅包括 Provider 多模型目录、严格映射、Science 条件刷新、对应 UI 和对应覆盖安装 Acceptance；没有继续旧的 Skill/MCP 全量目标。

## 模块结论

| 模块 | 主审/修改 | xhigh 复审 | 当前结论 |
| --- | --- | --- | --- |
| schema / preset | schema v4、原子 v3 migration、稳定 selector、版本化 provider preset | 第六轮无 P0/P1 | PASS |
| Gateway / adapters | 静态白名单 resolver、strict 400、Codex generation snapshot | 第五轮无 P0/P1 | PASS |
| 生命周期 / Science | 三指纹、候选验证、完整恢复、journal crash recovery、条件 restart | 第三轮无 P0/P1 | PASS WITH P2 |
| UI | 多模型白名单、default/roles、tools 警告、Codex 只读、browser fallback | 第二轮无 P0/P1/P2 | PASS |
| 覆盖安装 runtime | 专用 identity、同 HOME/同路径 v3→v4、exact/role/unknown、old→new Science PID | harness 已补齐 | 已覆盖安装并成功启动 |

## 综合证据

- Desktop/Tauri：344 passed、4 ignored；Gateway：231 + 1 passed；两个 crate strict clippy 通过。
- frontend、Python boundary、installed controller、Gateway exact/unknown 定向测试全部通过。
- 真实 Science Stage 0 在隔离环境证明 6 项 selector 可见、restart 后旧目录消失且选择原样送到 mock。
- 专用最终 Acceptance bundle 已在最后 UI 文案修正后重建；bundle id 为 `com.csswitch.acceptance.modelcatalog`。

## 当前交付状态

覆盖安装 harness 已完成，当前版本已覆盖到 `/Applications/CSSwitch Acceptance.app`，且安装后哈希与构建产物一致。清除覆盖复制产生的执行来源元数据后，App 已成功启动并保持运行。旧版本保存在 `/private/tmp/CSSwitch Acceptance.before-model-catalog-20260718.app`；完整 coverage runner 的 runtime 断言仍未执行。

## gpt-5.6-sol xhigh 总终审

最终结论为 `P0=0 / P1=0 / P2=1`，限定范围内源码准入。终审独立抽查了静态白名单 resolver、strict alias 顺序、Science 三指纹、UI model state、覆盖脚本的同 HOME/同路径迁移与 exact selector 断言，并确认真实 Science Stage 0 与覆盖安装 runtime 没有混写。

一个非阻塞 P2：

1. crash journal 恢复可进一步显式断言 `journal.target_profile_id == active_id`，并在隔离环境补真实 orphan Gateway 进程故障注入。
最终准入边界：schema/preset、Gateway、Science 生命周期、UI 与 Acceptance harness 源码通过；Acceptance 已覆盖安装并成功启动，完整 runtime 断言尚未执行。

安全结论：本轮没有替换、结束或修改正式 CSSwitch App；没有读取或写入真实凭据、真实 Science data-dir 或 Science 数据。
