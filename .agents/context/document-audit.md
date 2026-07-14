# v0.5.0 文档逐份审查账本

审查基线：`v0.5.0 / main@557a01f`；最后整理：2026-07-14。

事实优先级：目标 artifact / runtime > tag 与 release > `main@557a01f` 源码和测试 > context > docs > memory / handoff。每行分别记录具体依据、已确认事实、推断 / 未验证事项、冲突 / 断链和最终处置；“未验证”不得由旧 memory 补全。

| 基线文件 | 具体 v0.5.0 依据 | 已确认事实 | 推断 / 未验证 | 重复、冲突、断链 | 最终处置 |
|---|---|---|---|---|---|
| `README.md` | GitHub `v0.5.0` tag / release；最终 DMG digest；`desktop/src-tauri/tauri.conf.json`；runtime / Skill / SSH 源码 | 公开版本、Apple Silicon 包、Rust sidecar、外部 Skill / SSH 用户边界、ad-hoc / 未公证措辞成立 | 最终 DMG installed-runtime、live provider、真实 SSH server、任一 Skill 领域功能未由 README 自身证明 | 旧 architecture / Skill / upgrade 链接已换为新路径；链接检查通过 | 根目录保留中文公开入口；增加 docs 总入口并校准限制 |
| `README.en.md` | 与中文版相同的 release、artifact 与源码依据 | 英文用户行为与中文版同义 | 同上；双语一致不等于各证据层已执行 | 旧链接已换；与中文版功能边界复核一致 | 根目录保留英文公开入口，与中文版同步维护 |
| `CHANGELOG.md` | peeled tag `557a01f`；GitHub release；该 commit 源码 diff | v0.5.0 条目描述已发布变化；历史版本文字属于历史记录 | CHANGELOG 不证明 artifact、installed runtime 或 live provider | 仅更新迁移后的 upgrade 链接；历史 Python proxy 文字有版本语境，不是当前 runtime 声明 | 保留；历史正文原则上不重写，只修客观错误 / 链接 |
| `docs/ARCHITECTURE.md` | `runtime/science.rs`、`sandbox_session.rs`、Gateway、Skill / SSH 源码；根 README | 产品边界、所有权与主数据流可从源码支持 | 未来 Science 接口、任意 provider / Skill 兼容未验证 | 原文混入日期化事故；已拆出，旧路径现在单跳指针 | 稳定合同迁至 `docs/architecture/overview.md`；旧路径保留一发布周期 |
| `docs/SCIENCE_RUNTIME.md` | `runtime/science.rs` runtime selection / strong probes；`commands/runtime.rs` health-only status；2026-07-13 隔离证据 | App 优先、explicit fail-closed、one-shot cache、data-dir 分离、UI status 仅 health | 通用最低 Science 版本、未来 control API 未验证 | 旧文把稳定合同与 0.1.15 / 0.1.18 事故混合，并过度概括 status identity；已修正 | 稳定合同迁至 `docs/architecture/science-runtime.md`，事故迁入 dated investigation；旧路径留指针 |
| `docs/DEVELOPMENT.md` | `desktop/package.json`、Cargo manifests、`test/run-*.sh` | 开发 / 测试命令和 Python“仅测试、非 runtime proxy”边界成立 | 特定开发机依赖可用性未验证 | 安全、Git、证据规则曾与流程重复 | 流程迁至 `docs/operations/development.md`；规范只链接 `.agents/rules/` |
| `docs/RELEASE.md` | `test/run_all.sh` 五层语义；Tauri resources / externalBin；v0.5.0 artifact 检查 | source -> artifact -> runtime -> distribution -> published release 分层成立 | 未取得层必须逐版本写未建立；runbook 不是发布完成证据 | 原文只有 gates；SSH / status 语义已按源码校准 | 完整流程迁至 `docs/operations/release.md`；Agent 禁止项留 release rule |
| `docs/upgrade-and-rollback.md` | v0.5 config schema；`runtime/science.rs` data-dir；release evidence | v0.5.0 复用持久 data-dir、Rust sidecar 与保留 legacy 数据 | 任一旧版对未来配置的实际回读能力、用户本地回滚结果未验证 | 旧分类路径；签名边界需指向具体 evidence | 更新并迁至 `docs/operations/upgrade-and-rollback.md`；旧路径留双语指针 |
| `docs/EXTERNAL_SKILL_INSTALL.md` | Gateway `skill_install.rs` / `science_control.rs`；desktop `skill_install_bridge.rs` / `sandbox_session.rs`；bundled route；聚焦 tests / E2E | 工具名、copy / quarantine、native attach / detach、Agent route / connector / prompt 管理、running check 与 route-state 合同成立 | 最终 DMG UI、未来 Science API、Skill 领域功能、私有仓库 / update / restore 未验证 | 旧文混合稳定合同与 dated E2E；迁移初稿漏掉 Agent 配置与 restart 行为，已补 | 合同迁至 `docs/features/external-skill-bridge.md`；版本证据迁入 investigation；旧路径留指针 |
| `docs/references/CSNATIVE.md` | reviewed external snapshot `eust-w/CSNative@64a68b1` | 只支持当时责任边界比较；不是 CSSwitch 代码来源 | 当前 CSNative HEAD / 后续行为未重查 | 原路径分类不足；无代码引用关系 | 迁至 `docs/references/external/csnative.md` 并保留 reviewed commit / 非代码来源声明 |
| `docs/release-evidence-v0.4.2.md` | 文件内 2026-07-12 source fingerprints、logs、artifact / signing 记录 | 只确认 pre-release 候选在记录环境中的层级结果 | 最终 public release 状态未建立；迁移后不能在当前树直接复现原 fingerprint | 容易被误认当前证据；原命令绑定旧路径，迁移后已加不可直接复现说明 | 原文归档至 `docs/evidence/releases/v0.4.2.md` 并标 historical / pre-release；旧路径留指针 |
| `test/REAL_MACHINE_TEST.md` | `real_machine_guard.sh`、Acceptance Tauri config、v0.5 runtime / Skill / SSH 源码、release packaging | 隔离 guard、原 RM-01～RM-18 语义、Rust sidecar / Science / Skill / SSH 新矩阵可由当前代码定位 | 矩阵不代表已全部真机执行；真实 provider / SSH 另行授权 | 旧 `Resources/proxy` 与 Python runtime 要求过期；迁移初稿重编号冲突，已恢复旧编号并追加 RM-19+ | 重写迁至 `docs/operations/real-machine-acceptance.md`；旧路径留指针 |
| `test/RM_RETEST_STEPS.md` | `git show 557a01f:test/RM_RETEST_STEPS.md`；当前 guard / matrix | 仅是旧 P1 / P2 patch handoff；安全 guard 与证据词汇仍有效 | 固定 pass 数和“下一步”不具长期事实价值 | Python proxy 依赖过期；`test/run-live.sh` 的旧引用已改到新 runbook | 有效内容已进入 rules / acceptance；删除，Git 历史即归档 |
| `desktop/src-tauri/resources/skills/csswitch-external-skill-tools/SKILL.md` | 原位文件；Gateway tool schemas；route / E2E tests | `install_external_skill` / `uninstall_external_skill`、禁止 `customize` / `host.skills.*` / shell、native attach / detach 一致 | 未来 Science 自然语言路由稳定性需重测 | 它是 packaged runtime 资产，不是维护文档；不能迁移或翻译破坏运行合同 | 原位保留，只做一致性核对 |

## 新增文件职责

| 文件 | 依据与职责 | 明确未证明 |
|---|---|---|
| `docs/operations/testing.md` | 直接解释 `test/run_all.sh` 的五层与两个总判定 | 不证明 artifact / installed runtime / live provider / distribution |
| `docs/features/system-ssh.md` | 对照 settings、launch script 与 shell tests，说明 default-off、opt-in fail-closed 和 OpenSSH 间接访问 | 不证明特定真实 server 连通 |
| `docs/evidence/investigations/2026-07-13-science-runtime-and-skill-bridge.md` | 承接 0.1.15 / 0.1.18 事故、隔离 Skill E2E 与 Agent 控制面观察 | 不外推到未来 Science、最终 DMG 或 Skill 领域功能 |
| `docs/evidence/releases/v0.5.0.md` | 记录 tag / release / asset digest、同 hash DMG、包内资源、codesign / spctl / stapler 与未执行层 | 未建立公开 URL 独立下载、最终 DMG installed runtime、live provider、Developer ID / notarization |

## 全局检查

- 根 README、CHANGELOG、分类索引和兼容指针均已改用新路径；当前正文不依赖被删除的 `RM_RETEST_STEPS.md`。
- `test/run-live.sh` 已指向 `docs/operations/real-machine-acceptance.md`。
- 独立审查修正后，52 份可追踪 Markdown 的相对链接、删除路径残留、尾随空白、ignore 与 `git diff --check` 已重跑通过；结果已刷新到 `verified-state.md`。
- 所有兼容指针计划保留到 v0.5.0 之后的下一个正式发布；届时另行审查，不在本轮预删。
