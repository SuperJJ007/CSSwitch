use std::path::Path;

use serde_json::{json, Value};

use crate::runtime::operation::{OperationKind, OperationStage, OperationTrace};
use crate::runtime::profile::merge_and_sort_models;
use crate::runtime::provider::{key_env_for_adapter, reject_openai_custom_anthropic_base};
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

fn discovery_adapter(
    template_id: &str,
    tpl_adapter: &'static str,
    api_format: &str,
) -> &'static str {
    if template_id == "custom" {
        match api_format {
            "openai_chat" => "openai-custom",
            "openai_responses" => "openai-responses",
            _ => tpl_adapter,
        }
    } else {
        tpl_adapter
    }
}

fn build_fetch_models_contract_response(
    outcome: &scratch::ProbeOutcome,
    status: Option<u16>,
    body: &str,
    builtin: &[&str],
) -> Result<Value, String> {
    match outcome {
        scratch::ProbeOutcome::Ok => {
            let v: Value =
                serde_json::from_str(body).map_err(|e| format!("解析模型列表失败：{e}"))?;
            let live: Vec<(String, Option<bool>)> = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let id = m.get("id")?.as_str()?.to_string();
                            let st = m.get("supports_tools").and_then(|b| b.as_bool());
                            Some((id, st))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if live.is_empty() {
                return Ok(json!({
                    "models": merge_and_sort_models(vec![], builtin),
                    "source": "builtin", "error_kind": null, "upstream_status": 200
                }));
            }
            Ok(json!({
                "models": merge_and_sort_models(live, builtin),
                "source": "live", "error_kind": null, "upstream_status": 200
            }))
        }
        scratch::ProbeOutcome::Auth(code) => {
            Err(format!("上游拒绝（{code}），key 或权限可能有误。"))
        }
        other => {
            let source = scratch::discovery_fallback_source(other);
            let error_kind = if source == "network" {
                json!("network")
            } else {
                json!(null)
            };
            Ok(json!({
                "models": merge_and_sort_models(vec![], builtin),
                "source": source,
                "error_kind": error_kind,
                "upstream_status": status
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

/// 「获取可用模型」——纯 scratch 探测：只用临时代理探候选 base_url/key 的 /v1/models，
/// 绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。
pub(crate) fn fetch_models(
    app: tauri::AppHandle,
    req: ModelDiscoveryRequest,
) -> Result<Value, String> {
    let tid = req.template_id.trim();
    let tpl = templates::by_id(tid).ok_or_else(|| format!("未知模板：{tid}"))?;
    let base_url = if tpl.base_url_editable {
        req.base_url.trim().to_string()
    } else {
        tpl.base_url.to_string()
    };
    if base_url.is_empty() || !(base_url.starts_with("http://") || base_url.starts_with("https://"))
    {
        return Err("请先填写 base_url（http:// 或 https:// 开头）。".into());
    }
    let api_format = effective_api_format_from_dir(
        &config::default_dir(),
        tpl,
        req.profile_id.as_deref(),
        req.api_format.as_deref(),
    )?;
    let key = resolve_probe_key(req.profile_id.as_deref(), &req.key)?;
    let adapter = discovery_adapter(tid, tpl.adapter, &api_format);
    reject_openai_custom_anthropic_base(adapter, &base_url)?;
    let backend = scratch::backend_for_app(&app, adapter)?;
    let trace = OperationTrace::start(
        OperationKind::FetchModels,
        format!("template_id={tid} adapter={adapter}"),
    );

    let res = scratch::scratch_probe(
        &backend,
        &scratch::ScratchTarget {
            provider: adapter,
            key_env: key_env_for_adapter(adapter),
            base_url: &base_url,
            key: &key,
            model: None,
            relay_thinking: if matches!(api_format.as_str(), "openai_chat" | "openai_responses") {
                ""
            } else {
                tpl.thinking_policy
            },
        },
        scratch::ProbeKind::Models,
        Some(&trace),
    );
    let builtin = tpl.builtin_models;
    let outcome = scratch::classify(res.status);
    match &outcome {
        scratch::ProbeOutcome::Ok => {
            trace.stage(OperationStage::ScratchUpstreamProbe, "outcome=ok");
            let live_count = live_model_count_from_body(&res.body)?;
            let response =
                build_fetch_models_contract_response(&outcome, res.status, &res.body, builtin)?;
            if live_count == 0 {
                trace.finish("ok source=builtin empty_live");
            } else {
                trace.finish(format!("ok source=live count={live_count}"));
            }
            Ok(response)
        }
        scratch::ProbeOutcome::Auth(code) => {
            trace.finish(format!("rejected status={code}"));
            build_fetch_models_contract_response(&outcome, res.status, &res.body, builtin)
        }
        // 非 200 且非 Auth：一律 builtin 兜底，但按语义分「发现不支持」(4xx) 与「网络/上游临时」(5xx/429/无响应)，
        // 供前端区分提示（spec v3 §3.4.3）。绝不把 Auth 混进来掩盖坏 key。
        other => {
            let source = scratch::discovery_fallback_source(other);
            trace.finish(format!("fallback source={source} outcome={other:?}"));
            build_fetch_models_contract_response(&outcome, res.status, &res.body, builtin)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_fetch_models_contract_response, discovery_adapter, effective_api_format_from_dir,
        resolve_probe_key, resolve_probe_key_from_dir,
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
    fn resolve_probe_key_can_reuse_profile_key_from_config() {
        let d = tmpdir_model_discovery();
        let id = create_profile_inner(&d, "glm", "GLM", Some("stored-key"), None, Some("glm-5.2"))
            .unwrap();
        let got = resolve_probe_key_from_dir(&d, Some(&id), "").unwrap();
        assert_eq!(got, "stored-key");
    }

    #[test]
    fn custom_profile_api_format_drives_model_discovery_adapter() {
        assert_eq!(
            discovery_adapter("custom", "relay", "openai_chat"),
            "openai-custom"
        );
        assert_eq!(
            discovery_adapter("custom", "relay", "openai_responses"),
            "openai-responses"
        );
        assert_eq!(discovery_adapter("glm", "relay", "openai_chat"), "relay");
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
            r#"{"data":[{"id":"m-live","supports_tools":true}]}"#,
            &["m-builtin"],
        )
        .unwrap();
        assert_eq!(live["source"], "live");
        assert_eq!(live["error_kind"], serde_json::Value::Null);
        assert_eq!(live["upstream_status"], 200);
        assert_eq!(live["models"][0]["id"], "m-live");
        assert_eq!(live["models"][0]["supports_tools"], true);

        let empty_live = build_fetch_models_contract_response(
            &ProbeOutcome::Ok,
            Some(200),
            r#"{"data":[]}"#,
            &["m-builtin"],
        )
        .unwrap();
        assert_eq!(empty_live["source"], "builtin");
        assert_eq!(empty_live["error_kind"], serde_json::Value::Null);
        assert_eq!(empty_live["upstream_status"], 200);
        assert_eq!(empty_live["models"][0]["id"], "m-builtin");
    }

    #[test]
    fn fetch_models_contract_keeps_auth_hard_and_soft_fallbacks_typed() {
        let auth = build_fetch_models_contract_response(
            &ProbeOutcome::Auth(401),
            Some(401),
            "",
            &["m-builtin"],
        );
        assert!(auth.unwrap_err().contains("401"));

        let unsupported = build_fetch_models_contract_response(
            &ProbeOutcome::Unsupported(405),
            Some(405),
            "",
            &["m-builtin"],
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
        )
        .unwrap();
        assert_eq!(network["source"], "network");
        assert_eq!(network["error_kind"], "network");
        assert!(network["upstream_status"].is_null());
    }
}
