import { CODEX_AUTH_REASONS, formatCodexAuthCommandError, parseCodexAuthCommandError } from "./codex-auth-protocol.js";
import { RUNTIME_STATUS_LABELS, aggregateRuntimeStatus, normalizeRuntimeLight } from "./runtime-status-state.js";
import {
  buildSimpleModelSubmission,
  mergeCatalogCandidates,
  projectSimpleModelFields,
} from "./model-catalog-state.js";

// CSSwitch 桌面面板前端。只调用后端 Tauri command，绝不碰任何密钥落盘逻辑。
// 后端只把 key 的【掩码】回显给这里；完整 key 永不进前端。
//
// ── Tauri 参数键约定（务必遵守）──────────────────────────────────────────────
// 本项目所有命令都是裸 `#[tauri::command]`（无 rename_all）。tauri-macros 默认
// `ArgumentCase::Camel`，会把 Rust 蛇形【顶层参数名】转成 lowerCamelCase 交给 JS：
//   template_id→templateId、base_url→baseUrl、api_format→apiFormat、skip_verify→skipVerify。
// 所以 invoke 顶层 args 用【小驼峰】。而 serde 结构体入参（`req`=FetchModelsReq、
// `cfg`=UiSettings）内部字段按结构体字段名（蛇形）：proxy_port/sandbox_port、
// template_id/base_url/key/profile_id。核对表见任务报告。
//
// 预览兜底：在普通浏览器（没有 Tauri 后端）里打开时用 mockInvoke 返回假数据，
// 让界面能完整渲染。真实 app 里 window.__TAURI__ 存在，走真后端，此兜底不生效。
const PREVIEW = !window.__TAURI__;
const QUERY = new URLSearchParams(window.location.search);
const SKILLS_PREVIEW = PREVIEW && QUERY.get("page") === "skills";
const PROFILE_INTERACTIVE_PREVIEW = PREVIEW && QUERY.get("profile_preview") === "1";
const PREVIEW_CODEX = PREVIEW && QUERY.get("codex") === "1";
const PREVIEW_CODEX_STALE = PREVIEW && QUERY.get("catalog") === "stale";
const PREVIEW_CODEX_NETWORK = PREVIEW && QUERY.get("catalog") === "network";
const PREVIEW_CONFIG_REFRESH_FAIL = PREVIEW && QUERY.get("config_refresh") === "fail";
const PREVIEW_SLOW_ACTIVATION = PREVIEW && QUERY.get("activation") === "slow";
const PREVIEW_RUNTIME_CACHE = PREVIEW && QUERY.get("runtime") === "cache";
const PREVIEW_BROWSER_FAIL = PREVIEW && QUERY.get("browser") === "fail";
const invoke = PREVIEW
  ? (cmd, args) => mockInvoke(cmd, args)
  : window.__TAURI__.core.invoke;

// ── 预览兜底 mock（仅浏览器预览用；node --check 只验语法，真实 app 走真后端） ──
const MOCK_CODEX_CAPABILITIES = {
  auth_mode: "csswitch_oauth", credential_source: "csswitch_oauth",
  base_url_required: false, model_required: false,
  model_discovery: "codex_account_catalog", supports_thinking_policy: false,
  thinking_policy: "", supports_tools_hint: "translated",
};
const MOCK_TEMPLATES = [
  { id: "deepseek", name: "DeepSeek", category: "cn_official", api_format: "anthropic", adapter: "deepseek", base_url: "https://api.deepseek.com/anthropic", base_url_editable: false, requires_model_override: false, builtin_models: ["claude-opus-4-8", "claude-haiku-4-5"], icon: "deepseek", icon_color: "#1E88E5", website_url: "https://platform.deepseek.com" },
  { id: "glm", name: "智谱 GLM", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://open.bigmodel.cn/api/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["glm-5.2", "glm-4.7", "glm-4.6", "glm-4.5-air"], icon: "glm", icon_color: "#2E6BE6", website_url: "https://open.bigmodel.cn" },
  { id: "xiaomi", name: "小米 MiMo", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.xiaomimimo.com/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["mimo-v2.5-pro"], icon: "xiaomi", icon_color: "#FF6900", website_url: "https://xiaomimimo.com" },
  { id: "siliconflow", name: "硅基流动", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.siliconflow.cn", base_url_editable: true, requires_model_override: true, builtin_models: ["deepseek-ai/DeepSeek-V4-Pro", "deepseek-ai/DeepSeek-V4-Flash", "deepseek-ai/DeepSeek-V3.2", "zai-org/GLM-5.2"], icon: "siliconflow", icon_color: "#7C3AED", website_url: "https://siliconflow.cn" },
  { id: "kimi", name: "Kimi（Moonshot）", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.moonshot.cn/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"], icon: "kimi", icon_color: "#16182F", website_url: "https://platform.moonshot.cn" },
  { id: "minimax", name: "MiniMax", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.minimaxi.com/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"], icon: "minimax", icon_color: "#E1341E", website_url: "https://platform.minimaxi.com" },
  { id: "openrouter", name: "OpenRouter", category: "custom", api_format: "anthropic", adapter: "relay", base_url: "https://openrouter.ai/api", base_url_editable: true, requires_model_override: true, builtin_models: ["anthropic/claude-sonnet-5", "anthropic/claude-opus-4.8", "anthropic/claude-opus-4.8-fast"], icon: "openrouter", icon_color: "#6467F2", website_url: "https://openrouter.ai" },
  { id: "qwen", name: "通义千问", category: "cn_official", api_format: "openai_chat", adapter: "qwen", base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1", base_url_editable: false, requires_model_override: false, builtin_models: ["qwen3.7-max", "qwen-plus-latest", "qwen-turbo"], icon: "qwen", icon_color: "#615CED", website_url: "https://dashscope.aliyun.com" },
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
const mockStore = {
  schema_version: 3,
  active_id: PREVIEW_CODEX ? "p-codex" : "p-demo1",
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
  if (existing) return { disposition: "existing", profile_id: existing.id };
  const id = "p-codex-login";
  mockStore.profiles.push({
    id, name: "Codex（实验）", template_id: "codex", category: "experimental",
    api_format: "openai_responses", base_url: "", model: "", key: "", has_key: false,
    has_credential: true, credential_source: "csswitch_oauth", model_policy: "dynamic_catalog",
    capabilities: MOCK_CODEX_CAPABILITIES, icon: "custom", icon_color: "#111827",
    website_url: "https://developers.openai.com/codex/", sort_index: mockStore.profiles.length + 1, notes: "",
  });
  return { disposition: "created", profile_id: id };
}
function mockInvoke(cmd, args) {
  args = args || {};
  switch (cmd) {
    case "get_config":
      if (mockStore.fail_next_get_config) {
        mockStore.fail_next_get_config = false;
        return Promise.reject("预览注入：配置刷新失败");
      }
      return Promise.resolve({
        schema_version: mockStore.schema_version, active_id: mockStore.active_id,
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
      return Promise.resolve({ validated: true });
    }
    case "clear_profile_key": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (p) p.key = "";
      return Promise.resolve(null);
    }
    case "delete_profile":
      mockStore.profiles = mockStore.profiles.filter((x) => x.id !== args.id);
      if (mockStore.active_id === args.id) mockStore.active_id = "";
      return Promise.resolve(null);
    case "set_active_profile": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (!p) return Promise.reject("找不到 profile：" + args.id);
      const commit = () => {
        mockStore.active_id = args.id;
        return { committed: true, active_id: args.id, hint: "（预览：已设为当前）" };
      };
      return PREVIEW_SLOW_ACTIVATION
        ? new Promise((resolve) => setTimeout(() => resolve(commit()), 1200))
        : Promise.resolve(commit());
    }
    case "fetch_models":
      if (args.req && args.req.template_id === "codex") {
        return Promise.resolve({
          models: PREVIEW_CODEX_NETWORK ? [] : [
            { id: "claude-csswitch-codex-gpt-5.6-sol", display_name: "Codex / GPT-5.6-Sol", supports_tools: true },
            { id: "claude-csswitch-codex-gpt-5.6-terra", display_name: "Codex / GPT-5.6-Terra", supports_tools: true },
            { id: "claude-csswitch-codex-gpt-5.6-luna", display_name: "Codex / GPT-5.6-Luna", supports_tools: true },
          ],
          source: PREVIEW_CODEX_STALE ? "stale-cache" : PREVIEW_CODEX_NETWORK ? "network" : "live",
          error_kind: PREVIEW_CODEX_STALE || PREVIEW_CODEX_NETWORK ? "network" : null,
          upstream_status: PREVIEW_CODEX_NETWORK ? null : 200, stale: PREVIEW_CODEX_STALE,
          age_seconds: PREVIEW_CODEX_STALE ? 7420 : 0,
        });
      }
      return Promise.resolve({ models: [{ id: "glm-4.6", display_name: "GLM 4.6", supports_tools: true, origin: "discovered", availability: "available" }, { id: "glm-5", display_name: "GLM 5", supports_tools: null, origin: "discovered", availability: "available" }], source: "live", error_kind: null, upstream_status: 200 });
    case "preview_profile_preset_sync": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      const t = MOCK_TEMPLATES.find((x) => x.id === p?.template_id);
      if (!p || !t?.recommended_catalog) return Promise.reject("该配置没有推荐目录");
      return Promise.resolve({
        profile_id: p.id, preset_catalog_id: t.id,
        additions: t.recommended_catalog.map((r) => r.upstream_model).filter((id) => !(p.model_catalog || []).some((r) => r.upstream_model === id)),
        removals: (p.model_catalog || []).map((r) => r.upstream_model).filter((id) => !t.recommended_catalog.some((r) => r.upstream_model === id)),
        model_catalog: t.recommended_catalog.map((r) => ({ ...r })),
        default_model_route_id: t.recommended_default_model_route_id,
        role_bindings: { ...t.recommended_role_bindings },
        preview_fingerprint: "a".repeat(64), requires_confirmation: true,
      });
    }
    case "apply_profile_preset_sync":
      return Promise.resolve({ committed: true, status: "ok", message: "已同步最新推荐。" });
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
      return Promise.resolve({ mode: args.settings.mode, ...mockStore.codex_network_resolved, restarted: false });
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
      return Promise.resolve(null);
    case "set_mode":
      mockStore.mode = args.mode;
      return Promise.resolve(null);
    case "one_click_login":
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
    case "boot_error":
      return Promise.resolve(null);
    case "app_version":
      return Promise.resolve("0.0.0-preview");
    case "run_doctor":
      return Promise.resolve("（预览模式：后端未运行，这里是占位文本）");
    default:
      return Promise.resolve(null);
  }
}

const $ = (id) => document.getElementById(id);
const els = {};
let statusTimer = null;
let busy = false;
let busyOp = null;
let activationInFlight = false;
let activationOp = null;
let busyMsgTimers = [];
let browserOpenInFlight = false;
let doctorInFlight = false;
let statusRecoveryMsg = "";
let skillPage = null;
let runtimeChoiceActiveId = null;
let mode = "proxy"; // "proxy" 第三方 | "official" 官方
let officialRuntimeState = "gray";
// 当前配置快照（get_config 结果）。全 key 绝不在此，只有掩码。
let configState = { profiles: [], templates: [], active_id: "", proxy_port: 18991, sandbox_port: 8990, reuse_system_ssh: false, experimental_codex_enabled: false, codex_network: { mode: "auto", proxy_url: "" }, codex_network_resolved: { source: "direct", proxy_scheme: null } };
let codexAuthState = null;          // 仅保存后端脱敏状态；绝不包含 token / email
let codexAuthOperation = null;      // 仅驻内存；operation ID 不展示、不写日志
let codexLoginStarting = false;
let codexProfileRepairNeeded = false;
let codexNetworkSaving = false;
let pendingSkipActivateId = null;   // set_active 校验含糊时，允许「跳过验证」再切
let pendingConfirm = null;          // 危险操作（清 key / 删除）的「再点一次确认」态
let wizardCatalog = [];
let connectionCatalog = [];
let wizardRoleRefs = { quality: "", balanced: "", fast: "" };
let connectionRoleRefs = { quality: "", balanced: "", fast: "" };

const CAT_LABELS = { official: "官方", cn_official: "国内", custom: "自定义", experimental: "实验" };
const MODEL_FAMILY_ICONS = {
  anthropic: { file: "anthropic.svg", label: "Anthropic" },
  deepseek: { file: "deepseek.svg", label: "DeepSeek" },
  generic: { file: "generic.svg", label: "自定义模型" },
  glm: { file: "glm.svg", label: "智谱 GLM" },
  kimi: { file: "kimi.svg", label: "Kimi" },
  minimax: { file: "minimax.svg", label: "MiniMax" },
  openai: { file: "openai.svg", label: "OpenAI" },
  openrouter: { file: "openrouter.svg", label: "OpenRouter" },
  qwen: { file: "qwen.svg", label: "通义千问" },
  siliconflow: { file: "siliconflow.svg", label: "硅基流动" },
  xiaomi: { file: "xiaomi.svg", label: "小米 MiMo" },
};

function modelFamilyKey(profile = {}) {
  const templateId = String(profile.template_id || "").toLowerCase();
  const icon = String(profile.icon || "").toLowerCase();
  const direct = templateId || icon;
  if (direct === "codex") return "openai";
  if (direct === "custom-openai" || direct === "custom-openai-responses") return "openai";
  if (direct === "custom") {
    if (String(profile.api_format || "").startsWith("openai")) return "openai";
    if (String(profile.api_format || "") === "anthropic") return "anthropic";
  }
  if (MODEL_FAMILY_ICONS[direct]) return direct;
  if (MODEL_FAMILY_ICONS[icon]) return icon;

  const value = [profile.model, profile.name, profile.base_url, profile.website_url]
    .filter(Boolean)
    .join(" ")
    .toLowerCase();
  if (/openrouter/.test(value)) return "openrouter";
  if (/siliconflow|siliconcloud/.test(value)) return "siliconflow";
  if (/deepseek/.test(value)) return "deepseek";
  if (/glm|zhipu|bigmodel|zai-org/.test(value)) return "glm";
  if (/kimi|moonshot/.test(value)) return "kimi";
  if (/minimax/.test(value)) return "minimax";
  if (/qwen|dashscope|tongyi/.test(value)) return "qwen";
  if (/mimo|xiaomi/.test(value)) return "xiaomi";
  if (/claude|anthropic/.test(value)) return "anthropic";
  if (/gpt|openai/.test(value)) return "openai";
  return "generic";
}

function modelFamilyMeta(profile = {}) {
  const key = modelFamilyKey(profile);
  const item = MODEL_FAMILY_ICONS[key] || MODEL_FAMILY_ICONS.generic;
  return { ...item, key, src: `/assets/model-icons/${item.file}` };
}

function updateModelIcon(image, profile) {
  if (!image) return;
  const meta = modelFamilyMeta(profile);
  image.src = meta.src;
  image.title = meta.label;
  image.dataset.modelFamily = meta.key;
}

function modelIconMarkup(profile) {
  const meta = modelFamilyMeta(profile);
  return `<img class="model-family-icon profile-model-icon" src="${meta.src}" alt="" aria-hidden="true" title="${escapeHtml(meta.label)}" data-model-family="${meta.key}" />`;
}
const PAGE_META = {
  switch: ["", "模型连接", ""],
  skills: ["", "Skill & MCP", ""],
  status: ["", "状态", ""],
  settings: ["", "设置", ""],
};

const THEME_STORAGE_KEY = "csswitch-theme";

function savedTheme() {
  try {
    return window.localStorage?.getItem(THEME_STORAGE_KEY) || "";
  } catch (_) {
    return "";
  }
}

function applyTheme(theme, { persist = true } = {}) {
  const value = theme === "dark" ? "dark" : "light";
  document.documentElement.dataset.theme = value;
  if (els.themeBtn) els.themeBtn.textContent = value === "dark" ? "切换浅色主题" : "切换深色主题";
  if (persist) {
    try { window.localStorage?.setItem(THEME_STORAGE_KEY, value); } catch (_) {}
  }
}

function setPage(page) {
  if (page === "profiles") page = "switch";
  const meta = PAGE_META[page] || PAGE_META.switch;
  document.querySelectorAll("[data-page]").forEach((node) => node.classList.toggle("active", node.dataset.page === page));
  document.querySelectorAll("[data-page-target]").forEach((node) => node.classList.toggle("active", node.dataset.pageTarget === page));
  if (els.pageEyebrow) els.pageEyebrow.textContent = meta[0];
  if (els.pageTitle) els.pageTitle.textContent = meta[1];
  if (els.pageSubtitle) els.pageSubtitle.textContent = meta[2];
  if (page === "skills") skillPage?.ensureLoaded();
}

async function configureDesktopWindow() {
  if (PREVIEW) return;
  try {
    const appWindow = window.__TAURI__.window.getCurrentWindow();
    const LogicalSize = window.__TAURI__.dpi.LogicalSize;
    await appWindow.setMinSize(new LogicalSize(760, 520));
    await appWindow.setSize(new LogicalSize(920, 600));
  } catch (_) {
    // 某些受限 WebView 权限下不能改窗口尺寸；界面本身仍保持响应式。
  }
}

function renderCurrentSummary() {
  if (!els.currentProfileName) return;
  if (mode === "official") {
    updateModelIcon(els.currentProfileIcon, { template_id: "anthropic" });
    els.currentProfileName.textContent = "官方 Claude";
    els.currentProfileState.textContent = "官方模式";
    els.currentProfileState.className = "state-pill neutral";
    els.currentRouteMode.textContent = "由 Science 管理";
    els.currentProfileModel.textContent = "Claude Science";
    els.currentProfileMeta.textContent = "订阅与登录由 Claude Science 管理";
    return;
  }
  const profile = (configState.profiles || []).find((item) => item.id === configState.active_id);
  els.currentRouteMode.textContent = "CSSwitch 代理";
  if (!profile) {
    updateModelIcon(els.currentProfileIcon, {});
    els.currentProfileName.textContent = "尚未选择配置";
    els.currentProfileState.textContent = "等待选择";
    els.currentProfileState.className = "state-pill neutral";
    els.currentProfileModel.textContent = "未配置";
    els.currentProfileMeta.textContent = "从下方选择配置方案";
    return;
  }
  const hasKey = typeof profile.has_key === "boolean" ? profile.has_key : !!profile.key;
  updateModelIcon(els.currentProfileIcon, profile);
  els.currentProfileName.textContent = profile.name || "未命名配置";
  const codexDisabled = isCodexSource(profile) && !configState.experimental_codex_enabled;
  els.currentProfileState.textContent = codexDisabled ? "入口已关闭" : "当前配置";
  els.currentProfileState.className = "state-pill neutral";
  els.currentProfileModel.textContent = profile.model || modelSummary(profile);
  els.currentProfileMeta.textContent = isCodexSource(profile)
    ? "CSSwitch OAuth · 账号动态模型目录"
    : (profile.base_url || (hasKey ? "Key 已保存" : "未填写端点"));
}

// ── 模型能力（纯函数，无 DOM）：native 映射 / relay 跟随 / relay 固定 / 账号动态目录。──
const CAP = { NATIVE: "native", FOLLOW: "follow", FIXED: "fixed", DYNAMIC: "dynamic" };
function templateCaps(t) { return (t && t.capabilities) || {}; }
function hasCapField(t, field) {
  return !!(t && t.capabilities && Object.prototype.hasOwnProperty.call(t.capabilities, field));
}
function legacyNativeAdapterFallback(t) {
  return !!(t && !t.capabilities && (t.adapter === "deepseek" || t.adapter === "qwen"));
}
function modelCapability(t) {
  if (!t) return CAP.FIXED;                       // 未知模板：最保守，要求填模型
  const caps = templateCaps(t);
  if (caps.model_discovery === "codex_account_catalog") return CAP.DYNAMIC;
  if (caps.model_discovery === "builtin_static" && caps.model_required === false) return CAP.NATIVE;
  if (caps.model_required === false) return CAP.FOLLOW;
  if (hasCapField(t, "model_required")) return CAP.FIXED;
  if (legacyNativeAdapterFallback(t)) return CAP.NATIVE; // 仅兼容旧后端 / preview mock；S1 DTO 走 capabilities
  return t.requires_model_override ? CAP.FIXED : CAP.FOLLOW; // 兼容旧后端 / preview mock
}
function isCodexSource(t) {
  return !!(t && (
    t.id === "codex" || t.template_id === "codex" ||
    templateCaps(t).model_discovery === "codex_account_catalog"
  ));
}
function modelRequired(t) {
  if (!t) return true;
  if (hasCapField(t, "model_required")) return !!templateCaps(t).model_required;
  return !!t.requires_model_override; // 兼容旧后端 / preview mock
}
function baseUrlRequired(t) {
  if (!t) return true;
  if (hasCapField(t, "base_url_required")) return !!templateCaps(t).base_url_required;
  return !!t.base_url_editable; // 兼容旧后端 / preview mock
}
function profileCapabilitySource(p, t) {
  if (!p || !p.capabilities) return t;
  return {
    ...(t || {}),
    ...p,
    builtin_models: (t && t.builtin_models) || [],
    base_url_editable: t ? t.base_url_editable : true,
    capabilities: p.capabilities,
  };
}
// 来源提示：据「地址是否可编辑 + 模型能力」生成，不能只看 category。
function sourceHint(t) {
  if (!t) return "选择来源后按提示填写。";
  if (isCodexSource(t)) {
    return "使用 CSSwitch 独立 Codex OAuth；无需 API Key 或地址。账号模型会动态显示在 Science 的 More models 中。";
  }
  // 真·自定义（可编辑且无预设地址）才叫「自定义端点」；预设虽可编辑但有官方默认，另行描述。
  if (t.base_url_editable && !t.base_url && t.api_format === "openai_chat") {
    return "自定义 OpenAI Chat Completions 兼容端点：填 base root、key 与模型名称，经代理转换协议。";
  }
  if (t.base_url_editable && !t.base_url && t.api_format === "openai_responses") {
    return "自定义 OpenAI Responses 兼容端点：填 base root、key 与模型名称，经代理转换协议。";
  }
  if (t.base_url_editable && !t.base_url) return "自定义 Anthropic 兼容端点：填写地址、key 和供应商提供的精确模型 ID。";
  const cap = modelCapability(t);
  if (cap === CAP.NATIVE) {
    // deepseek 是原生 Anthropic 透传；qwen 经代理做 Anthropic↔OpenAI 转换，别都叫「直连」。
    return t.api_format === "openai_chat" || t.api_format === "openai_responses"
      ? "官方端点（经代理转换协议）：填 API Key 即可，地址与推荐模型已内置。"
      : "官方原生端点（无需转换）：填 API Key 即可，地址与推荐模型已内置。";
  }
  // 预设地址可编辑：默认已填好官方地址，套餐/区域端点可改（如小米 token plan）。
  const addr = t.base_url_editable ? "地址已预填官方默认（套餐 / 区域端点可改）" : "地址已预设";
  if (cap === CAP.FOLLOW) return `填 API Key 即可，${addr}；模型可从推荐中选择或自由填写。`;
  return `填 API Key 和精确模型 ID，${addr}。`;
}
const MODEL_HINT = {
  native: "推荐模型只是初始建议；四个模型框都允许自由填写精确模型 ID。",
  follow: "可以选择推荐模型，也可以直接填写供应商或中转站提供的精确模型 ID。",
  fixed: "默认模型必填；质量、快速与 Fable 留空时自动继承。",
  dynamic: "这里只读取账号目录，不保存固定模型。启动 Science 后请在 More models 选择 Codex / …。",
};
const PROXY_UNHEALTHY_MSG = "代理进程不可达或已退出，请点击「一键开始」恢复。";

// 静态 provider 展示四个自由模型输入；Codex 动态账号目录保持只读。
function applyModelCapability(t, ui, currentModel) {
  const cap = modelCapability(t);
  const listId = ui.sel.getAttribute("list");
  const dl = listId && document.getElementById(listId);
  if (cap === CAP.DYNAMIC) {
    ui.info.textContent = MODEL_HINT.dynamic;
    ui.info.hidden = false;
    ui.sel.hidden = true;
    ui.sel.value = "";
    if (dl) dl.innerHTML = "";
    if (ui.fetchBtn) { ui.fetchBtn.hidden = false; ui.fetchBtn.textContent = "刷新账号模型"; }
    ui.hint.textContent = "模型选择权保留给 Science；CSSwitch 不会静默固定某一个 Codex 模型。";
    return cap;
  }
  // 静态 provider 统一使用四个自由输入框；推荐项只作为 datalist 建议。
  ui.info.textContent = cap === CAP.NATIVE ? MODEL_HINT.native : "";
  ui.info.hidden = cap !== CAP.NATIVE;
  ui.sel.hidden = false;
  if (ui.fetchBtn) {
    ui.fetchBtn.hidden = true;
  }
  const builtin = ((t && t.builtin_models) || []).slice();
  if (currentModel && !builtin.includes(currentModel)) builtin.unshift(currentModel);
  const models = builtin.map((id) => ({ id, supports_tools: null }));
  renderModelOptions(ui.sel, models, "内置");
  ui.sel.value = currentModel || (builtin[0] || "");
  ui.hint.textContent = MODEL_HINT.fixed;
  return cap;
}

function setMsg(text, kind) {
  // 去掉常驻「就绪。」：空消息或纯 idle 时整条反馈栏不占位，有真实反馈（结果/错误/自检）才冒出来。
  const t = text && text !== "就绪。" ? text : "";
  els.msg.textContent = t;
  els.msg.className = "msg" + (kind ? " " + kind : "");
  els.msg.parentElement.hidden = !t && (!els.browserFallback || els.browserFallback.hidden);
}

function setBrowserFallback(url) {
  const value = typeof url === "string" ? url.trim() : "";
  els.browserFallbackUrl.value = value;
  els.browserFallback.hidden = !value;
  els.msg.parentElement.hidden = !value && !els.msg.textContent;
}

function setLight(el, s) {
  const cls = { green: "g", amber: "a", red: "r", gray: "", unknown: "" }[normalizeRuntimeLight(s)];
  el.className = "lt" + (cls ? " " + cls : "");
  document.querySelectorAll('[data-mirror-light="' + el.id + '"]').forEach((node) => {
    node.className = "lt" + (cls ? " " + cls : "");
  });
}

function setStatusText(id, status) {
  const normalized = normalizeRuntimeLight(status);
  const node = $(id);
  if (node) node.textContent = RUNTIME_STATUS_LABELS[normalized];
  document.querySelectorAll('[data-mirror-text="' + id + '"]').forEach((mirror) => {
    mirror.textContent = RUNTIME_STATUS_LABELS[normalized];
  });
}

function proxyRecoveryMessage(status) {
  const err = status && status.last_error;
  if (err && err.type === "proxy_unhealthy") return err.message || PROXY_UNHEALTHY_MSG;
  return "";
}

function setStatusRecoveryMsg(text) {
  if (busy) return;
  const current = els.msg.textContent || "";
  if (text) {
    if (!current || current === statusRecoveryMsg) {
      setMsg(text, "err");
      statusRecoveryMsg = text;
    }
    return;
  }
  if (statusRecoveryMsg && current === statusRecoveryMsg) {
    setMsg("");
  }
  statusRecoveryMsg = "";
}

function clearBusyMsgTimers() {
  busyMsgTimers.forEach((t) => clearTimeout(t));
  busyMsgTimers = [];
}

function profileName(id) {
  const p = (configState.profiles || []).find((x) => x.id === id);
  return p ? p.name : id;
}

function syncProfileBusyState() {
  if (!els.profileList) return;
  els.profileList.querySelectorAll(".prow").forEach((row) => {
    const rowId = row.getAttribute("data-id");
    const isTarget = !!(
      (busyOp && busyOp.kind === "activate" && rowId === busyOp.id) ||
      (activationOp && activationOp.kind === "activate" && rowId === activationOp.id)
    );
    row.classList.toggle("pworking", isTarget);
    row.querySelectorAll("button[data-act]").forEach((btn) => {
      const act = btn.getAttribute("data-act");
      const permanentlyDisabled = btn.dataset.permanentlyDisabled === "true";
      btn.disabled = permanentlyDisabled || busy || (activationInFlight && act === "activate");
      if (act === "activate") {
        btn.textContent = isTarget ? "已提交" : "设为当前";
      }
    });
  });
}

function sameOp(a, b) {
  return !!(a && b && a.kind === b.kind && (a.id || "") === (b.id || ""));
}

function scheduleBusyMsg(ms, op, text) {
  const timer = setTimeout(() => {
    if (busy && sameOp(busyOp, op)) setMsg(text);
  }, ms);
  busyMsgTimers.push(timer);
}

function startFetchModelsFeedback(id) {
  clearBusyMsgTimers();
  setMsg("正在读取 CSSwitch Codex 账号模型目录；首次授权刷新可能需要约 2 分钟…");
  scheduleBusyMsg(12000, { kind: "fetchModels", id }, "仍在等待 Codex 账号模型目录。不会修改模型选择、OAuth 或当前配置。");
  scheduleBusyMsg(60000, { kind: "fetchModels", id }, "仍在等待官方目录响应；CSSwitch 不会把任意模型当作默认模型。");
  scheduleBusyMsg(120000, { kind: "fetchModels", id }, "模型目录接近等待上限。若官方暂时不可达，可能返回带年龄标记的安全缓存。");
}

function startActivateFeedback(id, skipVerify) {
  const name = profileName(id);
  if (skipVerify) {
    setMsg("已提交「" + name + "」：跳过上游校验，后台启动正式代理并探活。完成后会提示结果。");
    return;
  }
  setMsg("已提交「" + name + "」：后台校验上游并准备正式代理。完成后会提示结果。");
}

function startSaveConnectionFeedback(id, active) {
  clearBusyMsgTimers();
  if (active) {
    setMsg("正在保存当前生效配置：先校验新连接，再重启正式代理并探活…");
    scheduleBusyMsg(4500, { kind: "saveConnection", id }, "仍在等待新连接上游校验。失败会保留原连接和原代理。");
    scheduleBusyMsg(18000, { kind: "saveConnection", id }, "上游校验接近等待上限。完成后才会写盘并应用，失败会据实回滚。");
    return;
  }
  setMsg("保存连接中：正在做候选上游校验；无法确认时会保存但标记为未校验…");
  scheduleBusyMsg(4500, { kind: "saveConnection", id }, "仍在等待候选连接校验。不会影响当前正在运行的代理。");
}

function startOneClickFeedback() {
  clearBusyMsgTimers();
  setMsg("一键开始：检查代理 → 准备虚拟登录 → 启动/复用沙箱 → 探活…");
  scheduleBusyMsg(3500, { kind: "oneClick" }, "仍在准备代理或沙箱。若代理配置已变更，可能需要重启本地代理。");
  scheduleBusyMsg(9000, { kind: "oneClick" }, "仍在等待沙箱就绪。完成后会自动打开 Science；失败会显示日志摘要。");
}

function startSwitchModeFeedback(targetMode) {
  clearBusyMsgTimers();
  const toOfficial = targetMode === "official";
  setMsg(toOfficial
    ? "正在切到官方模式：停止第三方代理/沙箱并保存模式…"
    : "正在切到第三方模式：保存模式，完成后可选择配置并一键开始…");
  scheduleBusyMsg(3500, { kind: "switchMode", id: targetMode }, toOfficial
    ? "仍在停止第三方链路。真实 Claude Science 实例不会被触碰。"
    : "仍在保存模式切换。当前不会自动启动第三方代理。");
}

function startPortSaveFeedback(changed) {
  clearBusyMsgTimers();
  if (changed) {
    setMsg("正在保存端口设置：端口变化会先重置当前代理/沙箱链路…");
    scheduleBusyMsg(3500, { kind: "ports" }, "仍在应用端口设置。若旧沙箱无法停止，端口会保持原值并显示错误。");
    return;
  }
  setMsg("正在保存端口设置…");
}

function startDoctorFeedback() {
  clearBusyMsgTimers();
  setMsg("自检中：正在运行本地诊断脚本…");
  scheduleBusyMsg(3500, { kind: "doctor" }, "自检仍在运行。它会检查本地依赖、端口、配置摘要和 CSSwitch 管理的 Skill 路由；不会读取真实 Science HOME，也不会传出、打印或展示完整 key。");
}

function setBusy(on, op) {
  busy = on;
  busyOp = on ? (op || { kind: "global" }) : null;
  const activationBusy = on && busyOp && busyOp.kind === "activate";
  if (!on) clearBusyMsgTimers();
  [
    els.oneClickBtn, els.stopBtn, els.importSkillBtn, els.newBtn,
    els.runtimeUseCacheBtn, els.runtimeDownloadBtn, els.runtimeChoiceCancelBtn,
    els.wizSaveBtn, els.wizFetchBtn, els.wizCancelBtn,
    els.connSaveBtn, els.connFetchBtn, els.connClearBtn, els.connCancelBtn,
    els.wizModel, els.wizRoleQuality, els.wizRoleFast, els.wizRoleFable,
    els.connModel, els.connRoleQuality, els.connRoleFast, els.connRoleFable,
    els.metaSaveBtn, els.metaCancelBtn, els.skipActivateBtn,
    els.codexEnabled, els.codexStatusBtn, els.codexLoginBtn, els.codexRepairProfileBtn,
    els.codexCancelBtn, els.codexLogoutBtn, els.codexNetworkMode, els.codexProxyUrl,
    els.codexNetworkSaveBtn, els.codexDowngradeBtn,
    // 端口输入也纳入忙碌禁用：忙碌中改端口会与在途操作竞态（修 P1-c 前端侧）。
    els.proxyPort, els.sandboxPort, els.reuseSystemSsh,
  ].forEach((b) => b && (b.disabled = on));
  syncOpenBrowserControl();
  if (skillPage) skillPage.setGlobalBusy(on);
  if (els.doctorBtn) els.doctorBtn.disabled = on && !activationBusy;
  // 模式切换按钮同样禁用：忙碌中切官方会与「一键开始」竞态（修 P1-b 前端侧）。
  if (els.modeSeg) els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => (b.disabled = on));
  syncProfileBusyState();
  // 松开忙碌时，把模型必填保存门控交回门（避免 setBusy(false) 覆盖门控）。
  if (!on) { refreshWizGate(); refreshConnGate(); }
  syncActivationControls();
  syncCodexControls();
}

function syncOpenBrowserControl() {
  if (!els.openBrowserBtn) return;
  els.openBrowserBtn.disabled = busy || browserOpenInFlight;
  els.openBrowserBtn.textContent = browserOpenInFlight ? "打开中…" : "浏览器打开";
}

function syncActivationControls() {
  const writeLocked = busy;
  [
    els.newBtn, els.proxyPort, els.sandboxPort, els.reuseSystemSsh,
    els.connClearBtn, els.metaSaveBtn,
  ].forEach((b) => b && (b.disabled = writeLocked));
  if (els.modeSeg) els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => (b.disabled = writeLocked));
  if (els.skipActivateBtn) els.skipActivateBtn.disabled = busy;
  if (els.oneClickBtn) els.oneClickBtn.disabled = busy || activationInFlight;
  if (els.runtimeUseCacheBtn) els.runtimeUseCacheBtn.disabled = busy || activationInFlight;
  if (els.reuseSystemSsh) els.reuseSystemSsh.disabled = busy || activationInFlight;
  syncProfileBusyState();
  refreshWizGate();
  refreshConnGate();
  syncCodexControls();
}

function setActivationInFlight(on, op) {
  activationInFlight = on;
  activationOp = on ? op : null;
  syncActivationControls();
}

async function call(cmd, args) {
  return await invoke(cmd, args);
}

function escapeHtml(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

function tplById(id) {
  return (configState.templates || []).find((t) => t.id === id) || null;
}

// ── 视图切换：列表 / 新建向导 / 连接编辑 / 改名。一次只显示一个表单（列表隐去减少高度）。──
function showView(v) {
  els.connectionOverview.hidden = v !== "list";
  els.listSec.hidden = v !== "list";
  els.wizSec.hidden = v !== "wizard";
  els.connSec.hidden = v !== "conn";
  els.metaSec.hidden = v !== "meta";
  els.panel.classList.toggle("view-form", v !== "list");
  if (v !== "list") setPage("switch");
  if (v === "list") hideSkip();
}
function cancelForm() { showView("list"); setPage("switch"); setMsg("就绪。"); }

function showSkip() { els.skipActivateBtn.hidden = false; }
function hideSkip() { els.skipActivateBtn.hidden = true; pendingSkipActivateId = null; }

// 危险操作「再点一次确认」（避免依赖 window.confirm，Tauri webview 里不可靠）。
function confirmAction(token, promptText, fn) {
  if (pendingConfirm && pendingConfirm.token === token) {
    clearTimeout(pendingConfirm.timer);
    pendingConfirm = null;
    fn();
    return;
  }
  if (pendingConfirm) clearTimeout(pendingConfirm.timer);
  pendingConfirm = {
    token,
    timer: setTimeout(() => { pendingConfirm = null; setMsg("已取消。"); }, 4000),
  };
  setMsg(promptText + " —— 再点一次同一按钮确认（4 秒内）。", "err");
}

function clearStaleCodexAuthState() {
  codexAuthState = null;
  codexProfileRepairNeeded = false;
  renderCodexAuthState();
}

function runtimeCommandErrorText(error) {
  let authError;
  try {
    authError = parseCodexAuthCommandError(error);
  } catch (protocolError) {
    clearStaleCodexAuthState();
    return protocolError.message;
  }
  if (!authError) {
    if (typeof error === "string") return error;
    if (error instanceof Error && typeof error.message === "string") return error.message;
    return "后端返回了无法识别的错误。";
  }
  clearStaleCodexAuthState();
  return formatCodexAuthCommandError(authError);
}

function unwrapCodexAuthEnvelope(response, expectedCommand) {
  if (!response || response.schema_version !== 3 || response.command !== expectedCommand || typeof response.ok !== "boolean") {
    throw new Error("CSSwitch Codex 认证响应协议不匹配。");
  }
  if (!response.ok) {
    throw new Error("CSSwitch Codex 认证失败响应未通过结构化错误通道交付。");
  }
  const status = response.status;
  const validHash = status && (status.account_hash === null || typeof status.account_hash === "string");
  const validExpiry = status && (status.expires_at === null || Number.isSafeInteger(status.expires_at));
  const validEpoch = status && (status.auth_epoch === null || typeof status.auth_epoch === "string");
  const validReason = status && CODEX_AUTH_REASONS.has(status.reason);
  const validCombination = status && ((status.authenticated && status.reason === "ready") || (!status.authenticated && status.reason !== "ready"));
  if (!status || typeof status.authenticated !== "boolean" || typeof status.expiry_state !== "string" ||
      !validHash || !validExpiry || !validEpoch || !Number.isSafeInteger(status.auth_generation) || status.auth_generation < 0) {
    throw new Error("CSSwitch Codex 认证状态结构不匹配。");
  }
  if (!validReason || !validCombination) throw new Error("CSSwitch Codex 认证状态 reason 不匹配。");
  // 只投影 UI 需要的脱敏字段。即使后端未来扩展响应，未知字段也不会进入前端状态。
  return {
    authenticated: status.authenticated,
    account_hash: status.account_hash,
    expiry_state: status.expiry_state,
    expires_at: status.expires_at,
    auth_epoch: status.auth_epoch,
    auth_generation: status.auth_generation,
    reason: status.reason,
  };
}

function renderCodexAuthState() {
  if (!els.codexAuthStatus) return;
  if (codexOperationActive()) {
    renderCodexOperation();
    return;
  }
  if (!codexAuthState) {
    els.codexAuthStatus.textContent = "尚未检查 CSSwitch Codex 登录状态。";
  } else if (!codexAuthState.authenticated) {
    els.codexAuthStatus.textContent = ["state_missing", "state_uncommitted"].includes(codexAuthState.reason)
      ? "CSSwitch Codex 尚未登录（原生 Codex 登录状态未读取）。"
      : "CSSwitch Codex 本地认证记录不完整；请检查状态，不会自动修复或重新登录。";
  } else {
    const expiryLabels = { valid: "有效", expiring: "即将到期", expired: "已到期", unknown: "有效期未知" };
    const account = codexAuthState.account_hash ? String(codexAuthState.account_hash).slice(0, 8) : "未知";
    const expiry = expiryLabels[codexAuthState.expiry_state] || "状态未知";
    els.codexAuthStatus.textContent = "已登录 · 账号标识 " + account + "… · " + expiry;
  }
  syncCodexControls();
}

function hasCodexProfile() {
  return (configState.profiles || []).some((profile) => isCodexSource(profile));
}

function refreshCodexProfileRepairState() {
  codexProfileRepairNeeded = !!(
    codexAuthState && codexAuthState.authenticated &&
    configState.experimental_codex_enabled && !hasCodexProfile()
  );
}

const CODEX_TERMINAL_STATES = new Set(["succeeded", "failed", "cancelled"]);
const CODEX_OPERATION_STATES = new Set(["starting", "waiting", "exchanging", "committing", ...CODEX_TERMINAL_STATES]);
const CODEX_ERROR_STAGES = new Set(["proxy_config", "browser_open", "callback_wait", "token_exchange", "refresh", "revoke", "credential_commit", "profile_ensure", "cancelled"]);
const CODEX_RESPONSE_KINDS = new Set(["json", "html", "empty", "other", "unknown"]);
const CODEX_TRANSPORT_KINDS = new Set(["timeout", "dns_connect", "proxy_connect", "tls", "http", "unknown"]);
let handledCodexTerminal = "";

function parseCodexOperationSnapshot(value) {
  if (!value || value.schema_version !== 2 || !/^[0-9a-f]{32}$/.test(String(value.operation_id || "")) ||
      !Number.isSafeInteger(value.sequence) || value.sequence < 1 || value.method !== "browser" ||
      !CODEX_OPERATION_STATES.has(value.state) || !Number.isSafeInteger(value.started_at_ms) || !Number.isSafeInteger(value.updated_at_ms)) {
    throw new Error("CSSwitch Codex 登录 operation 协议不匹配。");
  }
  if (value.verification_url != null || value.user_code != null || value.expires_at_ms != null) {
    throw new Error("CSSwitch Codex 浏览器登录 operation 包含旧设备码字段。");
  }
  const snap = {
    schema_version: 2,
    operation_id: String(value.operation_id),
    sequence: value.sequence,
    method: value.method,
    state: value.state,
    started_at_ms: value.started_at_ms,
    updated_at_ms: value.updated_at_ms,
    error: null,
  };
  if (value.error != null) {
    const error = value.error;
    if (!error || typeof error.code !== "string" || !CODEX_ERROR_STAGES.has(error.stage) || typeof error.retryable !== "boolean" ||
        (error.upstream_status != null && (!Number.isSafeInteger(error.upstream_status) || error.upstream_status < 100 || error.upstream_status > 599)) ||
        (error.response_kind != null && !CODEX_RESPONSE_KINDS.has(error.response_kind)) ||
        (error.transport_kind != null && !CODEX_TRANSPORT_KINDS.has(error.transport_kind)) ||
        (error.challenge_detected != null && typeof error.challenge_detected !== "boolean")) {
      throw new Error("CSSwitch Codex 登录错误结构不匹配。");
    }
    snap.error = {
      code: error.code, stage: error.stage, retryable: error.retryable,
      upstream_status: error.upstream_status ?? null,
      response_kind: error.response_kind ?? null,
      challenge_detected: error.challenge_detected ?? null,
      transport_kind: error.transport_kind ?? null,
    };
  }
  return snap;
}

function codexOperationActive() {
  return !!(codexAuthOperation && !CODEX_TERMINAL_STATES.has(codexAuthOperation.state));
}

function codexOperationErrorText(error) {
  const labels = {
    oauth_challenge_response: "认证请求遇到上游安全挑战；请检查当前出口或代理路线。",
    oauth_unexpected_content_type: "认证端点返回了意外内容（例如 HTML），请检查代理或上游安全挑战。",
    proxy_connect_failed: "无法连接所选 Codex 代理。",
    tls_failed: "Codex 认证 TLS 连接失败。",
    oauth_network_error: "Codex 认证网络请求失败。",
    callback_timeout: "等待登录超时。",
    callback_unavailable: "本地登录回调端口 1455/1457 不可用。",
    browser_open_failed: "无法打开系统浏览器。",
    oauth_denied: "登录未获授权。",
    auth_cancelled: "登录已取消。",
    keychain_unavailable: "旧版 CSSwitch 本地认证存储不可用。",
    auth_storage_error: "CSSwitch 无法安全保存 Codex 授权。",
    identity_mismatch: "安装包内 Gateway 与 Desktop 不匹配。",
    profile_ensure_failed: "授权已保存，但 Codex 配置尚未创建。无需重新登录，可直接补建配置。",
  };
  let text = labels[error && error.code] || "Codex 登录未完成。";
  if (error && error.upstream_status) text += " 上游状态码 " + error.upstream_status + "。";
  return text;
}

function renderCodexOperation() {
  if (!els.codexAuthStatus || !codexAuthOperation) return;
  const op = codexAuthOperation;
  const labels = {
    starting: "正在启动 Codex 登录…",
    waiting: "正在等待浏览器授权…",
    exchanging: "授权已收到，正在交换令牌…",
    committing: "正在原子写入 CSSwitch 私有认证文件；此时取消不会中断提交。",
    succeeded: "Codex 登录成功，正在刷新状态…",
    failed: codexOperationErrorText(op.error),
    cancelled: "Codex 登录已取消；凭据与 generation 未变。",
  };
  els.codexAuthStatus.textContent = labels[op.state] || "Codex 登录状态未知。";
  syncCodexControls();
}

function acceptCodexOperationSnapshot(raw, allowReplacement) {
  const next = parseCodexOperationSnapshot(raw);
  if (codexAuthOperation && codexAuthOperation.operation_id !== next.operation_id && !allowReplacement) {
    if (!CODEX_TERMINAL_STATES.has(codexAuthOperation.state) || next.started_at_ms < codexAuthOperation.started_at_ms) return;
  }
  if (codexAuthOperation && codexAuthOperation.operation_id === next.operation_id && next.sequence <= codexAuthOperation.sequence) return;
  codexAuthOperation = next;
  codexLoginStarting = false;
  renderCodexOperation();
  if (CODEX_TERMINAL_STATES.has(next.state)) {
    const terminalKey = next.operation_id + ":" + next.sequence;
    if (handledCodexTerminal === terminalKey) return;
    handledCodexTerminal = terminalKey;
    if (next.state === "succeeded") {
      Promise.all([
        refreshCodexAuthStatus({ quiet: true }),
        loadConfig({ throwOnError: true }),
      ]).then(([authenticated]) => {
        refreshCodexProfileRepairState();
        syncCodexControls();
        if (!authenticated || !hasCodexProfile()) {
          throw new Error("登录终态与本地授权/配置状态不一致。");
        }
        const active = (configState.profiles || []).find((profile) => profile.id === configState.active_id);
        if (active && isCodexSource(active)) {
          setMsg("CSSwitch Codex 登录完成，Codex 配置已就绪。它仍是当前配置，但登录期间受管 Science/Gateway 已停止且未自动重启；请点击“一键开始”。原生 Codex OAuth 未被读取或改动。", "ok");
        } else {
          setMsg("CSSwitch Codex 登录完成，Codex 配置已就绪。下一步可在“模型连接 > 配置方案”中设为当前；原生 Codex OAuth 未被读取或改动。", "ok");
        }
      }).catch((error) => {
        refreshCodexProfileRepairState();
        syncCodexControls();
        setMsg("Codex 授权已完成，但刷新配置状态失败：" + error + " 请检查状态；若显示补建入口，无需重新登录。", "err");
      });
    } else if (next.state === "failed") {
      if (next.error && next.error.code === "profile_ensure_failed") {
        refreshCodexAuthStatus({ quiet: true }).then((authenticated) => {
          refreshCodexProfileRepairState();
          syncCodexControls();
          setMsg(authenticated
            ? "Codex 授权已安全保存，但配置尚未创建。请点“补建 Codex 配置”；无需重新登录。"
            : "Codex 配置创建失败，且无法确认授权状态。请先检查状态。", "err");
        });
      } else {
        codexProfileRepairNeeded = false;
        syncCodexControls();
        setMsg("CSSwitch Codex 登录失败：" + codexOperationErrorText(next.error), "err");
      }
    } else {
      codexProfileRepairNeeded = false;
      syncCodexControls();
      setMsg("CSSwitch Codex 登录已取消；未提交新的本地认证 generation。", "ok");
    }
  }
}

async function registerCodexAuthEvents() {
  if (!PREVIEW && window.__TAURI__.event) {
    await window.__TAURI__.event.listen("codex-auth://operation", (event) => {
      try { acceptCodexOperationSnapshot(event.payload, false); }
      catch (e) { setMsg("Codex 登录事件被安全拒绝：" + e, "err"); }
    });
  }
  try {
    const snapshot = await call("codex_auth_operation_status");
    if (snapshot) acceptCodexOperationSnapshot(snapshot, true);
  } catch (e) {
    setMsg("无法恢复 Codex 登录 operation 状态：" + e, "err");
  }
}

function syncCodexControls() {
  if (!els.codexEnabled) return;
  const enabled = !!configState.experimental_codex_enabled;
  const authActive = codexOperationActive() || codexLoginStarting;
  const locked = busy || activationInFlight || codexNetworkSaving;
  els.codexEnabled.checked = enabled;
  els.codexEnabled.disabled = locked || authActive;
  els.codexStatusBtn.disabled = locked || authActive;
  els.codexLoginBtn.disabled = locked || authActive || !enabled;
  els.codexCancelBtn.disabled = locked || !authActive || !codexAuthOperation;
  els.codexLogoutBtn.disabled = locked || authActive || !(codexAuthState && codexAuthState.authenticated);
  if (els.codexProfileRepairBox) els.codexProfileRepairBox.hidden = !enabled || !codexProfileRepairNeeded;
  if (els.codexRepairProfileBtn) {
    els.codexRepairProfileBtn.disabled = locked || authActive || !codexProfileRepairNeeded || !(codexAuthState && codexAuthState.authenticated);
  }
  if (els.codexNetworkMode) els.codexNetworkMode.disabled = locked || authActive;
  if (els.codexProxyUrl) els.codexProxyUrl.disabled = locked || authActive || els.codexNetworkMode.value !== "custom";
  if (els.codexNetworkSaveBtn) els.codexNetworkSaveBtn.disabled = locked || authActive;
  const codexCount = (configState.profiles || []).filter((profile) => isCodexSource(profile)).length;
  els.codexDowngradeBox.hidden = codexCount === 0;
  els.codexDowngradeBtn.disabled = locked || authActive || codexCount === 0;
}

function renderCodexNetwork() {
  if (!els.codexNetworkMode) return;
  const settings = configState.codex_network || { mode: "auto", proxy_url: "" };
  const resolved = configState.codex_network_resolved || { source: "direct", proxy_scheme: null };
  els.codexNetworkMode.value = settings.mode === "custom" ? "custom" : "auto";
  els.codexProxyUrl.value = settings.proxy_url || "";
  const sourceLabels = {
    direct: "直接 socket，可能由系统 TUN 接管",
    env_https: "来自 HTTPS_PROXY / https_proxy",
    env_all: "来自 ALL_PROXY / all_proxy",
    custom: "CSSwitch 显式代理",
    invalid: "代理配置非法",
  };
  const scheme = resolved.proxy_scheme ? " · " + resolved.proxy_scheme : "";
  els.codexNetworkResolved.textContent = "当前路线：" + (sourceLabels[resolved.source] || "未知") + scheme + "。";
  syncCodexControls();
}

function codexNetworkModeChanged() {
  if (els.codexNetworkMode.value !== "custom") els.codexProxyUrl.value = "";
  syncCodexControls();
}

async function saveCodexNetwork() {
  if (codexNetworkSaving || codexOperationActive()) return;
  const settings = {
    mode: els.codexNetworkMode.value === "custom" ? "custom" : "auto",
    proxy_url: els.codexNetworkMode.value === "custom" ? els.codexProxyUrl.value.trim() : "",
  };
  codexNetworkSaving = true;
  syncCodexControls();
  setMsg("正在校验 Codex 网络路线并停止受管 Codex 链路；保存后不会自动重启…");
  try {
    const result = await call("set_codex_network", { settings });
    if (!result || result.mode !== settings.mode || result.restarted !== false) {
      throw new Error("Codex 网络设置响应不一致。");
    }
    configState.codex_network = settings;
    configState.codex_network_resolved = { source: result.source, proxy_scheme: result.proxy_scheme ?? null };
    renderCodexNetwork();
    setMsg("Codex 网络路线已保存；受管 Codex Science 与 Gateway 保持停止，其他 provider 未受影响。", "ok");
  } catch (e) {
    setMsg("Codex 网络路线未更改：" + runtimeCommandErrorText(e), "err");
  } finally {
    codexNetworkSaving = false;
    syncCodexControls();
  }
}

async function refreshCodexAuthStatus(options) {
  const opts = options || {};
  if (!opts.quiet) setMsg("正在检查 CSSwitch 自有 Codex 登录状态…");
  try {
    codexAuthState = unwrapCodexAuthEnvelope(await call("codex_auth_status"), "status");
    refreshCodexProfileRepairState();
    renderCodexAuthState();
    if (!opts.quiet) {
      const incomplete = !codexAuthState.authenticated && !["state_missing", "state_uncommitted"].includes(codexAuthState.reason);
      setMsg(codexAuthState.authenticated
        ? "CSSwitch Codex 已登录；没有读取或修改原生 Codex 登录。"
        : incomplete
        ? "CSSwitch Codex 本地认证记录不完整；不会自动修复、退出或重新登录。"
        : "CSSwitch Codex 尚未登录。请先启用实验入口，再点“登录 Codex”。",
      codexAuthState.authenticated ? "ok" : "err");
    }
    return !!codexAuthState.authenticated;
  } catch (e) {
    codexAuthState = null;
    codexProfileRepairNeeded = false;
    renderCodexAuthState();
    if (!opts.quiet) setMsg("检查 CSSwitch Codex 登录状态失败：" + runtimeCommandErrorText(e), "err");
    return false;
  }
}

async function checkCodexAuth() {
  setBusy(true, { kind: "codexAuth" });
  await refreshCodexAuthStatus();
  setBusy(false);
}

async function toggleCodexFeature() {
  const desired = !!els.codexEnabled.checked;
  const previous = !!configState.experimental_codex_enabled;
  let backendChanged = false;
  setBusy(true, { kind: "codexFeature" });
  els.codexEnabled.checked = desired;
  setMsg(desired ? "正在启用 Codex 实验入口…" : "正在安全停止 Codex 链路并关闭实验入口…");
  try {
    const result = await call("set_experimental_codex_enabled", { enabled: desired });
    backendChanged = true;
    if (!result || result.experimental_codex_enabled !== desired) {
      throw new Error("后端返回的 Codex 实验开关状态不一致。");
    }
    configState.experimental_codex_enabled = desired;
    if (!desired) configState.templates = (configState.templates || []).filter((t) => !isCodexSource(t));
    renderList();
    syncCodexControls();
    await loadConfig({ throwOnError: true });
    setMsg(desired
      ? "Codex 实验入口已启用。下一步请在“设置 > Codex 账号与连接”登录 CSSwitch Codex。"
      : "Codex 实验入口已关闭；CSSwitch 自有 OAuth 凭据仍保留，可在此处检查或退出。", "ok");
  } catch (e) {
    if (backendChanged) {
      configState.experimental_codex_enabled = desired;
      if (!desired) configState.templates = (configState.templates || []).filter((t) => !isCodexSource(t));
      renderList();
      syncCodexControls();
      setMsg("Codex 实验入口已在后端" + (desired ? "启用" : "关闭") + "，但刷新完整配置失败：" + runtimeCommandErrorText(e) + " 请重新打开 CSSwitch 确认其余界面。", "err");
    } else {
      configState.experimental_codex_enabled = previous;
      els.codexEnabled.checked = previous;
      setMsg("Codex 实验入口未更改：" + runtimeCommandErrorText(e), "err");
    }
  } finally {
    setBusy(false);
  }
}

async function startCodexLogin() {
  if (!configState.experimental_codex_enabled) {
    setMsg("请先启用 Codex 实验入口。", "err");
    return;
  }
  if (codexOperationActive() || codexLoginStarting) return;
  codexProfileRepairNeeded = false;
  codexLoginStarting = true;
  syncCodexControls();
  setMsg("正在打开浏览器登录；最多等待 5 分钟。请完成授权后回到这里…");
  try {
    const snapshot = await call("codex_auth_start");
    acceptCodexOperationSnapshot(snapshot, true);
    if (PREVIEW) previewCodexLogin();
  } catch (e) {
    codexLoginStarting = false;
    syncCodexControls();
    setMsg("CSSwitch Codex 登录失败：" + runtimeCommandErrorText(e), "err");
  }
}

function previewCodexLogin() {
  const base = { ...mockCodexOperation };
  setTimeout(() => {
    if (!mockCodexOperation || mockCodexOperation.state === "cancelled") return;
    mockCodexOperation = { ...base, sequence: 2, state: "waiting", updated_at_ms: Date.now() };
    acceptCodexOperationSnapshot(mockCodexOperation, false);
  }, 250);
  setTimeout(() => {
    if (!mockCodexOperation || mockCodexOperation.state === "cancelled") return;
    mockCodexAuth = { authenticated: true, account_hash: "0123456789abcdef0123456789abcdef", expiry_state: "valid", expires_at: 1893456000, auth_epoch: "fedcba9876543210fedcba9876543210", auth_generation: mockCodexAuth.auth_generation + 1, reason: "ready" };
    mockEnsureCodexProfile();
    mockCodexOperation = { ...base, sequence: 3, state: "succeeded", updated_at_ms: Date.now() };
    acceptCodexOperationSnapshot(mockCodexOperation, false);
  }, 900);
}

async function cancelCodexLogin() {
  if (!codexOperationActive() || !codexAuthOperation) return;
  els.codexCancelBtn.disabled = true;
  try {
    const result = await call("codex_auth_cancel", { operationId: codexAuthOperation.operation_id });
    if (!result || !["accepted", "commit_in_progress", "already_terminal"].includes(result.disposition)) {
      throw new Error("取消响应协议不匹配。");
    }
    setMsg(result.disposition === "commit_in_progress"
      ? "本地认证提交已经开始；将由最终成功或失败决定状态。"
      : result.disposition === "already_terminal" ? "该登录 operation 已结束。" : "取消请求已被 sidecar 接受，正在回收…");
    if (PREVIEW && mockCodexOperation) acceptCodexOperationSnapshot(mockCodexOperation, false);
  } catch (e) {
    setMsg("取消 Codex 登录失败：" + e, "err");
  } finally {
    syncCodexControls();
  }
}

async function repairCodexProfile() {
  if (busy || codexOperationActive() || !codexProfileRepairNeeded) return;
  setBusy(true, { kind: "codexProfileRepair" });
  setMsg("正在用已保存的授权补建 Codex 配置；不会重新登录或切换当前 provider…");
  try {
    const result = await call("codex_ensure_profile");
    if (!result || !["created", "existing"].includes(result.disposition) ||
        typeof result.profile_id !== "string" || !result.profile_id || result.profile_id.length > 128) {
      throw new Error("补建配置响应协议不匹配。");
    }
    await loadConfig({ throwOnError: true });
    if (!hasCodexProfile()) throw new Error("补建后未能在配置列表中确认 Codex。");
    codexProfileRepairNeeded = false;
    syncCodexControls();
    setMsg(result.disposition === "created"
      ? "Codex 配置已补建。下一步可在“模型连接 > 配置方案”中设为当前。"
      : "Codex 配置已存在并确认就绪。下一步可设为当前。", "ok");
  } catch (e) {
    refreshCodexProfileRepairState();
    setMsg("补建 Codex 配置失败：" + runtimeCommandErrorText(e) + " 已保存的授权不会因此删除，可重试。", "err");
  } finally {
    setBusy(false);
  }
}

function logoutCodex() {
  confirmAction("codex-logout", "将退出 CSSwitch 自有 Codex 登录；不会退出原生 Codex", doLogoutCodex);
}

async function doLogoutCodex() {
  setBusy(true, { kind: "codexAuth" });
  setMsg("正在安全停止 Codex 链路并退出 CSSwitch Codex…");
  try {
    const response = await call("codex_auth_logout");
    codexAuthState = unwrapCodexAuthEnvelope(response, "logout");
    codexProfileRepairNeeded = false;
    renderCodexAuthState();
    const revokeSkipped = response && response.warning && response.warning.code === "revoke_skipped" && response.warning.reason === "proxy_config_invalid";
    setMsg(revokeSkipped
      ? "已清除 CSSwitch Codex 本地凭据；因代理配置非法，远端 revoke 已安全跳过。原生 Codex 登录未受影响。"
      : "已退出 CSSwitch Codex；原生 Codex 登录未被读取或修改。", "ok");
  } catch (e) {
    setMsg("退出 CSSwitch Codex 失败：" + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
  }
}

async function requestCodexDowngrade() {
  if (busy || activationInFlight) return;
  setBusy(true, { kind: "codexDowngradePreview" });
  setMsg("正在生成 Codex 配置降级预览；不会读取本地 OAuth 内容…");
  try {
    const preview = await call("codex_downgrade_preview");
    const profiles = preview && Array.isArray(preview.profiles) ? preview.profiles : [];
    if (preview.schema_version !== 1 || preview.action !== "export_then_remove_all" || !preview.credentials_unchanged || profiles.length < 1 || !preview.preview_fingerprint) {
      throw new Error("后端没有返回可执行的完整 Codex 降级预览。");
    }
    const ids = profiles.map((profile) => String(profile.id || ""));
    if (ids.some((id) => !id) || new Set(ids).size !== ids.length) {
      throw new Error("Codex 降级预览包含无效或重复 profile ID。");
    }
    const names = profiles.map((profile) => String(profile.name || profile.id)).join("、");
    setBusy(false);
    confirmAction(
      "codex-downgrade:" + ids.join(","),
      "将导出并移除全部 " + profiles.length + " 个 Codex 配置（" + names + "），原子降为 v2 后立即退出；CSSwitch 本地 OAuth 保留",
      () => doCodexDowngrade(ids, String(preview.preview_fingerprint))
    );
  } catch (e) {
    setBusy(false);
    setMsg("无法准备 Codex 配置降级：" + e, "err");
  }
}

async function doCodexDowngrade(expectedProfileIds, expectedPreviewFingerprint) {
  let downgradeCommitted = false;
  setBusy(true, { kind: "codexDowngrade" });
  setMsg("请选择 Codex profile 元数据导出文件。取消选择不会修改配置…");
  if (statusTimer) {
    clearInterval(statusTimer);
    statusTimer = null;
  }
  try {
    const result = await call("codex_downgrade_export_all", { expectedProfileIds, expectedPreviewFingerprint });
    if (result && result.status === "CANCELLED") {
      setMsg("已取消导出与降级；配置和本地认证文件均未修改。");
      if (!PREVIEW) statusTimer = setInterval(refreshStatus, 2500);
      return;
    }
    if (!result || result.schema_version !== 1 || result.status !== "DOWNGRADED_EXIT_REQUIRED" || !result.exported || !result.credentials_unchanged || !result.app_exit_required) {
      throw new Error("后端降级结果协议不匹配；请勿继续操作，先退出应用并检查备份。");
    }
    downgradeCommitted = true;
    setMsg("Codex 元数据已导出、配置已降为 v2；CSSwitch 本地 OAuth 保留。后端正在终态退出，请安装旧版后再打开。", "ok");
  } catch (e) {
    const terminalFailure = String(e).includes("进程已锁存并强制退出");
    if (terminalFailure) downgradeCommitted = true;
    setMsg(downgradeCommitted
      ? (terminalFailure
          ? "v2 发布后的持久化或回滚状态不确定，后端已锁存所有配置访问并强制退出：" + e + " 本地 OAuth 未被读取或删除。"
          : "配置已安全降为 v2，但应用自动退出失败：" + e + " 请立即手动退出，不要继续操作或重新读取配置；本地 OAuth 保留。")
      : "Codex 配置降级未完成：" + e + " 如果已选择导出文件，它可能已安全落盘；当前配置仍应保持 v3。", "err");
    if (!downgradeCommitted && !PREVIEW && !statusTimer) statusTimer = setInterval(refreshStatus, 2500);
  } finally {
    if (!downgradeCommitted) setBusy(false);
  }
}

// ── 加载配置 + 渲染列表 ──
async function loadConfig(options) {
  const opts = options || {};
  try {
    const cfg = await call("get_config");
    configState.profiles = cfg.profiles || [];
    configState.templates = cfg.templates || [];
    configState.active_id = cfg.active_id || "";
    configState.proxy_port = cfg.proxy_port ?? 18991;
    configState.sandbox_port = cfg.sandbox_port ?? 8990;
    configState.reuse_system_ssh = !!cfg.reuse_system_ssh;
    configState.experimental_codex_enabled = !!cfg.experimental_codex_enabled;
    configState.codex_network = cfg.codex_network || { mode: "auto", proxy_url: "" };
    configState.codex_network_resolved = cfg.codex_network_resolved || { source: "direct", proxy_scheme: null };
    els.proxyPort.value = configState.proxy_port;
    els.sandboxPort.value = configState.sandbox_port;
    els.reuseSystemSsh.checked = configState.reuse_system_ssh;
    refreshCodexProfileRepairState();
    renderCodexAuthState();
    renderCodexNetwork();
    applyMode(cfg.mode === "official" ? "official" : "proxy");
    renderList();
    showView("list");
    // 一次性迁移提示（#9 甲）：后端 get_config 读后已清盘，只会出现一次。
    if (cfg.pending_notice) setMsg(cfg.pending_notice, "ok");
  } catch (e) {
    setMsg("读取配置失败：" + e, "err");
    if (opts.throwOnError) throw e;
    return false;
  }
  return true;
}

// 列表优先展示默认 route；静态目录未配置时明确提示，Codex 仍由 Science 选择。
function modelSummary(p) {
  const route = (p.model_catalog || []).find((item) =>
    item.selector_id === p.default_model_route_id || item.upstream_model === p.model
  );
  if (route) return route.display_name || route.upstream_model;
  if (p.model) return p.model;
  const cap = modelCapability(p.capabilities ? p : tplById(p.template_id));
  if (cap === CAP.DYNAMIC) return "在 Science 中选择";
  return "目录未配置";
}

function profileModelOptions(p) {
  const template = tplById(p.template_id);
  const catalogModels = (p.model_catalog || []).map((route) => route.upstream_model);
  const available = catalogModels.length
    ? catalogModels
    : (p.model_options || []).length
    ? p.model_options
    : ((template && template.builtin_models) || []);
  const candidates = [p.model, ...available]
    .filter(Boolean);
  return [...new Set(candidates)];
}

function profileModelControl(p) {
  const model = p.model || "";
  if (!PROFILE_INTERACTIVE_PREVIEW) {
    const count = Number.isFinite(p.model_count) ? p.model_count : (p.model_catalog || []).length;
    const route = (p.model_catalog || []).find((item) =>
      item.selector_id === p.default_model_route_id || item.upstream_model === p.model
    );
    const upstream = route?.upstream_model || p.model || "";
    return `<strong class="profile-model-text">${escapeHtml(modelSummary(p))}</strong><span class="profile-model-meta">${count ? `${count} 个模型` : "动态目录"}${upstream ? ` · ${escapeHtml(upstream)}` : ""}</span>`;
  }
  const options = profileModelOptions(p);
  return `<select class="profile-model-select" data-profile-model="${escapeHtml(p.id)}" aria-label="${escapeHtml(p.name)} 的模型">
    ${options.map((value) => `<option value="${escapeHtml(value)}"${value === model ? " selected" : ""}>${escapeHtml(value)}</option>`).join("")}
  </select>`;
}

function renderList() {
  const list = els.profileList;
  const ps = configState.profiles || [];
  renderCurrentSummary();
  if (!ps.length) {
    list.innerHTML = '<div class="empty">还没有配置。使用“新建配置”添加一条第三方来源。</div>';
    return;
  }
  const header = `<div class="profile-list-head" aria-hidden="true"><span>配置</span><span>默认模型 / 目录</span><span>凭据</span><span>操作</span></div>`;
  list.innerHTML = header + ps.map((p) => {
    const active = p.id === configState.active_id;
    const codex = isCodexSource(p);
    const codexEnabled = !!configState.experimental_codex_enabled;
    const hasKey = typeof p.has_key === "boolean" ? p.has_key : !!p.key;
    const credential = codex ? "CSSwitch OAuth" : (hasKey ? escapeHtml(p.key_masked || p.key || "已保存") : "未填写");
    const editAction = codex
      ? '<button class="abtn" data-act="editconn" data-permanently-disabled="true" disabled aria-disabled="true" title="Codex 配置由 OAuth 与 Science 管理，无需编辑连接">编辑</button>'
      : '<button class="abtn" data-act="editconn">编辑</button>';
    return (
      '<div class="prow' + (active ? " pactive" : "") + '" data-id="' + escapeHtml(p.id) + '">' +
        '<div class="profile-identity">' +
          '<div class="prow-top">' +
            modelIconMarkup(p) +
            '<span class="pname">' + escapeHtml(p.name) + "</span>" +
            (active ? '<span class="badge on">当前</span>' : "") +
            (codex && !codexEnabled ? '<span class="badge warn">入口已关闭</span>' : "") +
          "</div>" +
        "</div>" +
        '<div class="profile-model-cell">' + profileModelControl(p) + "</div>" +
        '<div class="profile-key-cell"><strong>' + credential + "</strong></div>" +
        '<div class="prow-acts">' +
          (active || (codex && !codexEnabled) ? "" : '<button class="abtn prim" data-act="activate">设为当前</button>') +
          editAction +
          '<details class="profile-more"><summary>更多</summary><div class="profile-menu">' +
            '<button class="abtn" data-act="editmeta">名称与备注</button>' +
            (codex ? "" : '<button class="abtn" data-act="clearkey">清除 Key</button>') +
            '<button class="abtn danger" data-act="delete">删除配置</button>' +
          "</div></details>" +
        "</div>" +
      "</div>"
    );
  }).join("");
  syncProfileBusyState();
}

// ── 模式（第三方 / 官方）──
function applyMode(m) {
  const nextMode = m === "official" ? "official" : "proxy";
  if (nextMode !== mode && nextMode === "official") officialRuntimeState = "gray";
  mode = nextMode;
  els.panel.classList.toggle("mode-official", mode === "official");
  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.classList.toggle("active", b.dataset.mode === mode)
  );
  els.oneClickBtn.textContent =
    mode === "official" ? "打开官方 Claude Science" : "一键开始";
  renderCurrentSummary();
}

async function switchMode(m) {
  if (m === mode) return;
  if (busy) return; // 忙碌中不切模式（防与「一键开始」竞态；按钮亦已禁用，此为双保险）。修 P1-b
  skillPage?.invalidate();
  setBusy(true, { kind: "switchMode", id: m });
  startSwitchModeFeedback(m);
  try {
    await call("set_mode", { mode: m });
  } catch (e) {
    setMsg("切换模式失败：" + e, "err");
    setBusy(false);
    await skillPage?.refreshIfLoaded();
    return;
  }
  applyMode(m);
  setBusy(false);
  showView("list");
  setMsg(
    mode === "official"
      ? "已切到官方模式：第三方代理/沙箱已停，点上方按钮打开你真实的 Claude Science。"
      : "已切到第三方模式：选一条配置「设为当前」后点「一键开始」。"
  );
  await refreshStatus();
  await skillPage?.refreshIfLoaded();
}

async function openOfficial() {
  setBusy(true);
  setMsg("正在打开官方 Claude Science…");
  try {
    await call("open_official");
    officialRuntimeState = "gray";
    els.brandDot.className = "dot gray";
    setMsg("已发起打开官方 Claude Science；运行、登录与订阅状态由 Science 管理。", "ok");
  } catch (e) {
    officialRuntimeState = "gray";
    els.brandDot.className = "dot gray";
    setMsg("打开失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

// hero 按钮按当前模式分派。
async function heroClick() {
  if (mode === "official") await openOfficial();
  else await oneClick();
}

// ── 运行设置（端口 + 系统 SSH 配置授权；不含 provider/连接）──
async function persistRuntimeSettings() {
  if (busy) return; // 忙碌中不改端口（防与在途操作竞态；输入亦已禁用，此为双保险）。修 P1-c
  skillPage?.invalidate();
  const p = parseInt(els.proxyPort.value, 10) || 18991;
  const s = parseInt(els.sandboxPort.value, 10) || 8990;
  const reuseSystemSsh = !!els.reuseSystemSsh.checked;
  const portsChanged = p !== configState.proxy_port || s !== configState.sandbox_port;
  const sshChanged = reuseSystemSsh !== configState.reuse_system_ssh;
  const changed = portsChanged || sshChanged;
  // 本次端口提交全程置忙：仅靠开头的 `if (busy) return` 只挡「已在忙时进入」，挡不住本函数在途
  // 时其它操作（切模式/一键/连接编辑）启动。置忙 + 禁用控件才能保证操作顺序符合用户预期。修 GPT 三轮 P2
  setBusy(true, { kind: "ports" });
  startPortSaveFeedback(changed);
  try {
    await call("set_settings", { cfg: { proxy_port: p, sandbox_port: s, reuse_system_ssh: reuseSystemSsh } });
    configState.proxy_port = p;
    configState.sandbox_port = s;
    configState.reuse_system_ssh = reuseSystemSsh;
    // 后端在端口变化时会拆掉旧代理/沙箱（否则会复用指向旧端口的死链路），如实告知需重开。修 P1-c
    if (changed) {
      setMsg(sshChanged
        ? "SSH 授权设置已保存。正在运行的代理/沙箱已重置，请重新「一键开始」。"
        : "端口已保存。改端口会重置正在运行的代理/沙箱，请重新「一键开始」。", "ok");
      await refreshStatus();
      await skillPage?.refreshIfLoaded();
    } else {
      setMsg("端口未变化。", "ok");
    }
  } catch (e) {
    // 出错＝端口未落盘（校验不过 / 停旧沙箱失败）：把输入框还原成实际生效值，避免显示未保存的数字。
    els.proxyPort.value = configState.proxy_port;
    els.sandboxPort.value = configState.sandbox_port;
    els.reuseSystemSsh.checked = configState.reuse_system_ssh;
    setMsg(String(e), "err");
  } finally {
    setBusy(false);
    await skillPage?.refreshIfLoaded();
  }
}

// ── 模型候选渲染：填进 input 关联的 <datalist>，并按 supports_tools 标注。──
// input 的值由调用方另设，用户可自由编辑精确 upstream ID。
function renderModelOptions(sel, models, sourceLabel) {
  const listId = sel.getAttribute("list");
  const dl = listId && document.getElementById(listId);
  if (!dl) return;
  dl.innerHTML = "";
  for (const m of models || []) {
    const o = document.createElement("option");
    o.value = m.id;
    const tag = m.supports_tools === true ? " ·工具✓" : m.supports_tools === false ? " ·无工具" : "";
    const src = sourceLabel ? " [" + sourceLabel + "]" : "";
    o.label = m.id + tag + src;
    dl.appendChild(o);
  }
}

const MODEL_SOURCE_LABELS = {
  live: "实时", "fresh-cache": "新鲜缓存", "revalidated-cache": "已重新验证缓存",
  "stale-cache": "过期缓存", builtin: "内置", unsupported: "内置", protocol: "协议不兼容",
};

function modelSourceLabel(source) {
  return MODEL_SOURCE_LABELS[source] || "未验证";
}

function editorState(kind) {
  const wizard = kind === "wizard";
  return {
    model: wizard ? els.wizModel : els.connModel,
    staticRoot: wizard ? els.wizStaticCatalog : els.connStaticCatalog,
    warning: wizard ? els.wizCatalogWarning : els.connCatalogWarning,
    quality: wizard ? els.wizRoleQuality : els.connRoleQuality,
    fast: wizard ? els.wizRoleFast : els.connRoleFast,
    fable: wizard ? els.wizRoleFable : els.connRoleFable,
    get catalog() { return wizard ? wizardCatalog : connectionCatalog; },
    set catalog(value) { if (wizard) wizardCatalog = value; else connectionCatalog = value; },
    get refs() { return wizard ? wizardRoleRefs : connectionRoleRefs; },
    set refs(value) { if (wizard) wizardRoleRefs = value; else connectionRoleRefs = value; },
  };
}

function selectedReference(route) {
  return route.selector_id || route.upstream_model;
}

function roleReferences(bindings, fallback, defaultReference = fallback) {
  const roles = bindings || {};
  return {
    default: defaultReference || fallback || "",
    balanced: roles.sonnet || defaultReference || fallback || "",
    quality: roles.opus || roles.fable || fallback || "",
    fast: roles.haiku || fallback || "",
    fable: roles.fable || roles.opus || fallback || "",
  };
}

function initializeCatalogEditor(kind, routes, defaultRef, bindings, { dynamic = false } = {}) {
  const editor = editorState(kind);
  editor.staticRoot.hidden = dynamic;
  if (dynamic) {
    editor.catalog = [];
    editor.model.value = "";
    editor.quality.value = "";
    editor.fast.value = "";
    editor.fable.value = "";
    return;
  }
  editor.catalog = mergeCatalogCandidates([], (routes || []).map((route) => ({ ...route, enabled: true })), { enableNew: true });
  const fallback = defaultRef || selectedReference(editor.catalog[0] || {});
  editor.refs = roleReferences(bindings, fallback, defaultRef || fallback);
  const fields = projectSimpleModelFields(editor.catalog, editor.refs.default, bindings);
  editor.model.value = fields.default_model;
  editor.quality.value = fields.quality_model;
  editor.fast.value = fields.fast_model;
  editor.fable.value = fields.fable_model;
  renderModelOptions(editor.model, editor.catalog.map((item) => ({
    id: item.upstream_model,
    supports_tools: item.supports_tools,
  })), "推荐");
  const preservesLegacyBalanced = kind === "connection" && editor.refs.balanced !== editor.refs.default;
  editor.warning.textContent = preservesLegacyBalanced
    ? "这个旧配置曾单独保存均衡映射：不修改默认模型时会原样保留；修改默认模型后，均衡会随默认更新。"
    : "可以选择推荐模型，也可以直接填写供应商或中转站提供的精确模型 ID。";
}

function catalogSubmission(kind) {
  const editor = editorState(kind);
  return buildSimpleModelSubmission({
    default_model: editor.model.value,
    quality_model: editor.quality.value,
    fast_model: editor.fast.value,
    fable_model: editor.fable.value,
  }, {
    existing_routes: editor.catalog,
    existing_references: editor.refs,
    preserve_existing_sonnet: kind === "connection",
  });
}

function catalogRolesChanged(kind) {
  if (kind === "wizard") refreshWizGate(); else refreshConnGate();
}

function compactAge(seconds) {
  const n = Math.max(0, Number(seconds) || 0);
  if (n < 60) return Math.round(n) + " 秒";
  if (n < 3600) return Math.round(n / 60) + " 分钟";
  return (n / 3600).toFixed(n < 36000 ? 1 : 0) + " 小时";
}

function isSafeCodexDisplayName(value) {
  return typeof value === "string" && value.length > 0 &&
    new TextEncoder().encode(value).length <= 512 &&
    !/[\u0000-\u001f\u007f-\u009f]/.test(value);
}

function codexModelLabel(model) {
  const displayName = model && model.display_name;
  if (isSafeCodexDisplayName(displayName)) return displayName;
  return "显示名不可用 · " + String((model && model.id) || "");
}

function renderCodexCatalog(meta, list, r) {
  const models = (r && r.models) || [];
  const source = (r && r.source) || "unknown";
  const sourceText = modelSourceLabel(source);
  const age = Number(r && r.age_seconds) || 0;
  meta.textContent = sourceText + " · " + models.length + " 个账号模型" + (age ? " · 缓存年龄 " + compactAge(age) : "");
  list.innerHTML = models.length
    ? models.map((m) => '<div class="codex-model-item">' + escapeHtml(codexModelLabel(m)) + "</div>").join("")
    : '<div class="codex-model-empty">账号目录当前没有可展示模型。</div>';
  if (source === "stale-cache" || (r && r.stale)) {
    setMsg("官方目录暂时不可达，当前展示过期缓存（年龄 " + compactAge(age) + "）。可用于识别已有模型，但请稍后刷新确认。", "err");
  } else if (r && r.error_kind === "protocol") {
    setMsg("Codex 模型目录响应与当前 CSSwitch 兼容基线不一致；这不是网络繁忙。没有模型被伪造或写入配置。", "err");
  } else if (r && r.error_kind === "network") {
    setMsg("官方 Codex 模型目录当前不可达，且没有可用缓存。没有模型被伪造或写入配置，请稍后重试。", "err");
  } else {
    setMsg("已读取 " + models.length + " 个 Codex 账号模型（" + sourceText + "）。模型不会写入配置，请在 Science 的 More models 中选择。", "ok");
  }
}

// ── C2：新建向导 ──
function openWizard() {
  hideSkip();
  renderTemplateChips();
  const first = (configState.templates || [])[0];
  selectWizTemplate(first ? first.id : "");
  showView("wizard");
  setMsg("选择来源，按提示填写连接信息后创建。");
}

function renderTemplateChips() {
  els.wizTemplateChips.innerHTML = (configState.templates || []).map((t) => {
    const dot = t.icon_color ? ' style="background:' + escapeHtml(t.icon_color) + '"' : "";
    const cat = CAT_LABELS[t.category] || t.category || "";
    return (
      '<button type="button" class="chip" aria-pressed="false" data-tid="' + escapeHtml(t.id) + '">' +
        '<span class="chip-dot"' + dot + "></span>" +
        '<span class="chip-name">' + escapeHtml(t.name) + "</span>" +
        '<span class="chip-cat">' + escapeHtml(cat) + "</span>" +
      "</button>"
    );
  }).join("");
}

function selectWizTemplate(id) {
  els.wizTemplate.value = id;
  els.wizTemplateChips.querySelectorAll(".chip").forEach((c) => {
    const on = c.getAttribute("data-tid") === id;
    c.classList.toggle("sel", on);
    c.setAttribute("aria-pressed", on ? "true" : "false");
  });
  onWizTemplate();
}

function onWizTemplate() {
  const t = tplById(els.wizTemplate.value);
  if (!t) return;
  const codex = isCodexSource(t);
  els.wizName.value = t.name;
  // 把「新建不自动生效」放进顶部常驻提示（默认窗口下反馈区首屏可能在折叠线下，见 #6）。
  els.wizTplHint.textContent = sourceHint(t) + " 新建后需在列表点「设为当前」才生效。";
  els.wizBaseGroup.hidden = codex;
  els.wizKeyGroup.hidden = codex;
  els.wizCodexCatalog.hidden = !codex;
  els.wizModelLabel.textContent = codex ? "账号模型目录" : "模型配置";
  els.wizCodexCatalogMeta.textContent = "尚未读取账号模型目录。";
  els.wizCodexCatalogList.innerHTML = "";
  if (codex) {
    els.wizBase.value = "";
    els.wizModel.value = "";
  } else if (t.base_url_editable) {
    // 预设：预填官方默认地址（仍可改到套餐 / 区域端点）；真·自定义：留空 + 占位提示。
    els.wizBase.value = t.base_url || "";
    els.wizBase.readOnly = false;
    els.wizBase.placeholder = t.api_format === "openai_chat" || t.api_format === "openai_responses"
      ? "https://open.bigmodel.cn/api/paas/v4"
      : "https://your-relay/claude";
    els.wizBaseHint.textContent = t.base_url
      ? "官方默认地址，可改到 token 套餐 / 区域端点（如小米 token plan）。"
      : (t.api_format === "openai_chat"
        ? "OpenAI 兼容 base root，代理自动补 /chat/completions 与 /models。"
        : t.api_format === "openai_responses"
        ? "OpenAI 兼容 base root，代理自动补 /responses 与 /models。"
          : "自定义端点根地址（自动补 /v1/messages）。");
  } else {
    els.wizBase.value = t.base_url;
    els.wizBase.readOnly = true;
    els.wizBaseHint.textContent = "模板地址已填好（只读）。";
  }
  applyModelCapability(t, {
    info: els.wizModelInfo, sel: els.wizModel, hint: els.wizModelHint, fetchBtn: els.wizFetchBtn,
  }, "");
  const recommended = (t.recommended_catalog || (t.builtin_models || []).map((id) => ({
    selector_id: "", display_name: id, upstream_model: id,
    supports_tools: null, origin: "preset", availability: "unknown",
  }))).map((route) => ({ origin: "preset", availability: "unknown", ...route }));
  const defaultReference = t.recommended_default_model_route_id || recommended[0]?.selector_id || recommended[0]?.upstream_model || "";
  initializeCatalogEditor("wizard", recommended, defaultReference, t.recommended_role_bindings, { dynamic: codex });
  refreshWizGate();
  setMsg(codex
    ? "Codex 无需填写 Key 或地址。正常浏览器登录后会自动创建一条配置；此向导仅用于手工添加额外配置。"
    : "选择来源，按提示填写连接信息后创建。");
}

function refreshWizGate() {
  const t = tplById(els.wizTemplate ? els.wizTemplate.value : "");
  let invalidCatalog = false;
  if (t && !isCodexSource(t)) {
    try { catalogSubmission("wizard"); } catch (_) { invalidCatalog = true; }
  }
  els.wizSaveBtn.disabled = busy || !t || invalidCatalog;
}

function openaiCustomAnthropicBaseMessage(t, base) {
  if (t && (t.id === "custom-openai" || t.id === "custom-openai-responses") && (base || "").trim().toLowerCase().includes("/anthropic")) {
    return "这个地址看起来是 Anthropic 兼容端点。请改选「自定义 Anthropic」，或填写 OpenAI 兼容 base root（如 https://api.moonshot.cn/v1）。";
  }
  return "";
}

async function wizFetch() {
  const t = tplById(els.wizTemplate.value);
  if (!t || !isCodexSource(t)) return;
  setBusy(true, { kind: "fetchModels", id: "wizard" });
  startFetchModelsFeedback("wizard");
  try {
    const r = await call("fetch_models", { req: { template_id: t.id, base_url: "", key: "" } });
    renderCodexCatalog(els.wizCodexCatalogMeta, els.wizCodexCatalogList, r);
  } catch (e) {
    setMsg("获取模型失败：" + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
    refreshWizGate();
  }
}

async function wizSave() {
  const t = tplById(els.wizTemplate.value);
  if (!t) { setMsg("模板未加载。", "err"); return; }
  const name = els.wizName.value.trim() || t.name;
  const codex = isCodexSource(t);
  let catalogPayload = null;
  if (!codex) {
    try { catalogPayload = catalogSubmission("wizard"); }
    catch (error) { setMsg(String(error.message || error), "err"); return; }
  }
  const args = { templateId: t.id, name, key: codex ? "" : els.wizKey.value.trim() };
  if (catalogPayload) {
    args.modelCatalog = catalogPayload.model_catalog;
    args.defaultModelRouteId = catalogPayload.default_model_route_id;
    args.roleBindings = catalogPayload.role_bindings;
  }
  if (t.base_url_editable) {
    const base = els.wizBase.value.trim();
    if (baseUrlRequired(t) && !base) { setMsg("请先填写 base_url。", "err"); return; }
    const baseErr = openaiCustomAnthropicBaseMessage(t, base);
    if (baseErr) { setMsg(baseErr, "err"); return; }
    args.baseUrl = base;
  }
  setBusy(true);
  setMsg("创建中…");
  try {
    await call("create_profile", args);
    els.wizKey.value = "";
    await loadConfig();
    setMsg(codex
      ? "已创建「" + name + "」。先设为当前；一键开始后请在 Science 的 More models 选择 Codex / …。"
      : "已创建「" + name + "」。可在列表点「设为当前」启用。", "ok");
  } catch (e) {
    setMsg("创建失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

// ── C3：连接编辑（base_url/model/key）+ 清 key ──
function currentConn() {
  const id = els.connSec.dataset.id;
  return (configState.profiles || []).find((x) => x.id === id) || null;
}

function openConn(id) {
  const p = (configState.profiles || []).find((x) => x.id === id);
  if (!p) return;
  const t = tplById(p.template_id);
  const capSrc = profileCapabilitySource(p, t);
  const codex = isCodexSource(capSrc || p);
  if (codex && !configState.experimental_codex_enabled) {
    setMsg("Codex 实验入口已关闭；仍可改名或删除配置，也可在“设置 > Codex 账号与连接”检查/退出 OAuth。", "err");
    return;
  }
  const editable = t ? t.base_url_editable : true;
  const active = id === configState.active_id;
  els.connSec.dataset.id = id;
  els.connTitle.textContent = (codex ? "Codex 账号模型 · " : "编辑连接 · ") + p.name + (active ? "（当前生效）" : "");
  els.connBaseGroup.hidden = codex;
  els.connKeyGroup.hidden = codex;
  els.connCodexCatalog.hidden = !codex;
  els.connModelLabel.textContent = codex ? "账号模型目录" : "模型配置";
  els.connSaveBtn.hidden = codex;
  els.connClearBtn.hidden = codex;
  els.connCancelBtn.textContent = codex ? "返回" : "取消";
  els.connCodexCatalogMeta.textContent = "尚未读取账号模型目录。";
  els.connCodexCatalogList.innerHTML = "";
  els.connBase.value = p.base_url || (t ? t.base_url : "");
  els.connBase.readOnly = !editable;
  els.connBase.placeholder = capSrc && (capSrc.api_format === "openai_chat" || capSrc.api_format === "openai_responses")
    ? "https://open.bigmodel.cn/api/paas/v4"
    : "https://your-relay/claude";
  els.connBaseHint.textContent = editable
    ? (t && t.base_url
        ? "官方默认地址，可改到 token 套餐 / 区域端点。"
        : (capSrc && capSrc.api_format === "openai_chat"
          ? "OpenAI 兼容 base root，代理自动补 /chat/completions。"
          : capSrc && capSrc.api_format === "openai_responses"
          ? "OpenAI 兼容 base root，代理自动补 /responses。"
          : "自定义端点根地址。"))
    : (modelCapability(capSrc) === CAP.NATIVE
        ? "模板地址（只读）；模型名称可从推荐中选择或自由填写。"
        : "模板地址（只读）。模型名称可自由填写。");
  applyModelCapability(capSrc, {
    info: els.connModelInfo, sel: els.connModel, hint: els.connModelHint, fetchBtn: els.connFetchBtn,
  }, p.model || "");
  initializeCatalogEditor(
    "connection",
    (p.model_catalog || []).map((route) => ({
      origin: (t?.recommended_catalog || []).some((recommended) => recommended.upstream_model === route.upstream_model) ? "preset" : "manual",
      availability: "unknown",
      ...route,
    })),
    p.default_model_route_id || p.model,
    p.role_bindings,
    { dynamic: codex },
  );
  els.connKey.value = "";
  els.connKey.placeholder = p.key ? "已存：" + p.key + "（留空＝不改）" : "粘贴 key（只存本地）";
  showView("conn");
  refreshConnGate();
  setMsg(codex
    ? "这里只读取 CSSwitch OAuth 账号模型；不会在配置中固定模型。启动后请在 Science 的 More models 选择。"
    : (active
      ? "编辑当前生效配置：保存会先校验→切换，失败自动回退到原配置（不谎报生效）。"
      : "编辑连接后点「保存连接」。"));
}

function refreshConnGate() {
  const p = currentConn();
  let invalidCatalog = false;
  if (p && !isCodexSource(p)) {
    try { catalogSubmission("connection"); } catch (_) { invalidCatalog = true; }
  }
  els.connSaveBtn.disabled = busy || !p || invalidCatalog;
}

async function connFetch() {
  const p = currentConn();
  if (!p) return;
  const t = tplById(p.template_id);
  if (!isCodexSource(p.capabilities ? p : t)) return;
  setBusy(true, { kind: "fetchModels", id: p.id });
  startFetchModelsFeedback(p.id);
  try {
    const r = await call("fetch_models", {
      req: { template_id: p.template_id, api_format: p.api_format || (t ? t.api_format : ""), base_url: "", key: "", profile_id: p.id },
    });
    renderCodexCatalog(els.connCodexCatalogMeta, els.connCodexCatalogList, r);
  } catch (e) {
    setMsg("获取模型失败：" + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
    refreshConnGate();
  }
}

async function connSave() {
  const p = currentConn();
  if (!p) { setMsg("配置不存在。", "err"); return; }
  if (isCodexSource(p)) {
    setMsg("Codex 不保存连接地址、API Key 或固定模型；请直接返回列表。", "err");
    return;
  }
  const t = tplById(p.template_id);
  const capSrc = profileCapabilitySource(p, t);
  let catalogPayload;
  try { catalogPayload = catalogSubmission("connection"); }
  catch (error) { setMsg(String(error.message || error), "err"); return; }
  const editable = t ? t.base_url_editable : true;
  const base = editable ? els.connBase.value.trim() : (t ? t.base_url : els.connBase.value.trim());
  // base_url 是否必填由后端 capabilities 决定；旧后端无字段时才按可编辑地址保守兜底。
  if (baseUrlRequired(capSrc) && !base) { setMsg("中转 / 自定义端点必须填写连接地址（base_url）。", "err"); return; }
  const baseErr = openaiCustomAnthropicBaseMessage(t, base);
  if (baseErr) { setMsg(baseErr, "err"); return; }
  const active = p.id === configState.active_id;
  // key 留空＝不改；目录三件套必须一起提交，避免漏字段被解释为重置默认/roles。
  const args = {
    id: p.id,
    baseUrl: base,
    key: els.connKey.value.trim(),
    modelCatalog: catalogPayload.model_catalog,
    defaultModelRouteId: catalogPayload.default_model_route_id,
    roleBindings: catalogPayload.role_bindings,
  };
  setBusy(true, { kind: "saveConnection", id: p.id });
  startSaveConnectionFeedback(p.id, active);
  try {
    const r = await call("update_profile_connection", args);
    els.connKey.value = "";
    await loadConfig();
    if (r && (r.status === "error" || (active && r.committed === false))) {
      const recovery = r.recovery_status === "degraded" ? "；恢复也未完全成功，请先全部停止后检查" : r.recovery_status === "restored" ? "；旧配置已恢复" : "";
      setMsg((r.message || "连接未应用") + recovery + "（阶段：" + (r.stage || "unknown") + "）", "err");
    } else if (active) {
      setMsg((r && (r.message || r.hint)) || "已保存并应用新连接。", "ok");
    } else if (r && r.validated) {
      setMsg("已保存连接（已通过上游校验）。", "ok");
    } else {
      setMsg("已保存连接（未能连通上游校验，激活时会再验）。", "ok");
    }
  } catch (e) {
    // 后端错误文案已如实说明回滚/代理状态（可能是「已回滚到原配置」或「回滚未成功：代理当前已停」），
    // 前端不再盲目追加「仍在用原配置运行」，避免与「代理已停」相互矛盾。修 GPT 三轮 P2
    setMsg("连接未保存：" + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
    await refreshStatus();
    await skillPage?.refreshIfLoaded();
  }
}

// 清 key（行内 / 连接表单都可触发）：二次确认后 clear_profile_key。
function clearKey(id) {
  const p = (configState.profiles || []).find((x) => x.id === id);
  const nm = p ? p.name : id;
  confirmAction("clearkey:" + id, "将清除「" + nm + "」的 API key（需重填才能用）", () => doClearKey(id));
}
async function doClearKey(id) {
  const wasActive = id === configState.active_id;
  setBusy(true);
  setMsg("清除 key 中…");
  try {
    await call("clear_profile_key", { id });
    await loadConfig();
    setMsg(
      wasActive
        ? "已清除 key（该配置是当前生效，链路已断，请重新填 key 再「设为当前」）。"
        : "已清除 key。",
      "ok"
    );
  } catch (e) {
    setMsg("清除失败：" + e, "err");
  } finally {
    setBusy(false);
    await refreshStatus();
  }
}

// ── C4：改名/备注 + 删除 + 设为当前 ──
function openMeta(id) {
  const p = (configState.profiles || []).find((x) => x.id === id);
  if (!p) return;
  els.metaSec.dataset.id = id;
  els.metaName.value = p.name;
  els.metaNotes.value = p.notes || "";
  showView("meta");
  setMsg("改名 / 备注不影响运行中的代理。");
}
async function metaSave() {
  const id = els.metaSec.dataset.id;
  const name = els.metaName.value.trim();
  if (!name) { setMsg("名称不能为空。", "err"); return; }
  const notes = els.metaNotes.value.trim();
  setBusy(true);
  setMsg("保存中…");
  try {
    await call("update_profile_metadata", { id, name, notes });
    await loadConfig();
    setMsg("已保存。", "ok");
  } catch (e) {
    setMsg("保存失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

function del(id) {
  const p = (configState.profiles || []).find((x) => x.id === id);
  const nm = p ? p.name : id;
  confirmAction("delete:" + id, "将删除配置「" + nm + "」", () => doDelete(id));
}
async function doDelete(id) {
  const wasActive = id === configState.active_id;
  setBusy(true);
  setMsg("删除中…");
  try {
    await call("delete_profile", { id });
    await loadConfig();
    setMsg(
      wasActive
        ? "已删除。删掉的是当前生效配置，请重新选择一条并「设为当前」。"
        : "已删除。",
      "ok"
    );
  } catch (e) {
    setMsg("删除失败：" + e, "err");
  } finally {
    setBusy(false);
    await refreshStatus();
  }
}

// 设为当前：走后端切换事务（校验→起正式→健康才提交）。
// 返回体 committed:true=已生效；committed:false=未生效（可能可 skip）；抛错=回滚/中止。
async function activate(id, skipVerify) {
  if (activationInFlight) {
    setMsg("已有配置应用在后台完成。可以查看日志、反馈或自检；请稍后再提交另一条配置。");
    return;
  }
  const target = (configState.profiles || []).find((p) => p.id === id);
  const codex = isCodexSource(target);
  if (codex && !configState.experimental_codex_enabled) {
    setMsg("Codex 实验入口已关闭。请先在“设置 > Codex 账号与连接”重新启用。", "err");
    return;
  }
  hideSkip();
  setBrowserFallback("");
  setActivationInFlight(true, { kind: "activate", id });
  startActivateFeedback(id, !!skipVerify);
  try {
    const r = await call("set_active_profile", { id, skipVerify: !!skipVerify });
    if (r && r.committed) {
      await loadConfig();
      setMsg((r.hint || "已设为当前生效。") + (codex
        ? " 一键开始后，请在 Science 的 More models 中选择 Codex / …；默认 Claude 壳不会被静默映射。"
        : ""), "ok");
    } else {
      await loadConfig(); // 反映未变（仍是原 active）
      const recovery = r && r.recovery_status === "degraded" ? "；恢复未完全成功" : r && r.recovery_status === "restored" ? "；旧链路已恢复" : "";
      setMsg((r && (r.message || r.hint) || "校验未通过，未切换。") + recovery + (r && r.stage ? "（阶段：" + r.stage + "）" : ""), "err");
      setBrowserFallback(r && r.fallback_url);
      if (r && r.can_skip) { pendingSkipActivateId = id; showSkip(); }
    }
  } catch (e) {
    await loadConfig();
    setMsg("设为当前失败：" + runtimeCommandErrorText(e), "err");
  } finally {
    setActivationInFlight(false);
    await refreshStatus();
  }
}

function hideRuntimeChoice() {
  els.runtimeChoiceSec.hidden = true;
  els.runtimeChoiceText.textContent = "";
  runtimeChoiceActiveId = null;
}

function showRuntimeChoice(preflight) {
  const cachedVersion = preflight && preflight.cached_version;
  const canUseCache = preflight && preflight.status === "cached_choice_required" && !!cachedVersion;
  els.runtimeUseCacheBtn.hidden = !canUseCache;
  els.runtimeChoiceText.textContent = canUseCache
    ? "未找到通过安全预检的 Claude Science App。发现可确认版本的历史缓存：" + cachedVersion + "。你可以仅本次使用它，或前往官方页面安装 / 更新 Science。此选择不会保存。"
    : "未找到通过安全预检的 Claude Science App，历史缓存也无法确认版本。请先从官方页面安装 / 更新 Science。";
  els.runtimeChoiceSec.hidden = false;
  runtimeChoiceActiveId = configState.active_id || null;
}

async function checkOneClickBoundary() {
  if (activationInFlight) {
    setMsg("配置仍在后台应用。请等待它完成后再一键开始，避免按旧的当前配置启动。", "err");
    return false;
  }
  if (!configState.active_id) {
    setMsg("还没有「当前生效」的配置。请先点「新建配置」或在列表点「设为当前」选一条，再一键开始。", "err");
    return false;
  }
  const active = (configState.profiles || []).find((p) => p.id === configState.active_id);
  if (isCodexSource(active)) {
    if (!configState.experimental_codex_enabled) {
      setMsg("当前是 Codex 配置，但实验入口已关闭。请先在“设置 > Codex 账号与连接”重新启用。", "err");
      return false;
    }
  }
  return true;
}

async function runOneClick(runtimeChoice) {
  if (runtimeChoice) {
    if (!runtimeChoiceActiveId || runtimeChoiceActiveId !== configState.active_id) {
      hideRuntimeChoice();
      setMsg("当前生效配置已变化，本次缓存运行选择已作废。请重新点击「一键开始」。", "err");
      return;
    }
  }
  if (!(await checkOneClickBoundary())) return;
  skillPage?.invalidate();
  hideRuntimeChoice();
  setBusy(true, { kind: "oneClick" });
  setBrowserFallback("");
  startOneClickFeedback();
  try {
    const r = await call("one_click_login", { runtimeChoice: runtimeChoice || null });
    if (r && r.status === "error") {
      const recovery = r.recovery_status === "degraded"
        ? "；刷新未完成，安全事务记录已保留，可修正问题后重试"
        : "";
      setMsg((r.message || "一键开始未完成") + recovery + "（阶段：" + (r.stage || "unknown") + "）", "err");
      setBrowserFallback(r.fallback_url);
      await refreshStatus();
      return;
    }
    // 透传后端据实回传的 msg（已重开 / 已用新配置重启 / 沿用原对话 / 已启动 / 打开失败请手动打开）。
    const active = (configState.profiles || []).find((p) => p.id === configState.active_id);
    const message = r.msg || "已就绪，正在打开面板…";
    setMsg(message + (isCodexSource(active)
      ? " 请在 Science 的 More models 中选择 Codex / … 后再发第一条消息；默认 Claude 壳会被明确拒绝。"
      : ""), "ok");
    setBrowserFallback(r.fallback_url);
    await refreshStatus();
  } catch (e) {
    setMsg("一键开始失败：" + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
    await skillPage?.refreshIfLoaded();
  }
}

async function importLocalSkill() {
  if (busy) return;
  setBusy(true, { kind: "importSkill" });
  setMsg("正在选择并校验 Skill 包…");
  try {
    const result = await call("install_local_skill_package");
    if (result.status === "CANCELLED") {
      setMsg("已取消导入 Skill 包。");
    } else if (result.status === "INSTALLED_ATTACHED_VERIFY_REQUIRED") {
      setMsg("文件已安装并绑定 OPERON。请在 Science 中让 Agent 调用 skill(" + result.skill_name + ") 验证当前会话加载。", "ok");
    } else if (result.status === "BUNDLE_INSTALLED_ATTACHED") {
      const names = Array.isArray(result.skill_names) ? result.skill_names : [];
      const summary = names.slice(0, 4).join("、") + (names.length > 4 ? " 等" : "");
      setMsg("已安装并绑定 " + names.length + " 个 Skill" + (summary ? "：" + summary : "") + "。", "ok");
    } else {
      setMsg((result.message || "Skill 包导入未完成") + " [" + result.status + "]", "err");
    }
    if (result && result.directory_commit === true && skillPage) {
      await skillPage.refresh();
    }
  } catch (e) {
    setMsg("导入 Skill 包失败：" + e, "err");
    await skillPage?.refreshIfLoaded();
  } finally {
    setBusy(false);
  }
}

// ── 一键开始：先确认本次实际 Science runtime，再进入原启动链路。──
async function oneClick() {
  if (!(await checkOneClickBoundary())) return;
  setBusy(true, { kind: "oneClick" });
  setMsg("正在确认本次使用的 Claude Science…");
  try {
    const preflight = await call("science_runtime_preflight");
    if (preflight && preflight.status === "installed_ready") {
      setBusy(false);
      await runOneClick(null);
      return;
    }
    showRuntimeChoice(preflight || { status: "missing" });
    setMsg(preflight && preflight.status === "cached_choice_required"
      ? "Claude Science App 不可用或未通过预检。请选择是否仅本次使用已确认版本的缓存。"
      : "Claude Science App 不可用或未通过预检，且没有可安全启动的缓存版本。", "err");
  } catch (e) {
    setMsg("Science 运行环境检查失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function openScienceDownload() {
  try {
    await call("open_science_download_page");
    setMsg("已打开 Claude 官方下载页。安装完成后请再次点击「一键开始」。");
  } catch (e) {
    setMsg("打开 Claude 官方下载页失败：" + e, "err");
  }
}

function cancelRuntimeChoice() {
  hideRuntimeChoice();
  setMsg("已取消，本次没有启动 Claude Science。");
}

async function stopAll() {
  skillPage?.invalidate();
  setBusy(true);
  setMsg("停止中…");
  try {
    await call("stop_all");
    setMsg("已停止代理与沙箱。", "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("停止失败：" + e, "err");
  } finally {
    setBusy(false);
    await skillPage?.refreshIfLoaded();
  }
}

async function openBrowser() {
  if (busy || browserOpenInFlight) return;
  browserOpenInFlight = true;
  syncOpenBrowserControl();
  setMsg("正在获取新的 Science 地址并交给默认浏览器打开…");
  try {
    const result = await call("open_url");
    if (result && result.status === "error") {
      setBrowserFallback(result.fallback_url);
      setMsg(result.message || "打开浏览器失败；请复制 URL 手动打开。", "err");
    } else {
      setBrowserFallback("");
      setMsg((result && result.message) || "已向默认浏览器发出打开 Science 的请求；若窗口没有切到前台，请从 Dock 或其他桌面切回默认浏览器。", "ok");
    }
  } catch (e) {
    setMsg("打开浏览器失败：" + e, "err");
  } finally {
    browserOpenInFlight = false;
    syncOpenBrowserControl();
  }
}

async function runDoctor() {
  const activationBusy = activationInFlight || (busy && busyOp && busyOp.kind === "activate");
  if (doctorInFlight || (busy && !activationBusy)) return;
  doctorInFlight = true;
  if (els.doctorBtn) els.doctorBtn.disabled = true;
  if (activationBusy) {
    setMsg("自检中：配置后台应用仍在继续。完成后会核验 CSSwitch 管理的 Skill 路由。");
  } else {
    setBusy(true, { kind: "doctor" });
    startDoctorFeedback();
  }
  try {
    const out = await call("run_doctor");
    setMsg(out, out.includes("失败 0") ? "ok" : null);
  } catch (e) {
    setMsg("自检失败：" + e, "err");
  } finally {
    doctorInFlight = false;
    if (els.doctorBtn) els.doctorBtn.disabled = busy && busyOp && busyOp.kind !== "activate";
    if (!activationBusy) setBusy(false);
  }
}

// 简单 semver 比较：a 是否比 b 新。
function isNewer(a, b) {
  const pa = String(a).split(".").map((n) => parseInt(n, 10) || 0);
  const pb = String(b).split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] || 0, y = pb[i] || 0;
    if (x !== y) return x > y;
  }
  return false;
}

async function checkUpdate() {
  setMsg("检查更新中…");
  let cur = "";
  try { cur = await call("app_version"); } catch (e) {}
  try {
    const resp = await fetch(
      "https://api.github.com/repos/SuperJJ007/CSSwitch/releases/latest",
      { headers: { Accept: "application/vnd.github+json" } }
    );
    if (!resp.ok) throw new Error("HTTP " + resp.status);
    const data = await resp.json();
    const latest = (data.tag_name || "").replace(/^v/, "");
    if (!latest) throw new Error("无版本信息");
    if (isNewer(latest, cur)) {
      setMsg("发现新版本 v" + latest + "（当前 v" + cur + "）。正在打开下载页…", "ok");
      try { await call("open_release_page"); } catch (_) {}
    } else {
      setMsg("已是最新版本（v" + cur + "）。", "ok");
    }
  } catch (e) {
    setMsg("无法自动检查更新（多为网络或代理限制）。已打开 Releases 页，请手动查看。", "err");
    try { await call("open_release_page"); } catch (_) {}
  }
}

async function refreshStatus() {
  try {
    const s = await call("status");
    setLight(els.ltProxy, s.proxy);
    setLight(els.ltSandbox, s.sandbox);
    setLight(els.ltUpstream, s.upstream);
    setStatusText("proxyStateText", s.proxy);
    setStatusText("sandboxStateText", s.sandbox);
    setStatusText("upstreamStateText", s.upstream);
    els.brandDot.className = "dot " + aggregateRuntimeStatus(s, { mode, officialState: officialRuntimeState });
    setStatusRecoveryMsg(proxyRecoveryMessage(s));
  } catch (e) {
    [els.ltProxy, els.ltSandbox, els.ltUpstream].forEach((l) => setLight(l, "unknown"));
    ["proxyStateText", "sandboxStateText", "upstreamStateText"].forEach((id) => setStatusText(id, "unknown"));
    els.brandDot.className = "dot gray";
  }
}

function wire() {
  [
    "oneClickBtn", "stopBtn", "importSkillBtn", "refreshSkillsBtn", "ltProxy", "ltSandbox", "ltUpstream",
    "runtimeChoiceSec", "runtimeChoiceText", "runtimeUseCacheBtn", "runtimeDownloadBtn", "runtimeChoiceCancelBtn",
    "msg", "browserFallback", "browserFallbackUrl", "browserFallbackCopyBtn", "browserFallbackRetryBtn", "brandDot", "openBrowserBtn", "doctorBtn", "updateBtn", "verLabel",
    "reportBtn", "logsBtn", "quitBtn", "modeSeg", "proxyPort", "sandboxPort", "reuseSystemSsh", "advSec",
    "codexEnabled", "codexAuthStatus", "codexStatusBtn", "codexLoginBtn", "codexCancelBtn", "codexLogoutBtn", "codexProfileRepairBox", "codexRepairProfileBtn",
    "codexNetworkMode", "codexProxyUrl", "codexNetworkResolved", "codexNetworkSaveBtn", "codexDowngradeBox", "codexDowngradeBtn",
    "connectionOverview", "listSec", "profileList", "newBtn", "skipActivateBtn",
    "wizSec", "wizTemplate", "wizTemplateChips", "wizTplLabel", "wizTplHint", "wizName", "wizBaseGroup", "wizBase", "wizBaseHint",
    "wizModelGroup", "wizModelLabel", "wizFetchBtn", "wizModelInfo", "wizModel", "wizModelHint", "wizCodexCatalog", "wizCodexCatalogMeta", "wizCodexCatalogList", "wizStaticCatalog", "wizRoleQuality", "wizRoleFast", "wizRoleFable", "wizCatalogWarning", "wizKeyGroup", "wizKey", "wizSaveBtn", "wizCancelBtn",
    "connSec", "connTitle", "connBaseGroup", "connBase", "connBaseHint", "connFetchBtn",
    "connModelGroup", "connModelLabel", "connModelInfo", "connModel", "connModelHint", "connCodexCatalog", "connCodexCatalogMeta", "connCodexCatalogList", "connStaticCatalog", "connRoleQuality", "connRoleFast", "connRoleFable", "connCatalogWarning", "connKeyGroup", "connKey", "connSaveBtn", "connClearBtn", "connCancelBtn",
    "metaSec", "metaName", "metaNotes", "metaSaveBtn", "metaCancelBtn",
    "themeBtn", "pageEyebrow", "pageTitle", "pageSubtitle",
    "currentProfileIcon", "currentProfileName", "currentProfileState", "currentRouteMode", "currentProfileModel", "currentProfileMeta",
    "proxyStateText", "sandboxStateText", "upstreamStateText",
  ].forEach((id) => (els[id] = $(id)));
  els.panel = document.querySelector(".panel");

  document.querySelectorAll("[data-page-target]").forEach((button) => {
    button.addEventListener("click", () => {
      if (button.dataset.pageTarget === "switch") showView("list");
      setPage(button.dataset.pageTarget);
    });
  });
  applyTheme(savedTheme() || document.documentElement.dataset.theme || "light", { persist: false });
  els.themeBtn.addEventListener("click", () => applyTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark"));

  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.addEventListener("click", () => switchMode(b.dataset.mode))
  );

  els.proxyPort.addEventListener("change", persistRuntimeSettings);
  els.sandboxPort.addEventListener("change", persistRuntimeSettings);
  els.reuseSystemSsh.addEventListener("change", persistRuntimeSettings);
  els.codexEnabled.addEventListener("change", toggleCodexFeature);
  els.codexStatusBtn.addEventListener("click", checkCodexAuth);
  els.codexLoginBtn.addEventListener("click", startCodexLogin);
  els.codexRepairProfileBtn.addEventListener("click", repairCodexProfile);
  els.codexCancelBtn.addEventListener("click", cancelCodexLogin);
  els.codexLogoutBtn.addEventListener("click", logoutCodex);
  els.codexNetworkMode.addEventListener("change", codexNetworkModeChanged);
  els.codexNetworkSaveBtn.addEventListener("click", saveCodexNetwork);
  els.codexDowngradeBtn.addEventListener("click", requestCodexDowngrade);

  // 列表行内操作（事件委托；忙碌时忽略）。
  els.profileList.addEventListener("click", (e) => {
    if (busy) return;
    const btn = e.target.closest("[data-act]");
    const row = e.target.closest("[data-id]");
    if (!btn || !row || btn.disabled || btn.dataset.permanentlyDisabled === "true") return;
    const id = row.getAttribute("data-id");
    const act = btn.getAttribute("data-act");
    if (act === "activate") activate(id, false);
    else if (act === "editconn") openConn(id);
    else if (act === "editmeta") openMeta(id);
    else if (act === "clearkey") clearKey(id);
    else if (act === "delete") del(id);
  });

  // 浏览器交互预览可单独启用模型下拉；真实 App 始终使用后端已保存模型。
  els.profileList.addEventListener("change", (e) => {
    const select = e.target.closest("[data-profile-model]");
    if (!select || !PROFILE_INTERACTIVE_PREVIEW) return;
    const id = select.getAttribute("data-profile-model");
    const model = select.value;
    const profile = (configState.profiles || []).find((item) => item.id === id);
    const stored = (mockStore.profiles || []).find((item) => item.id === id);
    if (!profile || !stored) return;
    profile.model = model;
    stored.model = model;
    renderList();
    setMsg(`预览：已将「${profile.name}」的模型设为 ${model}。`, "ok");
  });

  els.newBtn.addEventListener("click", openWizard);
  els.skipActivateBtn.addEventListener("click", () => {
    const id = pendingSkipActivateId;
    if (id) activate(id, true);
  });

  els.wizTemplateChips.addEventListener("click", (e) => {
    if (busy) return;
    const chip = e.target.closest(".chip");
    if (chip) selectWizTemplate(chip.getAttribute("data-tid"));
  });
  [els.wizModel, els.wizRoleQuality, els.wizRoleFast, els.wizRoleFable]
    .forEach((input) => input.addEventListener("input", () => catalogRolesChanged("wizard")));
  els.wizFetchBtn.addEventListener("click", wizFetch);
  els.wizSaveBtn.addEventListener("click", wizSave);
  els.wizCancelBtn.addEventListener("click", cancelForm);

  [els.connModel, els.connRoleQuality, els.connRoleFast, els.connRoleFable]
    .forEach((input) => input.addEventListener("input", () => catalogRolesChanged("connection")));
  els.connFetchBtn.addEventListener("click", connFetch);
  els.connSaveBtn.addEventListener("click", connSave);
  els.connClearBtn.addEventListener("click", () => clearKey(els.connSec.dataset.id));
  els.connCancelBtn.addEventListener("click", cancelForm);

  els.metaSaveBtn.addEventListener("click", metaSave);
  els.metaCancelBtn.addEventListener("click", cancelForm);

  els.oneClickBtn.addEventListener("click", heroClick);
  els.runtimeUseCacheBtn.addEventListener("click", () => runOneClick("cached_once"));
  els.runtimeDownloadBtn.addEventListener("click", openScienceDownload);
  els.runtimeChoiceCancelBtn.addEventListener("click", cancelRuntimeChoice);
  els.stopBtn.addEventListener("click", stopAll);
  els.importSkillBtn.addEventListener("click", importLocalSkill);
  els.openBrowserBtn.addEventListener("click", openBrowser);
  els.browserFallbackRetryBtn.addEventListener("click", openBrowser);
  els.browserFallbackCopyBtn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(els.browserFallbackUrl.value);
      setMsg("Science URL 已复制。", "ok");
    } catch (_) {
      els.browserFallbackUrl.select();
      setMsg("无法自动写入剪贴板；已选中 URL，请手动复制。", "err");
    }
  });
  els.doctorBtn.addEventListener("click", runDoctor);
  els.updateBtn.addEventListener("click", checkUpdate);
  els.reportBtn.addEventListener("click", () =>
    call("report_bug").catch((e) => setMsg("打开反馈页失败：" + e, "err"))
  );
  els.logsBtn.addEventListener("click", () =>
    call("open_logs").catch((e) => setMsg("打开日志失败：" + e, "err"))
  );
  els.quitBtn.addEventListener("click", () => {
    setBusy(true);
    setMsg("正在停止代理与隔离 Science…");
    call("quit_app").catch((e) => {
      setBusy(false);
      setMsg("退出失败：" + e + "请先使用“全部停止”重试。", "err");
    });
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  wire();
  try { await registerCodexAuthEvents(); } catch (e) { setMsg("无法订阅 Codex 登录状态：" + e, "err"); }
  await configureDesktopWindow();
  try {
    const { mountSkillPage } = await import("./skill-page.js");
    skillPage = mountSkillPage($("skillPageRoot"), {
      call,
      refreshButton: els.refreshSkillsBtn,
      importButton: els.importSkillBtn,
    });
  } catch (e) {
    $("skillPageRoot").innerHTML = '<div class="skill-loading">Skill 页面载入失败：' + escapeHtml(e) + "</div>";
  }
  setPage(SKILLS_PREVIEW ? "skills" : "switch");
  await loadConfig();
  if (!PREVIEW && window.__TAURI__.event) {
    window.__TAURI__.event.listen("boot://failed", (e) => {
      setMsg("自动启动未成功：" + (e.payload || "未知原因") + "\n可检查配置后点「一键开始」重试。", "err");
      refreshStatus();
    });
  }
  try {
    const bootError = await call("boot_error");
    if (bootError) setMsg("自动启动未成功：" + bootError + "\n可检查配置后点「一键开始」重试。", "err");
  } catch (e) {}
  try { els.verLabel.textContent = "v" + (await call("app_version")); } catch (e) {}
  await refreshStatus();
  if (!PREVIEW) statusTimer = setInterval(refreshStatus, 2500);
});
