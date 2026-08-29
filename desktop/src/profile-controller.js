import { call } from "./ipc-client.js";
import { PROFILE_INTERACTIVE_PREVIEW } from "./preview-adapter.js";
import {
  buildSimpleModelSubmission,
  mergeCatalogCandidates,
  projectSimpleModelFields,
} from "./model-catalog-state.js";
import {
  formatConfigMutationCommandError,
  parseConfigIntentOutcome,
  parseConfigMutationCommandError,
  parseConfigMutationResponse,
} from "./runtime-mutation-protocol.js";

export function createProfileController({
  els,
  getConfigState,
  getSkillPage,
  getMode,
  setMode,
  setOfficialRuntimeState,
  isBusy,
  getBusyOp,
  isActivationInFlight,
  getActivationOp,
  setActivationInFlight,
  setBusy,
  setMsg,
  setPage,
  showView,
  confirmAction,
  escapeHtml,
  startFetchModelsFeedback,
  startActivateFeedback,
  startSaveConnectionFeedback,
  startSwitchModeFeedback,
  startPortSaveFeedback,
  codex: codexController,
  runtime,
}) {
  let wizardCatalog = [];
  let connectionCatalog = [];
  let wizardDiscoveredCatalog = [];
  let connectionDiscoveredCatalog = [];
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

function mutationErrorText(error) {
  try {
    const parsed = parseConfigMutationCommandError(error);
    if (parsed) return formatConfigMutationCommandError(parsed);
  } catch (protocolError) {
    return protocolError.message;
  }
  return String(error && error.message ? error.message : error);
}

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

function renderCurrentSummary() {
  if (!els.currentProfileName) return;
  if (getMode() === "official") {
    updateModelIcon(els.currentProfileIcon, { template_id: "anthropic" });
    els.currentProfileName.textContent = "官方 Claude";
    els.currentProfileState.textContent = "官方模式";
    els.currentProfileState.className = "state-pill neutral";
    els.currentRouteMode.textContent = "由 Science 管理";
    els.currentProfileModel.textContent = "Claude Science";
    els.currentProfileMeta.textContent = "订阅与登录由 Claude Science 管理";
    return;
  }
  const profile = (getConfigState().profiles || []).find((item) => item.id === getConfigState().active_id);
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
  const codexDisabled = isCodexSource(profile) && !getConfigState().experimental_codex_enabled;
  const pending = getConfigState().selection_pending || getConfigState().active_id !== getConfigState().applied_profile_id;
  els.currentProfileState.textContent = codexDisabled
    ? "入口已关闭"
    : pending
    ? "当前选择 · 待一键开始应用"
    : "当前选择 · 上次应用";
  els.currentProfileState.className = "state-pill neutral";
  els.currentProfileModel.textContent = modelSummary(profile);
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
function canFetchModels(t) {
  const discovery = templateCaps(t).model_discovery;
  return discovery === "codex_account_catalog"
    || discovery === "anthropic_models_or_manual"
    || discovery === "openai_models_or_manual";
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
    if (ui.fetchBtn) {
      ui.fetchBtn.hidden = false;
      ui.fetchBtn.parentElement.hidden = false;
      ui.fetchBtn.textContent = "刷新账号模型";
    }
    ui.hint.textContent = "模型选择权保留给 Science；CSSwitch 不会静默固定某一个 Codex 模型。";
    return cap;
  }
  // 静态 provider 统一使用四个自由输入框；推荐项只作为 datalist 建议。
  ui.info.textContent = cap === CAP.NATIVE ? MODEL_HINT.native : "";
  ui.info.hidden = cap !== CAP.NATIVE;
  ui.sel.hidden = false;
  if (ui.fetchBtn) {
    const fetchable = canFetchModels(t);
    ui.fetchBtn.hidden = !fetchable;
    ui.fetchBtn.parentElement.hidden = !fetchable;
    ui.fetchBtn.textContent = "获取可用模型";
  }
  const builtin = ((t && t.builtin_models) || []).slice();
  if (currentModel && !builtin.includes(currentModel)) builtin.unshift(currentModel);
  const models = builtin.map((id) => ({ id, supports_tools: null }));
  renderModelOptions(ui.sel, models, "内置");
  ui.sel.value = currentModel || (builtin[0] || "");
  ui.hint.textContent = [MODEL_HINT.fixed, t && t.compatibility_notice]
    .filter(Boolean).join(" ");
  return cap;
}

function profileName(id) {
  const p = (getConfigState().profiles || []).find((x) => x.id === id);
  return p ? p.name : id;
}

function syncProfileBusyState() {
  if (!els.profileList) return;
  els.profileList.querySelectorAll(".prow").forEach((row) => {
    const rowId = row.getAttribute("data-id");
    const isTarget = !!(
      (getBusyOp() && getBusyOp().kind === "activate" && rowId === getBusyOp().id) ||
      (getActivationOp() && getActivationOp().kind === "activate" && rowId === getActivationOp().id)
    );
    row.classList.toggle("pworking", isTarget);
    row.querySelectorAll("button[data-act]").forEach((btn) => {
      const act = btn.getAttribute("data-act");
      const permanentlyDisabled = btn.dataset.permanentlyDisabled === "true";
      btn.disabled = permanentlyDisabled || isBusy() || (isActivationInFlight() && act === "activate");
      if (act === "activate") {
        btn.textContent = isTarget ? "已提交" : "设为当前";
      }
    });
  });
}

function tplById(id) {
  return (getConfigState().templates || []).find((t) => t.id === id) || null;
}

// ── 加载配置 + 渲染列表 ──
async function loadConfig(options) {
  const opts = options || {};
  try {
    const cfg = await call("get_config");
    getConfigState().profiles = cfg.profiles || [];
    getConfigState().templates = cfg.templates || [];
    getConfigState().active_id = cfg.active_id || "";
    getConfigState().applied_profile_id = cfg.applied_profile_id || null;
    getConfigState().selection_pending = !!cfg.selection_pending;
    getConfigState().proxy_port = cfg.proxy_port ?? 18991;
    getConfigState().sandbox_port = cfg.sandbox_port ?? 8990;
    getConfigState().reuse_system_ssh = !!cfg.reuse_system_ssh;
    getConfigState().experimental_codex_enabled = !!cfg.experimental_codex_enabled;
    getConfigState().codex_network = cfg.codex_network || { mode: "auto", proxy_url: "" };
    getConfigState().codex_network_resolved = cfg.codex_network_resolved || { source: "direct", proxy_scheme: null };
    els.proxyPort.value = getConfigState().proxy_port;
    els.sandboxPort.value = getConfigState().sandbox_port;
    els.reuseSystemSsh.checked = getConfigState().reuse_system_ssh;
    codexController.refreshCodexProfileRepairState();
    codexController.renderCodexAuthState();
    codexController.renderCodexNetwork();
    applyMode(cfg.mode === "official" ? "official" : "proxy");
    renderList();
    showView("list");
    if (cfg.pending_notice) {
      setMsg(cfg.pending_notice, "ok");
      if (cfg.pending_notice_id) {
        try {
          await call("acknowledge_pending_notice", { expectedNoticeId: cfg.pending_notice_id });
        } catch (_) {
          // The notice remains durable and will be offered again on a later read.
        }
      }
    }
  } catch (e) {
    setMsg("读取配置失败：" + e, "err");
    if (opts.throwOnError) throw e;
    return false;
  }
  return true;
}

async function loadConfigAfterCommit() {
  try {
    await loadConfig({ throwOnError: true });
  } catch (cause) {
    const error = new Error(String(cause));
    error.configCommitted = true;
    throw error;
  }
}

function committedRefreshMessage(action, error) {
  return action + "已提交，但界面刷新失败；请重新打开配置页确认当前状态。原因：" + error.message;
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
    const primary = modelSummary(p);
    const details = count ? `${count} 个可用模型` : "动态目录";
    return `<strong class="profile-model-text" title="${escapeHtml(primary)}">${escapeHtml(primary)}</strong><span class="profile-model-meta">${escapeHtml(details)}</span>`;
  }
  const options = profileModelOptions(p);
  return `<select class="profile-model-select" data-profile-model="${escapeHtml(p.id)}" aria-label="${escapeHtml(p.name)} 的模型">
    ${options.map((value) => `<option value="${escapeHtml(value)}"${value === model ? " selected" : ""}>${escapeHtml(value)}</option>`).join("")}
  </select>`;
}

function renderList() {
  const list = els.profileList;
  const ps = getConfigState().profiles || [];
  renderCurrentSummary();
  if (!ps.length) {
    list.innerHTML = '<div class="empty">还没有配置。使用“新建配置”添加一条第三方来源。</div>';
    return;
  }
  const header = `<div class="profile-list-head" aria-hidden="true"><span>配置</span><span>模型 / 目录</span><span>凭据</span><span>操作</span></div>`;
  list.innerHTML = header + ps.map((p) => {
    const active = p.id === getConfigState().active_id;
    const codex = isCodexSource(p);
    const codexEnabled = !!getConfigState().experimental_codex_enabled;
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
            (active ? '<span class="badge on">当前选择</span>' : "") +
            (p.id === getConfigState().applied_profile_id ? '<span class="badge">上次应用</span>' : "") +
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
  if (nextMode !== getMode() && nextMode === "official") setOfficialRuntimeState("gray");
  setMode(nextMode);
  els.panel.classList.toggle("mode-official", getMode() === "official");
  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.classList.toggle("active", b.dataset.mode === getMode())
  );
  els.oneClickBtn.textContent =
    getMode() === "official" ? "打开官方 Claude Science" : "一键开始";
  renderCurrentSummary();
}

function isExactConfigIntent(outcome, operation, dispositions) {
  return !!outcome &&
    outcome.operation === operation &&
    dispositions.includes(outcome.disposition) &&
    outcome.config_state === "committed" &&
    typeof outcome.intent_id === "string" &&
    outcome.validation === "not_run" &&
    outcome.science_running === false;
}

function isExactCompletedConfigMutation(outcome, operation, runtimeStates) {
  return !!outcome &&
    outcome.operation === operation &&
    outcome.disposition === "completed" &&
    outcome.config_state === "after" &&
    runtimeStates.includes(outcome.runtime_state) &&
    outcome.recovery_state === "not_needed" &&
    typeof outcome.operation_id === "string";
}

function isExactActiveProfileIntent(raw, outcome, selectedProfileId) {
  const expectedApplyState = outcome && outcome.applied_profile_id === selectedProfileId
    ? "applied"
    : "pending";
  return !!raw && !!outcome &&
    outcome.operation === "set_active_profile" &&
    ["committed", "no_change"].includes(outcome.disposition) &&
    outcome.config_state === "committed" &&
    outcome.selected_profile_id === selectedProfileId &&
    outcome.validation === "accepted" &&
    typeof outcome.science_running === "boolean" &&
    raw.committed === true &&
    raw.status === "ok" &&
    raw.apply_state === expectedApplyState &&
    raw.selected_profile_id === outcome.selected_profile_id &&
    raw.applied_profile_id === outcome.applied_profile_id &&
    raw.science_running === outcome.science_running;
}

async function switchMode(m) {
  if (m === getMode()) return;
  if (isBusy()) return; // 忙碌中不切模式（防与「一键开始」竞态；按钮亦已禁用，此为双保险）。修 P1-b
  getSkillPage()?.invalidate();
  setBusy(true, { kind: "switchMode", id: m });
  startSwitchModeFeedback(m);
  try {
    const result = await call("set_mode", { mode: m });
    const outcome = parseConfigMutationResponse(result);
    const validOutcome =
      isExactConfigIntent(outcome, "set_mode", ["committed"]) ||
      (m === "official" && isExactCompletedConfigMutation(
        outcome,
        "set_mode_official",
        ["stopped"]
      ));
    if (!validOutcome) {
      throw new Error("模式切换结果协议不匹配。");
    }
  } catch (e) {
    setMsg("切换模式失败：" + mutationErrorText(e), "err");
    setBusy(false);
    await getSkillPage()?.refreshIfLoaded();
    return;
  }
  applyMode(m);
  runtime.hideHistoryRecovery();
  setBusy(false);
  showView("list");
  setMsg(
    getMode() === "official"
      ? "已切到官方模式：第三方代理/沙箱已停，点上方按钮打开你真实的 Claude Science。"
      : "已切到第三方模式：选一条配置「设为当前」后点「一键开始」。"
  );
  await runtime.refreshStatus();
  await getSkillPage()?.refreshIfLoaded();
}

async function openOfficial() {
  setBusy(true);
  setMsg("正在打开官方 Claude Science…");
  try {
    await call("open_official");
    setOfficialRuntimeState("gray");
    els.brandDot.className = "dot gray";
    setMsg("已发起打开官方 Claude Science；运行、登录与订阅状态由 Science 管理。", "ok");
  } catch (e) {
    setOfficialRuntimeState("gray");
    els.brandDot.className = "dot gray";
    setMsg("打开失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

// hero 按钮按当前模式分派。
async function heroClick() {
  if (getMode() === "official") await openOfficial();
  else await runtime.oneClick();
}

// ── 运行设置（端口 + 系统 SSH 配置授权；不含 provider/连接）──
async function persistRuntimeSettings() {
  if (isBusy()) return; // 忙碌中不改端口（防与在途操作竞态；输入亦已禁用，此为双保险）。修 P1-c
  getSkillPage()?.invalidate();
  const p = parseInt(els.proxyPort.value, 10) || 18991;
  const s = parseInt(els.sandboxPort.value, 10) || 8990;
  const reuseSystemSsh = !!els.reuseSystemSsh.checked;
  const portsChanged = p !== getConfigState().proxy_port || s !== getConfigState().sandbox_port;
  const sshChanged = reuseSystemSsh !== getConfigState().reuse_system_ssh;
  const changed = portsChanged || sshChanged;
  // 本次端口提交全程置忙：仅靠开头的 `if (isBusy()) return` 只挡「已在忙时进入」，挡不住本函数在途
  // 时其它操作（切模式/一键/连接编辑）启动。置忙 + 禁用控件才能保证操作顺序符合用户预期。修 GPT 三轮 P2
  setBusy(true, { kind: "ports" });
  startPortSaveFeedback(changed);
  try {
    const result = await call("set_settings", { cfg: { proxy_port: p, sandbox_port: s, reuse_system_ssh: reuseSystemSsh } });
    const outcome = parseConfigMutationResponse(result);
    const validOutcome =
      isExactConfigIntent(outcome, "set_settings", ["committed", "no_change"]) ||
      isExactCompletedConfigMutation(
        outcome,
        "set_settings_destructive",
        ["stopped", "preserved"]
      );
    if (!validOutcome) {
      throw new Error("运行设置结果协议不匹配。");
    }
    getConfigState().proxy_port = p;
    getConfigState().sandbox_port = s;
    getConfigState().reuse_system_ssh = reuseSystemSsh;
    // set_settings invalidates backend history-recovery references even when
    // the submitted values are unchanged, so never leave stale buttons visible.
    runtime.hideHistoryRecovery();
    // 后端在端口变化时会拆掉旧代理/沙箱（否则会复用指向旧端口的死链路），如实告知需重开。修 P1-c
    if (changed) {
      setMsg(sshChanged
        ? "SSH 授权设置已保存。正在运行的代理/沙箱已重置，请重新「一键开始」。"
        : "端口已保存。改端口会重置正在运行的代理/沙箱，请重新「一键开始」。", "ok");
      await runtime.refreshStatus();
      await getSkillPage()?.refreshIfLoaded();
    } else {
      setMsg("端口未变化。", "ok");
    }
  } catch (e) {
    // 出错＝端口未落盘（校验不过 / 停旧沙箱失败）：把输入框还原成实际生效值，避免显示未保存的数字。
    els.proxyPort.value = getConfigState().proxy_port;
    els.sandboxPort.value = getConfigState().sandbox_port;
    els.reuseSystemSsh.checked = getConfigState().reuse_system_ssh;
    setMsg(mutationErrorText(e), "err");
  } finally {
    setBusy(false);
    await getSkillPage()?.refreshIfLoaded();
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
    const display = m.display_name && m.display_name !== m.id ? m.display_name + " · " : "";
    o.label = display + m.id + tag + src;
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
    get discovered() { return wizard ? wizardDiscoveredCatalog : connectionDiscoveredCatalog; },
    set discovered(value) { if (wizard) wizardDiscoveredCatalog = value; else connectionDiscoveredCatalog = value; },
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
    editor.discovered = [];
    editor.model.value = "";
    editor.quality.value = "";
    editor.fast.value = "";
    editor.fable.value = "";
    return;
  }
  editor.discovered = [];
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
    candidate_routes: editor.discovered,
    existing_references: editor.refs,
    preserve_existing_sonnet: kind === "connection",
    prune_replaced_bindings: kind === "connection",
  });
}

function renderStaticCatalogDiscovery(kind, r) {
  const editor = editorState(kind);
  const models = (r && r.models) || [];
  editor.discovered = mergeCatalogCandidates([], models, { enableNew: false });
  const suggestions = mergeCatalogCandidates(editor.catalog, editor.discovered, { enableNew: false });
  renderModelOptions(editor.model, suggestions.map((item) => ({
    id: item.upstream_model,
    display_name: item.display_name,
    supports_tools: item.supports_tools,
  })), modelSourceLabel(r && r.source));
  const meta = kind === "wizard" ? els.wizCodexCatalogMeta : els.connCodexCatalogMeta;
  const filtered = Number(r && r.filtered_unknown_count) || 0;
  const transport = r && r.route_catalog && r.route_catalog.transport;
  meta.textContent = modelSourceLabel(r && r.source) + " · " + models.length + " 个候选"
    + (transport ? " · " + transport : "")
    + (filtered ? " · 已按官方协议表过滤 " + filtered + " 个" : "");
  if (r && r.error_kind === "protocol") {
    setMsg("模型列表响应与当前协议不兼容；探测未写入配置，仍可在明确 transport 下手工填写精确模型 ID。", "err");
  } else if (r && r.error_kind === "network") {
    setMsg("模型探测暂时不可达；探测未写入配置，仍可手工填写精确模型 ID并保存。", "err");
  } else {
    setMsg("已读取 " + models.length + " 个候选模型。只有你在四个模型框中明确选择的 ID 才会保存；探测本身未修改配置。", "ok");
  }
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

function codexModelDetails(model) {
  const id = typeof model?.id === "string" ? model.id : "";
  const efforts = Array.isArray(model?.reasoning_efforts)
    ? model.reasoning_efforts.filter((effort) => typeof effort === "string" && effort.length > 0)
    : [];
  const defaultEffort = typeof model?.default_reasoning_effort === "string"
    ? model.default_reasoning_effort
    : "";
  const effortText = efforts.length
    ? "effort: " + efforts.join(" / ") + (defaultEffort ? "（默认 " + defaultEffort + "）" : "")
    : "effort: 目录未声明";
  const reviewerText = model?.reviewer_compatible === true
    ? "Reviewer 可选"
    : model?.reviewer_compatible === false
      ? "Reviewer 不兼容（Responses Lite 不支持强制 verdict 工具）"
      : "Reviewer 兼容性未知";
  return { id, effortText, reviewerText };
}

function renderCodexCatalog(meta, list, r) {
  const models = (r && r.models) || [];
  const source = (r && r.source) || "unknown";
  const sourceText = modelSourceLabel(source);
  const age = Number(r && r.age_seconds) || 0;
  meta.textContent = sourceText + " · " + models.length + " 个账号模型" + (age ? " · 缓存年龄 " + compactAge(age) : "");
  list.innerHTML = models.length
    ? models.map((m) => {
        const details = codexModelDetails(m);
        return '<div class="codex-model-item"><div class="codex-model-name">'
          + escapeHtml(codexModelLabel(m))
          + '</div><code class="codex-model-id">' + escapeHtml(details.id)
          + '</code><div class="codex-model-capabilities">' + escapeHtml(details.effortText)
          + " · " + escapeHtml(details.reviewerText) + "</div></div>";
      }).join("")
    : '<div class="codex-model-empty">账号目录当前没有可展示模型。</div>';
  if (source === "stale-cache" || (r && r.stale)) {
    setMsg("官方目录暂时不可达，当前展示过期缓存（年龄 " + compactAge(age) + "）。可用于识别已有模型，但请稍后刷新确认。", "err");
  } else if (r && r.error_kind === "protocol") {
    setMsg("Codex 模型目录响应与当前 CSSwitch 兼容基线不一致；这不是网络繁忙。没有模型被伪造或写入配置。", "err");
  } else if (r && r.error_kind === "network") {
    setMsg("官方 Codex 模型目录当前不可达，且没有可用缓存。没有模型被伪造或写入配置，请稍后重试。", "err");
  } else {
    setMsg("已读取 " + models.length + " 个 Codex 账号模型（" + sourceText + "）。普通模型与 Reviewer 模型均由你在 Science 中选择；Responses Lite 项不能用于 Reviewer。", "ok");
  }
}

// ── C2：新建向导 ──
function openWizard() {
  renderTemplateChips();
  const first = (getConfigState().templates || [])[0];
  selectWizTemplate(first ? first.id : "");
  showView("wizard");
  setMsg("选择来源，按提示填写连接信息后创建。");
}

function renderTemplateChips() {
  els.wizTemplateChips.innerHTML = (getConfigState().templates || []).map((t) => {
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
  els.wizTplHint.textContent = sourceHint(t) + " 新建后先设为当前选择，再点「一键开始」应用。";
  els.wizBaseGroup.hidden = codex;
  els.wizKeyGroup.hidden = codex;
  els.wizCodexCatalog.hidden = !codex;
  els.wizModelLabel.textContent = codex ? "账号模型目录" : "模型配置";
  els.wizCodexCatalogMeta.textContent = codex ? "尚未读取账号模型目录。" : "尚未探测模型。";
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
  els.wizSaveBtn.disabled = isBusy() || !t || invalidCatalog;
}

function openaiCustomAnthropicBaseMessage(t, base) {
  if (t && (t.id === "custom-openai" || t.id === "custom-openai-responses") && (base || "").trim().toLowerCase().includes("/anthropic")) {
    return "这个地址看起来是 Anthropic 兼容端点。请改选「自定义 Anthropic」，或填写 OpenAI 兼容 base root（如 https://api.moonshot.cn/v1）。";
  }
  return "";
}

async function wizFetch() {
  const t = tplById(els.wizTemplate.value);
  if (!t || !canFetchModels(t)) return;
  const codex = isCodexSource(t);
  setBusy(true, { kind: "fetchModels", id: "wizard" });
  startFetchModelsFeedback("wizard", codex);
  try {
    const r = await call("fetch_models", { req: {
      template_id: t.id,
      api_format: t.api_format || "",
      base_url: codex ? "" : els.wizBase.value.trim(),
      key: codex ? "" : els.wizKey.value.trim(),
    } });
    if (codex) renderCodexCatalog(els.wizCodexCatalogMeta, els.wizCodexCatalogList, r);
    else renderStaticCatalogDiscovery("wizard", r);
  } catch (e) {
    setMsg("获取模型失败：" + codexController.runtimeCommandErrorText(e), "err");
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
    await loadConfigAfterCommit();
    setMsg(codex
      ? "已创建「" + name + "」。先设为当前；一键开始后请在 Science 的 More models 选择 Codex / …。"
      : "已创建「" + name + "」。请设为当前选择，再点「一键开始」应用。", "ok");
  } catch (e) {
    setMsg(e && e.configCommitted ? committedRefreshMessage("创建配置", e) : "创建失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

// ── C3：连接编辑（base_url/model/key）+ 清 key ──
function currentConn() {
  const id = els.connSec.dataset.id;
  return (getConfigState().profiles || []).find((x) => x.id === id) || null;
}

function openConn(id) {
  const p = (getConfigState().profiles || []).find((x) => x.id === id);
  if (!p) return;
  const t = tplById(p.template_id);
  const capSrc = profileCapabilitySource(p, t);
  const codex = isCodexSource(capSrc || p);
  if (codex && !getConfigState().experimental_codex_enabled) {
    setMsg("Codex 实验入口已关闭；仍可改名或删除配置，也可在“设置 > Codex 账号与连接”检查/退出 OAuth。", "err");
    return;
  }
  const editable = t ? t.base_url_editable : true;
  const selected = id === getConfigState().active_id;
  els.connSec.dataset.id = id;
  els.connTitle.textContent = (codex ? "Codex 账号模型 · " : "编辑连接 · ") + p.name + (selected ? "（当前选择）" : "");
  els.connBaseGroup.hidden = codex;
  els.connKeyGroup.hidden = codex;
  els.connCodexCatalog.hidden = !codex;
  els.connModelLabel.textContent = codex ? "账号模型目录" : "模型配置";
  els.connSaveBtn.hidden = codex;
  els.connClearBtn.hidden = codex;
  els.connCancelBtn.textContent = codex ? "返回" : "取消";
  els.connCodexCatalogMeta.textContent = codex ? "尚未读取账号模型目录。" : "尚未探测模型。";
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
    : (selected
      ? "编辑当前选择：保存只更新候选连接；当前运行链保持不变，下次一键开始时核验并应用。"
      : "编辑连接后点「保存连接」。"));
}

function refreshConnGate() {
  const p = currentConn();
  let invalidCatalog = false;
  if (p && !isCodexSource(p)) {
    try { catalogSubmission("connection"); } catch (_) { invalidCatalog = true; }
  }
  els.connSaveBtn.disabled = isBusy() || !p || invalidCatalog;
}

async function connFetch() {
  const p = currentConn();
  if (!p) return;
  const t = tplById(p.template_id);
  const capSrc = p.capabilities ? p : t;
  if (!canFetchModels(capSrc)) return;
  const codex = isCodexSource(capSrc);
  setBusy(true, { kind: "fetchModels", id: p.id });
  startFetchModelsFeedback(p.id, codex);
  try {
    const r = await call("fetch_models", {
      req: {
        template_id: p.template_id,
        api_format: p.api_format || (t ? t.api_format : ""),
        base_url: codex ? "" : els.connBase.value.trim(),
        key: codex ? "" : els.connKey.value.trim(),
        profile_id: p.id,
      },
    });
    if (codex) renderCodexCatalog(els.connCodexCatalogMeta, els.connCodexCatalogList, r);
    else renderStaticCatalogDiscovery("connection", r);
  } catch (e) {
    setMsg("获取模型失败：" + codexController.runtimeCommandErrorText(e), "err");
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
  const selected = p.id === getConfigState().active_id;
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
  startSaveConnectionFeedback(p.id, selected);
  try {
    const r = await call("update_profile_connection", args);
    const intent = parseConfigIntentOutcome(r);
    if (!intent || intent.operation !== "update_profile_connection") throw new Error("连接保存结果协议不匹配。");
    els.connKey.value = "";
    await loadConfigAfterCommit();
    if (r && (r.status === "error" || r.committed === false)) {
      const recovery = r.recovery_status === "degraded" ? "；恢复也未完全成功，请先全部停止后检查" : r.recovery_status === "restored" ? "；旧配置已恢复" : "";
      setMsg((r.message || "连接未应用") + recovery + "（阶段：" + (r.stage || "unknown") + "）", "err");
    } else if (selected) {
      setMsg((r && (r.message || r.hint)) || "已保存连接；当前运行链保持不变，下次一键开始时应用。", "ok");
    } else if (r && r.validated) {
      setMsg("已保存连接（已通过上游校验）。", "ok");
    } else {
      setMsg("已保存连接（未能连通上游校验；下次一键开始时会再验并应用）。", "ok");
    }
  } catch (e) {
    // 后端错误文案已如实说明回滚/代理状态（可能是「已回滚到原配置」或「回滚未成功：代理当前已停」），
    // 前端不再盲目追加「仍在用原配置运行」，避免与「代理已停」相互矛盾。修 GPT 三轮 P2
    setMsg(e && e.configCommitted
      ? committedRefreshMessage("连接配置", e)
      : "连接未保存：" + codexController.runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
    await runtime.refreshStatus();
    await getSkillPage()?.refreshIfLoaded();
  }
}

// 清 key（行内 / 连接表单都可触发）：二次确认后 clear_profile_key。
function clearKey(id) {
  const p = (getConfigState().profiles || []).find((x) => x.id === id);
  const nm = p ? p.name : id;
  confirmAction("clearkey:" + id, "将清除「" + nm + "」的 API key（需重填才能用）", () => doClearKey(id));
}
async function doClearKey(id) {
  const wasSelected = id === getConfigState().active_id;
  setBusy(true);
  setMsg("清除 key 中…");
  try {
    const outcome = parseConfigMutationResponse(await call("clear_profile_key", { id }));
    const appliedMutation = isExactCompletedConfigMutation(
      outcome,
      "clear_applied_profile_key",
      ["stopped"]
    );
    const intentMutation = isExactConfigIntent(
      outcome,
      "clear_profile_key",
      ["committed", "no_change"]
    );
    if (!appliedMutation && !intentMutation) {
      throw new Error("清除 key 结果协议不匹配。");
    }
    await loadConfigAfterCommit();
    setMsg(
      appliedMutation
        ? "已清除 key（该配置属于上次提交的运行绑定，代理已停止；请重新填写并一键开始）。"
        : outcome.disposition === "no_change"
        ? "配置已不存在，无需清除 key。"
        : wasSelected
        ? "已清除当前选择的 key；当前运行链保持不变，重新填写后再一键开始。"
        : "已清除 key。",
      "ok"
    );
  } catch (e) {
    setMsg(e && e.configCommitted ? committedRefreshMessage("清除 key", e) : "清除失败：" + mutationErrorText(e), "err");
  } finally {
    setBusy(false);
    await runtime.refreshStatus();
  }
}

// ── C4：改名/备注 + 删除 + 设为当前 ──
function openMeta(id) {
  const p = (getConfigState().profiles || []).find((x) => x.id === id);
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
    await loadConfigAfterCommit();
    setMsg("已保存。", "ok");
  } catch (e) {
    setMsg(e && e.configCommitted ? committedRefreshMessage("配置元数据", e) : "保存失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

function del(id) {
  const p = (getConfigState().profiles || []).find((x) => x.id === id);
  const nm = p ? p.name : id;
  confirmAction("delete:" + id, "将删除配置「" + nm + "」", () => doDelete(id));
}
async function doDelete(id) {
  const wasSelected = id === getConfigState().active_id;
  setBusy(true);
  setMsg("删除中…");
  try {
    const outcome = parseConfigMutationResponse(await call("delete_profile", { id }));
    const appliedMutation = isExactCompletedConfigMutation(
      outcome,
      "delete_applied_profile",
      ["stopped"]
    );
    const intentMutation = isExactConfigIntent(
      outcome,
      "delete_profile",
      ["committed", "no_change"]
    );
    if (!appliedMutation && !intentMutation) {
      throw new Error("删除配置结果协议不匹配。");
    }
    await loadConfigAfterCommit();
    setMsg(
      appliedMutation
        ? "已删除上次提交的运行绑定配置，相关代理已停止。"
        : outcome.disposition === "no_change"
        ? "配置已不存在，无需再次删除。"
        : wasSelected
        ? "已删除当前选择，请重新选择一条并「设为当前」。"
        : "已删除。",
      "ok"
    );
  } catch (e) {
    setMsg(e && e.configCommitted ? committedRefreshMessage("删除配置", e) : "删除失败：" + mutationErrorText(e), "err");
  } finally {
    setBusy(false);
    await runtime.refreshStatus();
  }
}

// 设为当前只保存选择；真正 apply/start 的唯一边界是一键开始。
async function activate(id) {
  if (isActivationInFlight()) {
    setMsg("当前选择仍在保存。请稍后再提交另一条配置。");
    return;
  }
  const target = (getConfigState().profiles || []).find((p) => p.id === id);
  const codex = isCodexSource(target);
  if (codex && !getConfigState().experimental_codex_enabled) {
    setMsg("Codex 实验入口已关闭。请先在“设置 > Codex 账号与连接”重新启用。", "err");
    return;
  }
  setActivationInFlight(true, { kind: "activate", id });
  startActivateFeedback(id);
  try {
    const r = await call("set_active_profile", { id });
    const intent = parseConfigIntentOutcome(r);
    if (!isExactActiveProfileIntent(r, intent, id)) {
      throw new Error("当前选择结果协议不匹配。");
    }
    runtime.hideHistoryRecovery();
    await loadConfigAfterCommit();
    getConfigState().selection_pending = r.apply_state === "pending";
    renderList();
    const selectionMessage = r.apply_state === "applied"
      ? "当前选择已是上次应用配置。"
      : intent.disposition === "no_change"
        ? "当前选择未变；仍待一键开始核验并应用。"
        : "已设为当前选择，待一键开始核验并应用。";
    setMsg(selectionMessage + (codex
      ? " 一键开始后，请在 Science 的 More models 中选择 Codex / …；默认 Claude 壳不会被静默映射。"
      : ""), "ok");
  } catch (e) {
    if (!(e && e.configCommitted)) await loadConfig();
    setMsg(e && e.configCommitted
      ? committedRefreshMessage("当前选择", e)
      : "设为当前失败：" + codexController.runtimeCommandErrorText(e), "err");
  } finally {
    setActivationInFlight(false);
    await runtime.refreshStatus();
  }
}

  return {
    isCodexSource,
    profileName,
    syncProfileBusyState,
    refreshWizGate,
    refreshConnGate,
    loadConfig,
    renderList,
    switchMode,
    heroClick,
    persistRuntimeSettings,
    openWizard,
    selectWizTemplate,
    catalogRolesChanged,
    wizFetch,
    wizSave,
    openConn,
    connFetch,
    connSave,
    clearKey,
    openMeta,
    metaSave,
    del,
    activate,
  };
}
