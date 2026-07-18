# v0.7.0 多供应商模型目录：Gateway / adapter 审查报告

日期：2026-07-18

范围：静态 Provider selector resolver、Codex 动态 snapshot、`/v1/models`、Anthropic/OpenAI Chat/OpenAI Responses/DeepSeek/Qwen adapter 边界。

隔离：只使用单元测试和本地 loopback mock；没有真实供应商请求、真实凭据或正式 App 操作。

## 主审与修改

主审确认旧实现存在两类关键风险：静态 Provider 会把未知模型静默降级到 Qwen/DeepSeek 默认项；Science 看到的模型目录与实际请求解析不是同一份发布快照。现已改为：

- `StaticProfileResolver` 只接受当前 profile 保存的 selector 白名单；`/v1/models` 默认项排第一，但不改保存顺序。
- 解析顺序固定为 exact selector → 明确 legacy exact alias → 严格 Claude role/date alias → typed HTTP 400 `route_unknown`。
- Qwen `qwen-plus-latest`、DeepSeek `deepseek-v4-flash` 等未知模型兜底已删除；unknown selector 在任何 upstream POST 前拒绝。
- selector 只解析到 upstream model；协议转换、thinking、tool choice、token cap 继续由原 adapter 负责。
- 静态目录通过 canonical、受限、非敏感 JSON 注入 Gateway；不包含 key、base URL、prompt 或请求正文，启动前清除旧目录变量并验证 fingerprint。
- Codex 保持独立动态 resolver；`/v1/models` 与 inference 使用同一 generation snapshot，显式刷新原子替换，失败保留上一 generation，未知/消失 alias fail closed。

## xhigh 子 agent 复审

前四轮复审依次收紧了 Codex generation、ABA CAS、显式刷新和 unknown alias 的前置拒绝。第五轮终核确认：Codex unknown alias 返回 HTTP 400 typed `route_unknown`，且发生在 inference POST 前；本模块无残余 P0/P1，结论为 `PASS`。

## 验证

- Gateway Rust：231 passed；CLI integration：1 passed；doc tests 通过。
- Gateway strict clippy：`cargo clippy --all-targets -- -D warnings` 通过。
- 本地 loopback 定向：5 模型白名单 exact routing、Qwen 拒绝旧 Codex alias且上游增量为 0，2/2 通过。
- 子 agent 前轮 gateway contract suite：42/42；Codex catalog suite：14/14。

结论：Gateway/adapters 源码层准入。这里的 mock 结果不等于真实供应商全量在线验证。
