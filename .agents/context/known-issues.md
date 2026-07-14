# 当前已知问题与证据缺口

最后整理：2026-07-14。已解决历史放入 CHANGELOG 或 dated evidence，不在这里堆叠。

## 分发

- v0.5.0 最终 DMG 只有 ad-hoc 签名，没有 Developer ID 团队身份、notarization 或 stapled ticket；Gatekeeper 主签名评估拒绝。首次打开可能需要用户右键选择“打开”。

## Science / Skill

- 外部 Skill bridge 的最后一层已安装 CSSwitch App 人工 UI 确认尚未建立；2026-07-13 E2E 使用隔离环境、local mock provider 与公开 GitHub 内容。
- 安装、attach、load 与重启持久化不证明任一 Skill 的脚本、资产、网络、依赖或领域功能可用。
- 仅给名称时的来源搜索由 provider / Agent 能力决定；私有仓库、更新 / 覆盖、永久删除和恢复 UI 不受支持。
- route attachment 与 `host.agents.attach_skill` 等是观察到的 Science 合同；Science App 更新后必须重跑聚焦兼容性验证。
- Agent 控制面配置是多个顺序请求，不是原子事务；失败只降级为 warning，但已经完成的 route / connector / `customize` / prompt 步骤不会自动回滚。

## SSH

- wrapper 和配置语义已由源码 / 测试覆盖；默认关闭不影响启动，但用户 opt-in 后 config / wrapper 校验 fail closed。未对特定用户的真实 SSH server 做连通性验证。

## 测试

- 真机验收矩阵不是 v0.5.0 已全部执行的声明。每次验收应按 artifact 和环境另存证据，不把“需真机”记为通过。
