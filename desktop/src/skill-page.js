import {
  ATTACHMENT_META,
  SOURCE_META,
  filterSkills,
  normalizeSkillListResponse,
  skillImportReadiness,
} from "./skill-page-state.js";

const escapeHtml = (value) => String(value ?? "")
  .replaceAll("&", "&amp;")
  .replaceAll("<", "&lt;")
  .replaceAll(">", "&gt;")
  .replaceAll('"', "&quot;")
  .replaceAll("'", "&#39;");

const SCIENCE_META = Object.freeze({
  running_healthy: { label: "Science 运行正常", tone: "success" },
  stopped: { label: "Science 已停止", tone: "neutral" },
  unverified: { label: "Science 状态未知", tone: "warning" },
});

const READBACK_META = Object.freeze({
  verified: { label: "OPERON 已实时回读", tone: "success" },
  unavailable: { label: "绑定状态暂不可读", tone: "neutral" },
  failed: { label: "绑定回读失败", tone: "warning" },
});

const ACTIVE_ORG_COPY = Object.freeze({
  missing: ["尚未建立 Science 组织", "启动一次隔离 Science 后，这里会显示当前组织发现的 Skill。"],
  invalid: ["无法安全读取当前组织", "active-org 状态无效，CSSwitch 没有继续扫描 Skill 目录。"],
  changed: ["Science 组织刚刚发生变化", "请刷新列表，CSSwitch 不会混合两个组织的读取结果。"],
});

function statusText(meta, className = "skill-status") {
  return `<span class="${className} ${escapeHtml(meta.tone)}"><i></i>${escapeHtml(meta.label)}</span>`;
}

function sourceText(source) {
  const meta = SOURCE_META[source] || SOURCE_META.unverified;
  return `<span class="skill-source ${escapeHtml(meta.tone)}"><i></i>${escapeHtml(meta.label)}</span>`;
}

function attachmentText(state) {
  return statusText(ATTACHMENT_META[state] || ATTACHMENT_META.unknown);
}

function packageText(item) {
  return item.bundle_name ? `Bundle · ${item.bundle_name}` : "单 Skill";
}

export class SkillPage {
  constructor(root, options) {
    this.root = root;
    this.call = options.call;
    this.refreshButton = options.refreshButton;
    this.importButton = options.importButton;
    this.filters = { query: "", source: "all", attachment: "all" };
    this.data = null;
    this.error = "";
    this.loading = false;
    this.globalBusy = false;
    this.detailId = null;
    this.refreshedAt = null;
    this.loadAttempted = false;
    this.visibleLimit = 100;
    this.requestGeneration = 0;
  }

  mount() {
    this.root.addEventListener("click", (event) => this.onClick(event));
    this.root.addEventListener("input", (event) => this.onFilter(event));
    this.root.addEventListener("change", (event) => this.onFilter(event));
    this.root.addEventListener("keydown", (event) => this.onKeydown(event));
    this.refreshButton?.addEventListener("click", () => this.refresh());
    this.render();
  }

  ensureLoaded() {
    if (!this.loadAttempted && !this.loading) return this.refresh();
    return Promise.resolve(false);
  }

  refreshIfLoaded() {
    if (!this.loadAttempted) return Promise.resolve(false);
    return this.refresh();
  }

  invalidate() {
    if (!this.loadAttempted) return;
    this.requestGeneration += 1;
    this.loading = false;
    this.data = null;
    this.error = "";
    this.detailId = null;
    this.refreshedAt = null;
    this.syncButtons();
    this.render();
  }

  setGlobalBusy(value) {
    this.globalBusy = !!value;
    this.syncButtons();
  }

  async refresh() {
    if (this.loading) return false;
    const requestGeneration = ++this.requestGeneration;
    this.loadAttempted = true;
    this.loading = true;
    this.error = "";
    this.data = null;
    this.detailId = null;
    this.refreshedAt = null;
    this.syncButtons();
    this.render();
    try {
      const raw = await this.call("list_installed_skills");
      if (requestGeneration !== this.requestGeneration) return false;
      this.data = normalizeSkillListResponse(raw);
      this.refreshedAt = new Date();
      if (this.detailId && !this.data.items.some((item) => item.skill_id === this.detailId)) {
        this.detailId = null;
      }
      return true;
    } catch (error) {
      if (requestGeneration !== this.requestGeneration) return false;
      this.error = String(error?.message || error || "读取失败");
      this.data = null;
      this.detailId = null;
      this.refreshedAt = null;
      return false;
    } finally {
      if (requestGeneration === this.requestGeneration) {
        this.loading = false;
        this.syncButtons();
        this.render();
      }
    }
  }

  syncButtons() {
    if (this.refreshButton) this.refreshButton.disabled = this.loading || this.globalBusy;
    if (this.importButton) {
      const readiness = skillImportReadiness(this.data);
      this.importButton.disabled = this.loading || this.globalBusy || !readiness.ready;
      this.importButton.title = readiness.title;
    }
  }

  render() {
    const data = this.data;
    const items = data ? filterSkills(data.items, this.filters) : [];
    const sourceOptions = Object.entries(SOURCE_META).map(([value, meta]) =>
      `<option value="${value}"${this.filters.source === value ? " selected" : ""}>${escapeHtml(meta.label)}</option>`
    ).join("");
    const attachmentOptions = Object.entries(ATTACHMENT_META).map(([value, meta]) =>
      `<option value="${value}"${this.filters.attachment === value ? " selected" : ""}>${escapeHtml(meta.label)}</option>`
    ).join("");
    this.root.innerHTML = `<div class="skill-page-content"${this.detailId ? " inert aria-hidden=\"true\"" : ""}>
      <div class="extension-toolbar">
        <div class="extension-tabs" role="tablist" aria-label="Skill 与 MCP">
          <button class="extension-tab active" type="button" role="tab" aria-selected="true">Skills</button>
          <button class="extension-tab" type="button" role="tab" aria-selected="false" aria-disabled="true" disabled>MCP 暂未开放</button>
        </div>
        ${this.refreshedAt ? `<span class="refresh-stamp">更新于 ${escapeHtml(this.refreshedAt.toLocaleTimeString("zh-CN", { hour12: false }))}</span>` : ""}
      </div>
      ${data ? this.renderSummary(data) : ""}
      ${data?.warnings?.length ? this.renderWarnings(data.warnings) : ""}
      <section class="surface skill-filter-surface">
        <div class="skill-filter-row">
          <input type="search" placeholder="搜索名称、说明或 Bundle" aria-label="搜索 Skill" data-filter="query" value="${escapeHtml(this.filters.query)}" />
          <select aria-label="Skill 来源" data-filter="source"><option value="all">全部来源</option>${sourceOptions}</select>
          <select aria-label="绑定状态" data-filter="attachment"><option value="all">全部绑定状态</option>${attachmentOptions}</select>
          <button class="btn" type="button" data-action="clear-filters">清除筛选</button>
        </div>
      </section>
      <div data-skill-results>${this.renderBody(data, items)}</div>
      </div>
      ${this.renderDetail(data)}
    `;
  }

  renderResults() {
    const target = this.root.querySelector("[data-skill-results]");
    if (!target) return this.render();
    const items = this.data ? filterSkills(this.data.items, this.filters) : [];
    target.innerHTML = this.renderBody(this.data, items);
  }

  renderSummary(data) {
    const science = SCIENCE_META[data.science_state] || SCIENCE_META.unverified;
    const readback = READBACK_META[data.attachment_readback] || READBACK_META.unavailable;
    const attachedCount = data.items.filter((item) => item.attachment_state === "attached").length;
    return `<section class="skill-summary-grid" aria-label="Skill 列表摘要">
      <div class="surface skill-summary-card"><span>已发现</span><strong>${data.items.length}</strong><small>当前 Science 组织</small></div>
      <div class="surface skill-summary-card"><span>目标 Agent</span><strong>${escapeHtml(data.agent_name)}</strong><small>${data.attachment_readback === "verified" ? `已绑定 ${attachedCount} 个` : "不推断绑定结果"}</small></div>
      <div class="surface skill-summary-card"><span>实时状态</span>${statusText(science)}${statusText(readback)}</div>
    </section>`;
  }

  renderWarnings(warnings) {
    const first = warnings[0];
    const extra = warnings.length > 1 ? `，另有 ${warnings.length - 1} 项` : "";
    return `<div class="skill-warning" role="status"><strong>列表有需要注意的项目</strong><span>${escapeHtml(first.message)}${escapeHtml(extra)}</span></div>`;
  }

  renderBody(data, items) {
    if (this.loading && !data) {
      return '<div class="skill-loading">正在读取当前组织的 Skill…</div>';
    }
    if (this.error) {
      return `<div class="skill-empty error"><h3>Skill 列表读取失败</h3><p>${escapeHtml(this.error)}</p><button class="btn" type="button" data-action="retry">重新读取</button></div>`;
    }
    if (!data) return '<div class="skill-loading">准备读取 Skill…</div>';
    if (data.active_org_state !== "ready") {
      const copy = ACTIVE_ORG_COPY[data.active_org_state] || ACTIVE_ORG_COPY.invalid;
      return `<div class="skill-empty"><h3>${escapeHtml(copy[0])}</h3><p>${escapeHtml(copy[1])}</p></div>`;
    }
    if (!items.length) {
      const filtered = data.items.length > 0;
      return `<div class="skill-empty"><h3>${filtered ? "没有符合条件的 Skill" : "当前组织还没有 Skill"}</h3><p>${filtered ? "调整搜索或筛选条件后再试。" : "启动 Science 后可从系统文件选择器导入本地 Skill 包。"}</p></div>`;
    }
    const visibleItems = items.slice(0, this.visibleLimit);
    return `<div class="skill-list">
      <div class="skill-list-head" aria-hidden="true"><span>Skill</span><span>来源</span><span>形态</span><span>OPERON</span><span>操作</span></div>
      ${visibleItems.map((item) => `<article class="skill-row">
        <div class="skill-primary"><h3>${escapeHtml(item.display_name)}</h3><p>${escapeHtml(item.description || item.skill_id)}</p></div>
        <div class="skill-cell">${sourceText(item.source_kind)}</div>
        <div class="skill-cell skill-package" title="${escapeHtml(packageText(item))}">${escapeHtml(packageText(item))}</div>
        <div class="skill-cell">${attachmentText(item.attachment_state)}</div>
        <div class="skill-actions"><button class="btn" type="button" data-action="detail" data-id="${escapeHtml(item.skill_id)}">详情</button></div>
      </article>`).join("")}
      ${items.length > visibleItems.length ? `<button class="btn skill-show-more" type="button" data-action="show-more">再显示 ${Math.min(100, items.length - visibleItems.length)} 个</button>` : ""}
    </div>`;
  }

  renderDetail(data) {
    if (!data || !this.detailId) return "";
    const item = data.items.find((entry) => entry.skill_id === this.detailId);
    if (!item) return "";
    return `<div class="skill-drawer-backdrop" data-action="close-detail">
      <aside class="skill-drawer" role="dialog" aria-modal="true" aria-labelledby="skillDetailTitle">
        <div class="skill-drawer-head"><div><div class="section-kicker">SKILL DETAILS</div><h2 id="skillDetailTitle">${escapeHtml(item.display_name)}</h2></div><button class="drawer-close" type="button" data-action="close-detail" aria-label="关闭详情">关闭</button></div>
        <div class="skill-drawer-body">
          ${attachmentText(item.attachment_state)}
          <p class="skill-detail-description">${escapeHtml(item.description || "没有可安全读取的描述。")}</p>
          <div class="skill-detail-list">
            <div><span>Skill ID</span><strong>${escapeHtml(item.skill_id)}</strong></div>
            <div><span>来源</span><strong>${escapeHtml(SOURCE_META[item.source_kind]?.label || "来源未验证")}</strong></div>
            <div><span>形态</span><strong>${escapeHtml(packageText(item))}</strong></div>
            <div><span>目标 Agent</span><strong>${escapeHtml(data.agent_name)}</strong></div>
            <div><span>绑定状态</span><strong>${escapeHtml(ATTACHMENT_META[item.attachment_state]?.label || "绑定未知")}</strong></div>
          </div>
          <div class="skill-load-note"><strong>加载状态不在列表中推断</strong><p>“已绑定”只代表 OPERON 的实时回读。实际加载仍需在当前 Agent 会话调用 <code>skill()</code> 验证。</p></div>
        </div>
      </aside>
    </div>`;
  }

  onFilter(event) {
    const key = event.target.dataset.filter;
    if (!key) return;
    this.filters[key] = event.target.value;
    this.visibleLimit = 100;
    this.renderResults();
  }

  openDetail(skillId) {
    this.detailId = skillId;
    this.render();
    queueMicrotask(() => this.root.querySelector(".drawer-close")?.focus());
  }

  closeDetail() {
    const openerId = this.detailId;
    this.detailId = null;
    this.render();
    queueMicrotask(() => this.root.querySelector(`[data-action="detail"][data-id="${openerId || ""}"]`)?.focus());
  }

  onKeydown(event) {
    if (!this.detailId) return;
    if (event.key === "Escape") {
      event.preventDefault();
      this.closeDetail();
      return;
    }
    if (event.key !== "Tab") return;
    const dialog = this.root.querySelector('[role="dialog"]');
    const focusable = Array.from(dialog?.querySelectorAll('button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])') || []);
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  onClick(event) {
    const target = event.target.closest("[data-action]");
    if (!target) return;
    const action = target.dataset.action;
    if (action === "retry") this.refresh();
    else if (action === "clear-filters") {
      this.filters = { query: "", source: "all", attachment: "all" };
      this.visibleLimit = 100;
      this.render();
    } else if (action === "detail") {
      this.openDetail(target.dataset.id);
    } else if (action === "close-detail") {
      if (event.target === target) this.closeDetail();
    } else if (action === "show-more") {
      this.visibleLimit += 100;
      this.renderResults();
    }
  }
}

export function mountSkillPage(root, options) {
  const page = new SkillPage(root, options);
  page.mount();
  return page;
}
