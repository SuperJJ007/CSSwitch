# Worktree 使用边界

机器上的 worktree 路径、分支、脏状态和数量会持续变化，不适合作为公开主干的固定快照。本文件不记录个人 HOME、临时目录或未提交文件清单。

任何 Git 写操作前都必须实时执行：

```bash
git status --short --branch
git worktree list --porcelain
```

报告时分别说明目标 worktree、分支 / detached 状态和未提交修改；不得用本文件、历史 handoff 或其他机器的路径代替实时结果。保护和授权要求见 [Git / worktree 规则](../rules/git-worktrees.md)。
