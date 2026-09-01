// 预览兜底：在普通浏览器（没有 Tauri 后端）里打开时用 mockInvoke 返回假数据，
// 让界面能完整渲染。真实 app 里 window.__TAURI__ 存在，走真后端，此兜底不生效。
export const PREVIEW = !window.__TAURI__;
const QUERY = new URLSearchParams(window.location.search);
export const SKILLS_PREVIEW = PREVIEW && QUERY.get("page") === "skills";
export const PROFILE_INTERACTIVE_PREVIEW = PREVIEW && QUERY.get("profile_preview") === "1";
const PREVIEW_CODEX = PREVIEW && QUERY.get("codex") === "1";
const PREVIEW_CODEX_STALE = PREVIEW && QUERY.get("catalog") === "stale";
const PREVIEW_CODEX_NETWORK = PREVIEW && QUERY.get("catalog") === "network";
const PREVIEW_CONFIG_REFRESH_FAIL = PREVIEW && QUERY.get("config_refresh") === "fail";
const PREVIEW_SLOW_ACTIVATION = PREVIEW && QUERY.get("activation") === "slow";
const PREVIEW_RUNTIME_CACHE = PREVIEW && QUERY.get("runtime") === "cache";
const PREVIEW_BROWSER_FAIL = PREVIEW && QUERY.get("browser") === "fail";
// ── 预览兜底 mock（仅浏览器预览用；node --check 只验语法，真实 app 走真后端） ──
const MOCK_CODEX_CAPABILITIES = {
  auth_mode: "csswitch_oauth", credential_source: "csswitch_oauth",
  base_url_required: false, model_required: false,
  model_discovery: "codex_account_catalog", supports_thinking_policy: false,
  thinking_policy: "", supports_tools_hint: "translated",
};
const MOCK_DISCOVERABLE_CAPABILITIES = {
  auth_mode: "api_key", credential_source: "api_key",
  base_url_required: true, model_required: true,
  model_discovery: "openai_models_or_manual", supports_thinking_policy: false,
  thinking_policy: "", supports_tools_hint: "translated",
};
const MOCK_TEMPLATES = [
  { id: "deepseek", name: "DeepSeek", category: "cn_official", api_format: "anthropic", adapter: "deepseek", base_url: "https://api.deepseek.com/anthropic", base_url_editable: false, requires_model_override: false, builtin_models: ["claude-opus-4-8", "claude-haiku-4-5"], icon: "deepseek", icon_color: "#1E88E5", website_url: "https://platform.deepseek.com" },
  { id: "glm", name: "智谱 GLM", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://open.bigmodel.cn/api/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["glm-5.2", "glm-4.7", "glm-4.6", "glm-4.5-air"], icon: "glm", icon_color: "#2E6BE6", website_url: "https://open.bigmodel.cn" },
  { id: "xiaomi", name: "小米 MiMo", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.xiaomimimo.com/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["mimo-v2.5-pro"], icon: "xiaomi", icon_color: "#FF6900", website_url: "https://xiaomimimo.com" },
  { id: "siliconflow", name: "硅基流动", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.siliconflow.cn", base_url_editable: true, requires_model_override: true, builtin_models: ["deepseek-ai/DeepSeek-V4-Pro", "deepseek-ai/DeepSeek-V4-Flash", "deepseek-ai/DeepSeek-V3.2", "zai-org/GLM-5.2"], icon: "siliconflow", icon_color: "#7C3AED", website_url: "https://siliconflow.cn" },
  { id: "kimi", name: "Kimi（Moonshot）", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.moonshot.cn/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"], icon: "kimi", icon_color: "#16182F", website_url: "https://platform.moonshot.cn" },
  { id: "minimax", name: "MiniMax", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.minimaxi.com/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"], icon: "minimax", icon_color: "#E1341E", website_url: "https://platform.minimaxi.com" },
  { id: "openrouter", name: "OpenRouter", category: "custom", api_format: "anthropic", adapter: "relay", base_url: "https://openrouter.ai/api", base_url_editable: true, requires_model_override: true, builtin_models: ["anthropic/claude-sonnet-5", "anthropic/claude-opus-4.8", "anthropic/claude-opus-4.8-fast"], icon: "openrouter", icon_color: "#6467F2", website_url: "https://openrouter.ai" },
  { id: "qwen", name: "通义千问", category: "cn_official", api_format: "openai_chat", adapter: "qwen", base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1", base_url_editable: false, requires_model_override: false, builtin_models: ["qwen3.7-max", "qwen-plus-latest", "qwen-turbo"], icon: "qwen", icon_color: "#615CED", website_url: "https://dashscope.aliyun.com" },
  { id: "opencode-go-openai", name: "OpenCode Go — OpenAI Chat", category: "official", api_format: "openai_chat", adapter: "openai-custom", base_url: "https://opencode.ai/zen/go/v1", base_url_editable: false, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#111827", website_url: "https://opencode.ai/docs/zh-cn/go/", compatibility_notice: "兼容范围：文本、多轮、tools/tool_choice 与模型发现已纳入门禁；图片、厂商 reasoning、原生流式和结构化输出尚未通过兼容门禁。", capabilities: MOCK_DISCOVERABLE_CAPABILITIES },
  { id: "opencode-go-anthropic", name: "OpenCode Go — Anthropic Messages", category: "official", api_format: "anthropic", adapter: "relay", base_url: "https://opencode.ai/zen/go/v1", base_url_editable: false, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#111827", website_url: "https://opencode.ai/docs/zh-cn/go/", compatibility_notice: "兼容范围：文本、多轮、tools/tool_choice 与模型发现已纳入门禁；图片、厂商 reasoning、原生流式和结构化输出尚未通过兼容门禁。", capabilities: { ...MOCK_DISCOVERABLE_CAPABILITIES, model_discovery: "anthropic_models_or_manual", supports_tools_hint: "passthrough" } },
  { id: "grok", name: "Grok（xAI）", category: "official", api_format: "openai_chat", adapter: "openai-custom", base_url: "https://api.x.ai/v1", base_url_editable: false, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#111827", website_url: "https://docs.x.ai/developers/rest-api-reference/inference", compatibility_notice: "兼容范围：文本、多轮、tools/tool_choice 与模型发现已纳入门禁；图片、厂商 reasoning、原生流式和结构化输出尚未通过兼容门禁。", capabilities: MOCK_DISCOVERABLE_CAPABILITIES },
  { id: "gemini", name: "Gemini（OpenAI 兼容）", category: "official", api_format: "openai_chat", adapter: "openai-custom", base_url: "https://generativelanguage.googleapis.com/v1beta/openai", base_url_editable: false, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#4285F4", website_url: "https://ai.google.dev/gemini-api/docs/openai", compatibility_notice: "兼容范围：仅按官方 OpenAI compatibility 接入；文本、多轮、tools/tool_choice 与模型发现已纳入门禁；图片、厂商 reasoning、原生流式和结构化输出尚未通过兼容门禁。", capabilities: MOCK_DISCOVERABLE_CAPABILITIES },
  { id: "codex", name: "Codex（实验）", category: "experimental", api_format: "openai_responses", adapter: "codex", base_url: "", base_url_editable: false, requires_model_override: false, builtin_models: [], icon: "custom", icon_color: "#111827", website_url: "https://developers.openai.com/codex/", capabilities: MOCK_CODEX_CAPABILITIES },
  { id: "custom-openai", name: "自定义 OpenAI", category: "custom", api_format: "openai_chat", adapter: "openai-custom", base_url: "", base_url_editable: true, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#2563EB", website_url: "" },
  { id: "custom-openai-responses", name: "自定义 OpenAI Responses", category: "custom", api_format: "openai_responses", adapter: "openai-responses", base_url: "", base_url_editable: true, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#0F766E", website_url: "" },
  { id: "custom", name: "自定义 Anthropic", category: "custom", api_format: "anthropic", adapter: "relay", base_url: "", base_url_editable: true, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#6B7280", website_url: "" },
];
for (const template of MOCK_TEMPLATES) {
  if (!template.recommended_catalog && template.builtin_models?.length) {
    template.preset_catalog_id = template.id;
    template.model_catalog_source = "preset";
    template.recommended_catalog = template.builtin_models.map((id) => ({
      selector_id: "", display_name: id, upstream_model: id, supports_tools: null,
    }));
    template.recommended_default_model_route_id = template.builtin_models[0];
    template.recommended_role_bindings = {
      sonnet: template.builtin_models[0], opus: template.builtin_models[0],
      haiku: template.builtin_models.at(-1), fable: template.builtin_models[0],
    };
  }
}
export const mockStore = {
  schema_version: 3,
  active_id: PREVIEW_CODEX ? "p-codex" : "p-demo1",
  applied_profile_id: PREVIEW_CODEX ? "p-codex" : "p-demo1",
  selection_pending: false,
  proxy_port: 18991,
  sandbox_port: 8990,
  reuse_system_ssh: false,
  experimental_codex_enabled: PREVIEW_CODEX,
  codex_network: { mode: "auto", proxy_url: "" },
  codex_network_resolved: { source: "direct", proxy_scheme: null },
  fail_next_get_config: false,
  mode: "proxy",
  profiles: [
    { id: "p-demo1", name: "我的 GLM", template_id: "glm", category: "cn_official", api_format: "anthropic", base_url: "https://open.bigmodel.cn/api/anthropic", model: "glm-4.6", model_options: ["glm-5.2", "glm-4.7", "glm-4.6"], key: "••••••1234", icon: "glm", icon_color: "#2E6BE6", website_url: "https://open.bigmodel.cn", sort_index: 1, notes: "" },
    { id: "p-demo2", name: "DeepSeek 工作", template_id: "deepseek", category: "cn_official", api_format: "anthropic", base_url: "https://api.deepseek.com/anthropic", model: "deepseek-chat", model_options: ["deepseek-chat", "deepseek-reasoner"], key: "••••••8452", icon: "deepseek", icon_color: "#1E88E5", website_url: "https://platform.deepseek.com", sort_index: 2, notes: "" },
    { id: "p-demo3", name: "Kimi Coding", template_id: "kimi", category: "cn_official", api_format: "anthropic", base_url: "https://api.moonshot.cn/anthropic", model: "kimi-k2.7-code", model_options: ["kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"], key: "••••••7731", icon: "kimi", icon_color: "#16182F", website_url: "https://platform.moonshot.cn", sort_index: 3, notes: "" },
    ...(PREVIEW_CODEX ? [{ id: "p-codex", name: "我的 Codex", template_id: "codex", category: "experimental", api_format: "openai_responses", base_url: "", model: "", key: "", has_key: false, has_credential: true, credential_source: "csswitch_oauth", model_policy: "dynamic_catalog", capabilities: MOCK_CODEX_CAPABILITIES, icon: "custom", icon_color: "#111827", website_url: "https://developers.openai.com/codex/", sort_index: 4, notes: "" }] : []),
  ],
};
const MOCK_SKILLS = [
  { skill_id: "csswitch-external-skill-tools", display_name: "CSSwitch external Skill tools", description: "外部 Skill 安装与卸载的 CSSwitch 系统路由。", source_kind: "csswitch_system", bundle_name: null, attachment_state: "attached" },
  { skill_id: "internal-comms", display_name: "Internal communications", description: "从 GitHub 安装的团队沟通写作 Skill。", source_kind: "csswitch_github", bundle_name: null, attachment_state: "attached" },
  { skill_id: "research-workflow", display_name: "Research workflow", description: "从本地 Skill 包导入的研究流程。", source_kind: "csswitch_local", bundle_name: "research-kit", attachment_state: "detached" },
  { skill_id: "pdf", display_name: "PDF", description: "当前 Science 组织中发现的文档处理 Skill。", source_kind: "science_local", bundle_name: null, attachment_state: "attached" },
  { skill_id: "legacy-notes", display_name: "Legacy notes", description: "来源标记不完整，因此 CSSwitch 不推断分发方。", source_kind: "unverified", bundle_name: null, attachment_state: "detached" },
];
const mockImportedSkills = [];
function mockSkillListEnvelope() {
  const scenario = QUERY.get("skills") || "healthy";
  const stopped = scenario === "stopped";
  const unverified = scenario === "unverified";
  const empty = scenario === "empty";
  const items = empty ? [] : [...MOCK_SKILLS, ...mockImportedSkills].map((item) => ({
    ...item,
    attachment_state: stopped || unverified ? "unknown" : item.attachment_state,
  }));
  return {
    schema_version: 1,
    science_state: stopped ? "stopped" : unverified ? "unverified" : "running_healthy",
    active_org_state: "ready",
    attachment_readback: stopped ? "unavailable" : unverified ? "failed" : "verified",
    agent_name: "OPERON",
    items,
    warnings: scenario === "warning" ? [{ code: "SKILL_SOURCE_UNVERIFIED", skill_id: "legacy-notes", message: "一个 Skill 的来源标记无法验证" }] : [],
  };
}
let mockCodexAuth = {
  authenticated: PREVIEW_CODEX, account_hash: PREVIEW_CODEX ? "0123456789abcdef0123456789abcdef" : null,
  expiry_state: PREVIEW_CODEX ? "valid" : "missing", expires_at: PREVIEW_CODEX ? 1893456000 : null,
  auth_epoch: PREVIEW_CODEX ? "fedcba9876543210fedcba9876543210" : null, auth_generation: PREVIEW_CODEX ? 1 : 0,
  reason: PREVIEW_CODEX ? "ready" : "state_missing",
};
function mockCodexAuthEnvelope(command) {
  return { schema_version: 3, ok: true, command, status: { ...mockCodexAuth } };
}
let mockCodexOperation = null;
function mockMask(k) { return k ? "••••" + String(k).slice(-4) : ""; }
function mockEnsureCodexProfile() {
  const existing = mockStore.profiles.find((profile) => profile.template_id === "codex" && profile.credential_source === "csswitch_oauth");
  if (existing) return { ...mockIntentOutcome("codex_ensure_profile", "existing"), disposition: "existing", profile_id: existing.id };
  const id = "p-codex-login";
  mockStore.profiles.push({
    id, name: "Codex（实验）", template_id: "codex", category: "experimental",
    api_format: "openai_responses", base_url: "", model: "", key: "", has_key: false,
    has_credential: true, credential_source: "csswitch_oauth", model_policy: "dynamic_catalog",
    capabilities: MOCK_CODEX_CAPABILITIES, icon: "custom", icon_color: "#111827",
    website_url: "https://developers.openai.com/codex/", sort_index: mockStore.profiles.length + 1, notes: "",
  });
  return { ...mockIntentOutcome("codex_ensure_profile", "created", { selected_profile_id: null, applied_profile_id: null }), disposition: "created", profile_id: id };
}
function mockIntentOutcome(operation, disposition, extra = {}) {
  return {
    schema_version: 1,
    operation,
    intent_id: "abcdef0123456789abcdef0123456789",
    disposition,
    config_state: operation === "set_active_profile" || disposition !== "no_change" ? "committed" : "unchanged",
    validation: "accepted",
    science_running: false,
    ...extra,
  };
}
export function mockInvoke(cmd, args) {
  args = args || {};
  const mockIntentOutcome = (operation, disposition, extra = {}) => ({
    schema_version: 1,
    operation,
    intent_id: "abcdef0123456789abcdef0123456789",
    disposition,
    config_state: operation === "set_active_profile" || disposition !== "no_change" ? "committed" : "unchanged",
    validation: "accepted",
    science_running: false,
    ...extra,
  });
  switch (cmd) {
    case "get_config":
      if (mockStore.fail_next_get_config) {
        mockStore.fail_next_get_config = false;
        return Promise.reject("预览注入：配置刷新失败");
      }
      return Promise.resolve({
        schema_version: mockStore.schema_version, active_id: mockStore.active_id,
        applied_profile_id: mockStore.applied_profile_id,
        selection_pending: mockStore.selection_pending,
        proxy_port: mockStore.proxy_port, sandbox_port: mockStore.sandbox_port,
        reuse_system_ssh: mockStore.reuse_system_ssh,
        experimental_codex_enabled: mockStore.experimental_codex_enabled,
        codex_network: { ...mockStore.codex_network },
        codex_network_resolved: { ...mockStore.codex_network_resolved },
        mode: mockStore.mode, templates: MOCK_TEMPLATES.filter((t) => t.id !== "codex" || mockStore.experimental_codex_enabled),
        profiles: mockStore.profiles.map((p) => {
          const template = MOCK_TEMPLATES.find((item) => item.id === p.template_id) || {};
          const modelCatalog = p.model_catalog || (template.recommended_catalog || []).map((route) => ({ ...route }));
          return {
            ...p,
            model_catalog: modelCatalog,
            model_count: modelCatalog.length,
            default_model_route_id: p.default_model_route_id || p.model || modelCatalog[0]?.upstream_model || "",
            role_bindings: p.role_bindings || { ...(template.recommended_role_bindings || {}) },
          };
        }),
      });
    case "acknowledge_pending_notice":
      return Promise.resolve({ status: "already_acknowledged" });
    case "list_templates":
      return Promise.resolve(MOCK_TEMPLATES.filter((t) => t.id !== "codex" || mockStore.experimental_codex_enabled));
    case "create_profile": {
      const t = MOCK_TEMPLATES.find((x) => x.id === args.templateId) || {};
      const id = "p-" + Math.random().toString(16).slice(2, 10);
      mockStore.profiles.push({
        id, name: args.name || t.name || "新配置", template_id: args.templateId,
        category: t.category || "custom", api_format: t.api_format || "anthropic",
        base_url: args.baseUrl || t.base_url || "", model: args.model || args.modelCatalog?.[0]?.upstream_model || "",
        model_catalog: (args.modelCatalog || t.recommended_catalog || []).map((route) => ({ ...route })),
        default_model_route_id: args.defaultModelRouteId || args.model || "",
        role_bindings: args.roleBindings || { ...(t.recommended_role_bindings || {}) },
        key: mockMask(args.key || ""), model_options: [...(t.builtin_models || [])], icon: t.icon, icon_color: t.icon_color,
        website_url: t.website_url, sort_index: mockStore.profiles.length + 1, notes: "",
      });
      return Promise.resolve(id);
    }
    case "update_profile_metadata": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (!p) return Promise.reject("找不到 profile：" + args.id);
      p.name = args.name; p.notes = args.notes || "";
      return Promise.resolve(null);
    }
    case "update_profile_connection": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (!p) return Promise.reject("找不到 profile：" + args.id);
      if (args.baseUrl != null) p.base_url = args.baseUrl;
      if (args.model != null) p.model = args.model;
      if (args.modelCatalog) {
        p.model_catalog = args.modelCatalog.map((route) => ({ ...route }));
        p.default_model_route_id = args.defaultModelRouteId;
        p.role_bindings = { ...args.roleBindings };
        const selected = p.model_catalog.find((route) => route.selector_id === args.defaultModelRouteId || route.upstream_model === args.defaultModelRouteId);
        p.model = selected?.upstream_model || p.model_catalog[0]?.upstream_model || "";
      }
      if (args.key) p.key = mockMask(args.key);
      if (mockStore.active_id === args.id) mockStore.selection_pending = true;
      return Promise.resolve(mockIntentOutcome("update_profile_connection", "committed", { validated: true, committed: true, status: "ok" }));
    }
    case "clear_profile_key": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (p) p.key = "";
      if (mockStore.applied_profile_id === args.id) {
        mockStore.applied_profile_id = null;
        mockStore.selection_pending = !!mockStore.active_id;
      } else if (mockStore.active_id === args.id) {
        mockStore.selection_pending = true;
      }
      return Promise.resolve(mockIntentOutcome("clear_profile_key", "committed", { committed: true, status: "ok" }));
    }
    case "delete_profile":
      mockStore.profiles = mockStore.profiles.filter((x) => x.id !== args.id);
      if (mockStore.active_id === args.id) mockStore.active_id = "";
      if (mockStore.applied_profile_id === args.id) mockStore.applied_profile_id = null;
      mockStore.selection_pending = !!mockStore.active_id &&
        mockStore.active_id !== mockStore.applied_profile_id;
      return Promise.resolve(mockIntentOutcome("delete_profile", "committed", { committed: true, status: "ok" }));
    case "set_active_profile": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (!p) return Promise.reject("找不到 profile：" + args.id);
      const commit = () => {
        const disposition = mockStore.active_id === args.id ? "no_change" : "committed";
        mockStore.active_id = args.id;
        mockStore.selection_pending = args.id !== mockStore.applied_profile_id;
        const applyState = mockStore.selection_pending ? "pending" : "applied";
        return {
          ...mockIntentOutcome("set_active_profile", disposition, { selected_profile_id: args.id, applied_profile_id: mockStore.applied_profile_id }),
          committed: true,
          status: "ok",
          selected_profile_id: args.id,
          applied_profile_id: mockStore.applied_profile_id,
          apply_state: applyState,
          science_running: false,
          hint: "（预览：已设为当前选择，待一键开始应用）",
        };
      };
      return PREVIEW_SLOW_ACTIVATION
        ? new Promise((resolve) => setTimeout(() => resolve(commit()), 1200))
        : Promise.resolve(commit());
    }
    case "fetch_models":
      if (args.req && args.req.template_id === "codex") {
        return Promise.resolve({
          models: PREVIEW_CODEX_NETWORK ? [] : [
            { id: "claude-csswitch-codex-gpt-5.6-sol", display_name: "Codex / GPT-5.6-Sol", supports_tools: true, reasoning_efforts: ["none", "low", "medium", "high", "xhigh", "max"], default_reasoning_effort: "medium", reviewer_compatible: false, transport: "responses_lite" },
            { id: "claude-csswitch-codex-gpt-5.6-terra", display_name: "Codex / GPT-5.6-Terra", supports_tools: true, reasoning_efforts: ["none", "low", "medium", "high", "xhigh", "max"], default_reasoning_effort: "medium", reviewer_compatible: false, transport: "responses_lite" },
            { id: "claude-csswitch-codex-gpt-5.6-luna", display_name: "Codex / GPT-5.6-Luna", supports_tools: true, reasoning_efforts: ["none", "low", "medium", "high", "xhigh", "max"], default_reasoning_effort: "medium", reviewer_compatible: false, transport: "responses_lite" },
          ],
          source: PREVIEW_CODEX_STALE ? "stale-cache" : PREVIEW_CODEX_NETWORK ? "network" : "live",
          error_kind: PREVIEW_CODEX_STALE || PREVIEW_CODEX_NETWORK ? "network" : null,
          upstream_status: PREVIEW_CODEX_NETWORK ? null : 200, stale: PREVIEW_CODEX_STALE,
          age_seconds: PREVIEW_CODEX_STALE ? 7420 : 0,
        });
      }
      if (args.req && args.req.template_id === "opencode-go-openai") {
        return Promise.resolve({ models: [{ id: "kimi-k3", display_name: "Kimi K3", supports_tools: true, origin: "discovered", availability: "available", route_known: true }, { id: "deepseek-v4-pro", display_name: "DeepSeek V4 Pro", supports_tools: true, origin: "discovered", availability: "available", route_known: true }], source: "live", error_kind: null, upstream_status: 200, filtered_unknown_count: 1, route_catalog: { schema_version: 1, transport: "openai_chat" } });
      }
      if (args.req && args.req.template_id === "opencode-go-anthropic") {
        return Promise.resolve({ models: [{ id: "minimax-m3", display_name: "MiniMax M3", supports_tools: true, origin: "discovered", availability: "available", route_known: true }, { id: "qwen3.7-max", display_name: "Qwen 3.7 Max", supports_tools: true, origin: "discovered", availability: "available", route_known: true }], source: "live", error_kind: null, upstream_status: 200, filtered_unknown_count: 1, route_catalog: { schema_version: 1, transport: "anthropic" } });
      }
      return Promise.resolve({ models: [{ id: "provider-model", display_name: "Provider model", supports_tools: true, origin: "discovered", availability: "available" }], source: "live", error_kind: null, upstream_status: 200 });
    case "validate_profile_catalog_model":
      return Promise.resolve({ validated: true, status: "ok", message: "该模型已通过隔离 scratch 请求验证。" });
    case "set_experimental_codex_enabled":
      mockStore.experimental_codex_enabled = !!args.enabled;
      if (PREVIEW_CONFIG_REFRESH_FAIL) mockStore.fail_next_get_config = true;
      return Promise.resolve({ experimental_codex_enabled: mockStore.experimental_codex_enabled });
    case "codex_auth_status":
      return Promise.resolve(mockCodexAuthEnvelope("status"));
    case "codex_auth_start":
      mockCodexOperation = {
        schema_version: 2, operation_id: "0123456789abcdef0123456789abcdef", sequence: 1,
        method: "browser", state: "starting", started_at_ms: Date.now(), updated_at_ms: Date.now(),
      };
      return Promise.resolve({ ...mockCodexOperation });
    case "codex_auth_operation_status":
      return Promise.resolve(mockCodexOperation ? { ...mockCodexOperation } : null);
    case "codex_auth_cancel":
      if (mockCodexOperation) {
        mockCodexOperation = { ...mockCodexOperation, sequence: mockCodexOperation.sequence + 1, state: "cancelled", updated_at_ms: Date.now(), error: { code: "auth_cancelled", stage: "cancelled", retryable: true } };
      }
      return Promise.resolve({ disposition: "accepted" });
    case "codex_ensure_profile":
      if (!mockCodexAuth.authenticated) return Promise.reject({ code: "codex_login_required", reason: mockCodexAuth.reason, retryable: false });
      return Promise.resolve(mockEnsureCodexProfile());
    case "codex_auth_logout":
      mockCodexAuth = {
        authenticated: false, account_hash: null, expiry_state: "missing", expires_at: null,
        auth_epoch: null, auth_generation: mockCodexAuth.auth_generation + 1, reason: "state_uncommitted",
      };
      return Promise.resolve(mockCodexAuthEnvelope("logout"));
    case "set_codex_network": {
      mockStore.codex_network = { ...args.settings };
      const custom = args.settings && args.settings.mode === "custom";
      mockStore.codex_network_resolved = {
        source: custom ? "custom" : "direct",
        proxy_scheme: custom ? String(args.settings.proxy_url || "").split(":", 1)[0] : null,
      };
      return Promise.resolve({ schema_version: 1, operation_id: "abcdef0123456789abcdef0123456789", operation: "set_codex_network", disposition: "completed", config_state: "after", runtime_state: "stopped", recovery_state: "not_needed", mode: args.settings.mode, ...mockStore.codex_network_resolved, restarted: false });
    }
    case "codex_downgrade_preview": {
      const profiles = mockStore.profiles.filter((p) => p.credential_source === "csswitch_oauth");
      return Promise.resolve({ schema_version: 1, action: "export_then_remove_all", profile_count: profiles.length, profiles: profiles.map((p) => ({ id: p.id, name: p.name })), active_will_clear: profiles.some((p) => p.id === mockStore.active_id), catalog_export_count: 0, preview_fingerprint: "preview-mock-v1", credentials_unchanged: true, app_exit_required: true });
    }
    case "codex_downgrade_export_all":
      return Promise.resolve({ schema_version: 1, status: "CANCELLED", credentials_unchanged: true });
    case "set_settings":
      if (args.cfg) {
        mockStore.proxy_port = args.cfg.proxy_port;
        mockStore.sandbox_port = args.cfg.sandbox_port;
        mockStore.reuse_system_ssh = !!args.cfg.reuse_system_ssh;
      }
      return Promise.resolve(mockIntentOutcome("set_settings", "committed"));
    case "set_mode":
      mockStore.mode = args.mode;
      return Promise.resolve(mockIntentOutcome("set_mode", "committed"));
    case "one_click_login":
      mockStore.applied_profile_id = mockStore.active_id || null;
      mockStore.selection_pending = false;
      return Promise.resolve(PREVIEW_BROWSER_FAIL
        ? {
            msg: "（预览模式：服务已就绪；自动打开失败。）",
            action: "started", stage: "complete", status: "ok", recovery_status: "not_needed",
            fallback_url: "http://127.0.0.1:8990/?nonce=preview-fallback",
          }
        : { msg: "（预览模式：假装已就绪）", action: "started", stage: "complete", status: "ok", recovery_status: "not_needed", fallback_url: null });
    case "science_runtime_preflight":
      return Promise.resolve(PREVIEW_RUNTIME_CACHE
        ? { status: "cached_choice_required", selected_source: null, selected_version: null, cached_version: "0.0.0-preview-cache", download_url: "https://claude.com/download" }
        : { status: "installed_ready", selected_source: "installed_app", selected_version: "0.0.0-preview", cached_version: null, download_url: "https://claude.com/download" });
    case "science_runtime_update_status":
      return Promise.resolve({ schema_version: 1, status: "ready", active_source: "installed_app", active_version: "0.0.0-preview", pending_update: null, last_checked_at_ms: null, activation_policy: "next_cold_start" });
    case "science_runtime_update_action":
      return Promise.resolve({ schema_version: 1, status: "ready", active_source: "installed_app", active_version: "0.0.0-preview", pending_update: null, last_checked_at_ms: Date.now(), activation_policy: "next_cold_start" });
    case "install_local_skill_package":
      if (!mockImportedSkills.some((item) => item.skill_id === "demo-reader")) {
        mockImportedSkills.push({ skill_id: "demo-reader", display_name: "Demo reader", description: "刚刚从本地包导入的示例 Skill。", source_kind: "csswitch_local", bundle_name: "demo-bundle", attachment_state: "attached" });
      }
      return Promise.resolve({ schema_version: 2, status: "BUNDLE_INSTALLED_ATTACHED", package_kind: "bundle", bundle_name: "demo-bundle", skill_names: ["demo-reader"], attach_verified: true, directory_commit: true, message: "bundle 文件已安装并绑定 OPERON。" });
    case "list_installed_skills":
      if (QUERY.get("skills") === "error") return Promise.reject("预览注入：Skill 列表读取失败");
      return Promise.resolve(mockSkillListEnvelope());
    case "open_science_download_page":
      return Promise.resolve(null);
    case "status":
      if (QUERY.get("status") === "error") return Promise.reject(new Error("预览注入：运行状态查询失败"));
      if (QUERY.get("status") === "partial") return Promise.resolve({ proxy: "green", sandbox: "green", upstream: "amber" });
      if (QUERY.get("status") === "stopped") return Promise.resolve({ proxy: "amber", sandbox: "amber", upstream: "amber" });
      return Promise.resolve({ proxy: "green", sandbox: "green", upstream: "green" });
    case "boot_snapshot":
      return Promise.resolve({ sequence: 0, state: "idle", payload: null });
    case "app_version":
      return Promise.resolve("0.0.0-preview");
    case "run_doctor_read_only":
      return Promise.resolve({ schema_version: 1, intent: "read_only_diagnostics", status: "passed", message: "（预览模式：只读诊断占位结果）" });
    case "repair_skill_route":
      return Promise.resolve({ schema_version: 1, intent: "repair_skill_route", status: "synchronized", message: "（预览模式：Skill 路由修复占位结果）" });
    default:
      return Promise.resolve(null);
  }
}

export function getMockCodexOperation() {
  return mockCodexOperation;
}

export function setMockCodexOperation(value) {
  mockCodexOperation = value;
}

export function completeMockCodexLogin() {
  mockCodexAuth = {
    authenticated: true,
    account_hash: "0123456789abcdef0123456789abcdef",
    expiry_state: "valid",
    expires_at: 1893456000,
    auth_epoch: "fedcba9876543210fedcba9876543210",
    auth_generation: mockCodexAuth.auth_generation + 1,
    reason: "ready",
  };
  mockEnsureCodexProfile();
}
