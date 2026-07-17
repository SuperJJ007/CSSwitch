use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STATIC_PROVIDER_CONTRACTS_JSON: &str =
    include_str!("../../../catalog/provider-contracts.v1.json");

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialSource {
    #[default]
    ApiKey,
    #[serde(alias = "keychain_oauth")]
    CsswitchOauth,
    None,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelPolicy {
    #[default]
    RequiredFixed,
    OptionalFixed,
    DynamicCatalog,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    ApiKey,
    #[serde(alias = "keychain_oauth")]
    CsswitchOauth,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelDiscovery {
    BuiltinStatic,
    AnthropicModelsOrManual,
    OpenaiModelsOrManual,
    CodexAccountCatalog,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Transport {
    AnthropicMessages,
    OpenaiChat,
    OpenaiResponses,
    CodexResponsesSse,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndpointPolicy {
    GatewayManagedOfficial,
    ProfileRequired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScratchPolicy {
    UpstreamProbe,
    GatewayOwnedAuth,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TimeoutPolicy {
    pub(crate) connect_ms: u64,
    pub(crate) total_ms: u64,
    pub(crate) read_idle_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachePolicy {
    pub(crate) normal_ttl_seconds: u64,
    pub(crate) stale_ttl_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderContract {
    pub(crate) id: String,
    pub(crate) template_ids: Vec<String>,
    pub(crate) api_formats: Vec<String>,
    pub(crate) adapter: String,
    pub(crate) auth_mode: AuthMode,
    pub(crate) credential_sources: Vec<CredentialSource>,
    pub(crate) default_credential_source: CredentialSource,
    pub(crate) model_policies: Vec<ModelPolicy>,
    pub(crate) default_model_policy: ModelPolicy,
    pub(crate) model_discovery: ModelDiscovery,
    pub(crate) transport: Transport,
    pub(crate) endpoint_policy: EndpointPolicy,
    pub(crate) api_key_env: Option<String>,
    pub(crate) scratch_policy: ScratchPolicy,
    pub(crate) thinking_policy: String,
    #[serde(default)]
    pub(crate) upstream_client_version: Option<String>,
    pub(crate) timeouts: TimeoutPolicy,
    pub(crate) cache: CachePolicy,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderContractCatalog {
    pub(crate) schema_version: u32,
    pub(crate) contracts: Vec<ProviderContract>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GatewayContractIdentity {
    pub(crate) contract_id: String,
    pub(crate) catalog_digest: String,
}

pub(crate) fn static_catalog_digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(STATIC_PROVIDER_CONTRACTS_JSON.as_bytes())
    )
}

pub(crate) fn gateway_contract_identity(
    provider: &str,
) -> Result<Option<GatewayContractIdentity>, String> {
    if provider != "codex" {
        return Ok(None);
    }
    let catalog = load_provider_contracts()?;
    let contract = catalog
        .contracts
        .iter()
        .find(|contract| contract.adapter == provider)
        .ok_or("Codex provider contract 不存在")?;
    Ok(Some(GatewayContractIdentity {
        contract_id: contract.id.clone(),
        catalog_digest: static_catalog_digest(),
    }))
}

pub(crate) fn load_provider_contracts() -> Result<ProviderContractCatalog, String> {
    let catalog: ProviderContractCatalog = serde_json::from_str(STATIC_PROVIDER_CONTRACTS_JSON)
        .map_err(|error| format!("provider contract catalog JSON 解析失败：{error}"))?;
    validate_provider_contracts(&catalog)?;
    Ok(catalog)
}

pub(crate) fn validate_provider_contracts(catalog: &ProviderContractCatalog) -> Result<(), String> {
    if catalog.schema_version != 1 {
        return Err(format!(
            "不支持的 provider contract schema_version：{}",
            catalog.schema_version
        ));
    }
    let mut ids = BTreeSet::new();
    let mut match_keys = BTreeSet::new();
    let allowed_adapters = [
        "deepseek",
        "qwen",
        "relay",
        "openai-custom",
        "openai-responses",
        "codex",
    ];
    let allowed_formats = ["anthropic", "openai_chat", "openai_responses"];
    if catalog.contracts.is_empty() {
        return Err("provider contract catalog 不能为空".into());
    }
    for contract in &catalog.contracts {
        if contract.id.trim().is_empty() || !ids.insert(contract.id.clone()) {
            return Err(format!("provider contract id 为空或重复：{}", contract.id));
        }
        if contract.template_ids.is_empty()
            || contract.api_formats.is_empty()
            || contract.adapter.trim().is_empty()
        {
            return Err(format!(
                "provider contract 匹配或 adapter 为空：{}",
                contract.id
            ));
        }
        if contract
            .template_ids
            .iter()
            .any(|value| value.trim().is_empty())
            || contract
                .api_formats
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return Err(format!(
                "provider contract template_id/api_format 不得为空：{}",
                contract.id
            ));
        }
        if !allowed_adapters.contains(&contract.adapter.as_str())
            || contract
                .api_formats
                .iter()
                .any(|format| !allowed_formats.contains(&format.as_str()))
        {
            return Err(format!(
                "provider contract adapter/api_format 不在运行时允许列表：{}",
                contract.id
            ));
        }
        if !contract
            .credential_sources
            .contains(&contract.default_credential_source)
            || !contract
                .model_policies
                .contains(&contract.default_model_policy)
        {
            return Err(format!(
                "provider contract 默认策略不在允许集合：{}",
                contract.id
            ));
        }
        if contract
            .credential_sources
            .iter()
            .enumerate()
            .any(|(index, source)| contract.credential_sources[index + 1..].contains(source))
            || contract
                .model_policies
                .iter()
                .enumerate()
                .any(|(index, policy)| contract.model_policies[index + 1..].contains(policy))
        {
            return Err(format!(
                "provider contract credential/model policy 存在重复项：{}",
                contract.id
            ));
        }
        match contract.auth_mode {
            AuthMode::ApiKey if contract.api_key_env.as_deref().unwrap_or("").is_empty() => {
                return Err(format!(
                    "API-key contract 缺少 api_key_env：{}",
                    contract.id
                ));
            }
            AuthMode::CsswitchOauth | AuthMode::None if contract.api_key_env.is_some() => {
                return Err(format!(
                    "非 API-key contract 不得声明 api_key_env：{}",
                    contract.id
                ));
            }
            _ => {}
        }
        let expected_source = match contract.auth_mode {
            AuthMode::ApiKey => CredentialSource::ApiKey,
            AuthMode::CsswitchOauth => CredentialSource::CsswitchOauth,
            AuthMode::None => CredentialSource::None,
        };
        if contract.default_credential_source != expected_source
            || contract
                .credential_sources
                .iter()
                .any(|source| *source != expected_source)
        {
            return Err(format!(
                "provider contract auth_mode 与 credential_sources 不一致：{}",
                contract.id
            ));
        }
        if contract.timeouts.connect_ms == 0
            || contract.timeouts.total_ms < contract.timeouts.connect_ms
            || contract.timeouts.read_idle_ms == 0
        {
            return Err(format!("provider contract 超时策略非法：{}", contract.id));
        }
        if contract.cache.stale_ttl_seconds < contract.cache.normal_ttl_seconds {
            return Err(format!(
                "provider contract stale cache TTL 小于 normal TTL：{}",
                contract.id
            ));
        }
        if !matches!(
            contract.thinking_policy.as_str(),
            "" | "adaptive" | "enabled"
        ) {
            return Err(format!(
                "provider contract thinking_policy 非法：{}",
                contract.id
            ));
        }
        let has_codex_characteristic = contract.adapter == "codex"
            || contract.auth_mode == AuthMode::CsswitchOauth
            || contract
                .credential_sources
                .contains(&CredentialSource::CsswitchOauth)
            || contract.default_credential_source == CredentialSource::CsswitchOauth
            || contract.model_discovery == ModelDiscovery::CodexAccountCatalog
            || contract.transport == Transport::CodexResponsesSse
            || contract.scratch_policy == ScratchPolicy::GatewayOwnedAuth
            || contract
                .model_policies
                .contains(&ModelPolicy::DynamicCatalog)
            || contract.default_model_policy == ModelPolicy::DynamicCatalog;
        if has_codex_characteristic {
            let exact_codex_contract = contract.template_ids == ["codex"]
                && contract.api_formats == ["openai_responses"]
                && contract.adapter == "codex"
                && contract.auth_mode == AuthMode::CsswitchOauth
                && contract.credential_sources == [CredentialSource::CsswitchOauth]
                && contract.default_credential_source == CredentialSource::CsswitchOauth
                && contract.model_policies == [ModelPolicy::DynamicCatalog]
                && contract.default_model_policy == ModelPolicy::DynamicCatalog
                && contract.model_discovery == ModelDiscovery::CodexAccountCatalog
                && contract.transport == Transport::CodexResponsesSse
                && contract.endpoint_policy == EndpointPolicy::GatewayManagedOfficial
                && contract.api_key_env.is_none()
                && contract.scratch_policy == ScratchPolicy::GatewayOwnedAuth
                && contract.thinking_policy.is_empty()
                && contract.upstream_client_version.as_deref() == Some("0.144.4")
                && contract.cache.normal_ttl_seconds == 300
                && contract.cache.stale_ttl_seconds == 86_400;
            if !exact_codex_contract {
                return Err(format!(
                    "Codex contract 必须使用完整且唯一的 OAuth/transport/cache 组合：{}",
                    contract.id
                ));
            }
        } else {
            let expected_transport = match contract.adapter.as_str() {
                "deepseek" | "relay" => Transport::AnthropicMessages,
                "qwen" | "openai-custom" => Transport::OpenaiChat,
                "openai-responses" => Transport::OpenaiResponses,
                _ => {
                    return Err(format!("非 Codex contract adapter 非法：{}", contract.id));
                }
            };
            let expected_endpoint = match contract.adapter.as_str() {
                "deepseek" | "qwen" => EndpointPolicy::GatewayManagedOfficial,
                _ => EndpointPolicy::ProfileRequired,
            };
            if contract.auth_mode != AuthMode::ApiKey
                || contract.scratch_policy != ScratchPolicy::UpstreamProbe
                || contract.transport != expected_transport
                || contract.endpoint_policy != expected_endpoint
                || contract.cache.normal_ttl_seconds != 0
                || contract.cache.stale_ttl_seconds != 0
                || contract.upstream_client_version.is_some()
            {
                return Err(format!(
                    "API provider contract 的 auth/transport/endpoint/cache 组合非法：{}",
                    contract.id
                ));
            }

            let expected = match contract.id.as_str() {
                "deepseek-native" => (
                    &["deepseek"][..],
                    &["anthropic"][..],
                    "deepseek",
                    "DEEPSEEK_API_KEY",
                    &[ModelPolicy::OptionalFixed, ModelPolicy::RequiredFixed][..],
                    ModelPolicy::OptionalFixed,
                    ModelDiscovery::BuiltinStatic,
                    Transport::AnthropicMessages,
                    EndpointPolicy::GatewayManagedOfficial,
                    "",
                ),
                "qwen-native" => (
                    &["qwen"][..],
                    &["openai_chat"][..],
                    "qwen",
                    "DASHSCOPE_API_KEY",
                    &[ModelPolicy::OptionalFixed, ModelPolicy::RequiredFixed][..],
                    ModelPolicy::OptionalFixed,
                    ModelDiscovery::BuiltinStatic,
                    Transport::OpenaiChat,
                    EndpointPolicy::GatewayManagedOfficial,
                    "",
                ),
                "anthropic-relay" => (
                    &["glm", "xiaomi", "siliconflow", "minimax", "openrouter"][..],
                    &["anthropic"][..],
                    "relay",
                    "CSSWITCH_RELAY_KEY",
                    &[ModelPolicy::RequiredFixed][..],
                    ModelPolicy::RequiredFixed,
                    ModelDiscovery::AnthropicModelsOrManual,
                    Transport::AnthropicMessages,
                    EndpointPolicy::ProfileRequired,
                    "adaptive",
                ),
                "kimi-anthropic-relay" => (
                    &["kimi"][..],
                    &["anthropic"][..],
                    "relay",
                    "CSSWITCH_RELAY_KEY",
                    &[ModelPolicy::RequiredFixed][..],
                    ModelPolicy::RequiredFixed,
                    ModelDiscovery::AnthropicModelsOrManual,
                    Transport::AnthropicMessages,
                    EndpointPolicy::ProfileRequired,
                    "enabled",
                ),
                "custom-anthropic" => (
                    &["custom"][..],
                    &["anthropic"][..],
                    "relay",
                    "CSSWITCH_RELAY_KEY",
                    &[ModelPolicy::RequiredFixed][..],
                    ModelPolicy::RequiredFixed,
                    ModelDiscovery::AnthropicModelsOrManual,
                    Transport::AnthropicMessages,
                    EndpointPolicy::ProfileRequired,
                    "adaptive",
                ),
                "custom-openai-chat" => (
                    &["custom-openai", "custom"][..],
                    &["openai_chat"][..],
                    "openai-custom",
                    "CSSWITCH_OPENAI_KEY",
                    &[ModelPolicy::RequiredFixed][..],
                    ModelPolicy::RequiredFixed,
                    ModelDiscovery::OpenaiModelsOrManual,
                    Transport::OpenaiChat,
                    EndpointPolicy::ProfileRequired,
                    "",
                ),
                "custom-openai-responses" => (
                    &["custom-openai-responses", "custom"][..],
                    &["openai_responses"][..],
                    "openai-responses",
                    "CSSWITCH_OPENAI_KEY",
                    &[ModelPolicy::RequiredFixed][..],
                    ModelPolicy::RequiredFixed,
                    ModelDiscovery::OpenaiModelsOrManual,
                    Transport::OpenaiResponses,
                    EndpointPolicy::ProfileRequired,
                    "",
                ),
                _ => {
                    return Err(format!("未知 API provider contract id：{}", contract.id));
                }
            };
            let strings_equal = |actual: &[String], expected: &[&str]| {
                actual.len() == expected.len()
                    && actual
                        .iter()
                        .zip(expected.iter())
                        .all(|(actual, expected)| actual == expected)
            };
            if !strings_equal(&contract.template_ids, expected.0)
                || !strings_equal(&contract.api_formats, expected.1)
                || contract.adapter != expected.2
                || contract.api_key_env.as_deref() != Some(expected.3)
                || contract.model_policies != expected.4
                || contract.default_model_policy != expected.5
                || contract.model_discovery != expected.6
                || contract.transport != expected.7
                || contract.endpoint_policy != expected.8
                || contract.thinking_policy != expected.9
            {
                return Err(format!(
                    "API provider contract 偏离已验证运行时矩阵：{}",
                    contract.id
                ));
            }
        }
        for template_id in &contract.template_ids {
            for api_format in &contract.api_formats {
                if !match_keys.insert((template_id.clone(), api_format.clone())) {
                    return Err(format!(
                        "provider contract 匹配重复：template_id={template_id} api_format={api_format}"
                    ));
                }
            }
        }
    }
    let expected_ids: BTreeSet<String> = [
        "deepseek-native",
        "qwen-native",
        "anthropic-relay",
        "kimi-anthropic-relay",
        "custom-anthropic",
        "custom-openai-chat",
        "custom-openai-responses",
        "codex-oauth",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if ids != expected_ids {
        return Err("provider contract catalog 缺少必需 contract 或含未知 contract".into());
    }
    Ok(())
}

pub(crate) fn contract_for(
    template_id: &str,
    api_format: &str,
) -> Result<ProviderContract, String> {
    let catalog = load_provider_contracts()?;
    let exact = catalog.contracts.into_iter().find(|contract| {
        contract.template_ids.iter().any(|id| id == template_id)
            && contract.api_formats.iter().any(|fmt| fmt == api_format)
    });
    exact.ok_or_else(|| {
        format!("没有匹配的 provider contract：template_id={template_id} api_format={api_format}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_catalog_value() -> serde_json::Value {
        serde_json::from_str(STATIC_PROVIDER_CONTRACTS_JSON).unwrap()
    }

    fn validate_value(value: serde_json::Value) -> Result<(), String> {
        let catalog: ProviderContractCatalog =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        validate_provider_contracts(&catalog)
    }

    #[test]
    fn static_provider_contracts_load_and_are_unambiguous() {
        let catalog = load_provider_contracts().unwrap();
        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.contracts.len(), 8);
        assert_eq!(static_catalog_digest().len(), 64);
        assert_eq!(
            gateway_contract_identity("codex").unwrap(),
            Some(GatewayContractIdentity {
                contract_id: "codex-oauth".into(),
                catalog_digest: static_catalog_digest(),
            })
        );
        assert_eq!(gateway_contract_identity("relay").unwrap(), None);
        assert_eq!(
            contract_for("codex", "openai_responses")
                .unwrap()
                .default_credential_source,
            CredentialSource::CsswitchOauth
        );
    }

    #[test]
    fn custom_api_format_selects_distinct_runtime_contract() {
        assert_eq!(
            contract_for("custom", "openai_chat").unwrap().adapter,
            "openai-custom"
        );
        assert_eq!(
            contract_for("custom", "openai_responses").unwrap().adapter,
            "openai-responses"
        );
    }

    #[test]
    fn catalog_rejects_unknown_fields_and_empty_contracts() {
        let mut unknown = parsed_catalog_value();
        unknown["contracts"][0]["surprise"] = serde_json::json!(true);
        assert!(validate_value(unknown).is_err());

        let mut empty = parsed_catalog_value();
        empty["contracts"] = serde_json::json!([]);
        assert!(validate_value(empty).is_err());
    }

    #[test]
    fn api_contract_runtime_matrix_rejects_behavior_mutations() {
        for (field, value) in [
            ("api_key_env", serde_json::json!("TYPO_API_KEY")),
            ("model_discovery", serde_json::json!("none")),
            ("api_formats", serde_json::json!(["openai_responses"])),
            ("thinking_policy", serde_json::json!("enabled")),
        ] {
            let mut mutated = parsed_catalog_value();
            mutated["contracts"][0][field] = value;
            assert!(
                validate_value(mutated).is_err(),
                "deepseek mutation {field} must fail closed"
            );
        }
    }

    #[test]
    fn codex_characteristics_are_bidirectionally_bound() {
        for (field, value) in [
            ("adapter", serde_json::json!("relay")),
            ("scratch_policy", serde_json::json!("upstream_probe")),
            ("model_policies", serde_json::json!(["required_fixed"])),
            (
                "cache",
                serde_json::json!({"normal_ttl_seconds": 400, "stale_ttl_seconds": 300}),
            ),
            ("upstream_client_version", serde_json::json!("0.0.0")),
        ] {
            let mut mutated = parsed_catalog_value();
            mutated["contracts"][7][field] = value;
            assert!(
                validate_value(mutated).is_err(),
                "Codex mutation {field} must fail closed"
            );
        }
    }

    #[test]
    fn duplicate_policy_entries_are_rejected() {
        let mut mutated = parsed_catalog_value();
        mutated["contracts"][0]["credential_sources"] = serde_json::json!(["api_key", "api_key"]);
        assert!(validate_value(mutated).is_err());

        let mut mutated = parsed_catalog_value();
        mutated["contracts"][0]["model_policies"] =
            serde_json::json!(["optional_fixed", "optional_fixed"]);
        assert!(validate_value(mutated).is_err());
    }

    #[test]
    fn legacy_keychain_oauth_value_reads_but_serializes_as_csswitch_oauth() {
        let source: CredentialSource = serde_json::from_str("\"keychain_oauth\"").unwrap();
        let mode: AuthMode = serde_json::from_str("\"keychain_oauth\"").unwrap();
        assert_eq!(source, CredentialSource::CsswitchOauth);
        assert_eq!(mode, AuthMode::CsswitchOauth);
        assert_eq!(
            serde_json::to_string(&source).unwrap(),
            "\"csswitch_oauth\""
        );
        assert_eq!(serde_json::to_string(&mode).unwrap(), "\"csswitch_oauth\"");
    }
}
