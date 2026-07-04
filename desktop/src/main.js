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
const invoke = PREVIEW
  ? (cmd, args) => mockInvoke(cmd, args)
  : window.__TAURI__.core.invoke;

// ── 预览兜底 mock（仅浏览器预览用；node --check 只验语法，真实 app 走真后端） ──
const MOCK_TEMPLATES = [
  { id: "deepseek", name: "DeepSeek", category: "cn_official", api_format: "anthropic", adapter: "deepseek", base_url: "https://api.deepseek.com/anthropic", base_url_editable: false, requires_model_override: false, builtin_models: ["claude-opus-4-8", "claude-haiku-4-5"], icon: "deepseek", icon_color: "#1E88E5", website_url: "https://platform.deepseek.com" },
  { id: "glm", name: "智谱 GLM", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://open.bigmodel.cn/api/anthropic", base_url_editable: false, requires_model_override: false, builtin_models: ["glm-4.6", "glm-5", "glm-4.5-air"], icon: "glm", icon_color: "#2E6BE6", website_url: "https://open.bigmodel.cn" },
  { id: "xiaomi", name: "小米 MiMo", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.xiaomimimo.com/anthropic", base_url_editable: false, requires_model_override: true, builtin_models: ["mimo-v2.5-pro"], icon: "xiaomi", icon_color: "#FF6900", website_url: "https://xiaomimimo.com" },
  { id: "siliconflow", name: "硅基流动", category: "cn_official", api_format: "anthropic", adapter: "relay", base_url: "https://api.siliconflow.cn", base_url_editable: false, requires_model_override: true, builtin_models: ["deepseek-ai/DeepSeek-V3", "zai-org/GLM-5.2"], icon: "siliconflow", icon_color: "#7C3AED", website_url: "https://siliconflow.cn" },
  { id: "openrouter", name: "OpenRouter", category: "custom", api_format: "anthropic", adapter: "relay", base_url: "https://openrouter.ai/api", base_url_editable: false, requires_model_override: false, builtin_models: ["anthropic/claude-sonnet-5", "anthropic/claude-opus-4.8-fast"], icon: "openrouter", icon_color: "#6467F2", website_url: "https://openrouter.ai" },
  { id: "qwen", name: "通义千问", category: "cn_official", api_format: "openai_chat", adapter: "qwen", base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1", base_url_editable: false, requires_model_override: false, builtin_models: ["qwen-max", "qwen-plus", "qwen-turbo"], icon: "qwen", icon_color: "#615CED", website_url: "https://dashscope.aliyun.com" },
  { id: "custom", name: "自定义", category: "custom", api_format: "anthropic", adapter: "relay", base_url: "", base_url_editable: true, requires_model_override: true, builtin_models: [], icon: "custom", icon_color: "#6B7280", website_url: "" },
];
const mockStore = {
  schema_version: 2,
  active_id: "",
  proxy_port: 18991,
  sandbox_port: 8990,
  mode: "proxy",
  profiles: [
    { id: "p-demo1", name: "我的 GLM", template_id: "glm", category: "cn_official", api_format: "anthropic", base_url: "https://open.bigmodel.cn/api/anthropic", model: "glm-4.6", key: "••••••1234", icon: "glm", icon_color: "#2E6BE6", website_url: "https://open.bigmodel.cn", sort_index: 1, notes: "" },
  ],
};
function mockMask(k) { return k ? "••••" + String(k).slice(-4) : ""; }
function mockInvoke(cmd, args) {
  args = args || {};
  switch (cmd) {
    case "get_config":
      return Promise.resolve({
        schema_version: mockStore.schema_version, active_id: mockStore.active_id,
        proxy_port: mockStore.proxy_port, sandbox_port: mockStore.sandbox_port,
        mode: mockStore.mode, templates: MOCK_TEMPLATES,
        profiles: mockStore.profiles.map((p) => ({ ...p })),
      });
    case "list_templates":
      return Promise.resolve(MOCK_TEMPLATES);
    case "create_profile": {
      const t = MOCK_TEMPLATES.find((x) => x.id === args.templateId) || {};
      const id = "p-" + Math.random().toString(16).slice(2, 10);
      mockStore.profiles.push({
        id, name: args.name || t.name || "新配置", template_id: args.templateId,
        category: t.category || "custom", api_format: t.api_format || "anthropic",
        base_url: args.baseUrl || t.base_url || "", model: args.model || "",
        key: mockMask(args.key || ""), icon: t.icon, icon_color: t.icon_color,
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
      mockStore.active_id = args.id;
      return Promise.resolve({ committed: true, active_id: args.id, hint: "（预览：已设为当前）" });
    }
    case "fetch_models":
      return Promise.resolve({ models: [{ id: "glm-4.6", supports_tools: true }, { id: "glm-5", supports_tools: null }], source: "live", error_kind: null, upstream_status: 200 });
    case "set_settings":
      if (args.cfg) { mockStore.proxy_port = args.cfg.proxy_port; mockStore.sandbox_port = args.cfg.sandbox_port; }
      return Promise.resolve(null);
    case "set_mode":
      mockStore.mode = args.mode;
      return Promise.resolve(null);
    case "one_click_login":
      return Promise.resolve({ url: "http://127.0.0.1:8990", msg: "（预览模式：假装已就绪）", action: "started" });
    case "status":
      return Promise.resolve({ proxy: "amber", sandbox: "amber", upstream: "amber" });
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
let mode = "proxy"; // "proxy" 第三方 | "official" 官方
// 当前配置快照（get_config 结果）。全 key 绝不在此，只有掩码。
let state = { profiles: [], templates: [], active_id: "", proxy_port: 18991, sandbox_port: 8990 };
let pendingSkipActivateId = null;   // set_active 校验含糊时，允许「跳过验证」再切
let pendingConfirm = null;          // 危险操作（清 key / 删除）的「再点一次确认」态

const CAT_LABELS = { official: "官方", cn_official: "国内", custom: "自定义" };

function setMsg(text, kind) {
  els.msg.textContent = text;
  els.msg.className = "msg" + (kind ? " " + kind : "");
}

function setLight(el, s) {
  const cls = { green: "g", amber: "a", red: "r" }[s] || "a";
  el.className = "lt " + cls;
}

function setBusy(on) {
  busy = on;
  [
    els.oneClickBtn, els.stopBtn, els.newBtn,
    els.wizSaveBtn, els.wizFetchBtn, els.wizCancelBtn,
    els.connSaveBtn, els.connFetchBtn, els.connClearBtn, els.connCancelBtn,
    els.metaSaveBtn, els.metaCancelBtn, els.skipActivateBtn,
    // 端口输入也纳入忙碌禁用：忙碌中改端口会与在途操作竞态（修 P1-c 前端侧）。
    els.proxyPort, els.sandboxPort,
  ].forEach((b) => b && (b.disabled = on));
  // 模式切换按钮同样禁用：忙碌中切官方会与「一键开始」竞态（修 P1-b 前端侧）。
  if (els.modeSeg) els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => (b.disabled = on));
  // 松开忙碌时，把 requires_model_override 的保存门控交回门（避免 setBusy(false) 覆盖门控）。
  if (!on) { refreshWizGate(); refreshConnGate(); }
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
  return (state.templates || []).find((t) => t.id === id) || null;
}

// ── 视图切换：列表 / 新建向导 / 连接编辑 / 改名。一次只显示一个表单（列表隐去减少高度）。──
function showView(v) {
  els.listSec.hidden = v !== "list";
  els.advSec.hidden = v !== "list";
  els.wizSec.hidden = v !== "wizard";
  els.connSec.hidden = v !== "conn";
  els.metaSec.hidden = v !== "meta";
  els.panel.classList.toggle("view-form", v !== "list");
  if (v === "list") hideSkip();
}
function cancelForm() { showView("list"); setMsg("就绪。"); }

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

// ── 加载配置 + 渲染列表 ──
async function loadConfig() {
  try {
    const cfg = await call("get_config");
    state.profiles = cfg.profiles || [];
    state.templates = cfg.templates || [];
    state.active_id = cfg.active_id || "";
    state.proxy_port = cfg.proxy_port ?? 18991;
    state.sandbox_port = cfg.sandbox_port ?? 8990;
    els.proxyPort.value = state.proxy_port;
    els.sandboxPort.value = state.sandbox_port;
    applyMode(cfg.mode === "official" ? "official" : "proxy");
    renderList();
    showView("list");
  } catch (e) {
    setMsg("读取配置失败：" + e, "err");
  }
}

function renderList() {
  const list = els.profileList;
  const ps = state.profiles || [];
  if (!ps.length) {
    list.innerHTML = '<div class="empty">还没有任何配置。<br>点右上「＋ 新建」添加一条第三方来源（DeepSeek / GLM / 千问 / 自定义端点…）。</div>';
    return;
  }
  list.innerHTML = ps.map((p) => {
    const active = p.id === state.active_id;
    const catLabel = CAT_LABELS[p.category] || p.category || "";
    const keyMask = p.key ? escapeHtml(p.key) : "未填 key";
    const modelTxt = p.model ? escapeHtml(p.model) : "（透传）";
    const dotStyle = p.icon_color ? ' style="background:' + escapeHtml(p.icon_color) + '"' : "";
    return (
      '<div class="prow' + (active ? " pactive" : "") + '" data-id="' + escapeHtml(p.id) + '">' +
        '<div class="prow-top">' +
          '<span class="pico"' + dotStyle + "></span>" +
          '<span class="pname">' + escapeHtml(p.name) + "</span>" +
          '<span class="badge">' + escapeHtml(catLabel) + "</span>" +
          (active ? '<span class="badge on">当前生效</span>' : "") +
        "</div>" +
        '<div class="pmeta">' + escapeHtml(p.base_url || "（未填地址）") + "</div>" +
        '<div class="pmeta">模型：' + modelTxt + " · Key：" + keyMask + "</div>" +
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

// ── 模式（第三方 / 官方）──
function applyMode(m) {
  mode = m === "official" ? "official" : "proxy";
  els.panel.classList.toggle("mode-official", mode === "official");
  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.classList.toggle("active", b.dataset.mode === mode)
  );
  els.oneClickBtn.textContent =
    mode === "official" ? "打开官方 Claude Science ↗" : "⚡ 一键开始";
}

async function switchMode(m) {
  if (m === mode) return;
  if (busy) return; // 忙碌中不切模式（防与「一键开始」竞态；按钮亦已禁用，此为双保险）。修 P1-b
  setBusy(true);
  try {
    await call("set_mode", { mode: m });
  } catch (e) {
    setMsg("切换模式失败：" + e, "err");
    setBusy(false);
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
}

async function openOfficial() {
  setBusy(true);
  setMsg("正在打开官方 Claude Science…");
  try {
    await call("open_official");
    setMsg("已打开官方 Claude Science（走你自己的官方登录与订阅）。", "ok");
  } catch (e) {
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

// ── 端口设置（替旧 set_config；纯端口，不含 provider/连接）──
async function persistPorts() {
  if (busy) return; // 忙碌中不改端口（防与在途操作竞态；输入亦已禁用，此为双保险）。修 P1-c
  const p = parseInt(els.proxyPort.value, 10) || 18991;
  const s = parseInt(els.sandboxPort.value, 10) || 8990;
  const changed = p !== state.proxy_port || s !== state.sandbox_port;
  // 本次端口提交全程置忙：仅靠开头的 `if (busy) return` 只挡「已在忙时进入」，挡不住本函数在途
  // 时其它操作（切模式/一键/连接编辑）启动。置忙 + 禁用控件才能保证操作顺序符合用户预期。修 GPT 三轮 P2
  setBusy(true);
  try {
    await call("set_settings", { cfg: { proxy_port: p, sandbox_port: s } });
    state.proxy_port = p;
    state.sandbox_port = s;
    // 后端在端口变化时会拆掉旧代理/沙箱（否则会复用指向旧端口的死链路），如实告知需重开。修 P1-c
    if (changed) {
      setMsg("端口已保存。改端口会重置正在运行的代理/沙箱，请重新「一键开始」。", "ok");
      await refreshStatus();
    }
  } catch (e) {
    // 出错＝端口未落盘（校验不过 / 停旧沙箱失败）：把输入框还原成实际生效值，避免显示未保存的数字。
    els.proxyPort.value = state.proxy_port;
    els.sandboxPort.value = state.sandbox_port;
    setMsg(String(e), "err");
  } finally {
    setBusy(false);
  }
}

// ── 模型下拉渲染（requires_override=false 时首项「透传」；按 supports_tools 标注）──
function renderModelSelect(sel, models, requiresOverride, sourceLabel) {
  sel.innerHTML = "";
  if (!requiresOverride) {
    const o = document.createElement("option");
    o.value = "";
    o.textContent = "（透传，用 Science 选择器）";
    sel.appendChild(o);
  }
  for (const m of models || []) {
    const o = document.createElement("option");
    o.value = m.id;
    const tag = m.supports_tools === true ? " ·工具✓" : m.supports_tools === false ? " ·无工具" : "";
    const src = sourceLabel ? " [" + sourceLabel + "]" : "";
    o.textContent = m.id + tag + src;
    sel.appendChild(o);
  }
}

// fetch_models 返回体 → 渲染下拉 + 提示（向导与连接编辑共用）。
function applyFetchResult(sel, requiresOverride, r) {
  const models = (r && r.models) || [];
  const srcLabel = r && r.source === "live" ? "实时" : r && r.source === "builtin" ? "内置" : "未验证";
  const prev = sel.value;
  renderModelSelect(sel, models, requiresOverride, srcLabel);
  if (prev && models.some((m) => m.id === prev)) sel.value = prev;
  if (r && r.error_kind === "network") {
    setMsg("未能连上上游验证，已铺内置模型（标「未验证」）。可仍试保存或重试。", "err");
  } else {
    setMsg("已获取 " + models.length + " 个模型（工具✓ 优先）。", "ok");
  }
}

// ── C2：新建向导 ──
function openWizard() {
  hideSkip();
  renderTemplateChips();
  const first = (state.templates || [])[0];
  selectWizTemplate(first ? first.id : "");
  showView("wizard");
  setMsg("选择来源，填 key 即可创建（新建不会自动生效）。");
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
  els.wizName.value = t.name;
  els.wizTplHint.textContent = t.website_url ? "官网：" + t.website_url : "";
  if (t.base_url_editable) {
    els.wizBase.value = "";
    els.wizBase.readOnly = false;
    els.wizBase.placeholder = "https://your-relay/claude";
    els.wizBaseHint.textContent = "自定义端点根地址（自动补 /v1/messages 与 /v1/models）。";
  } else {
    els.wizBase.value = t.base_url;
    els.wizBase.readOnly = true;
    els.wizBaseHint.textContent = "模板地址已填好（只读）。填 key 后可点「获取模型」。";
  }
  const builtin = (t.builtin_models || []).map((id) => ({ id, supports_tools: null }));
  renderModelSelect(els.wizModel, builtin, t.requires_model_override, "内置");
  els.wizModelHint.textContent = t.requires_model_override
    ? "该来源需选一个模型（不认 claude-* / 无透传）。"
    : "留「（透传）」则用 Science 选择器里的模型（claude-* 直传）。";
  refreshWizGate();
}

function refreshWizGate() {
  const t = tplById(els.wizTemplate ? els.wizTemplate.value : "");
  const need = t && t.requires_model_override;
  els.wizSaveBtn.disabled = busy || !!(need && !els.wizModel.value);
}

async function wizFetch() {
  const t = tplById(els.wizTemplate.value);
  if (!t) return;
  const base = t.base_url_editable ? els.wizBase.value.trim() : t.base_url;
  if (!base) { setMsg("请先填写 base_url。", "err"); return; }
  const key = els.wizKey.value.trim();
  if (!key) { setMsg("请先填 key 再获取模型。", "err"); return; }
  setBusy(true);
  setMsg("获取模型中：起临时代理探 /v1/models…");
  try {
    const r = await call("fetch_models", { req: { template_id: t.id, base_url: base, key } });
    applyFetchResult(els.wizModel, t.requires_model_override, r);
  } catch (e) {
    setMsg("获取模型失败：" + e, "err");
  } finally {
    setBusy(false);
    refreshWizGate();
  }
}

async function wizSave() {
  const t = tplById(els.wizTemplate.value);
  if (!t) { setMsg("模板未加载。", "err"); return; }
  const name = els.wizName.value.trim() || t.name;
  const model = els.wizModel.value;
  if (t.requires_model_override && !model) {
    setMsg("该来源需要选一个模型才能创建。", "err");
    return;
  }
  const args = { templateId: t.id, name, key: els.wizKey.value.trim(), model };
  if (t.base_url_editable) {
    const base = els.wizBase.value.trim();
    if (!base) { setMsg("请先填写 base_url。", "err"); return; }
    args.baseUrl = base;
  }
  setBusy(true);
  setMsg("创建中…");
  try {
    await call("create_profile", args);
    els.wizKey.value = "";
    await loadConfig();
    setMsg("已创建「" + name + "」。可在列表点「设为当前」启用。", "ok");
  } catch (e) {
    setMsg("创建失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

// ── C3：连接编辑（base_url/model/key）+ 清 key ──
function currentConn() {
  const id = els.connSec.dataset.id;
  return (state.profiles || []).find((x) => x.id === id) || null;
}

function openConn(id) {
  const p = (state.profiles || []).find((x) => x.id === id);
  if (!p) return;
  const t = tplById(p.template_id);
  const editable = t ? t.base_url_editable : true;
  const req = t ? t.requires_model_override : true;
  const active = id === state.active_id;
  els.connSec.dataset.id = id;
  els.connTitle.textContent = "编辑连接 · " + p.name + (active ? "（当前生效）" : "");
  els.connBase.value = p.base_url || (t ? t.base_url : "");
  els.connBase.readOnly = !editable;
  els.connBaseHint.textContent = editable ? "自定义端点根地址。" : "模板地址（只读）。填 key 后可「获取模型」。";
  const builtin = (t && t.builtin_models ? t.builtin_models : []).map((m) => ({ id: m, supports_tools: null }));
  if (p.model && !builtin.some((m) => m.id === p.model)) builtin.push({ id: p.model, supports_tools: null });
  renderModelSelect(els.connModel, builtin, req, "内置");
  if (p.model) els.connModel.value = p.model;
  els.connModelHint.textContent = req
    ? "该来源需选一个模型。"
    : "留「（透传）」则用 Science 选择器里的模型。";
  els.connKey.value = "";
  els.connKey.placeholder = p.key ? "已存：" + p.key + "（留空＝不改）" : "粘贴 key（只存本地）";
  showView("conn");
  refreshConnGate();
  setMsg(active
    ? "编辑当前生效配置：保存会先校验→切换，失败自动回退到原配置（不谎报生效）。"
    : "编辑连接后点「保存连接」。");
}

function refreshConnGate() {
  const p = currentConn();
  const t = p ? tplById(p.template_id) : null;
  const need = t && t.requires_model_override;
  els.connSaveBtn.disabled = busy || !!(need && !els.connModel.value);
}

async function connFetch() {
  const p = currentConn();
  if (!p) return;
  const t = tplById(p.template_id);
  const editable = t ? t.base_url_editable : true;
  const base = editable ? els.connBase.value.trim() : (t ? t.base_url : els.connBase.value.trim());
  if (!base) { setMsg("请先填写 base_url。", "err"); return; }
  setBusy(true);
  setMsg("获取模型中：起临时代理探 /v1/models…");
  try {
    const key = els.connKey.value.trim(); // 有新 key 带上；空则后端用已存 key（profileId）
    const r = await call("fetch_models", {
      req: { template_id: p.template_id, base_url: base, key, profile_id: p.id },
    });
    applyFetchResult(els.connModel, t ? t.requires_model_override : true, r);
  } catch (e) {
    setMsg("获取模型失败：" + e, "err");
  } finally {
    setBusy(false);
    refreshConnGate();
  }
}

async function connSave() {
  const p = currentConn();
  if (!p) { setMsg("配置不存在。", "err"); return; }
  const t = tplById(p.template_id);
  const req = t ? t.requires_model_override : true;
  const model = els.connModel.value;
  if (req && !model) { setMsg("该来源需要选一个模型才能保存。", "err"); return; }
  const editable = t ? t.base_url_editable : true;
  const base = editable ? els.connBase.value.trim() : (t ? t.base_url : els.connBase.value.trim());
  const active = p.id === state.active_id;
  // key 留空＝不改（后端语义）；base_url/model 照传。api_format 不在此改（保留模板值）。
  const args = { id: p.id, baseUrl: base, model, key: els.connKey.value.trim() };
  setBusy(true);
  setMsg(active ? "校验中→切换中…（保存当前生效配置的新连接）" : "保存连接中…");
  try {
    const r = await call("update_profile_connection", args);
    els.connKey.value = "";
    await loadConfig();
    // 非 active：后端如实回传 validated，连不通/native 也保存，但据实说明未校验（修 P2-d truthful-save）。
    if (active) {
      setMsg("已保存并应用新连接。", "ok");
    } else if (r && r.validated) {
      setMsg("已保存连接（已通过上游校验）。", "ok");
    } else {
      setMsg("已保存连接（未能连通上游校验，激活时会再验）。", "ok");
    }
  } catch (e) {
    // 后端错误文案已如实说明回滚/代理状态（可能是「已回滚到原配置」或「回滚未成功：代理当前已停」），
    // 前端不再盲目追加「仍在用原配置运行」，避免与「代理已停」相互矛盾。修 GPT 三轮 P2
    setMsg("连接未保存：" + e, "err");
  } finally {
    setBusy(false);
    await refreshStatus();
  }
}

// 清 key（行内 / 连接表单都可触发）：二次确认后 clear_profile_key。
function clearKey(id) {
  const p = (state.profiles || []).find((x) => x.id === id);
  const nm = p ? p.name : id;
  confirmAction("clearkey:" + id, "将清除「" + nm + "」的 API key（需重填才能用）", () => doClearKey(id));
}
async function doClearKey(id) {
  const wasActive = id === state.active_id;
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
  const p = (state.profiles || []).find((x) => x.id === id);
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
  const p = (state.profiles || []).find((x) => x.id === id);
  const nm = p ? p.name : id;
  confirmAction("delete:" + id, "将删除配置「" + nm + "」", () => doDelete(id));
}
async function doDelete(id) {
  const wasActive = id === state.active_id;
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
  hideSkip();
  setBusy(true);
  setMsg(skipVerify ? "跳过验证，切换中…" : "校验中→切换中…");
  try {
    const r = await call("set_active_profile", { id, skipVerify: !!skipVerify });
    if (r && r.committed) {
      await loadConfig();
      setMsg(r.hint || "已设为当前生效。", "ok");
    } else {
      await loadConfig(); // 反映未变（仍是原 active）
      setMsg((r && r.hint) || "校验未通过，未切换。", "err");
      if (r && r.can_skip) { pendingSkipActivateId = id; showSkip(); }
    }
  } catch (e) {
    await loadConfig();
    setMsg("设为当前失败：" + e, "err");
  } finally {
    setBusy(false);
    await refreshStatus();
  }
}

// ── 一键开始：读 active profile。无生效则引导先建/选一条（不再对旧 provider 槽落未提交输入）。──
async function oneClick() {
  if (!state.active_id) {
    setMsg("还没有「当前生效」的配置。请先「＋ 新建」或在列表点「设为当前」选一条，再一键开始。", "err");
    return;
  }
  setBusy(true);
  setMsg("一键开始：起代理 → 起沙箱 → 探活…");
  try {
    const r = await call("one_click_login");
    // 透传后端据实回传的 msg（已重开 / 已用新配置重启 / 沿用原对话 / 已启动 / 打开失败请手动打开）。
    setMsg((r.msg || "已就绪，正在打开面板…") + "\n" + (r.url || ""), "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("一键开始失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function stopAll() {
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
  }
}

async function openBrowser() {
  try {
    await call("open_url");
  } catch (e) {
    setMsg("打开浏览器失败：" + e, "err");
  }
}

async function runDoctor() {
  setMsg("自检中…");
  try {
    const out = await call("run_doctor");
    setMsg(out, out.includes("失败 0") ? "ok" : null);
  } catch (e) {
    setMsg("自检失败：" + e, "err");
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
      "https://api.github.com/repos/SuperJJ007/CSswitch/releases/latest",
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
    els.brandDot.className = "dot" + (s.proxy === "green" ? "" : " amber");
  } catch (e) {
    [els.ltProxy, els.ltSandbox, els.ltUpstream].forEach((l) => setLight(l, "amber"));
  }
}

function wire() {
  [
    "oneClickBtn", "stopBtn", "ltProxy", "ltSandbox", "ltUpstream",
    "msg", "brandDot", "openBrowserBtn", "doctorBtn", "updateBtn", "verLabel",
    "reportBtn", "logsBtn", "quitBtn", "modeSeg", "proxyPort", "sandboxPort", "advSec",
    "listSec", "profileList", "newBtn", "skipActivateBtn",
    "wizSec", "wizTemplate", "wizTemplateChips", "wizTplLabel", "wizTplHint", "wizName", "wizBase", "wizBaseHint",
    "wizFetchBtn", "wizModel", "wizModelHint", "wizKey", "wizSaveBtn", "wizCancelBtn",
    "connSec", "connTitle", "connBase", "connBaseHint", "connFetchBtn",
    "connModel", "connModelHint", "connKey", "connSaveBtn", "connClearBtn", "connCancelBtn",
    "metaSec", "metaName", "metaNotes", "metaSaveBtn", "metaCancelBtn",
  ].forEach((id) => (els[id] = $(id)));
  els.panel = document.querySelector(".panel");

  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.addEventListener("click", () => switchMode(b.dataset.mode))
  );

  els.proxyPort.addEventListener("change", persistPorts);
  els.sandboxPort.addEventListener("change", persistPorts);

  // 列表行内操作（事件委托；忙碌时忽略）。
  els.profileList.addEventListener("click", (e) => {
    if (busy) return;
    const btn = e.target.closest("[data-act]");
    const row = e.target.closest("[data-id]");
    if (!btn || !row) return;
    const id = row.getAttribute("data-id");
    const act = btn.getAttribute("data-act");
    if (act === "activate") activate(id, false);
    else if (act === "editconn") openConn(id);
    else if (act === "editmeta") openMeta(id);
    else if (act === "clearkey") clearKey(id);
    else if (act === "delete") del(id);
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
  els.wizModel.addEventListener("change", refreshWizGate);
  els.wizFetchBtn.addEventListener("click", wizFetch);
  els.wizSaveBtn.addEventListener("click", wizSave);
  els.wizCancelBtn.addEventListener("click", cancelForm);

  els.connModel.addEventListener("change", refreshConnGate);
  els.connFetchBtn.addEventListener("click", connFetch);
  els.connSaveBtn.addEventListener("click", connSave);
  els.connClearBtn.addEventListener("click", () => clearKey(els.connSec.dataset.id));
  els.connCancelBtn.addEventListener("click", cancelForm);

  els.metaSaveBtn.addEventListener("click", metaSave);
  els.metaCancelBtn.addEventListener("click", cancelForm);

  els.oneClickBtn.addEventListener("click", heroClick);
  els.stopBtn.addEventListener("click", stopAll);
  els.openBrowserBtn.addEventListener("click", openBrowser);
  els.doctorBtn.addEventListener("click", runDoctor);
  els.updateBtn.addEventListener("click", checkUpdate);
  els.reportBtn.addEventListener("click", () =>
    call("report_bug").catch((e) => setMsg("打开反馈页失败：" + e, "err"))
  );
  els.logsBtn.addEventListener("click", () =>
    call("open_logs").catch((e) => setMsg("打开日志失败：" + e, "err"))
  );
  els.quitBtn.addEventListener("click", () => call("quit_app").catch(() => {}));
}

window.addEventListener("DOMContentLoaded", async () => {
  wire();
  await loadConfig();
  try { els.verLabel.textContent = "v" + (await call("app_version")); } catch (e) {}
  await refreshStatus();
  if (PREVIEW) {
    setMsg("预览模式：仅看界面，按钮不连后端（真实 app 里会连进程管家）。");
  } else {
    statusTimer = setInterval(refreshStatus, 2500);
  }
});
