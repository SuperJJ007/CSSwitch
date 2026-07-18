# v0.7.0 多模型目录：UI / 覆盖安装 Acceptance 审查报告

日期：2026-07-18

范围：Provider 多模型编辑器、Codex 动态目录、browser fallback，以及专用同路径覆盖安装 harness。

安全边界：专用 `com.csswitch.acceptance.modelcatalog`、临时 HOME、`.csswitch-acceptance`、动态端口和本地 mock；未替换或结束 `/Applications` 中任何 App，未读取真实凭据或真实 Science data-dir。

## 主审与第一轮 xhigh 复审

第一轮复审发现并阻塞准入的 P1 包括：重复 upstream 的合法 selector 被 UI 折叠；手动打开失败拿不到 fallback URL；覆盖脚本未发 exact selector、未启动旧 Science、未证明 PID 刷新，并跳过 mock phase。修改后：

- 编辑器按 selector 保留保存条目；多个 selector 可指向同一 upstream，display/selector 不混写。默认项保留原 selector，role 下拉变化同步回 editor state。
- UI 统一展示“白名单 + 默认/质量/均衡/快速”，不再写“选一个固定所有请求”。明确无 tools 的模型不能启用，能力未知项显示警告并允许逐项 scratch 验证。
- Codex 保持只读动态账号目录，不写静态 catalog/roles。
- 手动 `open_url` 每次重新获取 Science URL；opener 失败返回结构化 `status:error + fallback_url`，UI 显示复制 URL/再次打开；成功只表述“已向默认浏览器发出请求”。
- 覆盖脚本用同一临时 HOME 和同一安装路径：旧 v3 Qwen auto-boot 后对精确临时 App PID 做 crash 模拟，精确结束旧测试 Gateway并保留旧 Science；覆盖新 bundle 后要求 v4 migration、byte-identical backup、unknown field 保留和新 Science PID。
- Gateway Acceptance 按 discovery→scratch→formal→reuse→restart 顺序消费 mock。scratch/reuse/restart 发送目录中 `qwen3.7-max` exact selector；formal 发送 Claude opus alias；旧 Codex alias 必须 400 `route_unknown` 且 upstream 增量为 0。
- runner 默认执行本地 bundle 结构校验，失败输出结构化结果。

## 自动验证

第二轮 gpt-5.6-sol xhigh 终核结论为 `P0=0 / P1=0 / P2=0`。终核确认重复 upstream selector、默认/role 引用、浏览器 fallback、exact selector、覆盖时 Science PID 变化和 fallback/retry 五组问题均已关闭；同时清理了残余“内置映射/跟随 Science”的旧单模型文案。

- frontend：model catalog state 4/4；完整 `test/run-frontend.sh` 通过。
- Python UI/runtime boundary：11/11；installed provider controller：24/24。
- Acceptance Python compile、shell `bash -n`、`git diff --check` 通过。
- 隔离 fake-Science fallback→retry runtime Acceptance：1/1。
- 当前专用 Bundle 构建成功；bundle id 为 `com.csswitch.acceptance.modelcatalog`；本地结构校验通过。
- 当前 Bundle executable SHA-256：desktop `2bd40ec5e7907bd78bfe7cd39cb9ac77811be111c89e9ea672086ad0454c133d`；Gateway `b2454d3ae05c0cb0d0fdd9f9cf8539690b3db755aeccd5584975635a7c0b0e2c`。

## 覆盖安装实际状态：已安装并成功启动

完整 runner 已成功构建旧 v3 bundle 和当时的 current bundle，但那次本地尝试在启动临时 bundle 阶段提前结束，没有执行 migration、Gateway、Science 和浏览器断言，因此不计为覆盖安装通过。

修复后的 current bundle 已按 `com.csswitch.acceptance` 同 identity 构建，并覆盖到 `/Applications/CSSwitch Acceptance.app`。安装后 desktop 与 Gateway 哈希和构建产物一致；旧 Acceptance 已备份到 `/private/tmp/CSSwitch Acceptance.before-model-catalog-20260718.app`。正式 CSSwitch App 未被停止或替换。

已安装 Acceptance executable SHA-256：desktop `d496397131f0e1fbd7589fb6220fbf9ef57cf103e750781ce744522266581d4e`；Gateway `b2454d3ae05c0cb0d0fdd9f9cf8539690b3db755aeccd5584975635a7c0b0e2c`。

覆盖复制产生的执行来源元数据导致首次点击后进程在 App 代码运行前退出；清除该元数据后重新启动成功，并确认运行进程使用上述新 desktop binary。完整 coverage runner 的 migration/Gateway/Science 断言仍未在该安装上执行。

结论（上一阶段）：UI 与 Acceptance harness 源码复审通过；当时的 Acceptance 已覆盖安装并成功启动。下方增量结论取代这条安装状态。

## 四输入简化增量（最终状态）

本轮把静态 Provider 的白名单/发现编辑器收敛为四个可自由填写的模型输入：默认（均衡）、高质量、快速、Fable。只有默认必填；质量和快速留空继承默认，Fable 留空继承质量再继承默认。推荐模型只进入 `datalist`，不再要求发现、搜索、勾选或手工“加入目录”。Codex 继续使用独立的只读动态账号目录。

兼容策略：连接编辑会保留稳定 selector 和未投影的历史额外 route。若旧配置的 `default_model_route_id` 与 `role_bindings.sonnet` 不同，默认模型未改变时保留旧 sonnet selector；只有用户实际修改默认模型时才收敛为“默认＝均衡”。新建配置直接采用简化合同。该问题由 xhigh 复审发现并修复，第二轮复审结论为 `P0=0 / P1=0 / P2=0`。

Science 模型表不再展示内部占位词 `default`：配置规范化会把该展示名改成 upstream 模型名，Gateway `/v1/models` 对旧配置也做同样兼容。除了 Rust 单测，还新增真实 Gateway 进程测试，显式注入 `display_name=default` 并从 HTTP 响应确认实际模型名。

最终自动验证：

- frontend 全通过；模型状态 10/10。
- Gateway Rust 232/232，CLI integration 1/1；desktop Rust 345/345，4 项显式隔离测试未默认执行。
- Gateway 与 desktop `clippy --all-targets -- -D warnings` 通过；rustfmt 与 `git diff --check` 通过。
- Python Gateway/installed-provider/UI-runtime 合同 77/77；新增 `default` 进程回归 1/1。
- 专用同路径覆盖安装仍未通过：旧专用 bundle 无法由系统启动，流程在 migration/Gateway/Science 断言前结束。

`com.csswitch.acceptance` 新 bundle 已构建，源码对应的 linker-signed executable SHA-256 为 `a44282efdd987e3da9cbcf700fa9ef416cbc83ba752cc36e26c04bd903f24066`。新代码裸 release 在隔离 HOME 中可运行，但新 `.app` bundle 哈希在 `/Applications` 中被本机执行策略以 137 终止；原位更新也得到相同结果。因此本轮没有把“新 Acceptance 覆盖运行”写成通过。

为避免留下点击无反应的 App，`/Applications/CSSwitch Acceptance.app` 已恢复到可运行的原始备份，当前 desktop SHA-256 为 `1edd60f3e4c52672ec888285c6752257b6ed714d11b106474fdd47f7ed89f1df`，恢复后进程已确认存活。它不是本轮四输入新版本；本轮新版本的覆盖安装状态是 `ENV-BLOCKED / UNPASSED`。

## v0.8.0 Test 身份迁移（最新状态）

用户随后决定不再保留正式版与 Acceptance 两个已安装 App，只保留一套独立测试版。本工作树的 Desktop、Tauri 与 npm 版本已统一升为 `0.8.0`；真机测试覆盖配置改为 `CSSwitch Test.app / com.csswitch.test`，内部仍使用编译期隔离根 `$HOME/.csswitch-acceptance`，未删除正式或测试配置、凭据和 Science data-dir。

`/Applications/CSSwitch.app` 与 `/Applications/CSSwitch Acceptance.app` 已移出并在 Test 成功启动后删除；历史 `/private/tmp/CSSwitch Acceptance*.app` 与本工作树旧 Acceptance bundle 产物也已按精确路径清理。最终 `/Applications` 中只存在 `/Applications/CSSwitch Test.app`。

本机 AMFI 不接受新 ad-hoc CDHash。经用户明确授权，创建了 `CSSwitch Test Local Signing 20260718` 本地代码签名根证书：公钥证书保留在用户登录钥匙串的 code-signing trust 中；签署用临时私钥、PKCS#12 和临时 keychain 已在签署后删除，用户 keychain 搜索列表恢复为仅 login keychain。未关闭全局安全策略。

已安装 Test 的最终证据：bundle id `com.csswitch.test`，版本 `0.8.0`，desktop SHA-256 `0202655bbe1d12319740ef8152d57fb8aa7a01191cccf6e56ae25bd90f0d143d`，Gateway SHA-256 `bcb91214c2680edf4f3ed0086316282bfb39a78fa57cbaac6bf943803427f6c1`，CDHash `660787f95bb53cd61eedeb1b4b49876c77f3b54d`。`codesign --verify --deep --strict` 通过；进程从最终安装路径保持存活，Computer Use 回读到 `CSSwitch Test` 窗口、`v0.8.0`、四输入后的 profile 列表以及永久禁用的 Codex“编辑”按钮。删除临时私钥后又通过普通 LaunchServices 完成一次退出重开，PID 从 `53370` 更新为 `67857`。

本增量自动验证：版本 JSON/Cargo metadata 一致；frontend gate 全通过；installed-provider controller 24/24（沙箱外仅开放本地回环）；`git diff --check` 通过。以上是本机测试安装证据，不扩展为公开发布或真实 provider 全量结论。
