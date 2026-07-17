# 当前已知问题与证据缺口

最后整理：2026-07-17。已解决历史放入 CHANGELOG 或 dated evidence，不在这里堆叠。

## 分发

- v0.7.0 公开附件只有 ad-hoc seal，没有 Developer ID 团队身份、notarization 或 stapled ticket；Gatekeeper 对包内 app 的评估为 `rejected`。首次打开可能需要用户右键选择“打开”。

## Codex

- Codex 是默认关闭的实验能力。上游账号权限、动态模型目录和 Responses 协议可能变化；单账号、浏览器登录、macOS Apple Silicon 是当前边界。
- 不支持设备码、多账号、代理认证、PAC、自定义 CA、系统代理自动发现或 TUN 检测；Finder 启动的环境变量与终端可能不同，`direct` 也可能仍由系统 TUN 接管。
- Acceptance 候选已有真实 CSSwitch OAuth、模型和 Science 最小文本成功证据，但最终公开 v0.7.0 DMG 没有重新执行真实 OAuth / 模型 / 推理；两者不能合并为同一层证据。
- v3 配置回滚到 v0.6.0 前必须先在 v0.7.0 导出并降级到 v2，或停止全部 CSSwitch 进程后恢复 v2 备份；删除 Codex profile 本身不会降低 schema。

## Science / Skill

- 发布者报告 v0.6.0 大部分真机验收成功，但未留下逐项结构化日志，不能外推为完整矩阵全部通过。
- 安装、attach、load 与重启持久化不证明任一 Skill 的脚本、资产、网络、依赖或领域功能可用。
- 仅给名称时的来源搜索由 provider / Agent 能力决定；私有仓库、更新 / 覆盖、永久删除、恢复 UI 和 bundle 成员级物理删除不受支持。
- route attachment、nonce / CSRF control plane 与 `OPERON` Skill 绑定是观察到的 Science 合同；Science App 更新后必须重跑聚焦兼容性验证。
- Agent 控制面配置是多个顺序请求，不是原子事务；失败只降级为 warning，但已经完成的 route / connector / `customize` / prompt 步骤不会自动回滚。

## SSH

- wrapper 和配置语义已由源码 / 测试覆盖；默认关闭不影响启动，但用户 opt-in 后 config / wrapper 校验 fail closed。未对特定用户的真实 SSH server 做连通性验证。

## 测试

- 真机验收矩阵不是 v0.6.0 已全部执行的声明。每次验收应按 artifact 和环境另存证据，不把“需真机”或“大部分成功”记为逐项全部通过。
