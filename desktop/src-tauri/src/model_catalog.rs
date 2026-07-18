use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MODEL_PRESETS_JSON: &str = include_str!("../../../catalog/model-presets.v1.json");
pub(crate) const MAX_PROFILE_MODELS: usize = 64;
pub(crate) const MAX_MODEL_CATALOG_BYTES: usize = 64 * 1024;
const MAX_SELECTOR_BYTES: usize = 160;
const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_MODEL_TEXT_BYTES: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ModelRoute {
    pub selector_id: String,
    pub display_name: String,
    pub upstream_model: String,
    #[serde(default)]
    pub supports_tools: Option<bool>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoleBindings {
    #[serde(default)]
    pub sonnet: String,
    #[serde(default)]
    pub opus: String,
    #[serde(default)]
    pub haiku: String,
    #[serde(default)]
    pub fable: String,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl RoleBindings {
    pub(crate) fn all(&self) -> [(&'static str, &str); 4] {
        [
            ("sonnet", &self.sonnet),
            ("opus", &self.opus),
            ("haiku", &self.haiku),
            ("fable", &self.fable),
        ]
    }

    pub(crate) fn all_empty(&self) -> bool {
        self.all().iter().all(|(_, selector)| selector.is_empty())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PresetModel {
    upstream_model: String,
    display_name: String,
    supports_tools: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetRoleBindings {
    sonnet: String,
    opus: String,
    haiku: String,
    fable: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPreset {
    id: String,
    default_upstream_model: String,
    role_bindings: PresetRoleBindings,
    models: Vec<PresetModel>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPresetCatalog {
    schema_version: u32,
    presets: Vec<ModelPreset>,
}

static PRESETS: OnceLock<ModelPresetCatalog> = OnceLock::new();

fn presets() -> Result<&'static ModelPresetCatalog, String> {
    if let Some(catalog) = PRESETS.get() {
        return Ok(catalog);
    }
    let catalog: ModelPresetCatalog = serde_json::from_str(MODEL_PRESETS_JSON)
        .map_err(|error| format!("model preset catalog JSON 解析失败：{error}"))?;
    validate_presets(&catalog)?;
    let _ = PRESETS.set(catalog);
    PRESETS
        .get()
        .ok_or_else(|| "model preset catalog 初始化失败".to_string())
}

fn validate_presets(catalog: &ModelPresetCatalog) -> Result<(), String> {
    if catalog.schema_version != 1 || catalog.presets.is_empty() {
        return Err("model preset catalog schema 或内容无效".into());
    }
    let mut ids = BTreeSet::new();
    for preset in &catalog.presets {
        if preset.id.trim().is_empty() || !ids.insert(preset.id.clone()) {
            return Err(format!("model preset id 为空或重复：{}", preset.id));
        }
        if preset.models.is_empty() || preset.models.len() > MAX_PROFILE_MODELS {
            return Err(format!("model preset 模型数量无效：{}", preset.id));
        }
        let models: BTreeSet<&str> = preset
            .models
            .iter()
            .map(|model| model.upstream_model.as_str())
            .collect();
        if models.len() != preset.models.len()
            || !models.contains(preset.default_upstream_model.as_str())
        {
            return Err(format!("model preset 默认项或重复项无效：{}", preset.id));
        }
        for model in &preset.models {
            validate_text("upstream_model", &model.upstream_model)?;
            validate_text("display_name", &model.display_name)?;
            if model.display_name.len() > MAX_DISPLAY_NAME_BYTES {
                return Err(format!("model preset display name 过长：{}", preset.id));
            }
        }
        if serde_json::to_vec(&preset.models)
            .map_err(|error| format!("model preset 编码失败：{error}"))?
            .len()
            > MAX_MODEL_CATALOG_BYTES
        {
            return Err(format!("model preset 目录超过 64 KiB：{}", preset.id));
        }
        for binding in [
            &preset.role_bindings.sonnet,
            &preset.role_bindings.opus,
            &preset.role_bindings.haiku,
            &preset.role_bindings.fable,
        ] {
            if !models.contains(binding.as_str()) {
                return Err(format!("model preset role binding 悬空：{}", preset.id));
            }
        }
    }
    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_MODEL_TEXT_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("{field} 为空、过长或包含控制字符"));
    }
    Ok(())
}

fn slug(value: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() && out.len() < max_bytes {
                out.push('-');
            }
            pending_dash = false;
            if out.len() < max_bytes {
                out.push(ch.to_ascii_lowercase());
            }
        } else {
            pending_dash = true;
        }
        if out.len() >= max_bytes {
            break;
        }
    }
    if out.is_empty() {
        "model".into()
    } else {
        out.trim_end_matches('-').to_string()
    }
}

pub(crate) fn selector_id_v1(namespace: &str, upstream_model: &str) -> String {
    let digest = Sha256::digest(format!("selector-v1\0{namespace}\0{upstream_model}").as_bytes());
    let short_digest: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "claude-csswitch-{}-{}-{short_digest}",
        slug(namespace, 36),
        slug(upstream_model, 94)
    )
}

pub(crate) fn namespace_for(template_id: &str, api_format: &str) -> String {
    match template_id {
        "custom" | "custom-openai" | "custom-openai-responses" => {
            format!("custom-{api_format}")
        }
        other => other.to_string(),
    }
}

pub(crate) fn single_route_catalog(
    namespace: &str,
    upstream_model: &str,
    display_name: Option<&str>,
    supports_tools: Option<bool>,
) -> Result<(Vec<ModelRoute>, String, RoleBindings), String> {
    validate_text("upstream_model", upstream_model)?;
    let display_name = display_name.unwrap_or(upstream_model);
    validate_text("display_name", display_name)?;
    let selector = selector_id_v1(namespace, upstream_model);
    let route = ModelRoute {
        selector_id: selector.clone(),
        display_name: display_name.to_string(),
        upstream_model: upstream_model.to_string(),
        supports_tools,
        extra: BTreeMap::new(),
    };
    Ok((
        vec![route],
        selector.clone(),
        RoleBindings {
            sonnet: selector.clone(),
            opus: selector.clone(),
            haiku: selector.clone(),
            fable: selector,
            extra: BTreeMap::new(),
        },
    ))
}

pub(crate) fn preset_catalog(
    preset_id: &str,
) -> Result<(Vec<ModelRoute>, String, RoleBindings), String> {
    let preset = presets()?
        .presets
        .iter()
        .find(|preset| preset.id == preset_id)
        .ok_or_else(|| format!("model preset 不存在：{preset_id}"))?;
    let mut selector_by_upstream = BTreeMap::new();
    let mut routes = Vec::with_capacity(preset.models.len());
    for model in &preset.models {
        let selector = selector_id_v1(&preset.id, &model.upstream_model);
        selector_by_upstream.insert(model.upstream_model.as_str(), selector.clone());
        routes.push(ModelRoute {
            selector_id: selector,
            display_name: model.display_name.clone(),
            upstream_model: model.upstream_model.clone(),
            supports_tools: model.supports_tools,
            extra: BTreeMap::new(),
        });
    }
    let default = selector_by_upstream
        .get(preset.default_upstream_model.as_str())
        .cloned()
        .ok_or_else(|| format!("model preset 默认项不存在：{preset_id}"))?;
    if let Some(index) = routes.iter().position(|route| route.selector_id == default) {
        routes.swap(0, index);
    }
    let selector = |upstream: &str| {
        selector_by_upstream
            .get(upstream)
            .cloned()
            .ok_or_else(|| format!("model preset role binding 不存在：{preset_id}"))
    };
    Ok((
        routes,
        default,
        RoleBindings {
            sonnet: selector(&preset.role_bindings.sonnet)?,
            opus: selector(&preset.role_bindings.opus)?,
            haiku: selector(&preset.role_bindings.haiku)?,
            fable: selector(&preset.role_bindings.fable)?,
            extra: BTreeMap::new(),
        },
    ))
}

pub(crate) fn preset_upstream_models(preset_id: &str) -> Result<Vec<String>, String> {
    Ok(presets()?
        .presets
        .iter()
        .find(|preset| preset.id == preset_id)
        .ok_or_else(|| format!("model preset 不存在：{preset_id}"))?
        .models
        .iter()
        .map(|model| model.upstream_model.clone())
        .collect())
}

pub(crate) fn new_profile_catalog(
    template_id: &str,
    api_format: &str,
    requested_upstream: Option<&str>,
) -> Result<(Vec<ModelRoute>, String, RoleBindings), String> {
    let template = crate::templates::by_id(template_id)
        .ok_or_else(|| format!("template 不存在：{template_id}"))?;
    if template.model_catalog_source == "dynamic_codex" {
        return Ok((Vec::new(), String::new(), RoleBindings::default()));
    }
    let requested = requested_upstream
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(preset_id) = template.preset_catalog_id {
        let (mut routes, mut default, bindings) = preset_catalog(preset_id)?;
        if let Some(requested) = requested {
            if let Some(index) = routes.iter().position(|route| {
                route.upstream_model == requested || route.selector_id == requested
            }) {
                routes.swap(0, index);
                default = routes[0].selector_id.clone();
            } else {
                let (manual, manual_default, _) = single_route_catalog(
                    &namespace_for(template_id, api_format),
                    requested,
                    None,
                    None,
                )?;
                routes.insert(0, manual.into_iter().next().expect("single route"));
                default = manual_default;
            }
        }
        return Ok((routes, default, bindings));
    }
    match requested {
        Some(requested) => single_route_catalog(
            &namespace_for(template_id, api_format),
            requested,
            None,
            None,
        ),
        None => Ok((Vec::new(), String::new(), RoleBindings::default())),
    }
}

pub(crate) fn static_resolver_payload(
    adapter: &str,
    template_id: &str,
    routes: &[ModelRoute],
    default_selector: &str,
    bindings: &RoleBindings,
) -> Result<String, String> {
    validate_saved_catalog(routes, default_selector, bindings)?;
    let legacy_aliases: Vec<(String, String)> = routes
        .iter()
        .filter(|route| match template_id {
            "qwen" => matches!(
                route.upstream_model.as_str(),
                "qwen3.7-max" | "qwen-plus-latest" | "qwen-turbo"
            ),
            "deepseek" => matches!(
                route.upstream_model.as_str(),
                "deepseek-v4-pro" | "deepseek-v4-flash"
            ),
            _ => false,
        })
        .map(|route| (route.upstream_model.clone(), route.selector_id.clone()))
        .collect();
    let core = serde_json::json!({
        "schema_version": 1,
        "adapter": adapter,
        "default_selector_id": default_selector,
        "routes": routes.iter().map(|route| serde_json::json!({
            "selector_id": route.selector_id,
            "display_name": route.display_name,
            "upstream_model": route.upstream_model,
            "supports_tools": route.supports_tools,
        })).collect::<Vec<_>>(),
        "role_bindings": {
            "sonnet": bindings.sonnet,
            "opus": bindings.opus,
            "haiku": bindings.haiku,
            "fable": bindings.fable,
        },
        "legacy_aliases": legacy_aliases.iter().map(|(alias, selector_id)| serde_json::json!({
            "alias": alias,
            "selector_id": selector_id,
        })).collect::<Vec<_>>(),
    });
    let catalog_fp =
        static_catalog_fingerprint(adapter, routes, default_selector, bindings, &legacy_aliases);
    let mut value = core;
    value["catalog_fp"] = serde_json::Value::String(catalog_fp);
    let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_MODEL_CATALOG_BYTES {
        return Err("gateway 静态模型目录超过 64 KiB".into());
    }
    Ok(encoded)
}

fn static_fp_text(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u32).to_be_bytes());
    digest.update(value.as_bytes());
}

fn static_catalog_fingerprint(
    adapter: &str,
    routes: &[ModelRoute],
    default_selector: &str,
    bindings: &RoleBindings,
    legacy_aliases: &[(String, String)],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"csswitch-static-catalog-fp-v1\0");
    digest.update(1_u32.to_be_bytes());
    static_fp_text(&mut digest, adapter);
    static_fp_text(&mut digest, default_selector);
    digest.update((routes.len() as u32).to_be_bytes());
    for route in routes {
        static_fp_text(&mut digest, &route.selector_id);
        static_fp_text(&mut digest, &route.display_name);
        static_fp_text(&mut digest, &route.upstream_model);
        digest.update([match route.supports_tools {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }]);
    }
    for role in [
        &bindings.sonnet,
        &bindings.opus,
        &bindings.haiku,
        &bindings.fable,
    ] {
        static_fp_text(&mut digest, role);
    }
    digest.update((legacy_aliases.len() as u32).to_be_bytes());
    for (alias, selector) in legacy_aliases {
        static_fp_text(&mut digest, alias);
        static_fp_text(&mut digest, selector);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn set_profile_default_route(
    template_id: &str,
    api_format: &str,
    routes: &mut Vec<ModelRoute>,
    default_selector: &mut String,
    bindings: &mut RoleBindings,
    requested: &str,
) -> Result<String, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("默认模型不能为空".into());
    }
    if let Some(route) = routes
        .iter()
        .find(|route| route.selector_id == requested || route.upstream_model == requested)
    {
        *default_selector = route.selector_id.clone();
    } else {
        if routes.len() >= MAX_PROFILE_MODELS {
            return Err(format!("模型目录最多包含 {MAX_PROFILE_MODELS} 个模型"));
        }
        let (mut added, added_default, added_bindings) = single_route_catalog(
            &namespace_for(template_id, api_format),
            requested,
            None,
            None,
        )?;
        if routes
            .iter()
            .any(|route| route.selector_id == added[0].selector_id)
        {
            return Err("新模型 selector 与现有目录冲突".into());
        }
        routes.push(added.remove(0));
        *default_selector = added_default;
        if bindings.all_empty() {
            *bindings = added_bindings;
        }
    }
    validate_saved_catalog(routes, default_selector, bindings)?;
    routes
        .iter()
        .find(|route| route.selector_id == *default_selector)
        .map(|route| route.upstream_model.clone())
        .ok_or_else(|| "默认模型 route 不存在".into())
}

pub(crate) fn normalize_catalog_edit(
    template_id: &str,
    api_format: &str,
    mut routes: Vec<ModelRoute>,
    requested_default: &str,
    mut bindings: RoleBindings,
) -> Result<(Vec<ModelRoute>, String, RoleBindings, String), String> {
    let namespace = namespace_for(template_id, api_format);
    for route in &mut routes {
        route.upstream_model = route.upstream_model.trim().to_string();
        if route.display_name.trim().is_empty()
            || route.display_name.trim().eq_ignore_ascii_case("default")
        {
            route.display_name = route.upstream_model.clone();
        }
        if route.selector_id.trim().is_empty() {
            route.selector_id = selector_id_v1(&namespace, &route.upstream_model);
        }
    }
    let resolve_reference = |reference: &str| -> Result<String, String> {
        let reference = reference.trim();
        if let Some(route) = routes.iter().find(|route| route.selector_id == reference) {
            return Ok(route.selector_id.clone());
        }
        let matches: Vec<&ModelRoute> = routes
            .iter()
            .filter(|route| route.upstream_model == reference)
            .collect();
        match matches.as_slice() {
            [route] => Ok(route.selector_id.clone()),
            [] => Err(format!("模型目录引用不存在：{reference}")),
            _ => Err(format!(
                "模型目录 upstream 引用不唯一，请使用 selector：{reference}"
            )),
        }
    };
    let default = if requested_default.trim().is_empty() {
        routes
            .first()
            .map(|route| route.selector_id.clone())
            .ok_or("模型目录不能为空")?
    } else {
        resolve_reference(requested_default)?
    };
    for (_, reference) in bindings.all() {
        if !reference.is_empty() {
            let _ = resolve_reference(reference)?;
        }
    }
    bindings.sonnet = if bindings.sonnet.is_empty() {
        default.clone()
    } else {
        resolve_reference(&bindings.sonnet)?
    };
    bindings.opus = if bindings.opus.is_empty() {
        default.clone()
    } else {
        resolve_reference(&bindings.opus)?
    };
    bindings.haiku = if bindings.haiku.is_empty() {
        default.clone()
    } else {
        resolve_reference(&bindings.haiku)?
    };
    bindings.fable = if bindings.fable.is_empty() {
        default.clone()
    } else {
        resolve_reference(&bindings.fable)?
    };
    validate_saved_catalog(&routes, &default, &bindings)?;
    let model = routes
        .iter()
        .find(|route| route.selector_id == default)
        .map(|route| route.upstream_model.clone())
        .ok_or("默认模型 route 不存在")?;
    Ok((routes, default, bindings, model))
}

pub(crate) fn validate_saved_catalog(
    routes: &[ModelRoute],
    default_selector: &str,
    bindings: &RoleBindings,
) -> Result<(), String> {
    for reserved in ["sonnet", "opus", "haiku", "fable"] {
        if bindings.extra.contains_key(reserved) {
            return Err(format!(
                "role binding extension 与 canonical 字段冲突：{reserved}"
            ));
        }
    }
    if routes.is_empty() || routes.len() > MAX_PROFILE_MODELS {
        return Err(format!("模型目录必须包含 1 到 {MAX_PROFILE_MODELS} 个模型"));
    }
    let encoded = serde_json::to_vec(routes).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_MODEL_CATALOG_BYTES {
        return Err(format!(
            "模型目录编码后不得超过 {} 字节",
            MAX_MODEL_CATALOG_BYTES
        ));
    }
    let mut selectors = BTreeSet::new();
    for route in routes {
        for reserved in [
            "selector_id",
            "display_name",
            "upstream_model",
            "supports_tools",
        ] {
            if route.extra.contains_key(reserved) {
                return Err(format!(
                    "model route extension 与 canonical 字段冲突：{reserved}"
                ));
            }
        }
        if route.selector_id.is_empty()
            || route.selector_id.len() > MAX_SELECTOR_BYTES
            || !route.selector_id.starts_with("claude-")
            || !route.selector_id.is_ascii()
            || !route
                .selector_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err("selector_id 不符合 CSSwitch alias v1 约束".into());
        }
        if !selectors.insert(route.selector_id.as_str()) {
            return Err(format!("selector_id 重复：{}", route.selector_id));
        }
        validate_text("display_name", &route.display_name)?;
        if route.display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err("display_name 不得超过 256 字节".into());
        }
        validate_text("upstream_model", &route.upstream_model)?;
        if route.supports_tools == Some(false) {
            return Err(format!(
                "模型 `{}` 已知不支持 tools，不能加入启用目录",
                route.upstream_model
            ));
        }
    }
    if !selectors.contains(default_selector) {
        return Err("默认模型必须存在于模型目录".into());
    }
    for (role, selector) in bindings.all() {
        if selector.is_empty() || !selectors.contains(selector) {
            return Err(format!("{role} role binding 为空或指向不存在的 selector"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_v1_is_stable_provider_scoped_and_science_shaped() {
        let first = selector_id_v1("qwen", "qwen3.7-max");
        assert_eq!(first, "claude-csswitch-qwen-qwen3-7-max-427ec055ced9");
        assert_eq!(first, selector_id_v1("qwen", "qwen3.7-max"));
        assert_ne!(first, selector_id_v1("relay", "qwen3.7-max"));
        assert!(first.starts_with("claude-csswitch-qwen-qwen3-7-max-"));
        assert!(first.len() <= MAX_SELECTOR_BYTES);
        let longest = selector_id_v1(&"n".repeat(200), &"m".repeat(600));
        assert!(longest.len() <= MAX_SELECTOR_BYTES, "{}", longest.len());
    }

    #[test]
    fn every_preset_builds_a_valid_multi_model_catalog() {
        let catalog = presets().unwrap();
        for preset in &catalog.presets {
            let (routes, default, bindings) = preset_catalog(&preset.id).unwrap();
            validate_saved_catalog(&routes, &default, &bindings).unwrap();
        }
    }

    #[test]
    fn duplicate_selector_is_rejected_but_duplicate_upstream_is_allowed() {
        let (mut routes, default, bindings) = preset_catalog("qwen").unwrap();
        routes.push(ModelRoute {
            selector_id: "claude-csswitch-qwen-second-shell-000000000000".into(),
            display_name: "Second shell".into(),
            upstream_model: routes[0].upstream_model.clone(),
            supports_tools: None,
            extra: BTreeMap::new(),
        });
        validate_saved_catalog(&routes, &default, &bindings).unwrap();
        routes[1].selector_id = routes[0].selector_id.clone();
        assert!(validate_saved_catalog(&routes, &default, &bindings).is_err());
    }

    #[test]
    fn canonical_edit_replaces_default_placeholder_with_upstream_model_name() {
        let route = ModelRoute {
            selector_id: String::new(),
            display_name: "default".into(),
            upstream_model: "kimi-k3".into(),
            supports_tools: None,
            extra: BTreeMap::new(),
        };
        let (routes, _, _, model) = normalize_catalog_edit(
            "kimi",
            "anthropic",
            vec![route],
            "kimi-k3",
            RoleBindings::default(),
        )
        .unwrap();
        assert_eq!(routes[0].display_name, "kimi-k3");
        assert_eq!(model, "kimi-k3");
    }

    #[test]
    fn catalog_limits_and_text_safety_fail_closed() {
        let (routes, default, bindings) =
            single_route_catalog("qwen", "qwen-plus-latest", None, None).unwrap();

        let mut too_many = Vec::new();
        for index in 0..=MAX_PROFILE_MODELS {
            too_many.push(ModelRoute {
                selector_id: format!("claude-csswitch-limit-{index:02}"),
                display_name: format!("Route {index}"),
                upstream_model: format!("upstream-{index}"),
                supports_tools: None,
                extra: BTreeMap::new(),
            });
        }
        assert!(validate_saved_catalog(&too_many, &too_many[0].selector_id, &bindings).is_err());

        let mut unsafe_route = routes[0].clone();
        unsafe_route.selector_id = "claude-csswitch-千问".into();
        let unsafe_default = unsafe_route.selector_id.clone();
        let unsafe_bindings = RoleBindings {
            sonnet: unsafe_default.clone(),
            opus: unsafe_default.clone(),
            haiku: unsafe_default.clone(),
            fable: unsafe_default.clone(),
            extra: BTreeMap::new(),
        };
        assert!(
            validate_saved_catalog(&[unsafe_route], &unsafe_default, &unsafe_bindings).is_err()
        );

        let mut unsafe_route = routes[0].clone();
        unsafe_route.display_name = "bad\nname".into();
        assert!(validate_saved_catalog(&[unsafe_route], &default, &bindings).is_err());

        let mut unsafe_route = routes[0].clone();
        unsafe_route.supports_tools = Some(false);
        assert!(validate_saved_catalog(&[unsafe_route], &default, &bindings).is_err());
    }
}
