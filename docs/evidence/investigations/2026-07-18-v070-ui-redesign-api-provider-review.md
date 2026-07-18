# v0.7.0 新 UI：API / Provider 审查

## 审查对象与方法

- 对象：`codex/v070-ui-redesign@bd1f3ab` 上的未提交工作树；审查日期 2026-07-18。
- 主审按 provider 配置、adapter 路由、模型目录、状态诊断与现有合同测试逐层核对；随后由 `gpt-5.6-sol`、`xhigh` 子 agent 两轮只读复审。
- 成熟实现只作为合同参照，没有复制实现。主要参照 [CC Switch Provider 编辑合同](https://github.com/farion1231/cc-switch/blob/main/docs/user-manual/en/2-providers/2.3-edit.md)、[Codex DeepSeek 路由指南](https://github.com/farion1231/cc-switch/blob/main/docs/guides/codex-deepseek-routing-guide-en.md)及其[发布记录](https://github.com/farion1231/cc-switch/releases)。

## 主审发现

1. 状态聚合把“无独立 upstream”与“upstream 探测失败”混在同一琥珀色未就绪语义中；Codex、空 adapter 和 relay/provider 的适用性没有被明确区分。
2. 官方模式点击打开后可被 UI 暂时表现为绿色，这只是动作已发起，不是健康证明。
3. 未知或查询失败的状态也回退为琥珀色，和“确实未就绪”混在一起，无法区分“没有证据”与“已有未就绪证据”。
4. 新 UI 的 provider 入口和当前 Rust adapter 合同总体一致，没有发现偷偷扩大 provider 支持矩阵、重写 key 或更换 endpoint 的行为。

## 修改

- 在后端状态 DTO 中加入 `upstream_applicable`，由 adapter 语义决定：空 adapter 与 Codex 不适用；relay、DeepSeek 等适用 provider 即使 endpoint 解析或网络失败仍保持“适用”，失败显示为琥珀而非被吞成“不适用”。
- 新增前端纯状态模块 `runtime-status-state.js`：灰色表示不适用/未知，琥珀表示未运行或部分就绪，红色只保留给明确失败；聚合时忽略不适用层。
- 官方模式改为中性“由 Science 管理”，打开动作只显示“已发起”，不伪造绿色健康状态。
- 用前后端单测冻结适用性、聚合和未知状态合同。

## 子 agent 复审与终态

第一次复审重点检查 adapter 到状态语义的映射；修改后第二次复审未发现 P0–P3 遗留项。复审确认 relay/DeepSeek 不会因 endpoint 失败被误归为“不适用”，Codex 也不会因不存在独立 upstream 被错误纳入琥珀色未就绪聚合。

## 复证

- Tauri provider tests：17/17。
- Gateway provider tests：2/2。
- runtime command tests：4/4，另有 2 项显式隔离 smoke 被忽略，未冒充通过。
- diagnostics tests：8/8。
- `bash test/run-frontend.sh`：通过，其中状态模块 3/3。
- 全量 `bash test/run_all.sh --require-release-ready`：五层均 `pass`。

## 边界

本轮没有连接真实 provider、读取真实 key、切换正式 App 配置或修改正式 Science 数据。结论证明源码合同与隔离测试，不证明任一第三方 endpoint 当日在线，也不替代正式 provider 真机验收。
