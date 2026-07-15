# 当前已知问题与证据缺口

最后整理：2026-07-16。已解决历史放入 CHANGELOG 或 dated evidence，不在这里堆叠。

## 分发

- v0.6.0 公开附件只有 ad-hoc 签名，没有 Developer ID 团队身份、notarization 或 stapled ticket；Gatekeeper 对包内 app 的评估为 `rejected`。首次打开可能需要用户右键选择“打开”。

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
