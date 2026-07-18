use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ENV_NAME: &str = "CSSWITCH_STATIC_MODEL_CATALOG_V1";
const MAX_ENCODED_BYTES: usize = 64 * 1024;
const MAX_ROUTES: usize = 64;
const MAX_SELECTOR_BYTES: usize = 160;
const MAX_DISPLAY_BYTES: usize = 256;
const MAX_UPSTREAM_BYTES: usize = 512;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRoute {
    selector_id: String,
    display_name: String,
    upstream_model: String,
    supports_tools: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRoles {
    sonnet: String,
    opus: String,
    haiku: String,
    fable: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLegacyAlias {
    alias: String,
    selector_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCatalog {
    schema_version: u32,
    adapter: String,
    catalog_fp: String,
    default_selector_id: String,
    routes: Vec<WireRoute>,
    role_bindings: WireRoles,
    legacy_aliases: Vec<WireLegacyAlias>,
}

#[derive(Clone, Debug)]
pub struct StaticProfileResolver {
    routes: Vec<WireRoute>,
    adapter: String,
    catalog_fp: String,
    default_selector_id: String,
    role_bindings: WireRoles,
    by_selector: BTreeMap<String, usize>,
    legacy_exact: BTreeMap<String, usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionKind {
    ExactSelector,
    LegacyExact,
    OfficialRole,
}

#[derive(Clone, Copy, Debug)]
pub struct ResolvedRoute<'a> {
    route: &'a WireRoute,
    pub kind: ResolutionKind,
}

impl ResolvedRoute<'_> {
    pub fn upstream_model(&self) -> &str {
        &self.route.upstream_model
    }
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_selector(value: &str) -> bool {
    valid_text(value, MAX_SELECTOR_BYTES)
        && value.starts_with("claude-")
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn official_role_alias(value: &str) -> Option<&'static str> {
    if !value.is_ascii() || value.len() > MAX_SELECTOR_BYTES {
        return None;
    }
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() < 3 || parts[0] != "claude" {
        return None;
    }
    let role = match parts[1] {
        "sonnet" => "sonnet",
        "opus" => "opus",
        "haiku" => "haiku",
        "fable" => "fable",
        _ => {
            let mut historical = &parts[1..];
            if historical.last().is_some_and(|part| {
                part.len() == 8 && part.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                historical = &historical[..historical.len() - 1];
            }
            if historical.len() < 2 || historical.len() > 4 {
                return None;
            }
            let role = match historical.last().copied() {
                Some("sonnet") => "sonnet",
                Some("opus") => "opus",
                Some("haiku") => "haiku",
                _ => return None,
            };
            if historical[..historical.len() - 1].iter().any(|part| {
                part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                return None;
            }
            return Some(role);
        }
    };
    let mut version = &parts[2..];
    if version
        .last()
        .is_some_and(|part| part.len() == 8 && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        version = &version[..version.len() - 1];
    }
    if version.is_empty()
        || version.len() > 3
        || version.iter().any(|part| {
            part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    Some(role)
}

impl StaticProfileResolver {
    pub fn from_json(encoded: &str) -> Result<Self, String> {
        if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
            return Err("static model catalog 为空或超过 64 KiB".into());
        }
        let catalog: WireCatalog = serde_json::from_str(encoded)
            .map_err(|error| format!("static model catalog JSON 非法：{error}"))?;
        if catalog.schema_version != 1
            || !valid_text(&catalog.adapter, 64)
            || !catalog.adapter.is_ascii()
            || catalog.catalog_fp.len() != 64
            || !catalog
                .catalog_fp
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || catalog.routes.is_empty()
            || catalog.routes.len() > MAX_ROUTES
        {
            return Err("static model catalog schema 或 route 数量非法".into());
        }
        let computed_fp = wire_catalog_fingerprint(&catalog);
        if catalog.catalog_fp != computed_fp {
            return Err(format!(
                "static model catalog fingerprint 不匹配（claimed={} computed={}）",
                &catalog.catalog_fp[..12],
                &computed_fp[..12],
            ));
        }
        let mut by_selector = BTreeMap::new();
        for (index, route) in catalog.routes.iter().enumerate() {
            if !valid_selector(&route.selector_id)
                || !valid_text(&route.display_name, MAX_DISPLAY_BYTES)
                || !valid_text(&route.upstream_model, MAX_UPSTREAM_BYTES)
                || route.supports_tools == Some(false)
                || by_selector
                    .insert(route.selector_id.clone(), index)
                    .is_some()
            {
                return Err("static model catalog route 非法或 selector 重复".into());
            }
        }
        if !by_selector.contains_key(&catalog.default_selector_id) {
            return Err("static model catalog default selector 悬空".into());
        }
        for selector in [
            &catalog.role_bindings.sonnet,
            &catalog.role_bindings.opus,
            &catalog.role_bindings.haiku,
            &catalog.role_bindings.fable,
        ] {
            if !by_selector.contains_key(selector) {
                return Err("static model catalog role binding 悬空".into());
            }
        }
        let mut legacy_exact = BTreeMap::new();
        for legacy in &catalog.legacy_aliases {
            if !valid_text(&legacy.alias, MAX_UPSTREAM_BYTES)
                || !legacy.alias.is_ascii()
                || by_selector.contains_key(&legacy.alias)
                || legacy_exact.contains_key(&legacy.alias)
            {
                return Err("static model catalog legacy alias 非法或重复".into());
            }
            let Some(index) = by_selector.get(&legacy.selector_id).copied() else {
                return Err("static model catalog legacy alias selector 悬空".into());
            };
            legacy_exact.insert(legacy.alias.clone(), index);
        }
        Ok(Self {
            routes: catalog.routes,
            adapter: catalog.adapter,
            catalog_fp: catalog.catalog_fp,
            default_selector_id: catalog.default_selector_id,
            role_bindings: catalog.role_bindings,
            by_selector,
            legacy_exact,
        })
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub fn catalog_fp(&self) -> &str {
        &self.catalog_fp
    }

    pub fn resolve(&self, requested: &str) -> Option<ResolvedRoute<'_>> {
        if let Some(index) = self.by_selector.get(requested) {
            return Some(ResolvedRoute {
                route: &self.routes[*index],
                kind: ResolutionKind::ExactSelector,
            });
        }
        if let Some(index) = self.legacy_exact.get(requested) {
            return Some(ResolvedRoute {
                route: &self.routes[*index],
                kind: ResolutionKind::LegacyExact,
            });
        }
        let role = official_role_alias(requested)?;
        let selector = match role {
            "sonnet" => &self.role_bindings.sonnet,
            "opus" => &self.role_bindings.opus,
            "haiku" => &self.role_bindings.haiku,
            "fable" => &self.role_bindings.fable,
            _ => return None,
        };
        let index = self.by_selector.get(selector)?;
        Some(ResolvedRoute {
            route: &self.routes[*index],
            kind: ResolutionKind::OfficialRole,
        })
    }

    pub fn models_response(&self) -> Value {
        let mut indexes: Vec<usize> = (0..self.routes.len()).collect();
        if let Some(default_index) = self.by_selector.get(&self.default_selector_id).copied() {
            indexes.retain(|index| *index != default_index);
            indexes.insert(0, default_index);
        }
        let data: Vec<Value> = indexes
            .into_iter()
            .map(|index| {
                let route = &self.routes[index];
                // 旧配置可能把内部占位词 `default` 写进展示名。Science
                // 应展示用户实际配置的 upstream 模型，而不是伪模型名称。
                let display_name = if route.display_name.trim().eq_ignore_ascii_case("default") {
                    &route.upstream_model
                } else {
                    &route.display_name
                };
                json!({
                    "type": "model",
                    "id": route.selector_id,
                    "display_name": display_name,
                    "supports_tools": route.supports_tools,
                    "created_at": "2026-01-01T00:00:00Z",
                })
            })
            .collect();
        json!({
            "data": data,
            "has_more": false,
            "first_id": data.first().and_then(|item| item.get("id")).cloned().unwrap_or(Value::Null),
            "last_id": data.last().and_then(|item| item.get("id")).cloned().unwrap_or(Value::Null),
        })
    }
}

fn fingerprint_text(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u32).to_be_bytes());
    digest.update(value.as_bytes());
}

fn wire_catalog_fingerprint(catalog: &WireCatalog) -> String {
    let mut digest = Sha256::new();
    digest.update(b"csswitch-static-catalog-fp-v1\0");
    digest.update(catalog.schema_version.to_be_bytes());
    fingerprint_text(&mut digest, &catalog.adapter);
    fingerprint_text(&mut digest, &catalog.default_selector_id);
    digest.update((catalog.routes.len() as u32).to_be_bytes());
    for route in &catalog.routes {
        fingerprint_text(&mut digest, &route.selector_id);
        fingerprint_text(&mut digest, &route.display_name);
        fingerprint_text(&mut digest, &route.upstream_model);
        digest.update([match route.supports_tools {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }]);
    }
    for role in [
        &catalog.role_bindings.sonnet,
        &catalog.role_bindings.opus,
        &catalog.role_bindings.haiku,
        &catalog.role_bindings.fable,
    ] {
        fingerprint_text(&mut digest, role);
    }
    digest.update((catalog.legacy_aliases.len() as u32).to_be_bytes());
    for legacy in &catalog.legacy_aliases {
        fingerprint_text(&mut digest, &legacy.alias);
        fingerprint_text(&mut digest, &legacy.selector_id);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> StaticProfileResolver {
        let mut value = json!({
            "schema_version": 1,
            "adapter": "qwen",
            "default_selector_id": "claude-csswitch-qwen-plus-111111111111",
            "routes": [
                {"selector_id":"claude-csswitch-qwen-max-222222222222","display_name":"Max","upstream_model":"qwen-max","supports_tools":true},
                {"selector_id":"claude-csswitch-qwen-plus-111111111111","display_name":"Plus","upstream_model":"qwen-plus","supports_tools":null},
                {"selector_id":"claude-csswitch-qwen-fast-333333333333","display_name":"Fast","upstream_model":"qwen-fast","supports_tools":true}
            ],
            "role_bindings": {
                "sonnet":"claude-csswitch-qwen-plus-111111111111",
                "opus":"claude-csswitch-qwen-max-222222222222",
                "haiku":"claude-csswitch-qwen-fast-333333333333",
                "fable":"claude-csswitch-qwen-max-222222222222"
            },
            "legacy_aliases": [
                {"alias":"qwen-plus","selector_id":"claude-csswitch-qwen-plus-111111111111"}
            ]
        });
        let mut wire: WireCatalog = serde_json::from_value({
            value["catalog_fp"] = Value::String("0".repeat(64));
            value.clone()
        })
        .unwrap();
        wire.catalog_fp = wire_catalog_fingerprint(&wire);
        value["catalog_fp"] = Value::String(wire.catalog_fp);
        StaticProfileResolver::from_json(&value.to_string()).unwrap()
    }

    #[test]
    fn resolution_order_is_exact_legacy_role_then_unknown() {
        let resolver = resolver();
        assert_eq!(
            resolver
                .resolve("claude-csswitch-qwen-plus-111111111111")
                .unwrap()
                .kind,
            ResolutionKind::ExactSelector
        );
        assert_eq!(
            resolver.resolve("qwen-plus").unwrap().kind,
            ResolutionKind::LegacyExact
        );
        assert_eq!(
            resolver
                .resolve("claude-opus-4-8-20250514")
                .unwrap()
                .upstream_model(),
            "qwen-max"
        );
        assert_eq!(
            resolver
                .resolve("claude-3-5-sonnet-20241022")
                .unwrap()
                .upstream_model(),
            "qwen-plus"
        );
        for rejected in [
            "",
            "claude-opus",
            "claude-opus-4-8-latest",
            "claude-csswitch-codex-gpt-5",
            "claude-opus-4-8-forged",
        ] {
            assert!(resolver.resolve(rejected).is_none(), "{rejected}");
        }
    }

    #[test]
    fn models_response_places_default_first_without_mutating_saved_order() {
        let response = resolver().models_response();
        assert_eq!(
            response["data"][0]["id"],
            "claude-csswitch-qwen-plus-111111111111"
        );
        assert_eq!(response["data"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn models_response_never_exposes_default_as_a_display_name() {
        let mut resolver = resolver();
        let default_index = *resolver
            .by_selector
            .get(&resolver.default_selector_id)
            .unwrap();
        resolver.routes[default_index].display_name = "default".into();
        let response = resolver.models_response();
        assert_eq!(response["data"][0]["display_name"], "qwen-plus");
    }

    #[test]
    fn rejects_tampered_catalog_even_when_claimed_fingerprint_is_well_formed() {
        let mut value = serde_json::to_value(json!({
            "schema_version": 1,
            "adapter": "qwen",
            "default_selector_id": "claude-csswitch-qwen-plus-111111111111",
            "routes": [{"selector_id":"claude-csswitch-qwen-plus-111111111111","display_name":"Plus","upstream_model":"qwen-plus","supports_tools":true}],
            "role_bindings": {
                "sonnet":"claude-csswitch-qwen-plus-111111111111",
                "opus":"claude-csswitch-qwen-plus-111111111111",
                "haiku":"claude-csswitch-qwen-plus-111111111111",
                "fable":"claude-csswitch-qwen-plus-111111111111"
            },
            "legacy_aliases": []
        })).unwrap();
        value["catalog_fp"] = Value::String("0".repeat(64));
        let wire: WireCatalog = serde_json::from_value(value.clone()).unwrap();
        value["catalog_fp"] = Value::String(wire_catalog_fingerprint(&wire));
        value["routes"][0]["upstream_model"] = json!("tampered");
        assert!(StaticProfileResolver::from_json(&value.to_string())
            .unwrap_err()
            .contains("fingerprint"));
    }
}
