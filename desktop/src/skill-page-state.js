export const SOURCE_META = Object.freeze({
  csswitch_system: { label: "CSSwitch 系统", tone: "system" },
  csswitch_local: { label: "本地包", tone: "local" },
  csswitch_github: { label: "GitHub", tone: "github" },
  science_local: { label: "Science / 用户本地", tone: "science" },
  unverified: { label: "来源未验证", tone: "unverified" },
});

export const ATTACHMENT_META = Object.freeze({
  attached: { label: "已绑定", tone: "success" },
  detached: { label: "未绑定", tone: "warning" },
  unknown: { label: "绑定未知", tone: "neutral" },
});

const SCIENCE_STATES = new Set(["running_healthy", "stopped", "unverified"]);
const ACTIVE_ORG_STATES = new Set(["ready", "missing", "invalid", "changed"]);
const READBACK_STATES = new Set(["verified", "unavailable", "failed"]);
const ATTACHMENT_STATES = new Set(Object.keys(ATTACHMENT_META));
const SOURCE_STATES = new Set(Object.keys(SOURCE_META));

function boundedString(value, field, max, { nullable = false } = {}) {
  if (nullable && value == null) return null;
  if (typeof value !== "string" || Array.from(value).length > max) throw new Error(`${field} 字段非法`);
  return value;
}

function normalizeItem(item) {
  if (!item || typeof item !== "object") throw new Error("Skill 条目格式非法");
  const skillId = boundedString(item.skill_id, "skill_id", 80);
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$/.test(skillId)) throw new Error("skill_id 格式非法");
  const sourceKind = boundedString(item.source_kind, "source_kind", 32);
  const attachmentState = boundedString(item.attachment_state, "attachment_state", 16);
  if (!SOURCE_STATES.has(sourceKind)) throw new Error("source_kind 不受支持");
  if (!ATTACHMENT_STATES.has(attachmentState)) throw new Error("attachment_state 不受支持");
  return {
    skill_id: skillId,
    display_name: boundedString(item.display_name, "display_name", 120) || skillId,
    description: boundedString(item.description, "description", 500, { nullable: true }),
    source_kind: sourceKind,
    bundle_name: boundedString(item.bundle_name, "bundle_name", 120, { nullable: true }),
    attachment_state: attachmentState,
  };
}

function normalizeWarning(warning) {
  if (!warning || typeof warning !== "object") throw new Error("warning 格式非法");
  return {
    code: boundedString(warning.code, "warning.code", 80),
    skill_id: boundedString(warning.skill_id, "warning.skill_id", 80, { nullable: true }),
    message: boundedString(warning.message, "warning.message", 300),
  };
}

export function normalizeSkillListResponse(value) {
  if (!value || typeof value !== "object" || value.schema_version !== 1) {
    throw new Error("Skill 列表响应版本不受支持");
  }
  if (!SCIENCE_STATES.has(value.science_state)) throw new Error("science_state 不受支持");
  if (!ACTIVE_ORG_STATES.has(value.active_org_state)) throw new Error("active_org_state 不受支持");
  if (!READBACK_STATES.has(value.attachment_readback)) throw new Error("attachment_readback 不受支持");
  if (value.agent_name !== "OPERON") throw new Error("Skill 列表 Agent 不受支持");
  if (!Array.isArray(value.items) || value.items.length > 2000) throw new Error("Skill 列表数量非法");
  if (!Array.isArray(value.warnings) || value.warnings.length > 2000) throw new Error("Skill warning 数量非法");
  const items = value.items.map(normalizeItem);
  const ids = new Set();
  for (const item of items) {
    if (ids.has(item.skill_id)) throw new Error("Skill 列表包含重复 ID");
    ids.add(item.skill_id);
  }
  return {
    schema_version: 1,
    science_state: value.science_state,
    active_org_state: value.active_org_state,
    attachment_readback: value.attachment_readback,
    agent_name: "OPERON",
    items,
    warnings: value.warnings.map(normalizeWarning),
  };
}

export function filterSkills(items, filters = {}) {
  const query = String(filters.query || "").trim().toLowerCase();
  const source = filters.source || "all";
  const attachment = filters.attachment || "all";
  return items.filter((item) => {
    if (source !== "all" && item.source_kind !== source) return false;
    if (attachment !== "all" && item.attachment_state !== attachment) return false;
    if (!query) return true;
    return [item.skill_id, item.display_name, item.description, item.bundle_name]
      .filter(Boolean)
      .some((value) => String(value).toLowerCase().includes(query));
  });
}

export function shouldRefreshAfterInstall(result) {
  return !!result && result.directory_commit === true;
}

export function skillImportReadiness(data) {
  if (!data) return { ready: false, title: "先打开 Skill 页面并读取 Science 状态" };
  if (data.science_state !== "running_healthy") return { ready: false, title: "启动 Science 后可导入" };
  if (data.active_org_state !== "ready") return { ready: false, title: "当前 Science 组织尚未就绪" };
  if (data.attachment_readback !== "verified") return { ready: false, title: "OPERON 控制面尚未通过可信回读" };
  return { ready: true, title: "通过系统文件选择器导入 .zip 或 .skill" };
}
