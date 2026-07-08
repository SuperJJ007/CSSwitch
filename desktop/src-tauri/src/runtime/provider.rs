use crate::{config, templates};

/// key 的非加密指纹（SipHash），只用于判断「配置是否变了」。绝不打印、绝不落盘。
pub(crate) fn key_fingerprint(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// adapter -> 该 adapter 期望的 key 环境变量名（python 代理侧 PROVIDERS[...]["key_env"]）。
pub(crate) fn key_env_for_adapter(adapter: &str) -> &'static str {
    match adapter {
        "deepseek" => "DEEPSEEK_API_KEY",
        "qwen" => "DASHSCOPE_API_KEY",
        "openai-custom" | "openai-responses" => "CSSWITCH_OPENAI_KEY",
        _ => "CSSWITCH_RELAY_KEY", // relay / 兜底
    }
}

/// 从一条 profile 派生出起代理需要的全部参数（纯函数，便于测试）。
pub(crate) struct ProxyLaunch {
    pub(crate) adapter: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) key: String,
    pub(crate) key_env: &'static str,
    pub(crate) thinking_policy: &'static str,
}

pub(crate) fn adapter_for_profile(p: &config::Profile) -> &'static str {
    if p.template_id == "custom" {
        match p.api_format.as_str() {
            "openai_chat" => "openai-custom",
            "openai_responses" => "openai-responses",
            _ => templates::adapter_for(&p.template_id),
        }
    } else {
        templates::adapter_for(&p.template_id)
    }
}

pub(crate) fn proxy_args_for(p: &config::Profile) -> ProxyLaunch {
    let adapter = adapter_for_profile(p).to_string();
    let key_env = key_env_for_adapter(&adapter);
    ProxyLaunch {
        adapter,
        base_url: p.base_url.clone(),
        model: p.model.clone(),
        key: p.api_key.clone(),
        key_env,
        thinking_policy: templates::thinking_policy_for(&p.template_id),
    }
}

pub(crate) fn proxy_fingerprint(p: &config::Profile, launch: &ProxyLaunch) -> u64 {
    proxy_fingerprint_with_runtime(
        p,
        launch,
        gateway_kind_for_adapter(&launch.adapter),
        current_shim_mode_for_adapter(&launch.adapter),
    )
}

pub(crate) fn proxy_fingerprint_with_runtime(
    p: &config::Profile,
    launch: &ProxyLaunch,
    gateway_kind: &str,
    shim_mode: &str,
) -> u64 {
    key_fingerprint(&format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        p.template_id,
        p.api_format,
        launch.adapter,
        launch.base_url,
        launch.model,
        launch.thinking_policy,
        launch.key,
        gateway_kind,
        shim_mode
    ))
}

/// 本轨支持 anthropic / openai_chat / openai_responses；其余进 schema 但激活拒绝（待轨道 2：Rust 代理）。
pub(crate) fn assert_format_supported(p: &config::Profile) -> Result<(), String> {
    match p.api_format.as_str() {
        "anthropic" | "openai_chat" | "openai_responses" => Ok(()),
        other => Err(format!(
            "api_format `{other}` 暂不支持（待 Rust 代理），请选 anthropic、openai_chat 或 openai_responses。"
        )),
    }
}

fn looks_like_anthropic_endpoint(base_url: &str) -> bool {
    let u = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    u.contains("/anthropic")
}

pub(crate) fn reject_openai_custom_anthropic_base(
    adapter: &str,
    base_url: &str,
) -> Result<(), String> {
    if is_openai_adapter(adapter) && looks_like_anthropic_endpoint(base_url) {
        Err("这个地址看起来是 Anthropic 兼容端点。请改选「自定义 Anthropic」，或使用 OpenAI 兼容 base root（如 https://api.moonshot.cn/v1）。".to_string())
    } else {
        Ok(())
    }
}

/// deepseek/qwen 走各自固定官方端点（python 侧硬编码）；其余 = relay 家族，需带 base_url。
pub(crate) fn is_native_adapter(adapter: &str) -> bool {
    adapter == "deepseek" || adapter == "qwen"
}

pub(crate) fn is_openai_adapter(adapter: &str) -> bool {
    matches!(adapter, "openai-custom" | "openai-responses")
}

pub(crate) fn gateway_kind_for_adapter(_adapter: &str) -> &'static str {
    "python"
}

pub(crate) fn normalize_shim_mode(adapter: &str, raw: Option<&str>) -> &'static str {
    if adapter != "deepseek" {
        return "off";
    }
    match raw.unwrap_or("").trim() {
        "detect" => "detect",
        "rewrite" => "rewrite",
        _ => "off",
    }
}

pub(crate) fn current_shim_mode_for_adapter(adapter: &str) -> &'static str {
    normalize_shim_mode(
        adapter,
        std::env::var("CSSWITCH_TOOLUSE_SHIM").ok().as_deref(),
    )
}

/// 上游主机名（供 status 上游灯做 TCP 可达性探测）。relay 家族从其 base_url 解析。
pub(crate) fn upstream_host(adapter: &str, base_url: &str) -> String {
    match adapter {
        "deepseek" => "api.deepseek.com".to_string(),
        "qwen" => "dashscope.aliyuncs.com".to_string(),
        _ => parse_host(base_url).unwrap_or_default(),
    }
}

/// 从 `http(s)://host[:port]/path` 里抽出 host。解析不出返回 None（不引 url crate）。
pub(crate) fn parse_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 是否对候选连接跑上游 scratch 校验。
pub(crate) fn should_scratch_candidate(adapter: &str, key: &str, base_url: &str) -> bool {
    if key.is_empty() {
        return false; // 无 key -> 无从验，如实标记未校验。
    }
    if !is_native_adapter(adapter) && base_url.is_empty() {
        return false; // relay 家族缺 base_url -> 无从验。
    }
    true
}

/// 保存前守卫：非 native 家族空 base_url 的候选连接不可用。
pub(crate) fn relay_missing_base_url(adapter: &str, base_url: &str) -> bool {
    !is_native_adapter(adapter) && base_url.trim().is_empty()
}

/// 保存/激活前守卫：非 native 家族空（含纯空白）model 不可用。
pub(crate) fn relay_missing_model(adapter: &str, model: &str) -> bool {
    !is_native_adapter(adapter) && model.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        adapter_for_profile, assert_format_supported, gateway_kind_for_adapter,
        key_env_for_adapter, key_fingerprint, normalize_shim_mode, parse_host, proxy_args_for,
        proxy_fingerprint, proxy_fingerprint_with_runtime, reject_openai_custom_anthropic_base,
        relay_missing_base_url, relay_missing_model, should_scratch_candidate, upstream_host,
    };
    use crate::config::Profile;

    #[test]
    fn proxy_args_derive_adapter_and_key_env() {
        let ds = Profile {
            template_id: "deepseek".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-ds".into(),
            ..Default::default()
        };
        let a = proxy_args_for(&ds);
        assert_eq!(a.adapter, "deepseek");
        assert_eq!(a.key_env, "DEEPSEEK_API_KEY");

        let glm = Profile {
            template_id: "glm".into(),
            api_format: "anthropic".into(),
            base_url: "https://open.bigmodel.cn/api/anthropic".into(),
            api_key: "gk".into(),
            model: "glm-5".into(),
            ..Default::default()
        };
        let b = proxy_args_for(&glm);
        assert_eq!(b.adapter, "relay");
        assert_eq!(b.key_env, "CSSWITCH_RELAY_KEY");
        assert_eq!(b.base_url, "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(b.model, "glm-5");

        let custom_openai = Profile {
            template_id: "custom-openai".into(),
            api_format: "openai_chat".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            api_key: "ok".into(),
            model: "glm-4.5".into(),
            ..Default::default()
        };
        let c = proxy_args_for(&custom_openai);
        assert_eq!(c.adapter, "openai-custom");
        assert_eq!(c.key_env, "CSSWITCH_OPENAI_KEY");
        assert_eq!(c.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(c.model, "glm-4.5");

        let custom_responses = Profile {
            template_id: "custom-openai-responses".into(),
            api_format: "openai_responses".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: "ok".into(),
            model: "gpt-5.2".into(),
            ..Default::default()
        };
        let d = proxy_args_for(&custom_responses);
        assert_eq!(d.adapter, "openai-responses");
        assert_eq!(d.key_env, "CSSWITCH_OPENAI_KEY");
        assert_eq!(d.base_url, "https://api.openai.com/v1");
        assert_eq!(d.model, "gpt-5.2");

        let custom_profile_openai = Profile {
            template_id: "custom".into(),
            api_format: "openai_chat".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "ok".into(),
            model: "gpt-5.2".into(),
            ..Default::default()
        };
        let e = proxy_args_for(&custom_profile_openai);
        assert_eq!(adapter_for_profile(&custom_profile_openai), "openai-custom");
        assert_eq!(e.adapter, "openai-custom");
        assert_eq!(e.key_env, "CSSWITCH_OPENAI_KEY");

        let custom_profile_responses = Profile {
            api_format: "openai_responses".into(),
            ..custom_profile_openai
        };
        let f = proxy_args_for(&custom_profile_responses);
        assert_eq!(
            adapter_for_profile(&custom_profile_responses),
            "openai-responses"
        );
        assert_eq!(f.adapter, "openai-responses");
        assert_eq!(f.key_env, "CSSWITCH_OPENAI_KEY");

        let non_custom_openai_format = Profile {
            template_id: "glm".into(),
            api_format: "openai_chat".into(),
            base_url: "https://open.bigmodel.cn/api/anthropic".into(),
            api_key: "ok".into(),
            model: "glm-5".into(),
            ..Default::default()
        };
        assert_eq!(adapter_for_profile(&non_custom_openai_format), "relay");
    }

    #[test]
    fn unsupported_api_format_is_rejected() {
        let p = Profile {
            template_id: "custom".into(),
            api_format: "gemini_native".into(),
            base_url: "https://x/y".into(),
            api_key: "k".into(),
            ..Default::default()
        };
        assert!(assert_format_supported(&p).is_err());
        let ok = Profile {
            api_format: "anthropic".into(),
            ..p.clone()
        };
        assert!(assert_format_supported(&ok).is_ok());
        let ok2 = Profile {
            api_format: "openai_chat".into(),
            ..p.clone()
        };
        assert!(assert_format_supported(&ok2).is_ok());
        let ok3 = Profile {
            api_format: "openai_responses".into(),
            ..ok2
        };
        assert!(assert_format_supported(&ok3).is_ok());
    }

    #[test]
    fn custom_openai_rejects_anthropic_base_url() {
        let err = reject_openai_custom_anthropic_base(
            "openai-custom",
            "https://api.moonshot.cn/anthropic",
        )
        .unwrap_err();
        assert!(err.contains("自定义 Anthropic"));
        assert!(
            reject_openai_custom_anthropic_base("openai-custom", "https://api.moonshot.cn/v1",)
                .is_ok()
        );
        assert!(reject_openai_custom_anthropic_base(
            "openai-responses",
            "https://api.moonshot.cn/anthropic",
        )
        .is_err());
        assert!(
            reject_openai_custom_anthropic_base("relay", "https://api.moonshot.cn/anthropic",)
                .is_ok()
        );
    }

    #[test]
    fn key_env_for_adapter_maps_adapters() {
        assert_eq!(key_env_for_adapter("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(key_env_for_adapter("qwen"), "DASHSCOPE_API_KEY");
        assert_eq!(key_env_for_adapter("openai-custom"), "CSSWITCH_OPENAI_KEY");
        assert_eq!(
            key_env_for_adapter("openai-responses"),
            "CSSWITCH_OPENAI_KEY"
        );
        assert_eq!(key_env_for_adapter("relay"), "CSSWITCH_RELAY_KEY");
        assert_eq!(key_env_for_adapter("anything-else"), "CSSWITCH_RELAY_KEY");
    }

    #[test]
    fn proxy_fingerprint_includes_protocol_semantics() {
        let mut p = Profile {
            template_id: "kimi".into(),
            api_format: "anthropic".into(),
            base_url: "https://same.example/anthropic".into(),
            api_key: "same-key".into(),
            model: "same-model".into(),
            ..Default::default()
        };
        let kimi_launch = proxy_args_for(&p);
        let kimi_fp = proxy_fingerprint(&p, &kimi_launch);

        p.template_id = "custom".into();
        let custom_launch = proxy_args_for(&p);
        let custom_fp = proxy_fingerprint(&p, &custom_launch);
        assert_ne!(
            kimi_fp, custom_fp,
            "同 adapter/base/model/key 但模板语义不同，必须重启代理"
        );
    }

    #[test]
    fn proxy_fingerprint_includes_gateway_and_shim_identity() {
        let p = Profile {
            template_id: "deepseek".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "same-key".into(),
            model: "same-model".into(),
            ..Default::default()
        };
        let launch = proxy_args_for(&p);
        let python_off = proxy_fingerprint_with_runtime(&p, &launch, "python", "off");
        let rust_off = proxy_fingerprint_with_runtime(&p, &launch, "rust", "off");
        let python_detect = proxy_fingerprint_with_runtime(&p, &launch, "python", "detect");
        assert_ne!(python_off, rust_off, "gateway 切换必须阻止误复用");
        assert_ne!(python_off, python_detect, "shim 切换必须阻止误复用");
    }

    #[test]
    fn parse_host_extracts_host_from_relay_base_url() {
        assert_eq!(
            parse_host("https://byteswarm.ai/claude").as_deref(),
            Some("byteswarm.ai")
        );
        assert_eq!(
            parse_host("http://127.0.0.1:8080/v1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            parse_host("https://relay.example.com:8443").as_deref(),
            Some("relay.example.com")
        );
        assert_eq!(parse_host("byteswarm.ai/claude"), None);
        assert_eq!(parse_host(""), None);
    }

    #[test]
    fn upstream_host_by_adapter() {
        assert_eq!(upstream_host("deepseek", ""), "api.deepseek.com");
        assert_eq!(upstream_host("qwen", ""), "dashscope.aliyuncs.com");
        assert_eq!(
            upstream_host("openai-custom", "https://open.bigmodel.cn/api/paas/v4"),
            "open.bigmodel.cn"
        );
        assert_eq!(
            upstream_host("relay", "https://open.bigmodel.cn/api/anthropic"),
            "open.bigmodel.cn"
        );
        assert_eq!(upstream_host("", ""), "", "无生效配置 -> 空（灯显黄）");
    }

    #[test]
    fn runtime_identity_contract_defaults_to_python_and_deepseek_only_shim() {
        assert_eq!(gateway_kind_for_adapter("deepseek"), "python");
        assert_eq!(gateway_kind_for_adapter("openai-custom"), "python");
        assert_eq!(normalize_shim_mode("deepseek", Some("detect")), "detect");
        assert_eq!(normalize_shim_mode("deepseek", Some("rewrite")), "rewrite");
        assert_eq!(normalize_shim_mode("deepseek", Some("bad")), "off");
        assert_eq!(normalize_shim_mode("relay", Some("rewrite")), "off");
        assert_eq!(normalize_shim_mode("qwen", Some("detect")), "off");
    }

    #[test]
    fn key_fingerprint_stable_and_distinct() {
        assert_eq!(key_fingerprint("sk-aaaa"), key_fingerprint("sk-aaaa"));
        assert_ne!(key_fingerprint("sk-aaaa"), key_fingerprint("sk-bbbb"));
        assert_ne!(key_fingerprint(""), key_fingerprint("x"));
    }

    #[test]
    fn native_candidate_is_upstream_validated_even_without_base_url() {
        // 非 active 编辑：native 即便 base_url 空也要验（走硬编码官方端点）。
        assert!(should_scratch_candidate("deepseek", "sk-x", ""));
        assert!(should_scratch_candidate("qwen", "sk-x", ""));
        // relay 仍需 base_url；空 key 一律免验。
        assert!(!should_scratch_candidate("relay", "sk-x", ""));
        assert!(should_scratch_candidate("relay", "sk-x", "https://r"));
        assert!(!should_scratch_candidate("deepseek", "", ""));
    }

    #[test]
    fn relay_empty_base_url_is_rejected_before_save() {
        // 修 P2：relay/自定义端点空（或纯空白）base_url -> 拦下，不落盘。
        assert!(relay_missing_base_url("relay", ""));
        assert!(relay_missing_base_url("glm", "   "));
        assert!(relay_missing_base_url("custom", ""));
        // 带地址的 relay 放行。
        assert!(!relay_missing_base_url("relay", "https://r"));
        // native 走硬编码端点，空 base_url 无妨 -> 不拦。
        assert!(!relay_missing_base_url("deepseek", ""));
        assert!(!relay_missing_base_url("qwen", ""));
    }

    #[test]
    fn relay_empty_model_is_rejected() {
        // 修 #9 P1-a：relay/自定义端点空（或纯空白）model -> 拦下。
        assert!(relay_missing_model("relay", ""));
        assert!(relay_missing_model("glm", "   "));
        assert!(relay_missing_model("custom", ""));
        assert!(!relay_missing_model("relay", "glm-5.2"));
        // native 走内置映射/硬编码端点，model 可空 -> 不拦。
        assert!(!relay_missing_model("deepseek", ""));
        assert!(!relay_missing_model("qwen", ""));
    }
}
