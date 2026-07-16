# 2026-07-16 Codex → Science 模型 ID 兼容实验

状态：**installed-runtime 隔离兼容证据；不等于 live Codex 账号验收**。稳定产品合同见 [Codex → Claude Science 实验桥接](../../features/codex-science-bridge.md)。

## 问题

Codex 官方目录返回 `gpt-*` raw id，但 Claude Science 的模型选择器是否会原样展示和提交该 id，不能仅凭 Anthropic `/v1/models` 的协议形状推断。本实验只回答 raw id 与确定性 `claude-` shell alias 的 installed Science 兼容性。

## 隔离边界

- runtime：`/Applications/Claude Science.app/Contents/Resources/bin/claude-science`；
- 版本：`0.1.18-dev.20260709.t211149.shab3f5130`；
- 使用随机临时 HOME、独立 data-dir、动态 Science / preview / mock 端口，明确不使用 8765；
- 只用现有 OAuth forge 在临时 HOME 创建 `virtual@localhost.invalid` 假账号；
- Anthropic base URL 指向 loopback 假上游，token 无真实权限；
- 未读取或修改真实 `~/.claude-science`、`~/.codex`、系统 Codex OAuth 或真实 CSSwitch Keychain 项；
- 完成后先用相同 binary + data-dir 停止 daemon，再关闭 mock、浏览器页并删除两个临时根；临时手工 probe test 随后从工作树移除。

## 输入与观测

loopback `/v1/models` 在一个响应中返回两个逻辑上对应同一模型的条目：

| 类型 | id | display name |
|---|---|---|
| raw | `gpt-5.3-codex` | `GPT-5.3 Codex` |
| shell alias | `claude-csswitch-codex-gpt-5.3-codex` | `GPT-5.3 Codex (CSSwitch)` |

installed Science 的可观测结果：

1. daemon 对假上游只发出 `GET /v1/models?limit=1000`；
2. 模型菜单的 “More models” 只出现 alias 的 display name；
3. raw `gpt-5.3-codex` 在可见 DOM 中计数为 0；
4. alias 菜单项可选，选择后按钮变为 `Model: GPT-5.3 Codex (CSSwitch)`。

本机 binary 的只读静态取证与上述行为一致：其模型列表归一化会删除非 `claude-` id。installed-runtime 行为已足以决定不能直接把 Codex raw id 暴露给 Science。

## 实现结论与自动测试

Gateway 采用双 ID：官方响应与 `codex-models-cache.v2.json` 保留 raw id 及模型级 reasoning / tool capability；Science-facing `/v1/models` 暴露 `claude-csswitch-codex-<raw-id>` 并添加 `Codex / ` 显示前缀；`/v1/messages` 在账号目录校验成功后映射回 raw id，并按所选模型能力构造上游参数。raw id、未知 alias 和隐藏模型不能触发推理 POST。共享 HOME 的 formal/scratch 缓存另有持久化 cache epoch 与文件锁；双 catalog 竞态测试按“失效 → 新 live 恢复 → 旧请求最后完成”的 ABA 顺序证明旧在途响应无法复活缓存，重启也不会复用失效目录。

本次聚焦结果：

| 范围 | 结果 |
|---|---|
| Gateway `cargo test codex_ --lib` | 91 passed |
| Gateway `cargo test --all-targets` | 193 unit passed + 1 CLI integration passed |
| Tauri `cargo test model_discovery --lib` | 6 passed |
| Tauri `cargo test scratch --lib` | 14 passed |

这些结果证明 mock 目录、缓存、alias 可逆映射和 unknown-model 前置拒绝，不证明当前用户真实 Codex 账号有两个可用模型，也不证明 live 推理。后两项只能在用户亲自 OAuth 后按 RM-36 / RM-37 验收。
