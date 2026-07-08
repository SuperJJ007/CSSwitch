use serde_json::json;

pub(crate) struct StatusProbeInput {
    pub(crate) proxy_ok: bool,
    pub(crate) sandbox_ok: bool,
    pub(crate) upstream_ok: bool,
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

pub(crate) fn build_status_response(
    lights: StatusLights,
    active_profile: serde_json::Value,
    gateway_kind: &str,
    shim_mode: &str,
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
        "last_error": null,
    })
}

#[cfg(test)]
mod tests {
    use super::{build_status_response, status_lights, StatusProbeInput};
    use serde_json::json;

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
        );
        assert_eq!(v["proxy"], "green");
        assert_eq!(v["sandbox"], "amber");
        assert_eq!(v["upstream"], "green");
        assert_eq!(v["active_profile"]["template_id"], "glm");
        assert_eq!(v["runtime"]["gateway_kind"], "python");
        assert_eq!(v["runtime"]["shim_mode"], "off");
        assert!(v["last_error"].is_null());
    }
}
