use serde_json::json;

pub(crate) struct StatusProbeInput {
    pub(crate) proxy_ok: bool,
    pub(crate) sandbox_ok: bool,
    pub(crate) upstream_ok: bool,
}

pub(crate) struct ScienceDiagnosticsInput {
    pub(crate) sandbox_port: u16,
    pub(crate) sandbox_ok: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct StatusLights {
    pub(crate) proxy: &'static str,
    pub(crate) sandbox: &'static str,
    pub(crate) upstream: &'static str,
}

fn light(ok: bool) -> &'static str {
    if ok {
        "green"
    } else {
        "amber"
    }
}

/// Preserve the current status contract: each light is either `green` or `amber`.
pub(crate) fn status_lights(input: StatusProbeInput) -> StatusLights {
    StatusLights {
        proxy: light(input.proxy_ok),
        sandbox: light(input.sandbox_ok),
        upstream: light(input.upstream_ok),
    }
}

pub(crate) fn science_diagnostics(input: ScienceDiagnosticsInput) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "sandbox": {
            "port": input.sandbox_port,
            "health": light(input.sandbox_ok),
        },
        "auth": {
            "mode": "virtual_oauth",
            "real_account_verified": false,
            "real_home_verified": false,
            "known_boundary_rule_ids": [
                "science.auth.virtual-oauth-scope-boundary",
                "science.auth.refresh-hardcoded-0_1_15",
            ],
        },
        "version": {
            "status_probe": "not_run_in_status_poll",
            "known_rule_ids": [
                "science.version.0_1_15_dev.route-diff",
                "science.auth.refresh-hardcoded-0_1_15",
            ],
            "note": "status() does not run claude-science binary/version probes; use isolated HOME and non-8765 ports before making Science-version or real-account claims.",
        },
    })
}

pub(crate) fn proxy_status_last_error(
    secret_present: bool,
    proxy_ok: bool,
    proxy_port: u16,
) -> Option<serde_json::Value> {
    if secret_present && !proxy_ok {
        Some(json!({
            "type": "proxy_unhealthy",
            "message": "代理进程不可达或已退出，请点击「一键开始」或「启动代理」恢复。",
            "port": proxy_port,
        }))
    } else {
        None
    }
}

pub(crate) fn build_status_response(
    lights: StatusLights,
    active_profile: serde_json::Value,
    gateway_kind: &str,
    shim_mode: &str,
    catalog: serde_json::Value,
    science: serde_json::Value,
    last_error: Option<serde_json::Value>,
) -> serde_json::Value {
    json!({
        "proxy": lights.proxy,
        "sandbox": lights.sandbox,
        "upstream": lights.upstream,
        "active_profile": active_profile,
        "runtime": {
            "gateway_kind": gateway_kind,
            "shim_mode": shim_mode,
        },
        "catalog": catalog,
        "science": science,
        "last_error": last_error.unwrap_or(serde_json::Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_status_response, proxy_status_last_error, science_diagnostics, status_lights,
        ScienceDiagnosticsInput, StatusProbeInput,
    };
    use serde_json::{json, Value};

    fn assert_object_keys(v: &Value, expected: &[&str]) {
        let mut actual = v
            .as_object()
            .unwrap_or_else(|| panic!("expected object, got {v}"))
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        actual.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn status_lights_map_bools_to_existing_strings() {
        let all_green = status_lights(StatusProbeInput {
            proxy_ok: true,
            sandbox_ok: true,
            upstream_ok: true,
        });
        assert_eq!(all_green.proxy, "green");
        assert_eq!(all_green.sandbox, "green");
        assert_eq!(all_green.upstream, "green");

        let all_amber = status_lights(StatusProbeInput {
            proxy_ok: false,
            sandbox_ok: false,
            upstream_ok: false,
        });
        assert_eq!(all_amber.proxy, "amber");
        assert_eq!(all_amber.sandbox, "amber");
        assert_eq!(all_amber.upstream, "amber");
    }

    #[test]
    fn status_response_preserves_legacy_lights_and_adds_route_contract() {
        let lights = status_lights(StatusProbeInput {
            proxy_ok: true,
            sandbox_ok: false,
            upstream_ok: true,
        });
        let v = build_status_response(
            lights,
            json!({
                "id": "p1",
                "name": "GLM",
                "template_id": "glm",
                "api_format": "anthropic",
                "model": "glm-5.2",
            }),
            "python",
            "off",
            json!({
                "schema_version": 1,
                "status": "loaded",
                "active_rules": [],
                "boundary_rules": [],
            }),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 8990,
                sandbox_ok: false,
            }),
            None,
        );
        assert_eq!(v["proxy"], "green");
        assert_eq!(v["sandbox"], "amber");
        assert_eq!(v["upstream"], "green");
        assert_eq!(v["active_profile"]["template_id"], "glm");
        assert_eq!(v["runtime"]["gateway_kind"], "python");
        assert_eq!(v["runtime"]["shim_mode"], "off");
        assert_eq!(v["catalog"]["schema_version"], 1);
        assert_eq!(v["science"]["schema_version"], 1);
        assert_eq!(v["science"]["sandbox"]["port"], 8990);
        assert_eq!(v["science"]["sandbox"]["health"], "amber");
        assert_eq!(v["science"]["auth"]["real_account_verified"], false);
        assert_eq!(
            v["science"]["version"]["known_rule_ids"][1],
            "science.auth.refresh-hardcoded-0_1_15"
        );
        assert!(v["last_error"].is_null());
    }

    #[test]
    fn status_contract_freezes_healthy_active_profile_runtime_and_null_error_shape() {
        let v = build_status_response(
            status_lights(StatusProbeInput {
                proxy_ok: true,
                sandbox_ok: true,
                upstream_ok: true,
            }),
            json!({
                "id": "p1",
                "name": "GLM",
                "template_id": "glm",
                "api_format": "anthropic",
                "model": "glm-5.2",
                "capabilities": {
                    "base_url_required": true,
                    "model_required": true,
                    "model_discovery": "builtin_static",
                    "supports_thinking_policy": true,
                    "thinking_policy": "adaptive",
                    "supports_tools_hint": "native",
                },
            }),
            "python",
            "detect",
            json!({
                "schema_version": 1,
                "status": "loaded",
                "active_rules": [],
                "boundary_rules": [],
            }),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 18990,
                sandbox_ok: true,
            }),
            None,
        );

        assert_object_keys(
            &v,
            &[
                "active_profile",
                "catalog",
                "last_error",
                "proxy",
                "runtime",
                "sandbox",
                "science",
                "upstream",
            ],
        );
        assert_object_keys(
            &v["active_profile"],
            &[
                "api_format",
                "capabilities",
                "id",
                "model",
                "name",
                "template_id",
            ],
        );
        assert_object_keys(&v["runtime"], &["gateway_kind", "shim_mode"]);
        assert_eq!(v["active_profile"]["id"], "p1");
        assert_eq!(
            v["active_profile"]["capabilities"]["thinking_policy"],
            "adaptive"
        );
        assert_eq!(v["runtime"]["gateway_kind"], "python");
        assert_eq!(v["runtime"]["shim_mode"], "detect");
        assert!(v["last_error"].is_null());
    }

    #[test]
    fn status_contract_freezes_config_error_fail_closed_shape() {
        let v = build_status_response(
            status_lights(StatusProbeInput {
                proxy_ok: false,
                sandbox_ok: false,
                upstream_ok: false,
            }),
            Value::Null,
            "",
            "off",
            json!({
                "schema_version": 1,
                "status": "unavailable",
                "active_rules": [],
                "boundary_rules": [],
            }),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 0,
                sandbox_ok: false,
            }),
            Some(json!({
                "type": "config_error",
                "message": "config unreadable",
            })),
        );

        assert_eq!(v["proxy"], "amber");
        assert_eq!(v["sandbox"], "amber");
        assert_eq!(v["upstream"], "amber");
        assert!(v["active_profile"].is_null());
        assert_object_keys(&v["runtime"], &["gateway_kind", "shim_mode"]);
        assert_eq!(v["runtime"]["gateway_kind"], "");
        assert_eq!(v["runtime"]["shim_mode"], "off");
        assert_object_keys(&v["last_error"], &["message", "type"]);
        assert_eq!(v["last_error"]["type"], "config_error");
        assert_eq!(v["last_error"]["message"], "config unreadable");
    }

    #[test]
    fn status_response_can_surface_typed_last_error() {
        let v = build_status_response(
            status_lights(StatusProbeInput {
                proxy_ok: false,
                sandbox_ok: false,
                upstream_ok: false,
            }),
            serde_json::Value::Null,
            "python",
            "off",
            json!({"schema_version": 1}),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 8990,
                sandbox_ok: false,
            }),
            Some(json!({
                "type": "config_error",
                "message": "config unreadable",
            })),
        );
        assert_eq!(v["last_error"]["type"], "config_error");
        assert_eq!(v["last_error"]["message"], "config unreadable");
    }

    #[test]
    fn proxy_status_last_error_only_when_secreted_proxy_is_unhealthy() {
        assert!(proxy_status_last_error(false, false, 18991).is_none());
        assert!(proxy_status_last_error(true, true, 18991).is_none());

        let err = proxy_status_last_error(true, false, 18991).unwrap();
        assert_eq!(err["type"], "proxy_unhealthy");
        assert_eq!(err["port"], 18991);
        assert!(
            err["message"].as_str().unwrap().contains("代理进程不可达"),
            "should not imply an API key failure: {err}"
        );
    }

    #[test]
    fn status_response_can_surface_proxy_unhealthy_last_error() {
        let v = build_status_response(
            status_lights(StatusProbeInput {
                proxy_ok: false,
                sandbox_ok: true,
                upstream_ok: true,
            }),
            serde_json::Value::Null,
            "python",
            "off",
            json!({"schema_version": 1}),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 8990,
                sandbox_ok: true,
            }),
            proxy_status_last_error(true, false, 18991),
        );
        assert_eq!(v["proxy"], "amber");
        assert_eq!(v["sandbox"], "green");
        assert_eq!(v["upstream"], "green");
        assert_eq!(v["last_error"]["type"], "proxy_unhealthy");
        assert_eq!(v["last_error"]["port"], 18991);
    }

    #[test]
    fn status_contract_freezes_proxy_unhealthy_last_error_shape() {
        let v = build_status_response(
            status_lights(StatusProbeInput {
                proxy_ok: false,
                sandbox_ok: true,
                upstream_ok: true,
            }),
            Value::Null,
            "python",
            "off",
            json!({"schema_version": 1}),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 18990,
                sandbox_ok: true,
            }),
            proxy_status_last_error(true, false, 18991),
        );

        assert!(v["active_profile"].is_null());
        assert_object_keys(&v["runtime"], &["gateway_kind", "shim_mode"]);
        assert_eq!(v["runtime"]["gateway_kind"], "python");
        assert_eq!(v["runtime"]["shim_mode"], "off");
        assert_object_keys(&v["last_error"], &["message", "port", "type"]);
        assert_eq!(v["last_error"]["type"], "proxy_unhealthy");
        assert_eq!(v["last_error"]["port"], 18991);
        assert!(v["last_error"]["message"]
            .as_str()
            .unwrap()
            .contains("代理进程不可达"));
    }
}
