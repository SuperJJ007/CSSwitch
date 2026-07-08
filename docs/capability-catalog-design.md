# CSSwitch 能力 / 兼容性 Catalog 设计

本设计定义第一版静态只读 catalog。它不是 MCP-only catalog，而是把 provider/model、工具调用、MCP/skill、Science native route/version、transport/network 的已知兼容性事实放到同一个机器可读入口里。

第一版只新增数据文件和校验测试，不接 runtime、不改变代理行为、不改变 UI。目标是先把已经存在于源码、测试、issue 和文档里的隐式规则显式化，后续再小步接入 diagnostics、proxy rule lookup 或 UI 展示。

## 边界

CSSwitch 能维护和逐步自动化的范围：

- 代理协议兼容：Anthropic passthrough、OpenAI Chat、OpenAI Responses、model shell、tool_choice、thinking、token cap。
- 工具兼容：工具 schema 归一化、provider/model 级工具黑名单、server-tool block 过滤、DSML tool_use 兜底。
- 本地替代路径：本地 stdio MCP、本地/GitHub skill 的安装引导和可发现性验收。
- 网络/transport：Anthropic host fast-fail、CONNECT 行为、未来 upstream proxy 诊断。
- Science native version：已知 route/version 差异、隔离复测要求、哪些能力不能从旧版本外推。

CSSwitch 不能仅靠 catalog 承诺的范围：

- Anthropic-hosted MCP、Directory connectors、官方 remote skills、官方 claude.ai 托管能力。
- 真实账号态、真实 OAuth scope、真实 `~/.claude-science` live 状态。
- 未隔离复测的 Science/provider GUI E2E、DMG、codesign、notarization。

这些边界必须在 catalog 中标成 `unsupported`、`limited` 或 `unknown`，并以 `diagnose` / `document` 动作表达，而不是包装成“已支持”。

## 文件与 Schema

第一版文件位置：

- `catalog/capabilities.v1.json`
- `test/test_capability_catalog.py`

顶层结构固定：

```json
{
  "schema_version": 1,
  "providers": [],
  "tool_rules": [],
  "mcp_servers": [],
  "skills": [],
  "science_versions": [],
  "transport_rules": []
}
```

每条 entry 使用统一最小字段集：

```json
{
  "id": "stable-string-id",
  "scope": "provider|model|tool|mcp|skill|science_version|transport",
  "match": {},
  "status": "supported|limited|unsupported|unknown",
  "action": "none|normalize|drop|disable|degrade|diagnose|document",
  "reason": "short human-readable reason",
  "evidence": ["file-or-issue-reference"],
  "tests": ["test name or empty"]
}
```

字段含义：

- `id`：稳定规则 ID，用于测试、诊断、未来 UI 链接。
- `scope`：规则所属能力域。`model` 仍放在 `providers` section 内，表示 provider/model catalog 的子类。
- `match`：非执行 DSL，只描述当前规则适用条件；v1 不要求 runtime 解释。
- `status`：当前事实状态。`supported` 必须已有代码和测试或明确文档证据；`limited` 表示支持有条件或存在边界；`unsupported` 表示明确不能由 CSSwitch 修通；`unknown` 表示候选方向仍需探针。
- `action`：当前或未来处理方式。v1 只记录，不驱动代码。
- `evidence`：必须非空，可以是源码、测试、docs、issue/PR URL。
- `tests`：可以为空；为空表示该事实目前只能由文档/issue 或未来探针约束。

## 首批规则

v1 catalog 只登记已知事实：

- Kimi relay：`web_search` 会被视作 provider server tool；CSSwitch 不上送该 local client tool，并过滤 `server_tool_use` / `web_search_tool_result`。
- Relay Anthropic-compatible：空或松散 `input_schema` 会归一化成 object schema。
- DeepSeek：forced `tool_choice` 时禁用 thinking。
- Kimi relay thinking：`thinking_policy=enabled` 时去掉 forced `tool_choice`，保留 tools。
- DashScope/OpenAI Responses：drop `web_search`，带工具时使用 conservative output cap。
- Science model selector：正式 force 模型时返回 `claude-opus-4-8` 壳，真实模型名放 `display_name`。
- Hosted MCP / Directory connectors：虚拟 OAuth 下是官方托管能力边界，只能诊断和提供本地替代方向。
- Streamable HTTP MCP：外部 HTTP MCP 依赖 transport/upstream proxy 能力，当前标 `limited`。
- Science `0.1.0-dev` / `0.1.15-dev`：记录 route/version 差异，尤其 auth nonce、conda/pypi remote、skills resync。
- Transport：Anthropic domains CONNECT 401 fast-fail；非 Anthropic CONNECT 当前 direct tunnel；ordinary HTTP proxy/upstream proxy 仍不是已实现 runtime 能力。

## 后续接入顺序

1. 静态 catalog + 校验测试。
2. diagnostics 读取 catalog，只展示“已知边界/建议复测”，不改变运行行为。
3. proxy rule lookup，把已存在的硬编码规则逐步改为从 catalog 或编译时派生结构读取。
4. UI 展示 capability details，并为 MCP/skill 提供本地替代安装和可发现性验收入口。

每一步都必须保持当前安全边界：不读写真实 `~/.claude-science`，不使用 `8765`，不把真实账号态或官方托管能力说成已验证。
