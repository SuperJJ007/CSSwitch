//! Gateway model catalog verification for one-click / reopen paths.
use serde_json::Value;

#[cfg(test)]
use super::authority_snapshot::SANDBOX_SESSION_TEST_SEAMS;
use crate::config;
use crate::proc;
use crate::runtime::operation::{self, OperationStage, OperationTrace};

pub(super) const SCIENCE_CANONICAL_ROLE_MODEL_IDS: [&str; 5] = [
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-sonnet-4-6",
    "claude-haiku-4-5-20251001",
];

const SCIENCE_CODEX_MODEL_PREFIX: &str = "claude-csswitch-codex-";
const SCIENCE_CODEX_REVIEWER_MODEL_IDS: [&str; 3] = [
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
];

fn is_science_codex_model_alias(id: &str) -> bool {
    let Some(raw_id) = id.strip_prefix(SCIENCE_CODEX_MODEL_PREFIX) else {
        return false;
    };
    !raw_id.is_empty()
        && id.len() <= 256
        && raw_id.is_ascii()
        && raw_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn has_valid_codex_reviewer_slots(ids: &[&str]) -> bool {
    let published: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| SCIENCE_CODEX_REVIEWER_MODEL_IDS.contains(id))
        .collect();
    SCIENCE_CODEX_REVIEWER_MODEL_IDS.starts_with(&published)
}

pub(super) fn verify_gateway_model_catalog(
    port: u16,
    secret: &str,
    profile: &config::Profile,
) -> Result<(), String> {
    let timeout_ms = gateway_model_catalog_timeout_ms(profile);
    let (status, body) =
        proc::http_get_body_cancellable(port, Some(secret), "/v1/models", timeout_ms, None)
            .ok_or("gateway 模型目录探活无响应")?;
    if status != 200 {
        return Err(format!("gateway 模型目录探活返回 {status}"));
    }
    let value: Value = serde_json::from_str(&body).map_err(|_| "gateway 模型目录不是合法 JSON")?;
    let models = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("gateway 模型目录缺少 data array")?;
    let mut ids = Vec::with_capacity(models.len());
    let mut unique = std::collections::BTreeSet::new();
    for model in models {
        let id = model
            .as_object()
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("gateway 模型目录包含 malformed id row")?;
        if !unique.insert(id) {
            return Err("gateway 模型目录包含 duplicate id".into());
        }
        ids.push(id);
    }
    if profile.model_policy == crate::provider_contracts::ModelPolicy::DynamicCatalog {
        // Codex assigns its first three standard Responses models to the three
        // Reviewer selector ids understood by Science 0.1.41. An account with
        // one to three visible standard models can therefore publish no
        // `claude-csswitch-codex-*` row at all. The old verifier required both
        // a dynamic alias and all five historical Claude roles, so it rejected
        // every such valid snapshot after Reviewer model selection was added.
        if ids.is_empty()
            || ids.iter().any(|id| {
                !is_science_codex_model_alias(id) && !SCIENCE_CODEX_REVIEWER_MODEL_IDS.contains(id)
            })
            || !has_valid_codex_reviewer_slots(&ids)
        {
            return Err("Codex published model snapshot 为空或包含非法 alias".into());
        }
        return Ok(());
    }
    let mut expected: std::collections::BTreeSet<&str> = profile
        .model_catalog
        .iter()
        .map(|route| route.selector_id.as_str())
        .collect();
    expected.extend(SCIENCE_CANONICAL_ROLE_MODEL_IDS);
    let actual: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
    if actual != expected || ids.first().copied() != Some(profile.default_model_route_id.as_str()) {
        return Err("gateway 模型目录与已提交白名单/default selector 不一致".into());
    }
    Ok(())
}

pub(super) fn gateway_model_catalog_timeout_ms(profile: &config::Profile) -> u64 {
    if profile.model_policy == crate::provider_contracts::ModelPolicy::DynamicCatalog {
        operation::CODEX_MODELS_PROBE_TIMEOUT_MS
    } else {
        operation::LOCAL_HEALTH_TIMEOUT_MS
    }
}

pub(super) fn verify_gateway_model_catalog_traced(
    trace: &OperationTrace,
    port: u16,
    secret: &str,
    profile: &config::Profile,
) -> Result<(), String> {
    trace.stage(
        OperationStage::CatalogVerify,
        format!(
            "start policy={:?} timeout_ms={}",
            profile.model_policy,
            gateway_model_catalog_timeout_ms(profile)
        ),
    );
    #[cfg(test)]
    if SANDBOX_SESSION_TEST_SEAMS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .catalog_failure_port
        == Some(port)
    {
        trace.stage(OperationStage::CatalogVerify, "outcome=test_error");
        trace.finish("error=test_catalog_verify_after_gateway_restart");
        return Err("test-only healthy reopen catalog failure after Gateway restart".into());
    }
    #[cfg(test)]
    if SANDBOX_SESSION_TEST_SEAMS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .catalog_bypass_port
        == Some(port)
    {
        trace.stage(OperationStage::CatalogVerify, "outcome=test_bypass");
        return Ok(());
    }
    match verify_gateway_model_catalog(port, secret, profile) {
        Ok(()) => {
            trace.stage(OperationStage::CatalogVerify, "outcome=ok");
            Ok(())
        }
        Err(error) => {
            trace.stage(OperationStage::CatalogVerify, "outcome=error");
            trace.finish("error=catalog_verify");
            Err(error)
        }
    }
}
