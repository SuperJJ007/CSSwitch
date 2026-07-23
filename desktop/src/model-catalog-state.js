const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/;

function boundedText(value, maxBytes = 512) {
  const text = String(value ?? "").trim();
  if (!text || CONTROL_CHARS.test(text) || new TextEncoder().encode(text).length > maxBytes) {
    return "";
  }
  return text;
}

function normalizeCapabilities(value) {
  const reasoning = ["none", "native", "csswitch_opaque"].includes(value?.reasoning_round_trip)
    ? value.reasoning_round_trip
    : "none";
  const optionalBoolean = (candidate) => typeof candidate === "boolean" ? candidate : null;
  return {
    reasoning_round_trip: reasoning,
    forced_tool_choice: optionalBoolean(value?.forced_tool_choice),
    structured_output: optionalBoolean(value?.structured_output),
    vision: optionalBoolean(value?.vision),
  };
}

export function normalizeCatalogCandidate(item, enabled = false) {
  const upstream = boundedText(item?.upstream_model || item?.id);
  if (!upstream) return null;
  const display = boundedText(item?.display_name) || upstream;
  const selector = boundedText(item?.selector_id, 160);
  return {
    selector_id: selector,
    display_name: display,
    upstream_model: upstream,
    supports_tools: typeof item?.supports_tools === "boolean" ? item.supports_tools : null,
    capabilities: normalizeCapabilities(item?.capabilities),
    origin: boundedText(item?.origin, 64) || "manual",
    availability: boundedText(item?.availability, 64) || "unknown",
    enabled: !!enabled && item?.supports_tools !== false,
  };
}

export function mergeCatalogCandidates(current, incoming, { enableNew = false } = {}) {
  const merged = [];
  for (const raw of current || []) {
    const item = normalizeCatalogCandidate(raw, raw?.enabled !== false);
    if (item) merged.push(item);
  }
  for (const raw of incoming || []) {
    const discovered = normalizeCatalogCandidate(raw, enableNew);
    if (!discovered) continue;
    if (discovered.selector_id) {
      const selectorIndex = merged.findIndex((saved) =>
        saved.selector_id === discovered.selector_id
      );
      if (selectorIndex < 0) {
        merged.push(discovered);
      } else {
        const saved = merged[selectorIndex];
        merged[selectorIndex] = {
          ...discovered,
          selector_id: saved.selector_id,
          capabilities: raw?.capabilities ? discovered.capabilities : saved.capabilities,
          enabled: saved.enabled && discovered.supports_tools !== false,
        };
      }
      continue;
    }
    const matches = merged
      .map((saved, index) => ({ saved, index }))
      .filter(({ saved }) => saved.upstream_model === discovered.upstream_model);
    if (!matches.length) {
      merged.push(discovered);
      continue;
    }
    // A saved catalog may intentionally expose multiple stable selector IDs
    // for the same upstream model. Refresh metadata for every compatible
    // entry without collapsing those aliases into one route.
    for (const { saved, index } of matches) {
      merged[index] = {
        ...discovered,
        selector_id: saved.selector_id || discovered.selector_id,
        capabilities: raw?.capabilities ? discovered.capabilities : saved.capabilities,
        display_name: matches.length > 1
          ? saved.display_name
          : (discovered.display_name || saved.display_name),
        enabled: saved.enabled && discovered.supports_tools !== false,
      };
    }
  }
  return merged.slice(0, 64);
}

export function addManualCatalogItem(current, upstreamModel) {
  const item = normalizeCatalogCandidate({
    id: upstreamModel,
    display_name: upstreamModel,
    origin: "manual",
    availability: "unknown",
    supports_tools: null,
  }, true);
  if (!item) throw new Error("模型 ID 不能为空、不能含控制字符，且最长 512 字节。");
  return mergeCatalogCandidates(current, [item], { enableNew: true }).map((candidate) =>
    candidate.upstream_model === item.upstream_model ? { ...candidate, enabled: true } : candidate
  );
}

function resolveEnabledReference(routes, reference, label) {
  const value = boundedText(reference, 512);
  const matches = routes.filter((route) =>
    route.selector_id === value || route.upstream_model === value
  );
  if (matches.length !== 1) throw new Error(`${label}必须指向一条已启用模型。`);
  return matches[0].selector_id || matches[0].upstream_model;
}

export function preferredCatalogReference(items, reference, previousReference = "") {
  const routes = (items || [])
    .filter((item) => item?.enabled)
    .map((item) => normalizeCatalogCandidate(item, true))
    .filter(Boolean);
  const value = boundedText(reference, 512);
  const exactSelector = routes.find((route) => route.selector_id && route.selector_id === value);
  if (exactSelector) return exactSelector.selector_id;
  const upstreamMatches = routes.filter((route) => route.upstream_model === value);
  const previous = upstreamMatches.find((route) =>
    route.selector_id && route.selector_id === previousReference
  );
  const selected = previous || upstreamMatches[0];
  return selected ? (selected.selector_id || selected.upstream_model) : value;
}

export function buildCatalogSubmission(items, references = {}) {
  const routes = (items || [])
    .filter((item) => item?.enabled)
    .map((item) => normalizeCatalogCandidate(item, true))
    .filter(Boolean);
  if (!routes.length) throw new Error("至少启用一个支持工具或能力未知的模型。");
  if (routes.length > 64) throw new Error("每个配置最多启用 64 个模型。");
  if (routes.some((route) => route.supports_tools === false)) {
    throw new Error("明确不支持 tools 的模型不能启用。");
  }
  const seenSelectors = new Set();
  for (const route of routes) {
    if (route.selector_id && seenSelectors.has(route.selector_id)) {
      throw new Error(`selector ID 重复：${route.selector_id}`);
    }
    if (route.selector_id) seenSelectors.add(route.selector_id);
  }
  const defaultRef = references.default || routes[0].selector_id || routes[0].upstream_model;
  const balancedRef = references.balanced || defaultRef;
  const qualityRef = references.quality || defaultRef;
  const fastRef = references.fast || defaultRef;
  const cleanRoutes = routes.map(({ selector_id, display_name, upstream_model, supports_tools, capabilities }) => ({
    selector_id, display_name, upstream_model, supports_tools, capabilities,
  }));
  return {
    model_catalog: cleanRoutes,
    default_model_route_id: resolveEnabledReference(routes, defaultRef, "默认模型"),
    role_bindings: {
      sonnet: resolveEnabledReference(routes, balancedRef, "均衡模型"),
      opus: resolveEnabledReference(routes, qualityRef, "质量模型"),
      haiku: resolveEnabledReference(routes, fastRef, "快速模型"),
      fable: resolveEnabledReference(routes, qualityRef, "质量模型"),
    },
  };
}

function exactModelId(value, label, { required = false } = {}) {
  const raw = String(value ?? "");
  const model = boundedText(raw);
  if (!model && raw.trim()) {
    throw new Error(`${label}不能含控制字符，且最长 512 字节。`);
  }
  if (!model && required) throw new Error(`请填写${label}。`);
  return model;
}

function routeReference(route) {
  return route?.selector_id || route?.upstream_model || "";
}

function routeByReference(routes, reference) {
  const value = String(reference || "").trim();
  return (routes || []).find((route) =>
    route?.selector_id === value || route?.upstream_model === value
  ) || null;
}

export function summarizeProfileRoleModels(profile) {
  const routes = Array.isArray(profile?.model_catalog) ? profile.model_catalog : [];
  if (!routes.length) return null;
  const defaultRoute = routeByReference(
    routes,
    profile?.default_model_route_id || profile?.model,
  ) || routes[0];
  if (!defaultRoute?.upstream_model) return null;

  const bindings = profile?.role_bindings || {};
  const slots = [
    ["默认", defaultRoute],
    ["均衡", routeByReference(routes, bindings.sonnet) || defaultRoute],
    ["高质量", routeByReference(routes, bindings.opus) || defaultRoute],
    ["快速", routeByReference(routes, bindings.haiku) || defaultRoute],
    ["Fable", routeByReference(routes, bindings.fable)
      || routeByReference(routes, bindings.opus)
      || defaultRoute],
  ];
  const groups = [];
  const byUpstream = new Map();
  for (const [label, route] of slots) {
    const upstream = boundedText(route?.upstream_model);
    if (!upstream) continue;
    let group = byUpstream.get(upstream);
    if (!group) {
      const display = boundedText(route?.display_name);
      group = {
        labels: [],
        model: !display || display.toLowerCase() === "default" ? upstream : display,
      };
      byUpstream.set(upstream, group);
      groups.push(group);
    }
    group.labels.push(label);
  }
  if (!groups.length) return null;
  if (groups.length === 1) {
    return {
      primary: groups[0].model,
      secondary: "",
      inline: groups[0].model,
      split: false,
    };
  }
  const formatted = groups.map((group) => `${group.labels.join("/")}：${group.model}`);
  return {
    primary: formatted[0],
    secondary: formatted.slice(1).join(" · "),
    inline: formatted.join(" · "),
    split: true,
  };
}

function optionalDisplayName(value, label) {
  const raw = String(value ?? "");
  if (!raw.trim()) return "";
  const display = boundedText(raw, 256);
  if (!display) throw new Error(`${label}不能含控制字符，且最长 256 字节。`);
  return display;
}

function simplifiedRoute(route, upstreamModel, submittedDisplayName) {
  const submitted = optionalDisplayName(submittedDisplayName, "显示名称");
  const saved = boundedText(route?.display_name, 256);
  const display = submitted || (submittedDisplayName === undefined ? saved : "");
  return {
    selector_id: boundedText(route?.selector_id, 160),
    display_name: !display || display.toLowerCase() === "default" ? upstreamModel : display,
    upstream_model: upstreamModel,
    supports_tools: typeof route?.supports_tools === "boolean" ? route.supports_tools : null,
    capabilities: normalizeCapabilities(route?.capabilities),
  };
}

export function projectSimpleModelFields(routes, defaultReference, bindings = {}) {
  const catalog = (routes || []).map((route) => normalizeCatalogCandidate(route, true)).filter(Boolean);
  const fallback = routeByReference(catalog, defaultReference) || catalog[0] || null;
  const defaultModel = fallback?.upstream_model || "";
  const qualityRoute = routeByReference(catalog, bindings.opus) || fallback;
  const fastRoute = routeByReference(catalog, bindings.haiku) || fallback;
  const fableRoute = routeByReference(catalog, bindings.fable) || qualityRoute || fallback;
  const qualityModel = qualityRoute?.upstream_model || defaultModel;
  const fastModel = fastRoute?.upstream_model || defaultModel;
  const fableModel = fableRoute?.upstream_model || qualityModel || defaultModel;
  return {
    default_model: defaultModel,
    default_display_name: fallback?.display_name || defaultModel,
    quality_model: qualityModel && qualityModel !== defaultModel ? qualityModel : "",
    quality_display_name: qualityModel && qualityModel !== defaultModel ? (qualityRoute?.display_name || qualityModel) : "",
    fast_model: fastModel && fastModel !== defaultModel ? fastModel : "",
    fast_display_name: fastModel && fastModel !== defaultModel ? (fastRoute?.display_name || fastModel) : "",
    fable_model: fableModel && fableModel !== qualityModel ? fableModel : "",
    fable_display_name: fableModel && fableModel !== qualityModel ? (fableRoute?.display_name || fableModel) : "",
  };
}

export function buildSimpleModelSubmission(fields, options = {}) {
  const defaultModel = exactModelId(fields?.default_model, "默认模型", { required: true });
  const qualityInput = exactModelId(fields?.quality_model, "高质量模型");
  const fastInput = exactModelId(fields?.fast_model, "快速模型");
  const fableInput = exactModelId(fields?.fable_model, "Fable 模型");
  const defaultDisplayName = optionalDisplayName(fields?.default_display_name, "默认模型显示名称");
  const qualityDisplayName = optionalDisplayName(fields?.quality_display_name, "高质量模型显示名称");
  const fastDisplayName = optionalDisplayName(fields?.fast_display_name, "快速模型显示名称");
  const fableDisplayName = optionalDisplayName(fields?.fable_display_name, "Fable 模型显示名称");
  const qualityModel = qualityInput || defaultModel;
  const fastModel = fastInput || defaultModel;
  const fableModel = fableInput || qualityModel;
  const existingRoutes = (options.existing_routes || [])
    .map((route) => normalizeCatalogCandidate(route, true))
    .filter(Boolean);
  // Scratch discovery candidates may enrich a route the user explicitly
  // selects, but they are never preserved merely because a probe returned
  // them. This keeps fetch read-only with respect to the formal catalog.
  const candidateRoutes = (options.candidate_routes || [])
    .map((route) => normalizeCatalogCandidate(route, false))
    .filter(Boolean);
  const preferred = options.existing_references || {};
  const selectedRoutes = [];
  const selectedKeys = new Set();

  const choose = (model, displayName, preferredReference) => {
    const preferredRoute = routeByReference(existingRoutes, preferredReference);
    const savedRoute = preferredRoute?.upstream_model === model
      ? preferredRoute
      : existingRoutes.find((route) => route.upstream_model === model);
    const candidateRoute = savedRoute
      ? null
      : candidateRoutes.find((route) => route.upstream_model === model);
    const existing = savedRoute || candidateRoute;
    if (existing?.supports_tools === false) {
      throw new Error(`模型 ${model} 已明确不支持 tools，不能用于 Science。`);
    }
    const resolvedDisplayName = displayName || candidateRoute?.display_name || "";
    const route = simplifiedRoute(existing, model, resolvedDisplayName);
    const key = route.selector_id ? `selector:${route.selector_id}` : `upstream:${route.upstream_model}`;
    if (!selectedKeys.has(key)) {
      selectedKeys.add(key);
      selectedRoutes.push(route);
    }
    return routeReference(route);
  };

  const defaultRef = choose(defaultModel, defaultDisplayName, preferred.default);
  // 留空的含义是继承同一条 selector，而不只是碰巧指向同名 upstream。
  const qualityRef = qualityInput ? choose(qualityModel, qualityDisplayName, preferred.quality) : defaultRef;
  const fastRef = fastInput ? choose(fastModel, fastDisplayName, preferred.fast) : defaultRef;
  const fableRef = fableInput ? choose(fableModel, fableDisplayName, preferred.fable) : qualityRef;
  const previousDefault = routeByReference(existingRoutes, preferred.default);
  const previousSonnet = routeByReference(existingRoutes, preferred.balanced);
  const preserveSonnet = options.preserve_existing_sonnet === true
    && previousDefault?.upstream_model === defaultModel
    && previousSonnet;
  const sonnetRef = preserveSonnet ? routeReference(previousSonnet) : defaultRef;

  // 简化 UI 只投影四个 role，但不能因一次编辑静默删除历史额外 route。
  for (const existing of existingRoutes) {
    const route = simplifiedRoute(existing, existing.upstream_model);
    const key = route.selector_id ? `selector:${route.selector_id}` : `upstream:${route.upstream_model}`;
    if (!selectedKeys.has(key)) {
      selectedKeys.add(key);
      selectedRoutes.push(route);
    }
  }
  if (selectedRoutes.length > 64) throw new Error("每个配置最多保存 64 个模型。");
  return {
    model_catalog: selectedRoutes,
    default_model_route_id: defaultRef,
    role_bindings: {
      sonnet: sonnetRef,
      opus: qualityRef,
      haiku: fastRef,
      fable: fableRef,
    },
  };
}

export function catalogWarning(items) {
  const enabled = (items || []).filter((item) => item?.enabled);
  if (enabled.some((item) => item.supports_tools == null)) {
    return "部分已启用模型的 tools 能力未知；激活时只强制验证默认模型，建议逐项验证。";
  }
  return "";
}
