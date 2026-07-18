const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/;

function boundedText(value, maxBytes = 512) {
  const text = String(value ?? "").trim();
  if (!text || CONTROL_CHARS.test(text) || new TextEncoder().encode(text).length > maxBytes) {
    return "";
  }
  return text;
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
  const cleanRoutes = routes.map(({ selector_id, display_name, upstream_model, supports_tools }) => ({
    selector_id, display_name, upstream_model, supports_tools,
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

function simplifiedRoute(route, upstreamModel) {
  const display = boundedText(route?.display_name);
  return {
    selector_id: boundedText(route?.selector_id, 160),
    display_name: !display || display.toLowerCase() === "default" ? upstreamModel : display,
    upstream_model: upstreamModel,
    supports_tools: typeof route?.supports_tools === "boolean" ? route.supports_tools : null,
  };
}

export function projectSimpleModelFields(routes, defaultReference, bindings = {}) {
  const catalog = (routes || []).map((route) => normalizeCatalogCandidate(route, true)).filter(Boolean);
  const fallback = routeByReference(catalog, defaultReference) || catalog[0] || null;
  const defaultModel = fallback?.upstream_model || "";
  const qualityModel = (routeByReference(catalog, bindings.opus) || fallback)?.upstream_model || defaultModel;
  const fastModel = (routeByReference(catalog, bindings.haiku) || fallback)?.upstream_model || defaultModel;
  const fableModel = (routeByReference(catalog, bindings.fable) || routeByReference(catalog, bindings.opus) || fallback)?.upstream_model || qualityModel || defaultModel;
  return {
    default_model: defaultModel,
    quality_model: qualityModel && qualityModel !== defaultModel ? qualityModel : "",
    fast_model: fastModel && fastModel !== defaultModel ? fastModel : "",
    fable_model: fableModel && fableModel !== qualityModel ? fableModel : "",
  };
}

export function buildSimpleModelSubmission(fields, options = {}) {
  const defaultModel = exactModelId(fields?.default_model, "默认模型", { required: true });
  const qualityInput = exactModelId(fields?.quality_model, "高质量模型");
  const fastInput = exactModelId(fields?.fast_model, "快速模型");
  const fableInput = exactModelId(fields?.fable_model, "Fable 模型");
  const qualityModel = qualityInput || defaultModel;
  const fastModel = fastInput || defaultModel;
  const fableModel = fableInput || qualityModel;
  const existingRoutes = (options.existing_routes || [])
    .map((route) => normalizeCatalogCandidate(route, true))
    .filter(Boolean);
  const preferred = options.existing_references || {};
  const selectedRoutes = [];
  const selectedKeys = new Set();

  const choose = (model, preferredReference) => {
    const preferredRoute = routeByReference(existingRoutes, preferredReference);
    const existing = preferredRoute?.upstream_model === model
      ? preferredRoute
      : existingRoutes.find((route) => route.upstream_model === model);
    const route = simplifiedRoute(existing, model);
    const key = route.selector_id ? `selector:${route.selector_id}` : `upstream:${route.upstream_model}`;
    if (!selectedKeys.has(key)) {
      selectedKeys.add(key);
      selectedRoutes.push(route);
    }
    return routeReference(route);
  };

  const defaultRef = choose(defaultModel, preferred.default);
  // 留空的含义是继承同一条 selector，而不只是碰巧指向同名 upstream。
  const qualityRef = qualityInput ? choose(qualityModel, preferred.quality) : defaultRef;
  const fastRef = fastInput ? choose(fastModel, preferred.fast) : defaultRef;
  const fableRef = fableInput ? choose(fableModel, preferred.fable) : qualityRef;
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
