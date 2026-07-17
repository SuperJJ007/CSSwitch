# 2026-07-16 Codex → Science 实施基线

状态：**阶段 0 源码 / 自动测试基线**。它不证明 Codex OAuth、真实 provider、installed runtime、签名、公证或公开 release；稳定设计合同见 [Codex → Claude Science 实验桥接](../../features/codex-science-bridge.md)。

## 目标与环境

- source worktree：隔离的干净 checkout；
- commit：`0897e78f201e9e463be6a13e3d11888bde31f3b0`；
- branch：本地 `main`，当时与 `origin/main` 无已报告差异；
- 平台：macOS arm64，Asia/Shanghai；
- 日期：2026-07-16；
- 未读取真实 API key、OAuth token、Keychain、`~/.codex` 或真实 Science 数据；
- 未启动真实 provider 或已安装 CSSwitch App。

## 命令与结果

```bash
cd <clean-worktree>
bash test/run_all.sh
```

命令退出码为 0，聚合器报告：

| 层 | 结果 | 本次可见计数 / 说明 |
|---|---|---|
| offline | pass | 14 tests passed |
| loopback | pass | 82 tests passed |
| scripts | pass | bash / doctor / verify-proxy tests 全部通过 |
| rust | pass | Desktop 232 passed、3 explicit ignored；Gateway 102 passed |
| frontend | pass | `node --check desktop/src/main.js` |

聚合终态：`current-env clean: YES`、`release-ready green: YES`。

测试后执行：

```bash
git status --short --branch
git rev-parse HEAD
```

结果仍为干净 `main`，HEAD 仍是上述完整 SHA。随后从该 SHA 创建独立 worktree `/private/tmp/csswitch-codex-science-bridge` 和分支 `codex/codex-science-bridge`；没有在用户现有的其他 UI、Skill 或重构 worktree 上实施 Codex 改动。

## 结论边界

该证据只证明 Codex 分支继承的五层自动化基线为绿色，并证明实施 worktree 与已有工作区隔离。Codex 功能在此证据时点尚未实现，后续每阶段必须重新运行对应聚焦测试，最终阶段再运行完整五层总门。
