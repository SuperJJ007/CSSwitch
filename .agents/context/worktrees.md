# Worktree 快照

最后复核：2026-07-14。执行任何 Git 写操作前必须重新检查。

| 路径 | ref / base | 状态与用途 | 保护边界 |
|---|---|---|---|
| `/private/tmp/csswitch-skill-main-merge` | `codex/project-maintenance-docs`，基于 `557a01f` | 本轮 Agent / 文档维护区 | 只允许本轮规则和文档改动 |
| `/Users/superjj/ccproj/CSswitch` | `codex/rust-gateway-health-identity@512f494` | 15 项 porcelain，开发区 | 不 reset、clean、stage、覆盖或删除 |
| `/private/tmp/csswitch-v050-acceptance` | `codex/v050-acceptance@356c26a` | 6 项未提交修改，验收跟进 | 不 reset、clean、stage、覆盖或删除 |
| `/private/tmp/csswitch-skill-install-recovery` | detached `191e9f6` | 大型脏 recovery 实验区 | 未经用户明确确认绝不删除 |

状态数量只用于定位，不能代替逐路径清单。分支、remote ref 和 worktree 必须分别报告。
