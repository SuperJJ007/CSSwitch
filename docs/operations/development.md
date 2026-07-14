# 开发与维护

本文说明 v0.5.0 源码树的开发入口。安全、Git / worktree 和证据措辞分别以 [`.agents/rules/`](../../.agents/rules/) 为准。

## 环境

- macOS Apple Silicon（当前桌面发布目标）；
- Node.js / npm（Tauri 前端与构建）；
- Rust / Cargo（desktop backend 与 Rust gateway）；
- Python 3（测试驱动与 mock 使用，**不是** CSSwitch runtime proxy 依赖）；
- Claude Science App（只在隔离 runtime / 真机验收时需要）。

## 本地启动

```bash
cd desktop
npm install
npm run tauri dev
```

日常自动检查从仓库根目录运行：

```bash
bash test/run_all.sh
```

需要完整发布环境时才使用：

```bash
bash test/run_all.sh --require-release-ready
```

两种判定的准确含义见[测试文档](testing.md)。

## 组件级检查

```bash
(cd desktop/src-tauri && cargo fmt --check)
(cd desktop/src-tauri && cargo clippy --all-targets -- -D warnings)
(cd desktop/src-tauri && cargo test)

(cd desktop/gateway && cargo fmt --check)
(cd desktop/gateway && cargo clippy --all-targets -- -D warnings)
(cd desktop/gateway && cargo test)

python3 -m unittest discover -s test -p 'test_*.py' -v
node --check desktop/src/main.js
```

优先使用 `test/run-*.sh` 作为门禁，因为它们对缺失依赖、loopback 限制与层级状态有统一词汇；组件命令适合聚焦诊断。

## Science 相邻功能工作法

1. 在隔离环境确认上游 runtime 事实；
2. 明确 source of truth 与所有权；
3. 先验证不增加存储 / 状态机的最短路径；
4. 跑一条完整 E2E，并分别记录 copy、discover、attach、load、trigger、功能执行与重启；
5. 最后再决定是否需要 UI、catalog、cache 或新存储。

Science 已拥有的能力不应在 CSSwitch 再造一套 installer、目录所有权或生命周期。

## 隔离 runtime 开发

- 使用临时外层 `HOME`、临时持久 data-dir、动态端口和假 `security`；
- 不使用真实 `~/.claude-science`、端口 `8765` 或 `/Applications/CSSwitch.app`；
- installed-App candidate、缓存 candidate、实际 PID、版本 runtime 与 data-dir 分别取证；
- live provider、真实账号或真实 SSH server 测试需要额外授权。

详细步骤见[真机验收](real-machine-acceptance.md)。

## 文档维护

- 当前合同：`docs/architecture/`、`docs/features/`、`docs/operations/`；
- 当前快照：`.agents/context/`；
- 日期化结果：`docs/evidence/`；
- 公开行为：根 README；已发布变更：CHANGELOG；
- 临时下一步：`.agents/handoffs/`，任务结束后不保留为长期事实。

发布或重要 upstream runtime 变化后，应复核 architecture、功能限制、known issues 和 release evidence，而不是把新事实只留在聊天或 handoff。
