use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const STATIC_PROVIDER_CONTRACTS_JSON: &str =
    include_str!("../../../catalog/provider-contracts.v1.json");

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeoutPolicy {
    connect_ms: u64,
    total_ms: u64,
    read_idle_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachePolicy {
    normal_ttl_seconds: u64,
    stale_ttl_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderContract {
    id: String,
    template_ids: Vec<String>,
    api_formats: Vec<String>,
    adapter: String,
    auth_mode: String,
    credential_sources: Vec<String>,
    default_credential_source: String,
    model_policies: Vec<String>,
    default_model_policy: String,
    model_discovery: String,
    transport: String,
    endpoint_policy: String,
    api_key_env: Option<String>,
    scratch_policy: String,
    thinking_policy: String,
    #[serde(default)]
    upstream_client_version: Option<String>,
    timeouts: TimeoutPolicy,
    cache: CachePolicy,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderContractCatalog {
    schema_version: u32,
    contracts: Vec<ProviderContract>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexRuntimeContract {
    pub contract_id: String,
    pub catalog_digest: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub read_idle_timeout: Duration,
    pub normal_ttl_seconds: u64,
    pub stale_ttl_seconds: u64,
    pub model_catalog_client_version: String,
}

fn catalog_digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(STATIC_PROVIDER_CONTRACTS_JSON.as_bytes())
    )
}

pub(crate) fn load_codex_runtime_contract() -> Result<CodexRuntimeContract, String> {
    let catalog: ProviderContractCatalog = serde_json::from_str(STATIC_PROVIDER_CONTRACTS_JSON)
        .map_err(|error| format!("provider contract catalog parse failed: {error}"))?;
    if catalog.schema_version != 1 || catalog.contracts.is_empty() {
        return Err("provider contract catalog schema is unsupported".into());
    }
    let mut ids = BTreeSet::new();
    for contract in &catalog.contracts {
        if contract.id.trim().is_empty() || !ids.insert(contract.id.as_str()) {
            return Err("provider contract catalog contains an invalid id".into());
        }
    }
    let mut codex = catalog.contracts.iter().filter(|contract| {
        contract.id == "codex-oauth"
            || contract.adapter == "codex"
            || contract.auth_mode == "csswitch_oauth"
            || contract.transport == "codex_responses_sse"
    });
    let contract = codex
        .next()
        .ok_or("Codex provider contract is unavailable")?;
    if codex.next().is_some()
        || contract.id != "codex-oauth"
        || contract.template_ids != ["codex"]
        || contract.api_formats != ["openai_responses"]
        || contract.adapter != "codex"
        || contract.auth_mode != "csswitch_oauth"
        || contract.credential_sources != ["csswitch_oauth"]
        || contract.default_credential_source != "csswitch_oauth"
        || contract.model_policies != ["dynamic_catalog"]
        || contract.default_model_policy != "dynamic_catalog"
        || contract.model_discovery != "codex_account_catalog"
        || contract.transport != "codex_responses_sse"
        || contract.endpoint_policy != "gateway_managed_official"
        || contract.api_key_env.is_some()
        || contract.scratch_policy != "gateway_owned_auth"
        || !contract.thinking_policy.is_empty()
        || contract.upstream_client_version.as_deref() != Some("0.144.4")
        || contract.timeouts.connect_ms == 0
        || contract.timeouts.total_ms < contract.timeouts.connect_ms
        || contract.timeouts.read_idle_ms == 0
        || contract.cache.stale_ttl_seconds < contract.cache.normal_ttl_seconds
    {
        return Err("Codex provider contract is invalid".into());
    }
    Ok(CodexRuntimeContract {
        contract_id: contract.id.clone(),
        catalog_digest: catalog_digest(),
        connect_timeout: Duration::from_millis(contract.timeouts.connect_ms),
        request_timeout: Duration::from_millis(contract.timeouts.total_ms),
        read_idle_timeout: Duration::from_millis(contract.timeouts.read_idle_ms),
        normal_ttl_seconds: contract.cache.normal_ttl_seconds,
        stale_ttl_seconds: contract.cache.stale_ttl_seconds,
        model_catalog_client_version: contract
            .upstream_client_version
            .clone()
            .expect("validated Codex client version"),
    })
}

pub(crate) fn validate_managed_identity(
    contract: &CodexRuntimeContract,
    expected_id: Option<&str>,
    expected_digest: Option<&str>,
) -> Result<(), String> {
    match (expected_id, expected_digest) {
        (None, None) => Ok(()),
        (Some(id), Some(digest))
            if id == contract.contract_id && digest == contract.catalog_digest =>
        {
            Ok(())
        }
        _ => Err("managed provider contract identity mismatch".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_codex_contract_drives_gateway_runtime_values() {
        let contract = load_codex_runtime_contract().unwrap();
        assert_eq!(contract.contract_id, "codex-oauth");
        assert_eq!(contract.catalog_digest.len(), 64);
        assert_eq!(contract.connect_timeout, Duration::from_secs(10));
        assert_eq!(contract.request_timeout, Duration::from_secs(30));
        assert_eq!(contract.read_idle_timeout, Duration::from_secs(300));
        assert_eq!(contract.normal_ttl_seconds, 300);
        assert_eq!(contract.stale_ttl_seconds, 86_400);
        assert_eq!(contract.model_catalog_client_version, "0.144.4");
    }

    #[test]
    fn managed_identity_is_optional_for_standalone_but_fail_closed_when_present() {
        let contract = load_codex_runtime_contract().unwrap();
        assert!(validate_managed_identity(&contract, None, None).is_ok());
        assert!(validate_managed_identity(
            &contract,
            Some(&contract.contract_id),
            Some(&contract.catalog_digest)
        )
        .is_ok());
        assert!(validate_managed_identity(&contract, Some("wrong"), None).is_err());
        assert!(
            validate_managed_identity(&contract, Some(&contract.contract_id), Some("wrong"))
                .is_err()
        );
    }
}
