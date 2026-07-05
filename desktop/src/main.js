// CSSwitch desktop frontend. The UI is driven by the v2 profile schema:
// named profiles + active_id. Full API keys never enter this script; the
// backend only returns masked key tails.
const PREVIEW = !window.__TAURI__;
const invoke = PREVIEW ? (cmd, args) => mockInvoke(cmd, args) : window.__TAURI__.core.invoke;

const MOCK_TEMPLATES = [
  { id: "deepseek", name: "DeepSeek", category: "cn_official", api_format: "anthropic", adapter: "deepseek", base_url: "https://api.deepseek.com/anthropic", base_url_editable: false, requires_model_override: false, builtin_models: ["claude-opus-4-8", "claude-haiku-4-5"], website_url: "https://platform.deepseek.com", icon: "deepseek", icon_color: "#1E88E5" },
  { id: "glm", name: "智谱 GLM", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://open.bigmodel.cn/api/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["glm-5.2", "glm-4.7", "glm-4.6"], website_url: "https://open.bigmodel.cn", icon: "glm", icon_color: "#2E6BE6" },
  { id: "kimi", name: "Kimi（Moonshot）", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.moonshot.cn/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["kimi-k2.7-code", "kimi-k2.7-code-highspeed"], website_url: "https://platform.moonshot.cn", icon: "kimi", icon_color: "#16182F" },
  { id: "minimax", name: "MiniMax", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.minimaxi.com/anthropic", base_url_editable: true, requires_model_override: true, builtin_models: ["MiniMax-M3", "MiniMax-M2.7"], website_url: "https://platform.minimaxi.com", icon: "minimax", icon_color: "#E1341E" },
  { id: "qwen", name: "通义千问", category: "cn_official", api_format: "openai_chat", adapter: "qwen", base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1", base_url_editable: false, requires_model_override: false, builtin_models: ["qwen-max", "qwen-plus", "qwen-turbo"], website_url: "https://dashscope.aliyun.com", icon: "qwen", icon_color: "#615CED" },
  { id: "custom", name: "自定义", category: "custom", api_format: "anthropic", adapter: "relay", base_url: "", base_url_editable: true, requires_model_override: true, builtin_models: [], website_url: "", icon: "custom", icon_color: "#6B7280" },
];

const mockStore = {
  schema_version: 2,
  active_id: "",
  proxy_port: 18991,
  sandbox_port: 8990,
  mode: "proxy",
  profiles: [],
  remoteProfiles: [],
};

function mockMask(key) {
  return key ? "••••" + String(key).slice(-4) : "";
}

function mockInvoke(cmd, args) {
  args = args || {};
  switch (cmd) {
    case "get_config":
      return Promise.resolve({
        schema_version: 2,
        active_id: mockStore.active_id,
        proxy_port: mockStore.proxy_port,
        sandbox_port: mockStore.sandbox_port,
        mode: mockStore.mode,
        templates: MOCK_TEMPLATES,
        profiles: mockStore.profiles.map((p) => ({ ...p })),
      });
    case "list_templates":
      return Promise.resolve(MOCK_TEMPLATES);
    case "set_settings":
    case "set_config":
      mockStore.proxy_port = args.cfg.proxy_port;
      mockStore.sandbox_port = args.cfg.sandbox_port;
      return Promise.resolve(null);
    case "set_mode":
      mockStore.mode = args.mode;
      return Promise.resolve(null);
    case "create_profile": {
      const tpl = MOCK_TEMPLATES.find((t) => t.id === args.templateId) || MOCK_TEMPLATES[0];
      const id = "p-" + Math.random().toString(16).slice(2, 10);
      mockStore.profiles.push({
        id,
        name: args.name || tpl.name,
        template_id: tpl.id,
        category: tpl.category,
        api_format: tpl.api_format,
        base_url: args.baseUrl || tpl.base_url || "",
        model: args.model || "",
        key: mockMask(args.key || ""),
        icon: tpl.icon,
        icon_color: tpl.icon_color,
        website_url: tpl.website_url,
        notes: "",
      });
      return Promise.resolve(id);
    }
    case "update_profile_metadata": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (!p) return Promise.reject("找不到配置");
      p.name = args.name;
      p.notes = args.notes || "";
      return Promise.resolve(null);
    }
    case "update_profile_connection": {
      const p = mockStore.profiles.find((x) => x.id === args.id);
      if (!p) return Promise.reject("找不到配置");
      if (args.baseUrl != null) p.base_url = args.baseUrl;
      if (args.apiFormat != null) p.api_format = args.apiFormat;
      if (args.model != null) p.model = args.model;
      if (args.key) p.key = mockMask(args.key);
      return Promise.resolve(null);
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
      if (!p) return Promise.reject("找不到配置");
      mockStore.active_id = args.id;
      return Promise.resolve({ committed: true, active_id: args.id, hint: "预览模式：已设为当前。" });
    }
    case "fetch_models":
      return Promise.resolve({ models: [{ id: "glm-5.2", supports_tools: true }, { id: "glm-4.7", supports_tools: null }], source: "preview" });
    case "status":
      return Promise.resolve({ proxy: "amber", sandbox: "amber", upstream: "amber" });
    case "one_click_login":
      return Promise.resolve({ url: "http://127.0.0.1:8990", msg: "预览模式：假装已启动。" });
    case "stop_all":
    case "open_url":
    case "open_official":
    case "open_release_page":
    case "report_bug":
    case "open_logs":
    case "quit_app":
      return Promise.resolve(null);
    case "run_doctor":
      return Promise.resolve("预览模式：后端未运行。");
    case "app_version":
      return Promise.resolve("0.0.0-preview");
    case "remote_list_profiles":
      return Promise.resolve(mockStore.remoteProfiles.map((p) => ({ ...p })));
    case "remote_save_profile": {
      const p = args.profile;
      const i = mockStore.remoteProfiles.findIndex((x) => x.id === p.id);
      if (i >= 0) mockStore.remoteProfiles[i] = p;
      else mockStore.remoteProfiles.unshift(p);
      return Promise.resolve(p);
    }
    case "remote_delete_profile":
      mockStore.remoteProfiles = mockStore.remoteProfiles.filter((p) => p.id !== args.id);
      return Promise.resolve(true);
    case "remote_check_health":
      return Promise.resolve({ reachable: true, helperInstalled: false, compatible: false, platform: "linux", arch: "x86_64", proxyRunning: false, sandboxRunning: false, lastError: "预览模式" });
    case "remote_status":
      return Promise.resolve({ proxy: "amber", sandbox: "amber", upstream: "amber", remote: true });
    case "remote_start_proxy":
      return Promise.resolve({ ok: true, port: args.port });
    case "remote_one_click":
      return Promise.resolve({
        ok: true,
        proxy_port: args.proxyPort,
        sandbox_port: args.sandboxPort,
        local_url: "http://127.0.0.1:" + args.sandboxPort,
        tunnel_hint: "ssh -N -L " + args.sandboxPort + ":127.0.0.1:" + args.sandboxPort + " user@host",
      });
    case "remote_stop_proxy":
      return Promise.resolve(null);
    case "remote_logs":
      return Promise.resolve({ content: "预览模式：无日志" });
    case "remote_doctor":
      return Promise.resolve({ checks: [{ name: "预览模式", ok: true }] });
    default:
      return Promise.resolve(null);
  }
}

const $ = (id) => document.getElementById(id);
const els = {};
let busy = false;
let mode = "proxy";
let target = "local";
let currentProfile = null;
let remoteProfiles = [];
let statusTimer = null;
let editingConnId = null;
let editingMetaId = null;
let pendingSkipActivateId = null;
let pendingConfirm = null;

let state = {
  profiles: [],
  templates: [],
  active_id: "",
  proxy_port: 18991,
  sandbox_port: 8990,
};

const CAT_LABELS = { official: "官方", cn_official: "国内", custom: "自定义" };

async function call(cmd, args) {
  return await invoke(cmd, args);
}

function escapeHtml(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function setMsg(text, kind) {
  if (!els.msg) return;
  els.msg.textContent = text || "";
  els.msg.className = "msg" + (kind ? " " + kind : "");
  const feedback = els.msg.closest(".feedback");
  if (feedback) feedback.hidden = !text;
}

function setLight(el, value) {
  if (!el) return;
  const cls = { green: "g", amber: "a", red: "r" }[value] || "a";
  el.className = "lt " + cls;
}

function setBusy(on) {
  busy = on;
  [
    "oneClickBtn", "stopBtn", "newBtn", "skipActivateBtn",
    "wizFetchBtn", "wizSaveBtn", "wizCancelBtn",
    "connFetchBtn", "connSaveBtn", "connClearBtn", "connCancelBtn",
    "metaSaveBtn", "metaCancelBtn", "manageProfilesBtn", "addProfileBtn",
    "saveProfileBtn", "testProfileBtn",
  ].forEach((id) => {
    if (els[id]) els[id].disabled = on;
  });
  if (!on) {
    refreshWizGate();
    refreshConnGate();
  }
}

function templateById(id) {
  return (state.templates || []).find((t) => t.id === id) || null;
}

function activeLocalProfile() {
  return (state.profiles || []).find((p) => p.id === state.active_id) || null;
}

function adapterForProfile(profile) {
  const tpl = templateById(profile && profile.template_id);
  return (tpl && tpl.adapter) || profile?.template_id || "relay";
}

function defaultModel(tpl) {
  return (tpl && tpl.builtin_models && tpl.builtin_models[0]) || "";
}

function showView(view) {
  els.panel.classList.toggle("view-form", view !== "list");
  els.listSec.hidden = view !== "list";
  els.advSec.hidden = view !== "list";
  els.wizSec.hidden = view !== "wizard";
  els.connSec.hidden = view !== "conn";
  els.metaSec.hidden = view !== "meta";
  if (view === "list") hideSkip();
}

function hideSkip() {
  pendingSkipActivateId = null;
  if (els.skipActivateBtn) els.skipActivateBtn.hidden = true;
}

function showSkip(id) {
  pendingSkipActivateId = id;
  els.skipActivateBtn.hidden = false;
}

function confirmAction(token, prompt, fn) {
  if (pendingConfirm && pendingConfirm.token === token) {
    clearTimeout(pendingConfirm.timer);
    pendingConfirm = null;
    fn();
    return;
  }
  if (pendingConfirm) clearTimeout(pendingConfirm.timer);
  pendingConfirm = {
    token,
    timer: setTimeout(() => {
      pendingConfirm = null;
      setMsg("已取消。");
    }, 4000),
  };
  setMsg(prompt + "。4 秒内再点一次确认。", "err");
}

async function loadConfig() {
  try {
    const cfg = await call("get_config");
    state.profiles = cfg.profiles || [];
    state.templates = cfg.templates || await call("list_templates");
    state.active_id = cfg.active_id || "";
    state.proxy_port = cfg.proxy_port ?? 18991;
    state.sandbox_port = cfg.sandbox_port ?? 8990;
    els.proxyPort.value = state.proxy_port;
    els.sandboxPort.value = state.sandbox_port;
    applyMode(cfg.mode === "official" ? "official" : "proxy");
    renderList();
    showView("list");
    if (cfg.pending_notice) setMsg(cfg.pending_notice, "ok");
  } catch (e) {
    setMsg("读取配置失败：" + e, "err");
  }
}

function modelSummary(profile) {
  if (profile.model) return escapeHtml(profile.model);
  const tpl = templateById(profile.template_id);
  return tpl && tpl.requires_model_override ? "未选模型" : "内置映射";
}

function renderList() {
  const profiles = state.profiles || [];
  if (!profiles.length) {
    els.profileList.innerHTML = '<div class="empty">还没有配置。点右上「＋ 新建」加一条第三方来源。</div>';
    return;
  }
  els.profileList.innerHTML = profiles.map((p) => {
    const active = p.id === state.active_id;
    const cat = CAT_LABELS[p.category] || p.category || "";
    const key = p.key ? escapeHtml(p.key) : "未填 key";
    const dot = p.icon_color ? ' style="background:' + escapeHtml(p.icon_color) + '"' : "";
    return (
      '<div class="prow' + (active ? " pactive" : "") + '" data-id="' + escapeHtml(p.id) + '">' +
        '<div class="prow-top">' +
          '<span class="pico"' + dot + "></span>" +
          '<span class="pname">' + escapeHtml(p.name) + "</span>" +
          '<span class="badge">' + escapeHtml(cat) + "</span>" +
          (active ? '<span class="badge on">当前生效</span>' : "") +
        "</div>" +
        '<div class="pmeta">' + escapeHtml(p.base_url || "（未填地址）") + "</div>" +
        '<div class="pmeta">模型：' + modelSummary(p) + " · Key：" + key + "</div>" +
        '<div class="prow-acts">' +
          (active ? "" : '<button class="abtn prim" data-act="activate">设为当前</button>') +
          '<button class="abtn" data-act="editconn">编辑连接</button>' +
          '<button class="abtn" data-act="editmeta">改名</button>' +
          '<button class="abtn" data-act="clearkey">清 key</button>' +
          '<button class="abtn danger" data-act="delete">删除</button>' +
        "</div>" +
      "</div>"
    );
  }).join("");
}

function applyMode(nextMode) {
  mode = nextMode === "official" ? "official" : "proxy";
  els.panel.classList.toggle("mode-official", mode === "official");
  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset.mode === mode);
  });
  els.oneClickBtn.textContent = mode === "official" ? "打开官方 Claude Science ↗" : "⚡ 一键开始";
}

async function switchMode(nextMode) {
  if (nextMode === mode) return;
  setBusy(true);
  try {
    await call("set_mode", { mode: nextMode });
    applyMode(nextMode);
    setMsg(nextMode === "official" ? "已切到官方模式。" : "已切到第三方模式。", "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("切换模式失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function persistPorts() {
  const cfg = {
    proxy_port: parseInt(els.proxyPort.value, 10) || 18991,
    sandbox_port: parseInt(els.sandboxPort.value, 10) || 8990,
  };
  try {
    try {
      await call("set_settings", { cfg });
    } catch (e) {
      await call("set_config", { cfg });
    }
    state.proxy_port = cfg.proxy_port;
    state.sandbox_port = cfg.sandbox_port;
    setMsg("端口设置已保存。", "ok");
  } catch (e) {
    setMsg("保存端口失败：" + e, "err");
  }
}

function renderTemplateChips() {
  els.wizTemplateChips.innerHTML = (state.templates || []).map((t) => {
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

function setModelOptions(input, datalist, models, fallbackValue) {
  const ids = (models || []).map((m) => typeof m === "string" ? m : m.id).filter(Boolean);
  datalist.innerHTML = ids.map((id) => '<option value="' + escapeHtml(id) + '"></option>').join("");
  input.value = fallbackValue || ids[0] || "";
}

function selectWizTemplate(id) {
  const tpl = templateById(id) || state.templates[0];
  if (!tpl) return;
  els.wizTemplate.value = tpl.id;
  els.wizTemplateChips.querySelectorAll(".chip").forEach((c) => {
    const selected = c.dataset.tid === tpl.id;
    c.classList.toggle("sel", selected);
    c.setAttribute("aria-pressed", selected ? "true" : "false");
  });
  els.wizName.value = tpl.name;
  els.wizBase.value = tpl.base_url || "";
  els.wizBase.disabled = !tpl.base_url_editable;
  els.wizTplHint.textContent = tpl.base_url_editable ? "可按你的套餐或区域端点修改地址。" : "该来源使用内置官方地址。";
  els.wizBaseHint.textContent = tpl.base_url_editable ? "" : "地址由适配器内置。";
  setModelOptions(els.wizModel, els.wizModelList, tpl.builtin_models || [], defaultModel(tpl));
  els.wizModelInfo.hidden = !!tpl.requires_model_override;
  els.wizModel.hidden = !tpl.requires_model_override;
  els.wizModelHint.textContent = tpl.requires_model_override ? "请选择或输入上游真实模型名。" : "该来源使用内置模型映射。";
  if (!tpl.requires_model_override) {
    els.wizModelInfo.textContent = "使用内置模型映射，无需手动选择。";
    els.wizModel.value = "";
  }
  refreshWizGate();
}

function refreshWizGate() {
  if (!els.wizSaveBtn || els.wizSec.hidden) return;
  const tpl = templateById(els.wizTemplate.value);
  const needsModel = tpl && tpl.requires_model_override;
  const needsBase = tpl && tpl.base_url_editable;
  els.wizSaveBtn.disabled = busy ||
    !els.wizName.value.trim() ||
    (needsBase && !els.wizBase.value.trim()) ||
    (needsModel && !els.wizModel.value.trim());
}

function openWizard() {
  hideSkip();
  renderTemplateChips();
  selectWizTemplate((state.templates[0] || {}).id || "");
  els.wizKey.value = "";
  showView("wizard");
  setMsg("选择来源，填 key 即可创建。");
}

async function createProfile() {
  const tpl = templateById(els.wizTemplate.value);
  if (!tpl) return;
  const args = {
    templateId: tpl.id,
    name: els.wizName.value.trim() || tpl.name,
    key: els.wizKey.value.trim() || null,
    baseUrl: els.wizBase.value.trim() || null,
    model: tpl.requires_model_override ? els.wizModel.value.trim() : null,
  };
  setBusy(true);
  try {
    await call("create_profile", args);
    els.wizKey.value = "";
    await loadConfig();
    setMsg("已创建配置。需要使用时点「设为当前」。", "ok");
  } catch (e) {
    setMsg("创建失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

function openConn(id) {
  const p = state.profiles.find((x) => x.id === id);
  if (!p) return;
  const tpl = templateById(p.template_id) || {};
  editingConnId = id;
  els.connTitle.textContent = "编辑连接：" + p.name;
  els.connBase.value = p.base_url || tpl.base_url || "";
  els.connBase.disabled = !tpl.base_url_editable;
  els.connKey.value = "";
  setModelOptions(els.connModel, els.connModelList, tpl.builtin_models || [], p.model || defaultModel(tpl));
  els.connModelInfo.hidden = !!tpl.requires_model_override;
  els.connModel.hidden = !tpl.requires_model_override;
  els.connModelHint.textContent = tpl.requires_model_override ? "请选择或输入上游真实模型名。" : "该来源使用内置模型映射。";
  if (!tpl.requires_model_override) {
    els.connModelInfo.textContent = "使用内置模型映射，无需手动选择。";
    els.connModel.value = "";
  }
  showView("conn");
  setMsg("留空 key 表示不修改已存 key。");
  refreshConnGate();
}

function refreshConnGate() {
  if (!els.connSaveBtn || els.connSec.hidden || !editingConnId) return;
  const p = state.profiles.find((x) => x.id === editingConnId);
  const tpl = templateById(p && p.template_id);
  const needsModel = tpl && tpl.requires_model_override;
  const needsBase = tpl && tpl.base_url_editable;
  els.connSaveBtn.disabled = busy ||
    (needsBase && !els.connBase.value.trim()) ||
    (needsModel && !els.connModel.value.trim());
}

async function saveConn() {
  const p = state.profiles.find((x) => x.id === editingConnId);
  const tpl = templateById(p && p.template_id);
  if (!p || !tpl) return;
  setBusy(true);
  try {
    await call("update_profile_connection", {
      id: p.id,
      baseUrl: els.connBase.value.trim(),
      apiFormat: tpl.api_format,
      model: tpl.requires_model_override ? els.connModel.value.trim() : "",
      key: els.connKey.value.trim() || null,
    });
    await loadConfig();
    setMsg("连接已保存。", "ok");
  } catch (e) {
    setMsg("保存连接失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

function openMeta(id) {
  const p = state.profiles.find((x) => x.id === id);
  if (!p) return;
  editingMetaId = id;
  els.metaName.value = p.name || "";
  els.metaNotes.value = p.notes || "";
  showView("meta");
  setMsg("修改名称或备注不会触发代理。");
}

async function saveMeta() {
  setBusy(true);
  try {
    await call("update_profile_metadata", {
      id: editingMetaId,
      name: els.metaName.value.trim() || "未命名",
      notes: els.metaNotes.value.trim() || null,
    });
    await loadConfig();
    setMsg("已保存。", "ok");
  } catch (e) {
    setMsg("保存失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function activateProfile(id, skipVerify) {
  setBusy(true);
  try {
    const result = await call("set_active_profile", { id, skipVerify: !!skipVerify });
    if (result && result.committed === false) {
      showSkip(id);
      setMsg(result.hint || "校验未通过，未切换。", "err");
      return;
    }
    await loadConfig();
    setMsg((result && result.hint) || "已设为当前。", "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("切换失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function clearProfileKey(id) {
  setBusy(true);
  try {
    await call("clear_profile_key", { id });
    await loadConfig();
    setMsg("Key 已清除。", "ok");
  } catch (e) {
    setMsg("清除失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function deleteProfile(id) {
  setBusy(true);
  try {
    await call("delete_profile", { id });
    await loadConfig();
    setMsg("配置已删除。", "ok");
  } catch (e) {
    setMsg("删除失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function fetchModelsFor(kind) {
  const isConn = kind === "conn";
  const p = isConn ? state.profiles.find((x) => x.id === editingConnId) : null;
  const tpl = templateById(isConn ? p?.template_id : els.wizTemplate.value);
  if (!tpl) return;
  const base = (isConn ? els.connBase.value : els.wizBase.value).trim();
  const key = (isConn ? els.connKey.value : els.wizKey.value).trim();
  const hint = isConn ? els.connModelHint : els.wizModelHint;
  const input = isConn ? els.connModel : els.wizModel;
  const list = isConn ? els.connModelList : els.wizModelList;
  setBusy(true);
  hint.textContent = "正在获取模型…";
  try {
    const res = await call("fetch_models", {
      req: {
        template_id: tpl.id,
        base_url: base,
        key,
        profile_id: p ? p.id : null,
      },
    });
    setModelOptions(input, list, res.models || [], input.value);
    hint.textContent = "已获取模型列表" + (res.source ? "（" + res.source + "）" : "") + "。";
  } catch (e) {
    hint.textContent = "获取失败：" + e;
  } finally {
    setBusy(false);
  }
}

async function oneClick() {
  if (mode === "official") {
    await openOfficial();
    return;
  }
  if (target === "remote") {
    await remoteOneClick();
    return;
  }
  if (!state.active_id) {
    setMsg("还没有当前生效的配置。请先「＋ 新建」或在列表点「设为当前」。", "err");
    return;
  }
  setBusy(true);
  setMsg("一键开始：起代理 → 起沙箱 → 探活…");
  try {
    const r = await call("one_click_login");
    setMsg((r.msg || "已就绪。") + "\n" + (r.url || ""), "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("一键开始失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function openOfficial() {
  setBusy(true);
  try {
    await call("open_official");
    setMsg("已打开官方 Claude Science。", "ok");
  } catch (e) {
    setMsg("打开失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function stopAll() {
  setBusy(true);
  try {
    if (target === "remote" && currentProfile) {
      await call("remote_stop_proxy", { profile: currentProfile });
      setMsg("远程代理已停止。", "ok");
    } else {
      await call("stop_all");
      setMsg("已停止代理与沙箱。", "ok");
    }
    await refreshStatus();
  } catch (e) {
    setMsg("停止失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function refreshStatus() {
  try {
    const s = target === "remote" && currentProfile
      ? await call("remote_status", { profile: currentProfile })
      : await call("status");
    setLight(els.ltProxy, s.proxy);
    setLight(els.ltSandbox, s.sandbox);
    setLight(els.ltUpstream, s.upstream);
    els.brandDot.className = "dot" + (s.proxy === "green" ? "" : " amber");
  } catch (e) {
    [els.ltProxy, els.ltSandbox, els.ltUpstream].forEach((el) => setLight(el, "amber"));
  }
}

async function openBrowser() {
  try {
    await call("open_url", {});
  } catch (e) {
    setMsg("打开浏览器失败：" + e, "err");
  }
}

async function runDoctor() {
  setMsg(target === "remote" ? "远程自检中…" : "自检中…");
  try {
    const out = target === "remote" && currentProfile
      ? await call("remote_doctor", { profile: currentProfile })
      : await call("run_doctor");
    setMsg(typeof out === "string" ? out : JSON.stringify(out.checks || out, null, 2), "ok");
  } catch (e) {
    setMsg("自检失败：" + e, "err");
  }
}

function isNewer(a, b) {
  const aa = String(a).split(".").map((n) => parseInt(n, 10) || 0);
  const bb = String(b).split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(aa.length, bb.length); i++) {
    if ((aa[i] || 0) !== (bb[i] || 0)) return (aa[i] || 0) > (bb[i] || 0);
  }
  return false;
}

async function checkUpdate() {
  setMsg("检查更新中…");
  let cur = "";
  try { cur = await call("app_version"); } catch (e) {}
  try {
    const resp = await fetch("https://api.github.com/repos/SuperJJ007/CSswitch/releases/latest", {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!resp.ok) throw new Error("HTTP " + resp.status);
    const data = await resp.json();
    const latest = String(data.tag_name || "").replace(/^v/, "");
    if (latest && isNewer(latest, cur)) {
      setMsg("发现新版本 v" + latest + "。正在打开下载页。", "ok");
      await call("open_release_page");
    } else {
      setMsg("已是最新版本（v" + cur + "）。", "ok");
    }
  } catch (e) {
    setMsg("无法自动检查更新，已打开 Releases 页。", "err");
    try { await call("open_release_page"); } catch (_) {}
  }
}

async function switchTarget(nextTarget) {
  if (nextTarget === target) return;
  target = nextTarget === "remote" ? "remote" : "local";
  els.panel.classList.toggle("target-remote", target === "remote");
  els.targetSeg.querySelectorAll(".seg-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset.target === target);
  });
  if (target === "remote") {
    await loadRemoteProfiles();
    setMsg("已切换到远程服务器。");
  } else {
    setMsg("已切换到本地模式。");
  }
  await refreshStatus();
}

async function loadRemoteProfiles() {
  try {
    remoteProfiles = await call("remote_list_profiles");
    els.profileSelect.innerHTML = '<option value="">-- 选择服务器 --</option>' + remoteProfiles.map((p) =>
      '<option value="' + escapeHtml(p.id) + '">' + escapeHtml(p.name) + " (" + escapeHtml(p.username) + "@" + escapeHtml(p.host) + ":" + escapeHtml(p.port) + ")</option>"
    ).join("");
    if (currentProfile && remoteProfiles.some((p) => p.id === currentProfile.id)) {
      els.profileSelect.value = currentProfile.id;
    } else {
      currentProfile = null;
    }
    updateRemoteHealthUI();
  } catch (e) {
    setMsg("加载服务器失败：" + e, "err");
  }
}

async function onProfileChange() {
  const id = els.profileSelect.value;
  currentProfile = remoteProfiles.find((p) => p.id === id) || null;
  updateRemoteHealthUI();
  if (currentProfile) await checkRemoteHealth();
}

function updateRemoteHealthUI() {
  if (!currentProfile) {
    els.remoteHealthDot.className = "lt a";
    els.remoteHealthText.textContent = "未连接";
    return;
  }
  els.remoteHealthDot.className = "lt a";
  els.remoteHealthText.textContent = "已选：" + currentProfile.name;
}

async function checkRemoteHealth() {
  if (!currentProfile) return;
  els.remoteHealthDot.className = "lt a pulsing";
  els.remoteHealthText.textContent = "连接中…";
  try {
    const h = await call("remote_check_health", { profile: currentProfile });
    if (h.reachable && h.helperInstalled && h.compatible) {
      els.remoteHealthDot.className = "lt g";
      els.remoteHealthText.textContent = "已连接 | " + (h.platform || "?") + " " + (h.arch || "?");
    } else if (h.reachable) {
      els.remoteHealthDot.className = "lt a";
      els.remoteHealthText.textContent = h.lastError || "已连接，Helper 需要安装或升级";
    } else {
      els.remoteHealthDot.className = "lt r";
      els.remoteHealthText.textContent = h.lastError || "连接失败";
    }
  } catch (e) {
    els.remoteHealthDot.className = "lt r";
    els.remoteHealthText.textContent = "检查失败：" + e;
  }
}

async function openProfileModal() {
  await loadRemoteProfiles();
  const list = document.getElementById("remoteProfileList");
  list.innerHTML = remoteProfiles.length
    ? remoteProfiles.map((p) => (
      '<div class="profile-item">' +
        '<div><div class="pi-name">' + escapeHtml(p.name) + '</div>' +
        '<div class="pi-detail">' + escapeHtml(p.username) + "@" + escapeHtml(p.host) + ":" + escapeHtml(p.port) + "</div></div>" +
        '<div class="pi-actions">' +
          '<span class="pi-act" data-action="edit" data-id="' + escapeHtml(p.id) + '">编辑</span>' +
          '<span class="pi-act del" data-action="delete" data-id="' + escapeHtml(p.id) + '">删除</span>' +
        "</div>" +
      "</div>"
    )).join("")
    : '<div class="hint">暂无服务器。点击「+ 添加」。</div>';
  els.profileModal.style.display = "flex";
}

function closeProfileModal() {
  els.profileModal.style.display = "none";
}

function openProfileEdit(id) {
  const p = id ? remoteProfiles.find((x) => x.id === id) : null;
  els.profileEditModal.dataset.editId = id || "";
  els.profileEditTitle.textContent = p ? "编辑服务器" : "添加服务器";
  els.editProfileName.value = p ? p.name : "";
  els.editProfileHost.value = p ? p.host : "";
  els.editProfilePort.value = p ? p.port : 22;
  els.editProfileUsername.value = p ? p.username : "";
  const auth = p && p.authMethod ? p.authMethod : { type: "sshAgent" };
  els.editProfileAuth.value = auth.type === "keyFile" ? "key_file" : "ssh_agent";
  els.editProfileKeyPath.value = auth.path || "~/.ssh/id_ed25519";
  els.editProfileHelperPath.value = p ? p.helperPath : "~/.csswitch/bin/csswitch-helper";
  els.profileEditMsg.textContent = "";
  toggleKeyFileGroup();
  els.profileEditModal.style.display = "flex";
}

function closeProfileEdit() {
  els.profileEditModal.style.display = "none";
}

function toggleKeyFileGroup() {
  els.keyFileGroup.style.display = els.editProfileAuth.value === "key_file" ? "" : "none";
}

function newRemoteId() {
  return globalThis.crypto && crypto.randomUUID ? crypto.randomUUID() : "r-" + Date.now().toString(16);
}

async function saveProfile() {
  const editId = els.profileEditModal.dataset.editId;
  const authMethod = els.editProfileAuth.value === "key_file"
    ? { type: "keyFile", path: els.editProfileKeyPath.value.trim() }
    : { type: "sshAgent" };
  const profile = {
    id: editId || newRemoteId(),
    name: els.editProfileName.value.trim() || "未命名",
    host: els.editProfileHost.value.trim(),
    port: parseInt(els.editProfilePort.value, 10) || 22,
    username: els.editProfileUsername.value.trim(),
    authMethod,
    helperPath: els.editProfileHelperPath.value.trim() || "~/.csswitch/bin/csswitch-helper",
  };
  if (!profile.host || !profile.username) {
    els.profileEditMsg.textContent = "服务器地址和用户名不能为空。";
    els.profileEditMsg.className = "msg err";
    return;
  }
  try {
    await call("remote_save_profile", { profile });
    closeProfileEdit();
    await openProfileModal();
    await loadRemoteProfiles();
  } catch (e) {
    els.profileEditMsg.textContent = "保存失败：" + e;
    els.profileEditMsg.className = "msg err";
  }
}

async function testProfileConnection() {
  const authMethod = els.editProfileAuth.value === "key_file"
    ? { type: "keyFile", path: els.editProfileKeyPath.value.trim() }
    : { type: "sshAgent" };
  const profile = {
    id: "_test_",
    name: "test",
    host: els.editProfileHost.value.trim(),
    port: parseInt(els.editProfilePort.value, 10) || 22,
    username: els.editProfileUsername.value.trim(),
    authMethod,
    helperPath: els.editProfileHelperPath.value.trim() || "~/.csswitch/bin/csswitch-helper",
  };
  els.testProfileBtn.disabled = true;
  els.profileEditMsg.textContent = "正在测试连接…";
  try {
    const h = await call("remote_check_health", { profile });
    els.profileEditMsg.textContent = h.reachable ? "连接成功。" : (h.lastError || "连接失败");
    els.profileEditMsg.className = h.reachable ? "msg ok" : "msg err";
  } catch (e) {
    els.profileEditMsg.textContent = "连接失败：" + e;
    els.profileEditMsg.className = "msg err";
  } finally {
    els.testProfileBtn.disabled = false;
  }
}

async function remoteOneClick() {
  if (!currentProfile) {
    setMsg("请先选择远程服务器。", "err");
    return;
  }
  const active = activeLocalProfile();
  if (!active) {
    setMsg("请先在本地配置里选择一条当前生效的模型来源。", "err");
    return;
  }
  setBusy(true);
  try {
    const proxyPort = parseInt(els.proxyPort.value, 10) || 18991;
    const sandboxPort = parseInt(els.sandboxPort.value, 10) || 8990;
    const r = await call("remote_one_click", {
      profile: currentProfile,
      provider: adapterForProfile(active),
      proxyPort,
      sandboxPort,
    });
    const localUrl = (r && r.local_url) || ("http://127.0.0.1:" + sandboxPort);
    const tunnelHint = r && r.tunnel_hint ? "\n端口转发：" + r.tunnel_hint : "";
    setMsg("远程代理与沙箱已启动。\n本地访问：" + localUrl + tunnelHint, "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("远程一键开始失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

function wire() {
  [
    "oneClickBtn", "stopBtn", "ltProxy", "ltSandbox", "ltUpstream", "msg", "brandDot",
    "openBrowserBtn", "doctorBtn", "updateBtn", "verLabel", "reportBtn", "logsBtn", "quitBtn",
    "modeSeg", "targetSeg", "proxyPort", "sandboxPort", "advSec", "listSec", "profileList",
    "newBtn", "skipActivateBtn", "wizSec", "wizTemplate", "wizTemplateChips", "wizTplHint",
    "wizName", "wizBase", "wizBaseHint", "wizFetchBtn", "wizModelInfo", "wizModel",
    "wizModelList", "wizModelHint", "wizKey", "wizSaveBtn", "wizCancelBtn", "connSec",
    "connTitle", "connBase", "connBaseHint", "connFetchBtn", "connModelInfo", "connModel",
    "connModelList", "connModelHint", "connKey", "connSaveBtn", "connClearBtn", "connCancelBtn",
    "metaSec", "metaName", "metaNotes", "metaSaveBtn", "metaCancelBtn", "profileSelect",
    "manageProfilesBtn", "remoteHealthDot", "remoteHealthText", "profileModal", "addProfileBtn",
    "closeProfileModal", "profileEditModal", "profileEditTitle", "editProfileName",
    "editProfileHost", "editProfilePort", "editProfileUsername", "editProfileAuth",
    "editProfileKeyPath", "editProfileHelperPath", "keyFileGroup", "testProfileBtn",
    "saveProfileBtn", "cancelProfileEditBtn", "profileEditMsg",
  ].forEach((id) => { els[id] = $(id); });
  els.panel = document.querySelector(".panel");

  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => b.addEventListener("click", () => switchMode(b.dataset.mode)));
  els.targetSeg.querySelectorAll(".seg-btn").forEach((b) => b.addEventListener("click", () => switchTarget(b.dataset.target)));
  els.proxyPort.addEventListener("change", persistPorts);
  els.sandboxPort.addEventListener("change", persistPorts);
  els.newBtn.addEventListener("click", openWizard);
  els.skipActivateBtn.addEventListener("click", () => pendingSkipActivateId && activateProfile(pendingSkipActivateId, true));

  els.profileList.addEventListener("click", (e) => {
    if (busy) return;
    const button = e.target.closest("[data-act]");
    const row = e.target.closest("[data-id]");
    if (!button || !row) return;
    const id = row.dataset.id;
    const act = button.dataset.act;
    if (act === "activate") activateProfile(id, false);
    if (act === "editconn") openConn(id);
    if (act === "editmeta") openMeta(id);
    if (act === "clearkey") confirmAction("clear:" + id, "确定清除这条配置的 key", () => clearProfileKey(id));
    if (act === "delete") confirmAction("delete:" + id, "确定删除这条配置", () => deleteProfile(id));
  });

  els.wizTemplateChips.addEventListener("click", (e) => {
    const chip = e.target.closest("[data-tid]");
    if (chip) selectWizTemplate(chip.dataset.tid);
  });
  [els.wizName, els.wizBase, els.wizModel].forEach((el) => el.addEventListener("input", refreshWizGate));
  els.wizFetchBtn.addEventListener("click", () => fetchModelsFor("wiz"));
  els.wizSaveBtn.addEventListener("click", createProfile);
  els.wizCancelBtn.addEventListener("click", () => { showView("list"); setMsg(""); });

  [els.connBase, els.connModel].forEach((el) => el.addEventListener("input", refreshConnGate));
  els.connFetchBtn.addEventListener("click", () => fetchModelsFor("conn"));
  els.connSaveBtn.addEventListener("click", saveConn);
  els.connClearBtn.addEventListener("click", () => editingConnId && confirmAction("clear:" + editingConnId, "确定清除这条配置的 key", () => clearProfileKey(editingConnId)));
  els.connCancelBtn.addEventListener("click", () => { showView("list"); setMsg(""); });
  els.metaSaveBtn.addEventListener("click", saveMeta);
  els.metaCancelBtn.addEventListener("click", () => { showView("list"); setMsg(""); });

  els.oneClickBtn.addEventListener("click", oneClick);
  els.stopBtn.addEventListener("click", stopAll);
  els.openBrowserBtn.addEventListener("click", openBrowser);
  els.doctorBtn.addEventListener("click", runDoctor);
  els.updateBtn.addEventListener("click", checkUpdate);
  els.reportBtn.addEventListener("click", () => call("report_bug").catch((e) => setMsg("打开反馈页失败：" + e, "err")));
  els.logsBtn.addEventListener("click", () => {
    if (target === "remote" && currentProfile) {
      call("remote_logs", { profile: currentProfile, name: "proxy", lines: 80 })
        .then((out) => setMsg((out && out.content) || "日志为空。", "ok"))
        .catch((e) => setMsg("获取日志失败：" + e, "err"));
    } else {
      call("open_logs").catch((e) => setMsg("打开日志失败：" + e, "err"));
    }
  });
  els.quitBtn.addEventListener("click", () => call("quit_app").catch(() => {}));

  els.profileSelect.addEventListener("change", onProfileChange);
  els.manageProfilesBtn.addEventListener("click", openProfileModal);
  els.addProfileBtn.addEventListener("click", () => { closeProfileModal(); openProfileEdit(null); });
  els.closeProfileModal.addEventListener("click", closeProfileModal);
  els.saveProfileBtn.addEventListener("click", saveProfile);
  els.cancelProfileEditBtn.addEventListener("click", closeProfileEdit);
  els.testProfileBtn.addEventListener("click", testProfileConnection);
  els.editProfileAuth.addEventListener("change", toggleKeyFileGroup);
  document.querySelectorAll(".modal-overlay").forEach((overlay) => {
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.style.display = "none";
    });
  });
  document.getElementById("remoteProfileList").addEventListener("click", async (e) => {
    const action = e.target.closest("[data-action]");
    if (!action) return;
    const id = action.dataset.id;
    if (action.dataset.action === "edit") openProfileEdit(id);
    if (action.dataset.action === "delete") {
      confirmAction("remote-delete:" + id, "确定删除这个远程服务器", async () => {
        await call("remote_delete_profile", { id });
        await openProfileModal();
        await loadRemoteProfiles();
      });
    }
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  wire();
  await loadConfig();
  try { els.verLabel.textContent = "v" + await call("app_version"); } catch (e) {}
  await refreshStatus();
  if (PREVIEW) {
    setMsg("预览模式：只展示界面，不连接后端。");
  } else {
    statusTimer = setInterval(refreshStatus, 2500);
  }
});
