# v0.7.0 多模型目录：生命周期 / Science 刷新审查报告

日期：2026-07-18

范围：`route_fp` / `catalog_fp` / `binding_fp`、profile 切换事务、崩溃恢复、Science 条件刷新、auto-boot 和结构化失败。

隔离：临时 HOME/data-dir、动态 loopback、fake Science；真实 Science 只使用单独 Stage 0 隔离测试。

## 主审与修改

- `route_fp` 包含 adapter、endpoint、凭据指纹、selector→upstream、roles 与 shim；变化时更新 Gateway。
- `catalog_fp` 包含有序 selector/display/capability、默认项、roles 和目录类型；`binding_fp` 包含端口/path-secret、sandbox 与 Science runtime identity。
- 只换 key/base URL/同 selector upstream 时 Science PID 保持；目录、默认项、role、profile 或 binding 变化时强身份停止并刷新 Science。
- profile 切换先在候选端口验证 Gateway，再原子发布候选 config + safe journal + previous gateway identity，之后才替换正式 Gateway。
- Science 启动失败会恢复旧 config/Gateway，并按已确认 identity 强制恢复旧 Science；只有完整恢复并探活才返回 `restored`，双重失败返回 `degraded`。
- one-click 与 auto-boot 都会消费中断 journal。orphan Gateway 必须同时匹配 path-secret、formal health、目标或 previous identity、当前 UID、唯一 listener PID 与 canonical packaged binary；SIGTERM 前再次验证。未知 listener 不结束。
- auto-boot 识别结构化 `status:error` 并进入 Failed，不再误记 Ready；前端始终显示 stage/recovery/message 并释放 spinner。

## xhigh 子 agent 复审

首轮与二轮提出的 crash recovery、rollback 及 auto-boot P0/P1 已逐项修改。第三轮终核结论：无剩余 P0/P1，`PASS WITH P2`。非阻塞建议是进一步显式断言 journal target 等于 active profile，并增加真实 orphan 进程故障注入。

## 验证

- Desktop/Tauri：344 passed、0 failed、4 个显式隔离项未计入自动通过。
- strict clippy：`cargo clippy --all-targets -- -D warnings` 通过。
- profile switch 3/3、proxy lifecycle 16/16、transaction 3/3、auto-boot structured failure 1/1。
- 隔离 fake-Science runtime Acceptance：1/1；覆盖重复 open 产生新 nonce、opener 首次失败返回同一次 fallback URL、再次打开获取更新 URL，以及同路径 runtime binary 原子替换。
- [真实 Science Stage 0](2026-07-18-v070-model-catalog-stage0.md)：6 个 alias 可见，旧目录在受控 restart 后消失，选择第 5 项后 mock 收到原样 selector；1 passed。

结论：生命周期源码层准入；真实 Science selector 证据来自 Stage 0，不能由 Gateway `/v1/models` 代替。
