# v0.7.0 新 UI：Skill / MCP 审查

## 审查对象与参考

- 对象：真实 Skill 列表、搜索筛选、组织绑定、bundle 导入、外部 Skill route 与 MCP 边界。
- 主审后由 `gpt-5.6-sol`、`xhigh` 子 agent 两轮只读复审。
- 参考项目选择长期维护、职责明确的上游：[Anthropic Skills](https://github.com/anthropics/skills)、[obra/superpowers](https://github.com/obra/superpowers)、[MCP Registry](https://github.com/modelcontextprotocol/registry) 与 [Claude Code plugins](https://github.com/anthropics/claude-code/blob/main/plugins/README.md)。[MCP servers](https://github.com/modelcontextprotocol/servers) 明确只作参考实现集合，不当作生产安全背书。

## 主审发现

1. 搜索输入每键都会整页重渲染，输入框失焦；详情抽屉没有完整模态焦点管理。
2. Skill 快照长期驻留，runtime、组织或模式变化后可能继续展示旧数据。
3. 导入 readiness 只看部分前端状态，没有同时绑定健康 runtime、ready org 与可信回读。
4. 列表请求和导入之间存在 active org 改变窗口；外部 route 也没有把 expected org 贯穿检查。
5. 展示长度的 JavaScript 计数与 Rust Unicode scalar 合同不一致。
6. Science 响应先完整缓冲再检查大小，超限响应仍可能消耗不必要内存。
7. listing 对本地 marker 的校验不够完整；大量列表没有渐进展示。

## 修改

- 搜索只重绘结果区域，保持焦点与输入值；详情抽屉加入 `inert`、dialog 语义、初始焦点、Escape、Tab 循环及关闭后焦点恢复。
- Skill 页面改为按需加载；runtime 启停、设置、模式和绑定变化均使快照失效，刷新先清旧数据，失败时不继续伪装为实时结果。
- 新增纯函数 readiness：只有健康 runtime、ready org 和可信回读同时成立才允许提交导入。
- listing 在读取前后复核 active org，变化即丢弃结果；external route 显式携带并检查 expected org。
- 前端按 Unicode scalar 计数；列表默认分批显示 100 条，可继续加载。
- Science 响应改为流式有界读取；marker 增加 tuple 交叉校验；system route 的 current/legacy `SKILL.md` 与 `.import-origin` 也统一使用 `O_NOFOLLOW|O_NONBLOCK`、打开后 fstat 和 `limit + 1` 上限。
- 生产页继续只暴露已实现的 MCP/Skill 能力，没有加入虚假的“刷新 MCP”“启停服务”等按钮。
- `invalidate()` 使用请求代次废弃在途旧响应，并允许立即发起 replacement refresh；active org 改变时清空旧 items 和旧 warnings。
- listing 测试目录改为原子计数加排他 `create_dir`，消除默认并发下的 fixture 碰撞。

## 子 agent 复审与终态

第一次复审列出 race、readiness、Unicode、响应上限、焦点和旧快照等问题。交叉复审随后又发现 system route 二次鉴别仍有无界读取、旧请求可穿透失效、旧 warning 泄漏与并行 fixture 碰撞；四项均修正并补测试。最终复审确认 P0–P2 清零。保留一项 P3 理论边界：从父目录解析到最终目标之间仍不是完整 `openat` 逐段绑定，但现有最终对象 `O_NOFOLLOW`、私有目录、类型与权限检查已覆盖实际威胁模型；继续扩展会显著增加复杂度，暂不为此过度工程化。

## 复证

- Skill package：55/55，通过；另有 4 项显式忽略的外部/隔离测试。
- Skill listing command：4/4。
- system route：10/10；core listing 默认并发：9/9。
- 前端 Skill state：9/9，其中包含在途旧响应竞态回归。
- Python runtime boundary：9/9。
- 浏览器实测：连续搜索焦点保持在输入框；抽屉打开后聚焦关闭按钮，Escape 关闭并恢复到原详情按钮；920×600 与 760×520 无横向滚动。
- 全量 `bash test/run_all.sh --require-release-ready`：五层均 `pass`。

## 边界

没有访问真实 GitHub 私有仓库、真实 MCP 服务或真实 Science 数据，也没有把参考服务器列表视为可信安装源。第三方 bundle 仍需来源、签名/散列与权限层面的独立判断。
