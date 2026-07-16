use serde::Serialize;

use crate::config;
use crate::provider_contracts::{
    self, AuthMode, CachePolicy, CredentialSource, EndpointPolicy, ModelDiscovery, ModelPolicy,
    ScratchPolicy, TimeoutPolicy, Transport,
};

/// key 的非加密指纹（SipHash），只用于判断「配置是否变了」。绝不打印、绝不落盘。
pub(crate) fn key_fingerprint(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

enum CredentialHandle {
    ApiKey { env: String, value: String },
    CodexDefault,
    None,
}

/// provider-contract catalog 与 profile 合并后的私有计划。credential 保持 opaque，
/// 不实现 Debug/Serialize，不能被日志或 Tauri IPC 意外投影。
pub(crate) struct ResolvedLaunchPlan {
    pub(crate) adapter: String,
    pub(crate) endpoint: String,
    pub(crate) model: String,
    credential: CredentialHandle,
    pub(crate) model_policy: ModelPolicy,
    pub(crate) model_discovery: ModelDiscovery,
    pub(crate) transport: Transport,
    pub(crate) endpoint_policy: EndpointPolicy,
    pub(crate) scratch_policy: ScratchPolicy,
    pub(crate) timeouts: TimeoutPolicy,
    pub(crate) cache: CachePolicy,
    pub(crate) thinking_policy: String,
}

pub(crate) enum FormalCredential {
    ApiKey { env: String, value: String },
    GatewayCodexDefault,
    None,
}

pub(crate) struct FormalGatewayPlan {
    pub(crate) adapter: String,
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) credential: FormalCredential,
    pub(crate) model_policy: ModelPolicy,
    pub(crate) transport: Transport,
    pub(crate) endpoint_policy: EndpointPolicy,
    pub(crate) timeouts: TimeoutPolicy,
    pub(crate) cache: CachePolicy,
    pub(crate) thinking_policy: String,
}

pub(crate) enum ScratchCredential {
    ApiKey { env: String, value: String },
    GatewayOwnedAuth,
    None,
}

pub(crate) struct ScratchPlan {
    pub(crate) provider: String,
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) credential: ScratchCredential,
    pub(crate) policy: ScratchPolicy,
    pub(crate) endpoint_policy: EndpointPolicy,
    pub(crate) thinking_policy: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PublicPlanView {
    pub(crate) adapter: String,
    pub(crate) auth_mode: AuthMode,
    pub(crate) credential_source: CredentialSource,
    pub(crate) credential_configured: bool,
    pub(crate) model_policy: ModelPolicy,
    pub(crate) model_discovery: ModelDiscovery,
    pub(crate) transport: Transport,
    pub(crate) endpoint_policy: EndpointPolicy,
    pub(crate) scratch_policy: ScratchPolicy,
    pub(crate) thinking_policy: String,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) total_timeout_ms: u64,
    pub(crate) read_idle_timeout_ms: u64,
    pub(crate) cache_normal_ttl_seconds: u64,
    pub(crate) cache_stale_ttl_seconds: u64,
}

impl ResolvedLaunchPlan {
    pub(crate) fn formal(&self) -> FormalGatewayPlan {
        let credential = match &self.credential {
            CredentialHandle::ApiKey { env, value } => FormalCredential::ApiKey {
                env: env.clone(),
                value: value.clone(),
            },
            CredentialHandle::CodexDefault => FormalCredential::GatewayCodexDefault,
            CredentialHandle::None => FormalCredential::None,
        };
        FormalGatewayPlan {
            adapter: self.adapter.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            credential,
            model_policy: self.model_policy,
            transport: self.transport,
            endpoint_policy: self.endpoint_policy,
            timeouts: self.timeouts.clone(),
            cache: self.cache.clone(),
            thinking_policy: self.thinking_policy.clone(),
        }
    }

    pub(crate) fn scratch(&self) -> ScratchPlan {
        let credential = match &self.credential {
            CredentialHandle::ApiKey { env, value } => ScratchCredential::ApiKey {
                env: env.clone(),
                value: value.clone(),
            },
            CredentialHandle::CodexDefault => ScratchCredential::GatewayOwnedAuth,
            CredentialHandle::None => ScratchCredential::None,
        };
        ScratchPlan {
            provider: self.adapter.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            credential,
            policy: self.scratch_policy,
            endpoint_policy: self.endpoint_policy,
            thinking_policy: self.thinking_policy.clone(),
        }
    }

    pub(crate) fn public(&self) -> PublicPlanView {
        let (auth_mode, credential_source, credential_configured) = match &self.credential {
            CredentialHandle::ApiKey { value, .. } => (
                AuthMode::ApiKey,
                CredentialSource::ApiKey,
                !value.is_empty(),
            ),
            CredentialHandle::CodexDefault => (
                AuthMode::KeychainOauth,
                CredentialSource::KeychainOauth,
                true,
            ),
            CredentialHandle::None => (AuthMode::None, CredentialSource::None, true),
        };
        PublicPlanView {
            adapter: self.adapter.clone(),
            auth_mode,
            credential_source,
            credential_configured,
            model_policy: self.model_policy,
            model_discovery: self.model_discovery,
            transport: self.transport,
            endpoint_policy: self.endpoint_policy,
            scratch_policy: self.scratch_policy,
            thinking_policy: self.thinking_policy.clone(),
            connect_timeout_ms: self.timeouts.connect_ms,
            total_timeout_ms: self.timeouts.total_ms,
            read_idle_timeout_ms: self.timeouts.read_idle_ms,
            cache_normal_ttl_seconds: self.cache.normal_ttl_seconds,
            cache_stale_ttl_seconds: self.cache.stale_ttl_seconds,
        }
    }
}

impl FormalGatewayPlan {
    pub(crate) fn credential_configured(&self) -> bool {
        match &self.credential {
            FormalCredential::ApiKey { value, .. } => !value.is_empty(),
            FormalCredential::GatewayCodexDefault | FormalCredential::None => true,
        }
    }

    fn credential_fingerprint_material(&self) -> &str {
        match &self.credential {
            FormalCredential::ApiKey { value, .. } => value,
            FormalCredential::GatewayCodexDefault => "csswitch:codex:default",
            FormalCredential::None => "none",
        }
    }
}

impl ScratchPlan {
    pub(crate) fn credential_parts(&self) -> (&str, &str) {
        match &self.credential {
            ScratchCredential::ApiKey { env, value } => (env, value),
            ScratchCredential::GatewayOwnedAuth | ScratchCredential::None => ("", ""),
        }
    }

    pub(crate) fn should_probe(&self) -> bool {
        if self.policy == ScratchPolicy::Disabled {
            return false;
        }
        if self.endpoint_policy == EndpointPolicy::ProfileRequired
            && self.endpoint.trim().is_empty()
        {
            return false;
        }
        match &self.credential {
            ScratchCredential::ApiKey { value, .. } => !value.is_empty(),
            ScratchCredential::GatewayOwnedAuth | ScratchCredential::None => true,
        }
    }
}

pub(crate) fn resolve_launch_plan(p: &config::Profile) -> Result<ResolvedLaunchPlan, String> {
    let contract = provider_contracts::contract_for(&p.template_id, &p.api_format)?;
    if !contract.credential_sources.contains(&p.credential_source) {
        return Err(format!(
            "profile `{}` 的 credential_source 不符合 provider contract",
            p.id
        ));
    }
    if !contract.model_policies.contains(&p.model_policy) {
        return Err(format!(
            "profile `{}` 的 model_policy 不符合 provider contract",
            p.id
        ));
    }
    let credential = match p.credential_source {
        CredentialSource::ApiKey => {
            if p.credential_ref.is_some() {
                return Err("API-key profile 不得带 credential_ref".into());
            }
            CredentialHandle::ApiKey {
                env: contract
                    .api_key_env
                    .clone()
                    .ok_or("provider contract 缺少 API-key env")?,
                value: p.api_key.clone(),
            }
        }
        CredentialSource::KeychainOauth => {
            if p.credential_ref.as_deref() != Some("csswitch:codex:default")
                || !p.api_key.is_empty()
            {
                return Err("Codex OAuth profile 的 credential_ref 或 api_key 非法".into());
            }
            CredentialHandle::CodexDefault
        }
        CredentialSource::None => {
            if p.credential_ref.is_some() || !p.api_key.is_empty() {
                return Err("无凭据 profile 不得带 credential 数据".into());
            }
            CredentialHandle::None
        }
    };
    let endpoint = match contract.endpoint_policy {
        EndpointPolicy::ProfileRequired => p.base_url.clone(),
        EndpointPolicy::GatewayManagedOfficial => String::new(),
    };
    Ok(ResolvedLaunchPlan {
        adapter: contract.adapter,
        endpoint,
        model: p.model.clone(),
        credential,
        model_policy: p.model_policy,
        model_discovery: contract.model_discovery,
        transport: contract.transport,
        endpoint_policy: contract.endpoint_policy,
        scratch_policy: contract.scratch_policy,
        timeouts: contract.timeouts,
        cache: contract.cache,
        thinking_policy: contract.thinking_policy,
    })
}

/// UI 模板预览也走与正式 profile 相同的 resolver。OAuth 模板只构造固定 opaque ref，
/// 不读 Keychain；PublicPlanView 不会序列化这个 ref。
pub(crate) fn resolve_template_plan(
    template_id: &str,
    api_format: &str,
) -> Result<ResolvedLaunchPlan, String> {
    let contract = provider_contracts::contract_for(template_id, api_format)?;
    let profile = config::Profile {
        template_id: template_id.to_string(),
        api_format: api_format.to_string(),
        base_url: crate::templates::by_id(template_id)
            .map(|template| template.base_url.to_string())
            .unwrap_or_default(),
        credential_source: contract.default_credential_source,
        credential_ref: (contract.default_credential_source == CredentialSource::KeychainOauth)
            .then(|| "csswitch:codex:default".to_string()),
        model_policy: contract.default_model_policy,
        ..Default::default()
    };
    resolve_launch_plan(&profile)
}

pub(crate) fn adapter_for_profile(p: &config::Profile) -> String {
    provider_contracts::contract_for(&p.template_id, &p.api_format)
        .map(|contract| contract.adapter)
        .unwrap_or_else(|_| "unsupported".to_string())
}

pub(crate) fn proxy_args_for(p: &config::Profile) -> Result<ResolvedLaunchPlan, String> {
    resolve_launch_plan(p)
}

#[cfg(test)]
pub(crate) fn proxy_fingerprint(p: &config::Profile, launch: &FormalGatewayPlan) -> u64 {
    proxy_fingerprint_with_runtime(
        p,
        launch,
        gateway_kind_for_adapter(&launch.adapter),
        current_shim_mode_for_adapter(&launch.adapter),
    )
}

pub(crate) fn proxy_fingerprint_with_runtime(
    p: &config::Profile,
    launch: &FormalGatewayPlan,
    gateway_kind: &str,
    shim_mode: &str,
) -> u64 {
    let shim_mode = normalize_shim_mode(&launch.adapter, Some(shim_mode));
    key_fingerprint(&format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{:?}\n{:?}\n{}\n{}\n{}\n{}\n{}",
        p.template_id,
        p.api_format,
        launch.adapter,
        launch.endpoint,
        launch.model,
        launch.thinking_policy,
        launch.credential_fingerprint_material(),
        gateway_kind,
        shim_mode,
        launch.transport,
        launch.endpoint_policy,
        launch.timeouts.connect_ms,
        launch.timeouts.total_ms,
        launch.timeouts.read_idle_ms,
        launch.cache.normal_ttl_seconds,
        launch.cache.stale_ttl_seconds
    ))
}

/// 当前支持 anthropic / openai_chat / openai_responses；其它 schema 值激活时失败关闭。
pub(crate) fn assert_format_supported(p: &config::Profile) -> Result<(), String> {
    provider_contracts::contract_for(&p.template_id, &p.api_format)
        .map(|_| ())
        .map_err(|_| {
            format!(
                "api_format `{}` 与模板 `{}` 的组合暂不支持。",
                p.api_format, p.template_id
            )
        })
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

/// deepseek/qwen 走各自固定官方端点；其余 = relay 家族，需带 base_url。
pub(crate) fn is_native_adapter(adapter: &str) -> bool {
    adapter == "deepseek" || adapter == "qwen"
}

pub(crate) fn is_openai_adapter(adapter: &str) -> bool {
    matches!(adapter, "openai-custom" | "openai-responses")
}

pub(crate) fn gateway_kind_for_adapter(_adapter: &str) -> &'static str {
    "rust"
}

pub(crate) fn normalize_shim_mode(adapter: &str, raw: Option<&str>) -> &'static str {
    if adapter != "deepseek" {
        return "off";
    }
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "detect" => "detect",
        "rewrite" => "rewrite",
        _ => "off",
    }
}

pub(crate) fn managed_shim_mode(adapter: &str, raw: Option<&str>) -> &'static str {
    if adapter == "deepseek" && raw.is_none() {
        return "rewrite";
    }
    normalize_shim_mode(adapter, raw)
}

pub(crate) fn current_shim_mode_for_adapter(adapter: &str) -> &'static str {
    managed_shim_mode(
        adapter,
        std::env::var("CSSWITCH_TOOLUSE_SHIM").ok().as_deref(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpstreamEndpoint {
    pub(crate) host: String,
    pub(crate) port: u16,
}

/// 上游 authority（host + port），供 status 灯按真实 scheme/端口探测。
pub(crate) fn upstream_endpoint(adapter: &str, base_url: &str) -> Option<UpstreamEndpoint> {
    match adapter {
        "deepseek" => Some(UpstreamEndpoint {
            host: "api.deepseek.com".to_string(),
            port: 443,
        }),
        "qwen" => Some(UpstreamEndpoint {
            host: "dashscope.aliyuncs.com".to_string(),
            port: 443,
        }),
        "codex" => None,
        _ => parse_endpoint(base_url),
    }
}

/// Status accepts the explicit diagnostic override for every adapter, but only
/// when it names loopback. This is deliberately status-only for relay/custom:
/// managed gateway commands remove `CSSWITCH_UPSTREAM_URL` for those adapters,
/// so a stale diagnostic value cannot replace the profile endpoint or receive
/// its key. If the override is malformed or external, fail closed instead of
/// silently probing the real provider host during an isolated/local-mock run.
pub(crate) fn status_upstream_endpoint(
    adapter: &str,
    base_url: &str,
    diagnostic_override: Option<&std::ffi::OsStr>,
) -> Option<UpstreamEndpoint> {
    if adapter.is_empty() || adapter == "codex" {
        return None;
    }
    if let Some(raw_os) = diagnostic_override {
        // `var_os` at the call site preserves the distinction between absent and
        // explicitly non-UTF-8. An invalid explicit value must fail closed here,
        // never collapse into the production endpoint fallback.
        let raw = raw_os.to_str()?;
        let endpoint = parse_endpoint(raw)?;
        let host = endpoint.host.trim_end_matches('.');
        let explicit_loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        return explicit_loopback.then_some(endpoint);
    }
    upstream_endpoint(adapter, base_url)
}

/// 从 `http(s)://host[:port]/path` 里抽出 host + port。解析不出返回 None（不引 url crate）。
pub(crate) fn parse_endpoint(url: &str) -> Option<UpstreamEndpoint> {
    let (rest, default_port) = url
        .strip_prefix("https://")
        .map(|r| (r, 443))
        .or_else(|| url.strip_prefix("http://").map(|r| (r, 80)))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(after_open) = authority.strip_prefix('[') {
        let (host, rest) = after_open.split_once(']')?;
        let port = match rest {
            "" => default_port,
            _ => match rest.strip_prefix(':') {
                Some(raw) if !raw.is_empty() => raw.parse().ok()?,
                _ => return None,
            },
        };
        (host.to_string(), port)
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, raw)) if !raw.is_empty() => (host, raw.parse().ok()?),
            Some(_) => return None,
            None => (authority, default_port),
        };
        (host.to_string(), port)
    };
    if host.is_empty() {
        None
    } else {
        Some(UpstreamEndpoint { host, port })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        adapter_for_profile, assert_format_supported, gateway_kind_for_adapter, key_fingerprint,
        managed_shim_mode, normalize_shim_mode, parse_endpoint, proxy_args_for, proxy_fingerprint,
        proxy_fingerprint_with_runtime, reject_openai_custom_anthropic_base, resolve_template_plan,
        status_upstream_endpoint, upstream_endpoint,
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
        let a = proxy_args_for(&ds).unwrap().formal();
        assert_eq!(a.adapter, "deepseek");
        assert!(
            matches!(a.credential, super::FormalCredential::ApiKey { ref env, .. } if env == "DEEPSEEK_API_KEY")
        );

        let glm = Profile {
            template_id: "glm".into(),
            api_format: "anthropic".into(),
            base_url: "https://open.bigmodel.cn/api/anthropic".into(),
            api_key: "gk".into(),
            model: "glm-5".into(),
            ..Default::default()
        };
        let b = proxy_args_for(&glm).unwrap().formal();
        assert_eq!(b.adapter, "relay");
        assert!(
            matches!(b.credential, super::FormalCredential::ApiKey { ref env, .. } if env == "CSSWITCH_RELAY_KEY")
        );
        assert_eq!(b.endpoint, "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(b.model, "glm-5");

        let custom_openai = Profile {
            template_id: "custom-openai".into(),
            api_format: "openai_chat".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            api_key: "ok".into(),
            model: "glm-4.5".into(),
            ..Default::default()
        };
        let c = proxy_args_for(&custom_openai).unwrap().formal();
        assert_eq!(c.adapter, "openai-custom");
        assert!(
            matches!(c.credential, super::FormalCredential::ApiKey { ref env, .. } if env == "CSSWITCH_OPENAI_KEY")
        );
        assert_eq!(c.endpoint, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(c.model, "glm-4.5");

        let custom_responses = Profile {
            template_id: "custom-openai-responses".into(),
            api_format: "openai_responses".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: "ok".into(),
            model: "gpt-5.2".into(),
            ..Default::default()
        };
        let d = proxy_args_for(&custom_responses).unwrap().formal();
        assert_eq!(d.adapter, "openai-responses");
        assert!(
            matches!(d.credential, super::FormalCredential::ApiKey { ref env, .. } if env == "CSSWITCH_OPENAI_KEY")
        );
        assert_eq!(d.endpoint, "https://api.openai.com/v1");
        assert_eq!(d.model, "gpt-5.2");

        let custom_profile_openai = Profile {
            template_id: "custom".into(),
            api_format: "openai_chat".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "ok".into(),
            model: "gpt-5.2".into(),
            ..Default::default()
        };
        let e = proxy_args_for(&custom_profile_openai).unwrap().formal();
        assert_eq!(adapter_for_profile(&custom_profile_openai), "openai-custom");
        assert_eq!(e.adapter, "openai-custom");
        assert!(
            matches!(e.credential, super::FormalCredential::ApiKey { ref env, .. } if env == "CSSWITCH_OPENAI_KEY")
        );

        let custom_profile_responses = Profile {
            api_format: "openai_responses".into(),
            ..custom_profile_openai
        };
        let f = proxy_args_for(&custom_profile_responses).unwrap().formal();
        assert_eq!(
            adapter_for_profile(&custom_profile_responses),
            "openai-responses"
        );
        assert_eq!(f.adapter, "openai-responses");
        assert!(
            matches!(f.credential, super::FormalCredential::ApiKey { ref env, .. } if env == "CSSWITCH_OPENAI_KEY")
        );

        let non_custom_openai_format = Profile {
            template_id: "glm".into(),
            api_format: "openai_chat".into(),
            base_url: "https://open.bigmodel.cn/api/anthropic".into(),
            api_key: "ok".into(),
            model: "glm-5".into(),
            ..Default::default()
        };
        assert_eq!(
            adapter_for_profile(&non_custom_openai_format),
            "unsupported"
        );
        assert!(proxy_args_for(&non_custom_openai_format).is_err());
    }

    #[test]
    fn codex_resolver_projects_opaque_credentials_by_trust_boundary() {
        let profile = Profile {
            id: "codex-1".into(),
            template_id: "codex".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::KeychainOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            base_url: "https://attacker.invalid/must-never-be-injected".into(),
            ..Default::default()
        };
        let resolved = proxy_args_for(&profile).unwrap();
        let formal = resolved.formal();
        assert_eq!(formal.adapter, "codex");
        assert!(formal.endpoint.is_empty());
        assert!(matches!(
            formal.credential,
            super::FormalCredential::GatewayCodexDefault
        ));
        assert_eq!(
            formal.transport,
            crate::provider_contracts::Transport::CodexResponsesSse
        );
        let scratch = resolved.scratch();
        assert!(scratch.endpoint.is_empty());
        assert!(matches!(
            scratch.credential,
            super::ScratchCredential::GatewayOwnedAuth
        ));
        assert_eq!(scratch.credential_parts(), ("", ""));
        assert!(scratch.should_probe());

        let public = serde_json::to_string(&resolved.public()).unwrap();
        assert!(public.contains("keychain_oauth"));
        assert!(public.contains("dynamic_catalog"));
        assert!(!public.contains("csswitch:codex:default"));
        assert!(!public.contains("credential_ref"));
        assert!(!public.contains("api_key"));
    }

    #[test]
    fn codex_template_plan_uses_opaque_default_without_keychain_read() {
        let public = serde_json::to_string(
            &resolve_template_plan("codex", "openai_responses")
                .unwrap()
                .public(),
        )
        .unwrap();
        assert!(public.contains("keychain_oauth"));
        assert!(public.contains("codex_account_catalog"));
        assert!(public.contains("codex_responses_sse"));
        assert!(!public.contains("csswitch:codex:default"));
        assert!(!public.contains("credential_ref"));
        assert!(!public.contains("api_key"));
    }

    #[test]
    fn api_key_public_projection_never_contains_key_material() {
        let profile = Profile {
            template_id: "glm".into(),
            api_format: "anthropic".into(),
            api_key: "sk-super-secret".into(),
            model: "glm-5.2".into(),
            ..Default::default()
        };
        let public = serde_json::to_value(proxy_args_for(&profile).unwrap().public()).unwrap();
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(!encoded.contains("super-secret"));
        assert!(public.get("key").is_none());
        assert!(public.get("api_key_value").is_none());
        assert_eq!(public["credential_configured"], true);
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
    fn proxy_fingerprint_includes_protocol_semantics() {
        let mut p = Profile {
            template_id: "kimi".into(),
            api_format: "anthropic".into(),
            base_url: "https://same.example/anthropic".into(),
            api_key: "same-key".into(),
            model: "same-model".into(),
            ..Default::default()
        };
        let kimi_launch = proxy_args_for(&p).unwrap().formal();
        let kimi_fp = proxy_fingerprint(&p, &kimi_launch);

        p.template_id = "custom".into();
        let custom_launch = proxy_args_for(&p).unwrap().formal();
        let custom_fp = proxy_fingerprint(&p, &custom_launch);
        assert_ne!(
            kimi_fp, custom_fp,
            "同 adapter/base/model/key 但模板语义不同，必须重启代理"
        );
    }

    #[test]
    fn proxy_fingerprint_includes_runtime_and_shim_identity() {
        let p = Profile {
            template_id: "deepseek".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "same-key".into(),
            model: "same-model".into(),
            ..Default::default()
        };
        let launch = proxy_args_for(&p).unwrap().formal();
        let other_runtime_off = proxy_fingerprint_with_runtime(&p, &launch, "other", "off");
        let rust_off = proxy_fingerprint_with_runtime(&p, &launch, "rust", "off");
        let other_runtime_detect = proxy_fingerprint_with_runtime(&p, &launch, "other", "detect");
        assert_ne!(
            other_runtime_off, rust_off,
            "runtime identity 变化必须阻止误复用"
        );
        assert_ne!(
            other_runtime_off, other_runtime_detect,
            "shim 切换必须阻止误复用"
        );

        let relay_profile = Profile {
            template_id: "glm".into(),
            api_format: "anthropic".into(),
            base_url: "https://relay.example/v1".into(),
            api_key: "same-key".into(),
            model: "same-model".into(),
            ..Default::default()
        };
        let relay_launch = proxy_args_for(&relay_profile).unwrap().formal();
        assert_eq!(
            proxy_fingerprint_with_runtime(&relay_profile, &relay_launch, "rust", "off"),
            proxy_fingerprint_with_runtime(&relay_profile, &relay_launch, "rust", " Rewrite "),
            "非 DSML provider 的污染 shim 必须先 canonicalize 为 off"
        );
    }

    #[test]
    fn parse_endpoint_preserves_scheme_default_and_explicit_ports() {
        assert_eq!(
            parse_endpoint("https://relay.example.com/api"),
            Some(super::UpstreamEndpoint {
                host: "relay.example.com".to_string(),
                port: 443,
            })
        );
        assert_eq!(
            parse_endpoint("http://127.0.0.1:11434/v1"),
            Some(super::UpstreamEndpoint {
                host: "127.0.0.1".to_string(),
                port: 11434,
            })
        );
        assert_eq!(
            parse_endpoint("http://localhost/v1"),
            Some(super::UpstreamEndpoint {
                host: "localhost".to_string(),
                port: 80,
            })
        );
        assert_eq!(parse_endpoint("https://relay.example.com:"), None);
        assert_eq!(parse_endpoint("http://[::1]garbage"), None);
        assert_eq!(parse_endpoint("http://[::1]@external.example"), None);
    }

    #[test]
    fn upstream_endpoint_by_adapter() {
        assert_eq!(
            upstream_endpoint("openai-custom", "http://127.0.0.1:11434/v1"),
            Some(super::UpstreamEndpoint {
                host: "127.0.0.1".to_string(),
                port: 11434,
            })
        );
        assert_eq!(upstream_endpoint("", ""), None);
        assert_eq!(
            upstream_endpoint("codex", "https://attacker.invalid/probe"),
            None
        );
        assert_eq!(
            status_upstream_endpoint(
                "codex",
                "https://attacker.invalid/probe",
                Some(std::ffi::OsStr::new("http://127.0.0.1:32128/mock")),
            ),
            None,
            "Codex status must use managed gateway health and never profile or override endpoints"
        );
    }

    #[test]
    fn status_diagnostic_override_accepts_only_explicit_loopback_and_never_falls_through() {
        assert_eq!(
            status_upstream_endpoint(
                "deepseek",
                "https://api.deepseek.com/anthropic",
                Some(std::ffi::OsStr::new("http://127.0.0.1:32123/mock")),
            ),
            Some(super::UpstreamEndpoint {
                host: "127.0.0.1".to_string(),
                port: 32123,
            })
        );
        assert_eq!(
            status_upstream_endpoint(
                "qwen",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                Some(std::ffi::OsStr::new("http://[::1]:32124/mock")),
            ),
            Some(super::UpstreamEndpoint {
                host: "::1".to_string(),
                port: 32124,
            })
        );
        assert_eq!(
            status_upstream_endpoint(
                "relay",
                "https://api.siliconflow.cn",
                Some(std::ffi::OsStr::new("https://provider.invalid/mock")),
            ),
            None,
            "an explicit external override must not fall through to the real provider host"
        );
        assert_eq!(
            status_upstream_endpoint(
                "openai-custom",
                "https://provider.example/v1",
                Some(std::ffi::OsStr::new("not-a-url")),
            ),
            None,
            "a malformed explicit override must fail closed"
        );
    }

    #[test]
    fn status_without_override_uses_normal_production_or_profile_endpoints() {
        assert_eq!(
            status_upstream_endpoint("deepseek", "https://ignored.example", None,),
            Some(super::UpstreamEndpoint {
                host: "api.deepseek.com".to_string(),
                port: 443,
            })
        );
        assert_eq!(
            status_upstream_endpoint("relay", "http://127.0.0.1:32125/anthropic", None,),
            Some(super::UpstreamEndpoint {
                host: "127.0.0.1".to_string(),
                port: 32125,
            }),
            "relay/custom status must follow the candidate profile base URL without an override"
        );
        assert_eq!(
            status_upstream_endpoint(
                "relay",
                "http://api.siliconflow.cn",
                Some(std::ffi::OsStr::new("http://127.0.0.1:32126/status-only",)),
            ),
            Some(super::UpstreamEndpoint {
                host: "127.0.0.1".to_string(),
                port: 32126,
            }),
            "an explicit loopback diagnostic must keep relay status from probing the provider host"
        );
        assert_eq!(
            status_upstream_endpoint(
                "",
                "https://provider.example",
                Some(std::ffi::OsStr::new("http://127.0.0.1:32127/status-only")),
            ),
            None,
            "no active adapter must never acquire an upstream light from a diagnostic override"
        );
    }

    #[cfg(unix)]
    #[test]
    fn status_non_utf8_diagnostic_override_fails_closed() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = std::ffi::OsString::from_vec(vec![0xff, 0xfe]);
        assert_eq!(
            status_upstream_endpoint(
                "relay",
                "https://provider.example",
                Some(invalid.as_os_str()),
            ),
            None,
            "an explicit non-UTF-8 override must not fall through to the profile endpoint"
        );
    }

    #[test]
    fn runtime_identity_contract_is_rust_only() {
        assert_eq!(gateway_kind_for_adapter("deepseek"), "rust");
        assert_eq!(gateway_kind_for_adapter("openai-custom"), "rust");
        assert_eq!(gateway_kind_for_adapter("relay"), "rust");
        assert_eq!(normalize_shim_mode("deepseek", Some(" detect ")), "detect");
        assert_eq!(normalize_shim_mode("deepseek", Some("DETECT")), "detect");
        assert_eq!(
            normalize_shim_mode("deepseek", Some(" Rewrite ")),
            "rewrite"
        );
        assert_eq!(normalize_shim_mode("deepseek", Some("bad")), "off");
        assert_eq!(normalize_shim_mode("deepseek", Some("")), "off");
        assert_eq!(normalize_shim_mode("relay", Some("rewrite")), "off");
        assert_eq!(normalize_shim_mode("qwen", Some("detect")), "off");
        assert_eq!(
            normalize_shim_mode("openai-custom", Some(" Rewrite ")),
            "off"
        );
        assert_eq!(
            normalize_shim_mode("openai-responses", Some("DETECT")),
            "off"
        );
        assert_eq!(normalize_shim_mode("unknown", Some("rewrite")), "off");
    }

    #[test]
    fn managed_deepseek_defaults_to_rewrite_without_changing_other_providers() {
        assert_eq!(managed_shim_mode("deepseek", None), "rewrite");
        assert_eq!(managed_shim_mode("deepseek", Some("off")), "off");
        assert_eq!(managed_shim_mode("deepseek", Some("detect")), "detect");
        assert_eq!(managed_shim_mode("deepseek", Some("")), "off");
        assert_eq!(managed_shim_mode("qwen", None), "off");
        assert_eq!(managed_shim_mode("relay", None), "off");
    }

    #[test]
    fn key_fingerprint_stable_and_distinct() {
        assert_eq!(key_fingerprint("sk-aaaa"), key_fingerprint("sk-aaaa"));
        assert_ne!(key_fingerprint("sk-aaaa"), key_fingerprint("sk-bbbb"));
        assert_ne!(key_fingerprint(""), key_fingerprint("x"));
    }
}
