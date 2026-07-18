# v0.7.0 新 UI：Codex 审查

## 审查对象与参考

- 对象：`codex/v070-ui-redesign@bd1f3ab` 未提交工作树中的 Codex UI、认证状态、模型目录、Responses 转换和运行时诊断。
- 主审后由 `gpt-5.6-sol`、`xhigh` 子 agent 两轮只读复审。
- 成熟实现参照为 [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)；本地只读快照为 `v7.2.78@768b4c4`，审查时比其 `origin/main` 落后 6 个提交，因此只用于核对已存在的 Responses/Codex 路由模式，不作为“当前上游完全一致”的证明。

## 主审发现

1. 当前 Codex 没有独立 upstream 探测层，但新 UI 会把该空层纳入健康聚合，形成错误的琥珀色未就绪状态。
2. 激活 Codex 后的说明仍像静态 provider 模型列表，没有准确表达 OAuth 账号驱动的动态模型目录。
3. 新信息架构合并后仍残留“我的配置”“高级设置”“＋ 新建”等模糊或已不存在的旧路径文案，诊断指引不能准确对应当前入口。
4. 官方模式动作反馈可能被误看成正式健康验证。

## 修改

- Codex 的 upstream 层明确为“不适用”，只由适用的 Gateway/Proxy 等层参与聚合。
- 激活摘要改为 `CSSwitch OAuth · 账号动态模型目录`，不承诺静态全量模型。
- 所有相关诊断和认证指引改到当前真实路径：“设置 > Codex 账号与连接”“模型连接 > 配置方案”“新建配置”。
- 官方打开动作保持中性，不把打开成功等同认证、模型目录或推理健康。
- 浏览器认证结构化错误合同与既有 Responses、catalog、OAuth、网络层实现保持不变；本轮没有为了 UI 重写成熟协议栈。

## 子 agent 复审与终态

第一次复审要求重点对照 CLIProxyAPI 的职责边界和本仓库已有严格合同；修改后第二次复审未发现 P0–P3。复审确认命名工具选择、Responses reducer、OAuth、catalog 缓存和网络代理合同没有被新 UI 绕过，模糊/过期路径文案已清零并与当前信息架构一致。

## 复证

- Codex browser auth contract：8/8。
- 前端结构化认证错误：3/3。
- 全量 Rust 层中 Codex auth/model/protocol/transport/network 相关测试通过。
- `cargo check` 与本地 `cargo build --release` 通过；后者只生成本地 release 二进制，不代表 App bundle。
- 全量 `bash test/run_all.sh --require-release-ready`：五层均 `pass`。

## 边界

没有打开正式 App，没有发起真实 OAuth、读取真实账号凭据、调用真实 Codex 推理或修改真实 Science 数据。CLIProxyAPI 快照仅是实现参照；本报告不声称与其最新 `main` 字节级等价。
