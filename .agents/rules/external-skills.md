# 外部 Skill 规则

- v0.5 bridge 是窄范围的外部 Skill 安装 / 隔离卸载路径，不是 Skill Manager、catalog、inventory 或部署平台。
- 安装必须得到准确的公开 GitHub 目录 URL；仅名称或来源含糊时不得静默猜测并安装。
- 复制文件不等于可用：还必须经 Science 原生 `attach_skill` 并成功 `skill()` 加载。
- 卸载只能隔离带有效 CSSwitch ownership marker 的目录，再经 Science 原生 `detach_skill` 并确认不可加载。
- 不覆盖同名 Skill，不修改 bundled / 用户 Skill，不模拟 OAuth / catalog，不 patch Science binary，不写 Science 数据库。
- bridge 请求必须限界、认证、短期有效、防路径逃逸，并通过 Science host-access 获得用户批准。
- 启动后的 Agent 控制面会按固定策略管理 route Skill、CSSwitch connector、旧 connector、`customize` 绑定和一段受管 custom prompt；该顺序不是原子事务，失败只产生 warning，但已完成步骤不会自动回滚，不能报告成“完全未改”。
- Science 已运行时只能只读检查 MCP / route 文件；发现漂移应返回 restart required，不能并发改写。成功控制面配置按 CSSwitch 版本、Science 版本和 route revision 记录状态，匹配时重复启动跳过重做。
- connector / route 注册、Agent 控制面配置和 Skill 操作失败不得阻断普通 Science 启动。
- 私有仓库、更新 / 覆盖、永久删除、恢复 UI 和 Skill 的领域功能均不在该合同内。
- 工具名、结果词汇和 attach / detach 流程以[功能合同](../../docs/features/external-skill-bridge.md)及原位 runtime `SKILL.md` 为准。
