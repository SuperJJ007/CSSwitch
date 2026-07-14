# 自动测试与证据判定

## 总入口

```bash
bash test/run_all.sh
```

脚本按顺序汇总五层：

| 层 | 入口 | 覆盖范围 |
|---|---|---|
| `offline` | `test/run-offline.sh` | Python 纯单元：capability、catalog、process ownership；不使用网络 |
| `loopback` | `test/run-loopback.sh` | 从当前源码构建 Rust gateway，运行 127.0.0.1 mock / provider / installed matrix 合同 |
| `scripts` | `test/run-scripts.sh` | shell、doctor 与 verify-proxy 运维合同 |
| `rust` | `test/run-rust.sh` | desktop backend 与 gateway 的 fmt、clippy、tests |
| `frontend` | `test/run-frontend.sh` | `desktop/src/main.js` 的 Node 语法检查 |

每层必须输出一个 `S0_LAYER <layer> <status>`。缺标记行按失败处理；loopback 层的有界重试不会把最终失败吞掉。

## 两种总判定

### `current-env clean`

本环境五层中没有 `fail`，脚本退出 0。允许存在 `env-blocked`、`skipped` 或 `needs-real-machine`，因此不能写成完整发布门禁通过。

### `release-ready green`

五层全部为 `pass`，不存在环境阻塞。发布机器使用：

```bash
bash test/run_all.sh --require-release-ready
```

有任何非 `pass` 层时，该模式退出 2；有真实失败时退出 1。报告必须同时写命令、退出码、五层状态与运行 commit。

## 自动化没有证明的层

五层全绿仍不自动证明：

- `.app` / DMG 从目标 commit 构建且内容正确；
- 临时安装副本或 installed runtime 可用；
- 当前 Claude Science 版本兼容；
- 外部 Skill 的自然语言路由、领域功能或重启持久化；
- 特定真实 provider / SSH server 可用；
- Developer ID 签名、notarization、Gatekeeper 或公开 release 附件一致。

这些层分别使用[真机验收](real-machine-acceptance.md)、[发布流程](release.md)和 dated evidence。

## 报告词汇

- `通过`：该层已执行且满足判据；
- `失败`：已执行但不满足；
- `ENV-BLOCKED`：当前环境缺能力，不能视为失败或通过；
- `NEEDS-REAL-MACHINE`：必须在指定真机 / artifact 上执行；
- `未执行`：没有取得该层证据；
- `需人工判断`：机器结果不足以自动确定。

mock / loopback、built artifact、installed copy、runtime、live provider 与发布附件必须分栏记录。
