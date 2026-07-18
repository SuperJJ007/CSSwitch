import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  addManualCatalogItem,
  buildCatalogSubmission,
  buildSimpleModelSubmission,
  mergeCatalogCandidates,
  preferredCatalogReference,
  projectSimpleModelFields,
} from "../desktop/src/model-catalog-state.js";

test("saved selectors survive discovery metadata refresh", () => {
  const merged = mergeCatalogCandidates([
    { selector_id: "claude-csswitch-relay-a-0123456789ab", upstream_model: "a", display_name: "Old", enabled: true },
  ], [
    { id: "a", display_name: "New", supports_tools: true, origin: "discovered", availability: "available" },
    { id: "b", display_name: "B", supports_tools: false },
  ]);
  assert.equal(merged[0].selector_id, "claude-csswitch-relay-a-0123456789ab");
  assert.equal(merged[0].display_name, "New");
  assert.equal(merged[1].enabled, false);
});

test("submission maps quality to opus and fable and rejects non-tool models", () => {
  const items = [
    { selector_id: "", upstream_model: "quality", display_name: "Quality", supports_tools: true, enabled: true },
    { selector_id: "", upstream_model: "fast", display_name: "Fast", supports_tools: null, enabled: true },
  ];
  const payload = buildCatalogSubmission(items, { default: "quality", balanced: "quality", quality: "quality", fast: "fast" });
  assert.equal(payload.role_bindings.opus, "quality");
  assert.equal(payload.role_bindings.fable, "quality");
  assert.equal(payload.role_bindings.haiku, "fast");
  assert.throws(() => buildCatalogSubmission([{ ...items[0], supports_tools: false }]), /不能启用/);
});

test("manual exact upstream IDs are enabled without inventing selectors", () => {
  const items = addManualCatalogItem([], "vendor/model-1");
  assert.equal(items[0].upstream_model, "vendor/model-1");
  assert.equal(items[0].selector_id, "");
  assert.equal(items[0].enabled, true);
});

test("compatible selectors sharing one upstream survive metadata refresh", () => {
  const current = [
    { selector_id: "claude-csswitch-relay-a-111111111111", upstream_model: "vendor/a", display_name: "Alias A", enabled: true },
    { selector_id: "claude-csswitch-relay-b-222222222222", upstream_model: "vendor/a", display_name: "Alias B", enabled: true },
  ];
  const initialized = mergeCatalogCandidates([], current, { enableNew: true });
  assert.equal(initialized.length, 2);
  assert.deepEqual(initialized.map((item) => item.display_name), ["Alias A", "Alias B"]);
  const merged = mergeCatalogCandidates(initialized, [
    { id: "vendor/a", display_name: "Vendor A", supports_tools: true, origin: "discovered", availability: "available" },
  ]);
  assert.equal(merged.length, 2);
  assert.deepEqual(merged.map((item) => item.selector_id), current.map((item) => item.selector_id));
  assert.deepEqual(merged.map((item) => item.display_name), ["Alias A", "Alias B"]);
  assert.equal(
    preferredCatalogReference(merged, "vendor/a", current[1].selector_id),
    current[1].selector_id,
  );
  const payload = buildCatalogSubmission(merged, {
    default: current[1].selector_id,
    quality: current[0].selector_id,
    balanced: current[1].selector_id,
    fast: current[0].selector_id,
  });
  assert.equal(payload.model_catalog.length, 2);
  assert.equal(payload.default_model_route_id, current[1].selector_id);
});

test("one freely entered model maps every Science role without a default pseudo model", () => {
  const payload = buildSimpleModelSubmission({ default_model: "kimi-k3" });
  assert.deepEqual(payload.model_catalog, [{
    selector_id: "",
    display_name: "kimi-k3",
    upstream_model: "kimi-k3",
    supports_tools: null,
  }]);
  assert.deepEqual(payload.role_bindings, {
    sonnet: "kimi-k3",
    opus: "kimi-k3",
    haiku: "kimi-k3",
    fable: "kimi-k3",
  });
});

test("four freely entered model fields map independently and deduplicate equal IDs", () => {
  const payload = buildSimpleModelSubmission({
    default_model: "vendor/balanced",
    quality_model: "vendor/quality",
    fast_model: "vendor/fast",
    fable_model: "vendor/fable",
  });
  assert.deepEqual(payload.model_catalog.map((route) => route.upstream_model), [
    "vendor/balanced", "vendor/quality", "vendor/fast", "vendor/fable",
  ]);
  assert.deepEqual(payload.role_bindings, {
    sonnet: "vendor/balanced",
    opus: "vendor/quality",
    haiku: "vendor/fast",
    fable: "vendor/fable",
  });
  const deduped = buildSimpleModelSubmission({
    default_model: "same", quality_model: "same", fast_model: "same", fable_model: "same",
  });
  assert.equal(deduped.model_catalog.length, 1);
});

test("simple model projection preserves selectors and unbound legacy routes", () => {
  const existing = [
    { selector_id: "sel-default", upstream_model: "balanced", display_name: "default", supports_tools: true },
    { selector_id: "sel-quality", upstream_model: "quality", display_name: "Quality", supports_tools: true },
    { selector_id: "sel-extra", upstream_model: "legacy-extra", display_name: "Legacy extra", supports_tools: null },
  ];
  const fields = projectSimpleModelFields(existing, "sel-default", {
    sonnet: "sel-default", opus: "sel-quality", haiku: "sel-default", fable: "sel-quality",
  });
  assert.deepEqual(fields, {
    default_model: "balanced", quality_model: "quality", fast_model: "", fable_model: "",
  });
  const payload = buildSimpleModelSubmission(fields, {
    existing_routes: existing,
    existing_references: { default: "sel-default", quality: "sel-quality", fast: "sel-default", fable: "sel-quality" },
  });
  assert.equal(payload.default_model_route_id, "sel-default");
  assert.equal(payload.model_catalog[0].display_name, "balanced");
  assert.ok(payload.model_catalog.some((route) => route.selector_id === "sel-extra"));
});

test("blank optional fields inherit the exact selector selected by their parent role", () => {
  const existing = [
    { selector_id: "sel-default", upstream_model: "same", display_name: "Same", supports_tools: true },
    { selector_id: "sel-old-quality", upstream_model: "same", display_name: "Old alias", supports_tools: true },
  ];
  const payload = buildSimpleModelSubmission({ default_model: "same" }, {
    existing_routes: existing,
    existing_references: {
      default: "sel-default", quality: "sel-old-quality", fast: "sel-old-quality", fable: "sel-old-quality",
    },
  });
  assert.deepEqual(payload.role_bindings, {
    sonnet: "sel-default", opus: "sel-default", haiku: "sel-default", fable: "sel-default",
  });
  assert.equal(payload.model_catalog.length, 2, "legacy alias is preserved without remaining role-bound");
});

test("editing preserves a legacy sonnet selector until the default model actually changes", () => {
  const existing = [
    { selector_id: "sel-default", upstream_model: "vendor/default", display_name: "Default", supports_tools: true },
    { selector_id: "sel-sonnet", upstream_model: "vendor/balanced", display_name: "Balanced", supports_tools: true },
  ];
  const refs = {
    default: "sel-default", balanced: "sel-sonnet",
    quality: "sel-default", fast: "sel-default", fable: "sel-default",
  };
  const unchanged = buildSimpleModelSubmission({ default_model: "vendor/default" }, {
    existing_routes: existing,
    existing_references: refs,
    preserve_existing_sonnet: true,
  });
  assert.equal(unchanged.default_model_route_id, "sel-default");
  assert.equal(unchanged.role_bindings.sonnet, "sel-sonnet");

  const changed = buildSimpleModelSubmission({ default_model: "vendor/new-default" }, {
    existing_routes: existing,
    existing_references: refs,
    preserve_existing_sonnet: true,
  });
  assert.equal(changed.role_bindings.sonnet, changed.default_model_route_id);

  const newProfile = buildSimpleModelSubmission({ default_model: "vendor/default" }, {
    existing_routes: existing,
    existing_references: refs,
  });
  assert.equal(newProfile.role_bindings.sonnet, newProfile.default_model_route_id);
});

test("provider forms expose four free model inputs and no static discovery checklist", () => {
  const html = readFileSync(new URL("../desktop/src/index.html", import.meta.url), "utf8");
  for (const prefix of ["wiz", "conn"]) {
    for (const id of [`${prefix}Model`, `${prefix}RoleQuality`, `${prefix}RoleFast`, `${prefix}RoleFable`]) {
      assert.match(html, new RegExp(`<input id="${id}"`));
    }
  }
  assert.doesNotMatch(html, /搜索已发现模型|添加精确 ID|勾选白名单|同步最新推荐/);
  assert.doesNotMatch(html, />获取模型</);
  assert.match(html, /刷新账号模型/);
});

test("Codex profile action aligns with providers as a permanently disabled edit button", () => {
  const js = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  const renderList = js.slice(js.indexOf("function renderList()"), js.indexOf("// ── 模式（第三方 / 官方）──"));
  const busyState = js.slice(js.indexOf("function syncProfileBusyState()"), js.indexOf("function sameOp("));
  assert.doesNotMatch(renderList, /查看模型/);
  assert.match(renderList, /data-permanently-disabled="true" disabled aria-disabled="true"[^>]*>编辑<\/button>/);
  assert.match(renderList, /'<button class="abtn" data-act="editconn">编辑<\/button>'/);
  assert.match(busyState, /permanentlyDisabled \|\| busy/);
  assert.match(js, /btn\.disabled \|\| btn\.dataset\.permanentlyDisabled === "true"/);
});
