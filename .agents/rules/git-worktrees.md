# Git 与 worktree 规则

- 所有未提交修改和未跟踪文件均按用户数据保护。
- Git 写操作前检查 `git status --short --branch` 与 `git worktree list --porcelain`；[worktree 快照](../context/worktrees.md)只用于定位，不能代替实时检查。
- 未经用户对该动作的明确授权，不执行 `git reset --hard`、`git clean`、删除 worktree 等破坏性清理。
- detached 或分支已过时都不是删除脏 worktree 的理由。
- 维护改动使用目标基线的干净 worktree；混合工作区只能按明确路径 staging，不能宽泛 `git add`。
- commit、push、force-push、创建 tag、删除分支、创建 PR 和发布 release 是彼此独立的授权。
- 报告时分别说明本地分支、远端引用和 worktree，不把它们合并成一个“分支已清理”结论。
