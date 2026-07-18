use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use crate::provider_contracts::{CredentialSource, EndpointPolicy};
use crate::runtime::operation::{OperationKind, OperationStage, OperationTrace};
use crate::runtime::profile::merge_and_sort_models;
use crate::runtime::provider::{reject_openai_custom_anthropic_base, resolve_launch_plan};
use crate::{config, scratch, templates};

pub(crate) struct ModelDiscoveryRequest {
    pub(crate) template_id: String,
    pub(crate) api_format: Option<String>,
    pub(crate) base_url: String,
    pub(crate) key: String,
    pub(crate) profile_id: Option<String>,
}

/// 解析探测用 key：新填的优先，否则沿用 profile_id 已存的（后端内部用，绝不回传前端）。
fn resolve_probe_key(profile_id: Option<&str>, candidate: &str) -> Result<String, String> {
    resolve_probe_key_from_dir(&config::default_dir(), profile_id, candidate)
}

fn resolve_probe_key_from_dir(
    dir: &Path,
    profile_id: Option<&str>,
    candidate: &str,
) -> Result<String, String> {
    let c = candidate.trim();
    if !c.is_empty() {
        return Ok(c.to_string());
    }
    let pid = profile_id.ok_or("请先填写 API Key / Token。")?;
    let cfg = config::load_from(dir).map_err(|e| e.to_string())?;
    cfg.profile_by_id(pid)
        .map(|p| p.api_key.clone())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "请先填写 API Key / Token。".to_string())
}

fn effective_api_format_from_dir(
    dir: &Path,
    tpl: &templates::Template,
    profile_id: Option<&str>,
    requested: Option<&str>,
) -> Result<String, String> {
    let requested = requested.unwrap_or("").trim();
    if !requested.is_empty() {
        return Ok(requested.to_string());
    }
    if let Some(pid) = profile_id {
        let cfg = config::load_from(dir).map_err(|e| e.to_string())?;
        if let Some(p) = cfg.profile_by_id(pid) {
            if !p.api_format.trim().is_empty() {
                return Ok(p.api_format.clone());
            }
        }
    }
    Ok(tpl.api_format.to_string())
}

fn build_fetch_models_contract_response(
    outcome: &scratch::ProbeOutcome,
    status: Option<u16>,
    body: &str,
    builtin: &[&str],
    saved: &[crate::model_catalog::ModelRoute],
) -> Result<Value, String> {
    let decorate = |live: Vec<(String, Option<bool>)>,
                    display_names: &BTreeMap<String, String>,
                    authoritative: bool,
                    healthy: bool| {
        let live_ids: BTreeSet<String> = live.iter().map(|(id, _)| id.clone()).collect();
        let mut candidates: Vec<String> = builtin.iter().map(|id| (*id).to_string()).collect();
        candidates.extend(saved.iter().map(|route| route.upstream_model.clone()));
        let candidate_refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        let builtin_ids: BTreeSet<&str> = builtin.iter().copied().collect();
        let mut models = merge_and_sort_models(live, &candidate_refs);
        for model in &mut models {
            let Some(id) = model.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            let saved_route = saved.iter().find(|route| route.upstream_model == id);
            let origin = if builtin_ids.contains(id.as_str()) {
                "preset"
            } else if saved_route.is_some() {
                "manual"
            } else {
                "discovered"
            };
            let availability = if live_ids.contains(&id) && healthy {
                "available"
            } else if authoritative {
                "not_reported"
            } else {
                "unknown"
            };
            model["origin"] = json!(origin);
            model["availability"] = json!(availability);
            if let Some(display_name) = display_names
                .get(&id)
                .or_else(|| saved_route.map(|route| &route.display_name))
            {
                model["display_name"] = json!(display_name);
            }
            if model.get("supports_tools").is_none_or(Value::is_null) {
                if let Some(value) = saved_route.and_then(|route| route.supports_tools) {
                    model["supports_tools"] = json!(value);
                }
            }
        }
        models
    };
    match outcome {
        scratch::ProbeOutcome::Ok => {
            let v: Value =
                serde_json::from_str(body).map_err(|e| format!("解析模型列表失败：{e}"))?;
            let diagnostics = v.get("diagnostics");
            let reported_source = diagnostics
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str)
                .filter(|source| {
                    matches!(
                        *source,
                        "live" | "fresh-cache" | "revalidated-cache" | "stale-cache"
                    )
                })
                .unwrap_or("live");
            let stale = diagnostics
                .and_then(|value| value.get("stale"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let age_seconds = diagnostics
                .and_then(|value| value.get("age_seconds"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mut display_names = BTreeMap::new();
            let live: Vec<(String, Option<bool>)> = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let id = m.get("id")?.as_str()?.to_string();
                            if let Some(display_name) = m
                                .get("display_name")
                                .and_then(Value::as_str)
                                .filter(|name| {
                                    !name.is_empty()
                                        && name.len() <= 512
                                        && !name.chars().any(char::is_control)
                                })
                            {
                                display_names.insert(id.clone(), display_name.to_string());
                            }
                            let st = m.get("supports_tools").and_then(|b| b.as_bool());
                            Some((id, st))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if live.is_empty() && (!builtin.is_empty() || !saved.is_empty()) {
                return Ok(json!({
                    "models": decorate(vec![], &display_names, false, false),
                    "source": "builtin", "error_kind": null, "upstream_status": 200,
                    "stale": false, "age_seconds": 0
                }));
            }
            let authoritative = !live.is_empty() && !stale;
            let models = decorate(live, &display_names, authoritative, !stale);
            Ok(json!({
                "models": models,
                "source": reported_source,
                "error_kind": if stale { json!("network") } else { Value::Null },
                "upstream_status": 200,
                "stale": stale,
                "age_seconds": age_seconds
            }))
        }
        scratch::ProbeOutcome::Auth(code) => {
            Err(format!("上游拒绝（{code}），key 或权限可能有误。"))
        }
        other => {
            let gateway_error_kind = scratch::gateway_models_error_kind(body);
            let source = if gateway_error_kind == Some("protocol") {
                "protocol"
            } else {
                scratch::discovery_fallback_source(other)
            };
            let error_kind = if gateway_error_kind == Some("protocol") {
                json!("protocol")
            } else if source == "network" {
                json!("network")
            } else {
                json!(null)
            };
            Ok(json!({
                "models": decorate(vec![], &BTreeMap::new(), false, false),
                "source": source,
                "error_kind": error_kind,
                "upstream_status": status,
                "stale": false,
                "age_seconds": 0
            }))
        }
    }
}

fn live_model_count_from_body(body: &str) -> Result<usize, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("解析模型列表失败：{e}"))?;
    Ok(v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|m| m.get("id").and_then(|id| id.as_str()).is_some())
                .count()
        })
        .unwrap_or(0))
}

fn builtin_static_response(tpl: &templates::Template) -> Result<Value, String> {
    let (preset, _, _) = tpl
        .preset_catalog_id
        .ok_or_else(|| "builtin_static 模板缺少 preset catalog".to_string())
        .and_then(crate::model_catalog::preset_catalog)?;
    Ok(json!({
        "models": preset.into_iter().map(|route| json!({
            "id": route.upstream_model,
            "display_name": route.display_name,
            "supports_tools": route.supports_tools,
            "origin": "preset",
            "availability": "unknown",
        })).collect::<Vec<_>>(),
        "source": "builtin",
        "error_kind": null,
        "upstream_status": null,
        "stale": false,
        "age_seconds": 0,
    }))
}

pub(crate) fn request_adapter(req: &ModelDiscoveryRequest) -> Result<String, String> {
    let tid = req.template_id.trim();
    let tpl = templates::by_id(tid).ok_or_else(|| format!("未知模板：{tid}"))?;
    let api_format = effective_api_format_from_dir(
        &config::default_dir(),
        tpl,
        req.profile_id.as_deref(),
        req.api_format.as_deref(),
    )?;
    Ok(crate::provider_contracts::contract_for(tid, &api_format)?
        .adapter
        .clone())
}

/// 「获取可用模型」——纯 scratch 探测：只用临时代理探候选 base_url/key 的 /v1/models，
/// 绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。
pub(crate) fn fetch_models(
    app: tauri::AppHandle,
    req: ModelDiscoveryRequest,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> Result<Value, String> {
    let tid = req.template_id.trim();
    let current_cfg =
        config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    config::require_template_enabled(&current_cfg, tid)?;
    let tpl = templates::by_id(tid).ok_or_else(|| format!("未知模板：{tid}"))?;
    let base_url = if tpl.base_url_editable {
        req.base_url.trim().to_string()
    } else {
        tpl.base_url.to_string()
    };
    let api_format = effective_api_format_from_dir(
        &config::default_dir(),
        tpl,
        req.profile_id.as_deref(),
        req.api_format.as_deref(),
    )?;
    let contract = crate::provider_contracts::contract_for(tid, &api_format)?;
    // Builtin-static providers (currently DeepSeek and Qwen) expose Science
    // compatibility shell aliases from their local gateway. Those aliases are
    // not upstream model IDs and must never enter a saved whitelist as live
    // discovery results. Return the versioned preset directly and be explicit
    // that no upstream availability check was performed.
    if contract.model_discovery == crate::provider_contracts::ModelDiscovery::BuiltinStatic {
        return builtin_static_response(tpl);
    }
    if contract.endpoint_policy == EndpointPolicy::ProfileRequired
        && (base_url.is_empty()
            || !(base_url.starts_with("http://") || base_url.starts_with("https://")))
    {
        return Err("请先填写 base_url（http:// 或 https:// 开头）。".into());
    }
    let mut candidate = if let Some(profile_id) = req.profile_id.as_deref() {
        config::load_from(&config::default_dir())
            .map_err(|error| error.to_string())?
            .profile_by_id(profile_id)
            .cloned()
            .ok_or_else(|| format!("找不到 profile：{profile_id}"))?
    } else {
        config::Profile {
            template_id: tid.to_string(),
            category: tpl.category.to_string(),
            api_format: api_format.clone(),
            credential_source: contract.default_credential_source,
            credential_ref: (contract.default_credential_source == CredentialSource::CsswitchOauth)
                .then(|| "csswitch:codex:default".to_string()),
            model_policy: contract.default_model_policy,
            ..Default::default()
        }
    };
    candidate.template_id = tid.to_string();
    candidate.api_format = api_format;
    candidate.base_url = base_url;
    let saved_catalog = candidate.model_catalog.clone();
    if candidate.credential_source == CredentialSource::ApiKey {
        candidate.api_key = resolve_probe_key(req.profile_id.as_deref(), &req.key)?;
    }
    if candidate.model_policy == crate::provider_contracts::ModelPolicy::SavedCatalog
        && candidate.model_catalog.is_empty()
    {
        let placeholder = (tpl.model_catalog_source == "manual_or_discovered")
            .then_some("csswitch-scratch-discovery-placeholder");
        let (catalog, default, roles) =
            crate::model_catalog::new_profile_catalog(tid, &candidate.api_format, placeholder)?;
        candidate.model = catalog
            .iter()
            .find(|route| route.selector_id == default)
            .map(|route| route.upstream_model.clone())
            .unwrap_or_default();
        candidate.model_catalog = catalog;
        candidate.default_model_route_id = default;
        candidate.role_bindings = roles;
    }
    let resolved = resolve_launch_plan(&candidate)?;
    let scratch_plan = resolved.scratch();
    crate::commands::codex::require_provider_auth_proof(&scratch_plan.provider, auth_proof)?;
    reject_openai_custom_anthropic_base(&scratch_plan.provider, &scratch_plan.endpoint)?;
    let backend = scratch::backend_for_app(&app, &scratch_plan.provider)?;
    let trace = OperationTrace::start(
        OperationKind::FetchModels,
        format!("template_id={tid} adapter={}", scratch_plan.provider),
    );
    let (key_env, key) = scratch_plan.credential_parts();

    let res = scratch::scratch_probe(
        &backend,
        &scratch::ScratchTarget {
            provider: &scratch_plan.provider,
            key_env,
            base_url: &scratch_plan.endpoint,
            key,
            model: None,
            static_model_catalog: None,
            relay_thinking: &scratch_plan.thinking_policy,
        },
        scratch::ProbeKind::Models,
        Some(&trace),
        auth_proof.map(|proof| proof.exit_cancel_flag()),
    );
    let builtin_owned = tpl
        .preset_catalog_id
        .map(crate::model_catalog::preset_upstream_models)
        .transpose()?
        .unwrap_or_default();
    let builtin: Vec<&str> = builtin_owned.iter().map(String::as_str).collect();
    let outcome = scratch::classify(res.status);
    match &outcome {
        scratch::ProbeOutcome::Ok => {
            trace.stage(OperationStage::ScratchUpstreamProbe, "outcome=ok");
            let live_count = live_model_count_from_body(&res.body)?;
            let response = build_fetch_models_contract_response(
                &outcome,
                res.status,
                &res.body,
                &builtin,
                &saved_catalog,
            )?;
            let source = response
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            trace.finish(format!("ok source={source} count={live_count}"));
            Ok(response)
        }
        scratch::ProbeOutcome::Auth(code) => {
            trace.finish(format!("rejected status={code}"));
            build_fetch_models_contract_response(
                &outcome,
                res.status,
                &res.body,
                &builtin,
                &saved_catalog,
            )
        }
        // 非 200 且非 Auth：一律 builtin 兜底，但按语义分「发现不支持」(4xx) 与「网络/上游临时」(5xx/429/无响应)，
        // 供前端区分提示（spec v3 §3.4.3）。绝不把 Auth 混进来掩盖坏 key。
        other => {
            let source = if scratch::gateway_models_error_kind(&res.body) == Some("protocol") {
                "protocol"
            } else {
                scratch::discovery_fallback_source(other)
            };
            trace.finish(format!("fallback source={source} outcome={other:?}"));
            build_fetch_models_contract_response(
                &outcome,
                res.status,
                &res.body,
                &builtin,
                &saved_catalog,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_fetch_models_contract_response, builtin_static_response,
        effective_api_format_from_dir, resolve_probe_key, resolve_probe_key_from_dir,
    };
    use crate::{config, runtime::profile::create_profile_inner, scratch::ProbeOutcome};

    fn tmpdir_model_discovery() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "csswitch-model-discovery-test-{}",
            std::process::id()
        ));
        let d = base.join(format!(
            "{:?}-{}",
            std::thread::current().id(),
            config::new_id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join(".csswitch")
    }

    #[test]
    fn resolve_probe_key_prefers_candidate() {
        assert_eq!(
            resolve_probe_key(Some("missing"), "  new-key ").unwrap(),
            "new-key"
        );
    }

    #[test]
    fn builtin_static_discovery_never_returns_science_shell_aliases_as_upstreams() {
        for template_id in ["deepseek", "qwen"] {
            let response =
                builtin_static_response(crate::templates::by_id(template_id).unwrap()).unwrap();
            let models = response["models"].as_array().unwrap();
            assert!(!models.is_empty());
            assert!(models.iter().all(|model| {
                model["origin"] == "preset"
                    && model["availability"] == "unknown"
                    && !model["id"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("claude-")
            }));
        }
    }

    #[test]
    fn resolve_probe_key_can_reuse_profile_key_from_config() {
        let d = tmpdir_model_discovery();
        let id = create_profile_inner(&d, "glm", "GLM", Some("stored-key"), None, Some("glm-5.2"))
            .unwrap();
        let got = resolve_probe_key_from_dir(&d, Some(&id), "").unwrap();
        assert_eq!(got, "stored-key");
    }

    #[test]
    fn provider_contract_drives_model_discovery_adapter() {
        assert_eq!(
            crate::provider_contracts::contract_for("custom", "openai_chat")
                .unwrap()
                .adapter,
            "openai-custom"
        );
        assert_eq!(
            crate::provider_contracts::contract_for("custom", "openai_responses")
                .unwrap()
                .adapter,
            "openai-responses"
        );
        assert!(crate::provider_contracts::contract_for("glm", "openai_chat").is_err());
    }

    #[test]
    fn effective_api_format_prefers_request_then_profile_then_template() {
        let d = tmpdir_model_discovery();
        let id = create_profile_inner(
            &d,
            "custom",
            "Custom",
            Some("stored-key"),
            Some("https://example.com/v1"),
            Some("model-a"),
        )
        .unwrap();
        config::update(&d, |c| {
            c.profile_by_id_mut(&id).unwrap().api_format = "openai_responses".into();
        })
        .unwrap();
        let tpl = crate::templates::by_id("custom").unwrap();
        assert_eq!(
            effective_api_format_from_dir(&d, tpl, Some(&id), None).unwrap(),
            "openai_responses"
        );
        assert_eq!(
            effective_api_format_from_dir(&d, tpl, Some(&id), Some("openai_chat")).unwrap(),
            "openai_chat"
        );
        assert_eq!(
            effective_api_format_from_dir(&d, tpl, None, None).unwrap(),
            "anthropic"
        );
    }

    #[test]
    fn fetch_models_contract_maps_live_and_empty_live_to_frozen_shape() {
        let live = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[{"id":"m-live","display_name":"Codex / GPT-5.6-Sol","supports_tools":true}]}"#,
            &["m-builtin"],
            &[],
        )
        .unwrap();
        assert_eq!(live["source"], "live");
        assert_eq!(live["error_kind"], serde_json::Value::Null);
        assert_eq!(live["upstream_status"], 200);
        assert_eq!(live["models"][0]["id"], "m-live");
        assert_eq!(live["models"][0]["display_name"], "Codex / GPT-5.6-Sol");
        assert_eq!(live["models"][0]["supports_tools"], true);

        let unsafe_display = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[{"id":"m-live","display_name":"bad\u0001name"}]}"#,
            &[],
            &[],
        )
        .unwrap();
        assert!(unsafe_display["models"][0]["display_name"].is_null());

        let empty_live = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[]}"#,
            &["m-builtin"],
            &[],
        )
        .unwrap();
        assert_eq!(empty_live["source"], "builtin");
        assert_eq!(empty_live["error_kind"], serde_json::Value::Null);
        assert_eq!(empty_live["upstream_status"], 200);
        assert_eq!(empty_live["models"][0]["id"], "m-builtin");

        let empty_dynamic = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[],"diagnostics":{"source":"live","stale":false,"age_seconds":0}}"#,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(empty_dynamic["source"], "live");
        assert!(empty_dynamic["models"].as_array().unwrap().is_empty());

        let stale = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[{"id":"m-cached"}],"diagnostics":{"source":"stale-cache","stale":true,"age_seconds":301}}"#,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(stale["source"], "stale-cache");
        assert_eq!(stale["stale"], true);
        assert_eq!(stale["age_seconds"], 301);
        assert_eq!(stale["error_kind"], "network");
    }

    #[test]
    fn fetch_models_contract_preserves_origins_and_authoritative_availability() {
        let saved = [crate::model_catalog::ModelRoute {
            selector_id: "claude-csswitch-custom-saved-123456789abc".into(),
            display_name: "Saved display".into(),
            upstream_model: "m-saved".into(),
            supports_tools: Some(true),
            ..Default::default()
        }];
        let response = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[{"id":"m-live","supports_tools":true},{"id":"m-saved"}]}"#,
            &["m-preset"],
            &saved,
        )
        .unwrap();
        let models = response["models"].as_array().unwrap();
        let by_id = |id: &str| models.iter().find(|model| model["id"] == id).unwrap();

        assert_eq!(by_id("m-live")["origin"], "discovered");
        assert_eq!(by_id("m-live")["availability"], "available");
        assert_eq!(by_id("m-live")["supports_tools"], true);
        assert_eq!(by_id("m-saved")["origin"], "manual");
        assert_eq!(by_id("m-saved")["availability"], "available");
        assert_eq!(by_id("m-saved")["display_name"], "Saved display");
        assert_eq!(by_id("m-saved")["supports_tools"], true);
        assert_eq!(by_id("m-preset")["origin"], "preset");
        assert_eq!(by_id("m-preset")["availability"], "not_reported");
    }

    #[test]
    fn fetch_models_contract_keeps_whitelist_unknown_on_empty_stale_or_network() {
        let saved = [crate::model_catalog::ModelRoute {
            selector_id: "claude-csswitch-custom-saved-123456789abc".into(),
            display_name: "Saved display".into(),
            upstream_model: "m-saved".into(),
            supports_tools: None,
            ..Default::default()
        }];
        let assert_whitelist_unknown = |response: &serde_json::Value| {
            let models = response["models"].as_array().unwrap();
            assert_eq!(models.len(), 2);
            for (id, origin) in [("m-preset", "preset"), ("m-saved", "manual")] {
                let model = models.iter().find(|model| model["id"] == id).unwrap();
                assert_eq!(model["origin"], origin);
                assert_eq!(model["availability"], "unknown");
            }
        };

        let empty = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[]}"#,
            &["m-preset"],
            &saved,
        )
        .unwrap();
        assert_whitelist_unknown(&empty);

        let stale = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[],"diagnostics":{"source":"stale-cache","stale":true,"age_seconds":301}}"#,
            &["m-preset"],
            &saved,
        )
        .unwrap();
        assert_whitelist_unknown(&stale);

        let network = build_fetch_models_contract_response(
            &ProbeOutcome::NoResponse,
            None,
            "",
            &["m-preset"],
            &saved,
        )
        .unwrap();
        assert_whitelist_unknown(&network);
    }

    #[test]
    fn fetch_models_display_names_match_gateway_safety_bounds() {
        let ascii_161 = "A".repeat(161);
        let unicode_510 = "界".repeat(170);
        let unicode_512 = format!("{}ab", unicode_510);
        let unicode_513 = "界".repeat(171);
        let trailing = "Codex / trailing  ";
        let html_like = "Codex / <b>Sol</b>";
        let body = serde_json::json!({
            "data": [
                {"id": "ascii-161", "display_name": ascii_161},
                {"id": "unicode-510", "display_name": unicode_510},
                {"id": "unicode-512", "display_name": unicode_512},
                {"id": "unicode-513", "display_name": unicode_513},
                {"id": "trailing", "display_name": trailing},
                {"id": "c1", "display_name": "bad\u{0085}name"},
                {"id": "html", "display_name": html_like}
            ]
        })
        .to_string();
        let response =
            build_fetch_models_contract_response(&ProbeOutcome::Ok, Some(200), &body, &[], &[])
                .unwrap();
        let models = response["models"].as_array().unwrap();
        let display = |id: &str| {
            models
                .iter()
                .find(|model| model["id"] == id)
                .and_then(|model| model.get("display_name"))
                .and_then(serde_json::Value::as_str)
        };

        assert_eq!(display("ascii-161"), Some(ascii_161.as_str()));
        assert_eq!(display("unicode-510"), Some(unicode_510.as_str()));
        assert_eq!(display("unicode-512"), Some(unicode_512.as_str()));
        assert_eq!(display("unicode-513"), None);
        assert_eq!(display("trailing"), Some(trailing));
        assert_eq!(display("c1"), None);
        assert_eq!(display("html"), Some(html_like));
    }

    #[test]
    fn fetch_models_contract_keeps_auth_hard_and_soft_fallbacks_typed() {
        let auth = build_fetch_models_contract_response(
            &ProbeOutcome::Auth(401),
            Some(401),
            "",
            &["m-builtin"],
            &[],
        );
        assert!(auth.unwrap_err().contains("401"));

        let unsupported = build_fetch_models_contract_response(
            &ProbeOutcome::Unsupported(405),
            Some(405),
            "",
            &["m-builtin"],
            &[],
        )
        .unwrap();
        assert_eq!(unsupported["source"], "unsupported");
        assert_eq!(unsupported["error_kind"], serde_json::Value::Null);
        assert_eq!(unsupported["upstream_status"], 405);
        assert_eq!(unsupported["models"][0]["id"], "m-builtin");

        let network = build_fetch_models_contract_response(
            &ProbeOutcome::NoResponse,
            None,
            "",
            &["m-builtin"],
            &[],
        )
        .unwrap();
        assert_eq!(network["source"], "network");
        assert_eq!(network["error_kind"], "network");
        assert!(network["upstream_status"].is_null());

        let protocol = build_fetch_models_contract_response(
            &ProbeOutcome::Ambiguous(Some(502)),
            Some(502),
            r#"{"error_kind":"protocol","message":"private detail"}"#,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(protocol["source"], "protocol");
        assert_eq!(protocol["error_kind"], "protocol");
        assert_eq!(protocol["upstream_status"], 502);
    }
}
