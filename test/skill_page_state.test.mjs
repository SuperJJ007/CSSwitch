import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { SkillPage } from "../desktop/src/skill-page.js";

import {
  filterSkills,
  normalizeSkillListResponse,
  shouldRefreshAfterInstall,
  skillImportReadiness,
} from "../desktop/src/skill-page-state.js";

const RESPONSE = {
  schema_version: 1,
  science_state: "running_healthy",
  active_org_state: "ready",
  attachment_readback: "verified",
  agent_name: "OPERON",
  items: [
    { skill_id: "system-route", display_name: "System Route", description: "routing", source_kind: "csswitch_system", bundle_name: null, attachment_state: "attached" },
    { skill_id: "github-skill", display_name: "GitHub Skill", description: "research", source_kind: "csswitch_github", bundle_name: "research-kit", attachment_state: "detached" },
    { skill_id: "local-skill", display_name: "Local Skill", description: null, source_kind: "csswitch_local", bundle_name: null, attachment_state: "unknown" },
    { skill_id: "science-skill", display_name: "Science Skill", description: "local", source_kind: "science_local", bundle_name: null, attachment_state: "attached" },
    { skill_id: "unknown-skill", display_name: "Unknown Skill", description: "marker", source_kind: "unverified", bundle_name: null, attachment_state: "detached" },
  ],
  warnings: [],
};

function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

test("真实 Skill 响应保留五类来源与绑定三态", () => {
  const value = normalizeSkillListResponse(RESPONSE);
  assert.deepEqual(new Set(value.items.map((item) => item.source_kind)), new Set([
    "csswitch_system", "csswitch_github", "csswitch_local", "science_local", "unverified",
  ]));
  assert.deepEqual(new Set(value.items.map((item) => item.attachment_state)), new Set([
    "attached", "detached", "unknown",
  ]));
});

test("搜索、来源和绑定筛选不修改原始数据", () => {
  const value = normalizeSkillListResponse(RESPONSE);
  assert.deepEqual(filterSkills(value.items, { query: "research" }).map((item) => item.skill_id), ["github-skill"]);
  assert.deepEqual(filterSkills(value.items, { source: "science_local" }).map((item) => item.skill_id), ["science-skill"]);
  assert.deepEqual(filterSkills(value.items, { attachment: "unknown" }).map((item) => item.skill_id), ["local-skill"]);
  assert.equal(value.items.length, 5);
});

test("只有目录已经提交的导入结果触发列表刷新", () => {
  assert.equal(shouldRefreshAfterInstall({ directory_commit: true, status: "FILES_COMMITTED_ATTACH_REQUIRED" }), true);
  assert.equal(shouldRefreshAfterInstall({ directory_commit: false, status: "SCIENCE_NOT_READY" }), false);
  assert.equal(shouldRefreshAfterInstall(null), false);
});

test("响应拒绝重复 ID、未知状态和过长展示文本", () => {
  assert.throws(() => normalizeSkillListResponse({ ...RESPONSE, items: [...RESPONSE.items, RESPONSE.items[0]] }), /重复 ID/);
  assert.throws(() => normalizeSkillListResponse({ ...RESPONSE, science_state: "healthy-ish" }), /science_state/);
  assert.throws(() => normalizeSkillListResponse({ ...RESPONSE, items: [{ ...RESPONSE.items[0], description: "x".repeat(501) }] }), /description/);
});

test("展示长度按 Unicode scalar 与 Rust 合同一致", () => {
  const displayName = "😀".repeat(120);
  assert.equal(normalizeSkillListResponse({
    ...RESPONSE,
    items: [{ ...RESPONSE.items[0], display_name: displayName }],
  }).items[0].display_name, displayName);
  assert.throws(() => normalizeSkillListResponse({
    ...RESPONSE,
    items: [{ ...RESPONSE.items[0], display_name: displayName + "😀" }],
  }), /display_name/);
});

test("导入同时要求健康 runtime、ready org 和可信回读", () => {
  assert.equal(skillImportReadiness(RESPONSE).ready, true);
  assert.equal(skillImportReadiness({ ...RESPONSE, active_org_state: "changed" }).ready, false);
  assert.equal(skillImportReadiness({ ...RESPONSE, attachment_readback: "failed" }).ready, false);
  assert.equal(skillImportReadiness({ ...RESPONSE, science_state: "stopped" }).ready, false);
});

test("失效中的旧请求不能覆盖后续组织快照", async () => {
  const oldRequest = deferred();
  const newRequest = deferred();
  let callCount = 0;
  const page = new SkillPage({ innerHTML: "" }, {
    call: () => (++callCount === 1 ? oldRequest.promise : newRequest.promise),
    refreshButton: { disabled: false },
    importButton: { disabled: false, title: "" },
  });
  const firstRefresh = page.refresh();
  page.invalidate();
  const replacementRefresh = page.refreshIfLoaded();
  assert.equal(callCount, 2);

  newRequest.resolve({
    ...RESPONSE,
    items: [{ ...RESPONSE.items[0], skill_id: "new-org-skill" }],
  });
  await replacementRefresh;
  oldRequest.resolve({
    ...RESPONSE,
    items: [{ ...RESPONSE.items[0], skill_id: "old-org-skill" }],
  });
  await firstRefresh;

  assert.deepEqual(page.data.items.map((item) => item.skill_id), ["new-org-skill"]);
  assert.equal(page.loading, false);
});

test("生产页面没有假 MCP 操作、原型文案或 CS 字块", async () => {
  const [html, main, page] = await Promise.all([
    readFile(new URL("../desktop/src/index.html", import.meta.url), "utf8"),
    readFile(new URL("../desktop/src/main.js", import.meta.url), "utf8"),
    readFile(new URL("../desktop/src/skill-page.js", import.meta.url), "utf8"),
  ]);
  const combined = html + main + page;
  assert.doesNotMatch(combined, /交互原型|仅本地导入连接后端|Claude Science 控制台/);
  assert.doesNotMatch(html, />CS<|brand-edition/);
  assert.match(html, /data:image\/png;base64/);
  assert.match(page, /MCP 暂未开放/);
  assert.match(page, /disabled>MCP 暂未开放<\/button>/);
  assert.doesNotMatch(page, /新建外部 MCP|mcp-attach|mcp-detach|fixture|模拟 load/);
  assert.match(main, /list_installed_skills/);
});

test("展示文本通过 HTML 转义函数进入模板", async () => {
  const page = await readFile(new URL("../desktop/src/skill-page.js", import.meta.url), "utf8");
  assert.match(page, /escapeHtml\(item\.display_name\)/);
  assert.match(page, /escapeHtml\(item\.description/);
  assert.doesNotMatch(page, /innerHTML\s*=\s*item\./);
});
