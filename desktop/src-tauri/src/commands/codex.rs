use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::codex_auth_supervisor::{
    AuthPreflightReservation, CodexAuthReadyProof, CodexAuthSupervisor, CodexMutationLease,
    LoginReservation, OperationErrorView, OperationSnapshot, SharedCodexAuthSupervisor,
};
use crate::commands::runtime::config_mutation::ConfigMutationCommandErrorV1;
use crate::lifecycle::RuntimeMutationDomain;
use crate::proc::ChildLiveness;
use crate::runtime::proxy_lifecycle::{
    gateway_bin_path, DurableGatewayObservation, GatewayController, GatewayStopClaim,
};
use crate::runtime::science::{
    SandboxScienceState, ScienceHostAdapter, ScienceStopOwnershipReceipt, ScienceStopRequest,
};
use crate::runtime::system::kill_child;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

const AUTH_SCHEMA_VERSION: u32 = 3;
const MAX_AUTH_LINE_BYTES: usize = 8 * 1024;
const MAX_AUTH_OUTPUT_BYTES: u64 = 64 * 1024;
const AUTH_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ACCEPTED_CANCEL_WATCHDOG: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexAuthUnavailableCause {
    KeychainUnavailable,
    InteractionTimeout,
    SidecarSpawnFailed,
    SidecarProtocolError,
    IdentityMismatch,
    AuthStateInvalid,
    StorageUnavailable,
    UnsupportedPlatform,
    AuthStateChanged,
}

impl CodexAuthUnavailableCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::KeychainUnavailable => "keychain_unavailable",
            Self::InteractionTimeout => "interaction_timeout",
            Self::SidecarSpawnFailed => "sidecar_spawn_failed",
            Self::SidecarProtocolError => "sidecar_protocol_error",
            Self::IdentityMismatch => "identity_mismatch",
            Self::AuthStateInvalid => "auth_state_invalid",
            Self::StorageUnavailable => "storage_unavailable",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::AuthStateChanged => "auth_state_changed",
        }
    }

    fn retryable(self) -> bool {
        matches!(
            self,
            Self::KeychainUnavailable
                | Self::InteractionTimeout
                | Self::StorageUnavailable
                | Self::AuthStateChanged
        )
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CodexAuthCommandError {
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<&'static str>,
    retryable: bool,
}

impl CodexAuthCommandError {
    fn login_required(reason: &str) -> Self {
        Self {
            code: "codex_login_required",
            reason: Some(reason.to_string()),
            cause: None,
            retryable: false,
        }
    }

    fn unavailable(cause: CodexAuthUnavailableCause) -> Self {
        Self {
            code: "codex_auth_unavailable",
            reason: None,
            cause: Some(cause.as_str()),
            retryable: cause.retryable(),
        }
    }

    fn busy() -> Self {
        Self {
            code: "codex_auth_busy",
            reason: None,
            cause: None,
            retryable: true,
        }
    }
}

const CODEX_DISABLE_RECEIPT_SCHEMA_VERSION: u32 = 1;
const MAX_CODEX_DISABLE_RECEIPT_BYTES: usize = 64 * 1024;

#[cfg(test)]
static CODEX_DISABLE_GATEWAY_STOP_FAILURE: std::sync::LazyLock<
    std::sync::Mutex<Option<std::thread::ThreadId>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexDisablePhaseFailureMode {
    ExactBeforeImage,
    ConfigDrift,
}

#[cfg(test)]
static CODEX_DISABLE_PHASE_FAILURE: std::sync::LazyLock<
    std::sync::Mutex<Option<(std::thread::ThreadId, CodexDisablePhaseFailureMode)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
struct CodexDisableGatewayStopFailureGuard;

#[cfg(test)]
impl Drop for CodexDisableGatewayStopFailureGuard {
    fn drop(&mut self) {
        *CODEX_DISABLE_GATEWAY_STOP_FAILURE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

#[cfg(test)]
fn test_arm_codex_disable_gateway_stop_failure() -> CodexDisableGatewayStopFailureGuard {
    *CODEX_DISABLE_GATEWAY_STOP_FAILURE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(std::thread::current().id());
    CodexDisableGatewayStopFailureGuard
}

#[cfg(test)]
struct CodexDisablePhaseFailureGuard;

#[cfg(test)]
impl Drop for CodexDisablePhaseFailureGuard {
    fn drop(&mut self) {
        *CODEX_DISABLE_PHASE_FAILURE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

#[cfg(test)]
fn test_arm_codex_disable_gateway_phase_failure(
    mode: CodexDisablePhaseFailureMode,
) -> CodexDisablePhaseFailureGuard {
    *CODEX_DISABLE_PHASE_FAILURE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some((std::thread::current().id(), mode));
    CodexDisablePhaseFailureGuard
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CodexDisableCommandError {
    code: &'static str,
    cause: &'static str,
    phase: &'static str,
    retryable: bool,
    attention_required: bool,
    config_state: &'static str,
}

impl CodexDisableCommandError {
    fn failed(cause: &'static str, phase: &'static str) -> Self {
        Self {
            code: "codex_disable_failed",
            cause,
            phase,
            retryable: true,
            attention_required: false,
            config_state: "unchanged",
        }
    }

    fn attention(cause: &'static str, phase: &'static str, config_committed: bool) -> Self {
        Self {
            code: "codex_disable_attention",
            cause,
            phase,
            retryable: false,
            attention_required: true,
            config_state: if config_committed {
                "disabled"
            } else {
                "unchanged"
            },
        }
    }

    pub(crate) fn safe_message(&self) -> &'static str {
        if self.attention_required {
            "Codex disable 恢复状态需要人工注意；durable receipt 已保留。"
        } else {
            "Codex 实验入口未更改；运行态已保持或精确恢复。"
        }
    }

    fn project_boot_attention(&self) -> Value {
        json!({
            "schema_version": CODEX_DISABLE_RECEIPT_SCHEMA_VERSION,
            "status": "attention",
            "operation": "experimental_codex_disable",
            "error": self,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexDisableIntentPublishError {
    Retryable { cause: &'static str },
    Attention { cause: &'static str },
}

impl CodexDisableIntentPublishError {
    fn into_command_error(self) -> CodexDisableCommandError {
        match self {
            Self::Retryable { cause } => CodexDisableCommandError::failed(cause, "intent"),
            Self::Attention { cause } => {
                CodexDisableCommandError::attention(cause, "intent", false)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CodexDisableAttentionCause {
    ReceiptIo,
    ConfigDrift,
    IdentityDrift,
    StopUncertain,
    RestoreFailed,
    RestoreUncertain,
    ReceiptCleanupFailed,
}

impl CodexDisableAttentionCause {
    fn code(self) -> &'static str {
        match self {
            Self::ReceiptIo => "receipt_io",
            Self::ConfigDrift => "config_drift",
            Self::IdentityDrift => "identity_drift",
            Self::StopUncertain => "stop_uncertain",
            Self::RestoreFailed => "restore_failed",
            Self::RestoreUncertain => "restore_uncertain",
            Self::ReceiptCleanupFailed => "receipt_cleanup_failed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodexDisableConfigReference {
    schema_version: u32,
    active_profile_id: String,
    proxy_port: u16,
    sandbox_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodexDisableGatewayPlan {
    profile_id: String,
    identity: config::GatewayRuntimeJournalIdentity,
    restore_launch_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodexDisableSciencePlan {
    prior: config::RuntimePriorScienceRecipe,
    restore_launch_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodexDisableDurablePlan {
    owner_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    science: Option<CodexDisableSciencePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gateway: Option<CodexDisableGatewayPlan>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CodexDisableComponent {
    Science,
    Gateway,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum CodexDisableReceiptPhase {
    Intent,
    Stopping {
        component: CodexDisableComponent,
        science_stopped: bool,
        gateway_stopped: bool,
    },
    EffectsApplied {
        science_stopped: bool,
        gateway_stopped: bool,
    },
    Restoring {
        science_stopped: bool,
        gateway_stopped: bool,
    },
    Restored {
        science_stopped: bool,
        gateway_stopped: bool,
    },
    ConfigCommitted,
    Attention {
        cause: CodexDisableAttentionCause,
    },
}

impl CodexDisableReceiptPhase {
    fn code(&self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Stopping { .. } => "stopping",
            Self::EffectsApplied { .. } => "effects_applied",
            Self::Restoring { .. } => "restoring",
            Self::Restored { .. } => "restored",
            Self::ConfigCommitted => "config_committed",
            Self::Attention { .. } => "attention",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodexDisableOperationReceipt {
    schema_version: u32,
    operation_id: String,
    operation: String,
    before_config_fingerprint: String,
    after_config_fingerprint: String,
    config_reference: CodexDisableConfigReference,
    plan: CodexDisableDurablePlan,
    phase: CodexDisableReceiptPhase,
}

struct OpenCodexDisableReceipt {
    record: CodexDisableOperationReceipt,
    bytes: Vec<u8>,
}

fn valid_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_gateway_launch_id(value: &str) -> bool {
    (24..=128).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_prior_science_recipe(recipe: &config::RuntimePriorScienceRecipe) -> bool {
    recipe.port != 0
        && recipe.port != 8765
        && recipe.runtime_path.is_absolute()
        && matches!(
            recipe.runtime_source.as_str(),
            "explicit" | "official_updated" | "installed_app" | "cached_once"
        )
        && valid_lower_hex(&recipe.runtime_fingerprint, 64)
        && valid_lower_hex(&recipe.launch_receipt_digest, 64)
        && recipe
            .runtime_adoption_attempt_id
            .as_deref()
            .is_some_and(|value| valid_lower_hex(value, 32))
}

fn validate_codex_disable_receipt(receipt: &CodexDisableOperationReceipt) -> Result<(), String> {
    if receipt.schema_version != CODEX_DISABLE_RECEIPT_SCHEMA_VERSION
        || receipt.operation != "experimental_codex_disable"
        || !valid_lower_hex(&receipt.operation_id, 32)
        || !valid_lower_hex(&receipt.before_config_fingerprint, 64)
        || !valid_lower_hex(&receipt.after_config_fingerprint, 64)
        || receipt.before_config_fingerprint == receipt.after_config_fingerprint
        || receipt.config_reference.schema_version != config::CURRENT_SCHEMA_VERSION
        || receipt.config_reference.proxy_port == 0
        || receipt.config_reference.sandbox_port == 0
        || receipt.config_reference.proxy_port == receipt.config_reference.sandbox_port
        || receipt.config_reference.proxy_port == 8765
        || receipt.config_reference.sandbox_port == 8765
        || (receipt.plan.science.is_none() && receipt.plan.gateway.is_none())
        || receipt.plan.owner_generation == 0
    {
        return Err("Codex disable receipt identity/schema 非法".into());
    }
    if let Some(science) = receipt.plan.science.as_ref() {
        if !valid_prior_science_recipe(&science.prior)
            || science.prior.port != receipt.config_reference.sandbox_port
            || !valid_lower_hex(&science.restore_launch_id, 32)
        {
            return Err("Codex disable Science receipt plan 非法".into());
        }
    }
    if let Some(gateway) = receipt.plan.gateway.as_ref() {
        let identity = &gateway.identity;
        if gateway.profile_id.is_empty()
            || identity.provider != "codex"
            || identity.shim.is_empty()
            || !valid_gateway_launch_id(&identity.launch_id)
            || identity.provider_contract_id.is_empty()
            || identity.provider_contract_digest.is_empty()
            || !valid_lower_hex(&gateway.restore_launch_id, 32)
        {
            return Err("Codex disable Gateway receipt plan 非法".into());
        }
    }
    if let CodexDisableReceiptPhase::Stopping {
        component,
        science_stopped,
        gateway_stopped,
    } = receipt.phase
    {
        let target_planned = match component {
            CodexDisableComponent::Science => receipt.plan.science.is_some() && !science_stopped,
            CodexDisableComponent::Gateway => receipt.plan.gateway.is_some() && !gateway_stopped,
        };
        if !target_planned
            || (science_stopped && receipt.plan.science.is_none())
            || (gateway_stopped && receipt.plan.gateway.is_none())
        {
            return Err("Codex disable receipt stopping progress 非法".into());
        }
    }
    if let CodexDisableReceiptPhase::EffectsApplied {
        science_stopped,
        gateway_stopped,
    }
    | CodexDisableReceiptPhase::Restoring {
        science_stopped,
        gateway_stopped,
    }
    | CodexDisableReceiptPhase::Restored {
        science_stopped,
        gateway_stopped,
    } = receipt.phase
    {
        if (!science_stopped && !gateway_stopped)
            || (science_stopped && receipt.plan.science.is_none())
            || (gateway_stopped && receipt.plan.gateway.is_none())
        {
            return Err("Codex disable receipt effect progress 非法".into());
        }
    }
    Ok(())
}

fn codex_disable_config_fingerprint(cfg: &config::Config) -> Result<String, String> {
    config::codex_disable_config_fingerprint(cfg)
        .map_err(|_| "无法编码 Codex disable config fingerprint".into())
}

fn codex_disable_intent_digest(receipt: &CodexDisableOperationReceipt) -> Result<String, String> {
    let mut intent = receipt.clone();
    intent.phase = CodexDisableReceiptPhase::Intent;
    let bytes = encode_codex_disable_receipt(&intent)?;
    let mut digest = Sha256::new();
    digest.update(b"csswitch-codex-disable-intent-v1\0");
    digest.update(bytes);
    Ok(format!("{:x}", digest.finalize()))
}

fn codex_disable_config_fence(
    receipt: &CodexDisableOperationReceipt,
) -> Result<config::CodexDisableOperationFence, String> {
    Ok(config::CodexDisableOperationFence::new(
        receipt.operation_id.clone(),
        codex_disable_intent_digest(receipt)?,
        receipt.before_config_fingerprint.clone(),
        receipt.after_config_fingerprint.clone(),
    ))
}

fn codex_disable_after_config(cfg: &config::Config) -> config::Config {
    let mut after = cfg.clone();
    after.experimental_codex_enabled = false;
    after
}

fn encode_codex_disable_receipt(receipt: &CodexDisableOperationReceipt) -> Result<Vec<u8>, String> {
    validate_codex_disable_receipt(receipt)?;
    let bytes = serde_json::to_vec(receipt).map_err(|_| "无法编码 Codex disable receipt")?;
    if bytes.is_empty() || bytes.len() > MAX_CODEX_DISABLE_RECEIPT_BYTES {
        return Err("Codex disable receipt 大小非法".into());
    }
    Ok(bytes)
}

fn read_codex_disable_receipt_at(dir: &Path) -> Result<Option<OpenCodexDisableReceipt>, String> {
    let Some(bytes) = config::read_codex_disable_operation_receipt(dir)
        .map_err(|_| "Codex disable receipt 不可读取")?
    else {
        return Ok(None);
    };
    if bytes.is_empty() || bytes.len() > MAX_CODEX_DISABLE_RECEIPT_BYTES {
        return Err("Codex disable receipt 大小非法".into());
    }
    let record: CodexDisableOperationReceipt =
        serde_json::from_slice(&bytes).map_err(|_| "Codex disable receipt 无法解析")?;
    validate_codex_disable_receipt(&record)?;
    Ok(Some(OpenCodexDisableReceipt { record, bytes }))
}

impl OpenCodexDisableReceipt {
    fn publish_intent(
        dir: &Path,
        expected_config: &config::Config,
        record: CodexDisableOperationReceipt,
    ) -> Result<Self, CodexDisableIntentPublishError> {
        let bytes = encode_codex_disable_receipt(&record).map_err(|_| {
            CodexDisableIntentPublishError::Retryable {
                cause: "receipt_io",
            }
        })?;
        let fence = codex_disable_config_fence(&record).map_err(|_| {
            CodexDisableIntentPublishError::Retryable {
                cause: "receipt_io",
            }
        })?;
        let _ = config::begin_codex_disable_operation(dir, expected_config, &fence, &bytes);

        // The begin call can fail either before publishing anything (for
        // example, an exact before-image race) or after leaving durable state.
        // Reconcile the disk state before choosing the user-visible outcome so
        // that "receipt retained" is never claimed for a proven-empty begin.
        let opened = read_codex_disable_receipt_at(dir).map_err(|_| {
            CodexDisableIntentPublishError::Attention {
                cause: "receipt_io",
            }
        })?;
        let current =
            config::load_from(dir).map_err(|_| CodexDisableIntentPublishError::Attention {
                cause: "receipt_io",
            })?;
        let current_fence = current.codex_disable_operation_fence().map_err(|_| {
            CodexDisableIntentPublishError::Attention {
                cause: "config_drift",
            }
        })?;

        match opened {
            Some(opened) if opened.bytes == bytes && current_fence.as_ref() == Some(&fence) => {
                Ok(opened)
            }
            Some(_) => Err(CodexDisableIntentPublishError::Attention {
                cause: if current_fence.as_ref() == Some(&fence) {
                    "mutation_conflict"
                } else {
                    "config_drift"
                },
            }),
            None if current_fence.is_some() => Err(CodexDisableIntentPublishError::Attention {
                cause: if current_fence.as_ref() == Some(&fence) {
                    "receipt_io"
                } else {
                    "mutation_conflict"
                },
            }),
            None if current == *expected_config => Err(CodexDisableIntentPublishError::Retryable {
                cause: "receipt_io",
            }),
            None => Err(CodexDisableIntentPublishError::Retryable {
                cause: "config_drift",
            }),
        }
    }

    fn transition(&mut self, dir: &Path, phase: CodexDisableReceiptPhase) -> Result<(), String> {
        #[cfg(test)]
        {
            let phase_failure = CODEX_DISABLE_PHASE_FAILURE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .filter(|(thread, _)| *thread == std::thread::current().id())
                .map(|(_, mode)| *mode);
            if matches!(
                &phase,
                CodexDisableReceiptPhase::Stopping {
                    component: CodexDisableComponent::Gateway,
                    ..
                }
            ) {
                if let Some(mode) = phase_failure {
                    if mode == CodexDisablePhaseFailureMode::ConfigDrift {
                        let mut drifted = config::load_from(dir)
                            .map_err(|_| "test-only Codex disable Config drift load failed")?;
                        drifted.reuse_system_ssh = !drifted.reuse_system_ssh;
                        config::test_save_to_without_history_authority_guard(dir, &drifted)
                            .map_err(|_| "test-only Codex disable Config drift save failed")?;
                    }
                    return Err("test-only Codex disable Gateway phase publication failure".into());
                }
            }
        }
        let fence = codex_disable_config_fence(&self.record)?;
        let current =
            config::load_from(dir).map_err(|_| "Codex disable receipt phase 前 Config 不可读取")?;
        if current
            .codex_disable_operation_fence()
            .map_err(|_| "Codex disable receipt phase 前 fence 不可解析")?
            .as_ref()
            != Some(&fence)
        {
            return Err("Codex disable receipt phase 前 fence 已变化；当前 receipt 已保留".into());
        }
        let terminal_fingerprint = match &phase {
            CodexDisableReceiptPhase::ConfigCommitted => {
                Some(&self.record.after_config_fingerprint)
            }
            CodexDisableReceiptPhase::Attention { .. } => None,
            CodexDisableReceiptPhase::Intent
            | CodexDisableReceiptPhase::Stopping { .. }
            | CodexDisableReceiptPhase::EffectsApplied { .. }
            | CodexDisableReceiptPhase::Restoring { .. }
            | CodexDisableReceiptPhase::Restored { .. } => {
                Some(&self.record.before_config_fingerprint)
            }
        };
        if terminal_fingerprint.is_some_and(|expected| {
            codex_disable_config_fingerprint(&current).as_ref() != Ok(expected)
        }) {
            return Err(
                "Codex disable receipt phase 前 Config 不匹配 exact terminal image；当前 receipt 已保留"
                    .into(),
            );
        }
        let mut next = self.record.clone();
        next.phase = phase;
        let bytes = encode_codex_disable_receipt(&next)?;
        config::write_codex_disable_operation_receipt(dir, &bytes, &self.bytes)
            .map_err(|_| "Codex disable receipt phase 无法持久化")?;
        let opened = read_codex_disable_receipt_at(dir)?
            .ok_or("Codex disable receipt phase 持久化后缺失")?;
        if opened.bytes != bytes {
            return Err("Codex disable receipt phase 持久化后回读不一致".into());
        }
        *self = opened;
        Ok(())
    }

    fn terminal_cleanup_cause(
        &self,
        dir: &Path,
        terminal_image: config::CodexDisableTerminalConfigImage,
    ) -> CodexDisableAttentionCause {
        let Ok(fence) = codex_disable_config_fence(&self.record) else {
            return CodexDisableAttentionCause::ReceiptIo;
        };
        let Ok(current) = config::load_from(dir) else {
            return CodexDisableAttentionCause::ReceiptIo;
        };
        if current
            .codex_disable_operation_fence()
            .ok()
            .flatten()
            .as_ref()
            != Some(&fence)
        {
            return CodexDisableAttentionCause::ConfigDrift;
        }
        let expected = match terminal_image {
            config::CodexDisableTerminalConfigImage::Before => {
                &self.record.before_config_fingerprint
            }
            config::CodexDisableTerminalConfigImage::After => &self.record.after_config_fingerprint,
        };
        if codex_disable_config_fingerprint(&current).as_ref() != Ok(expected) {
            CodexDisableAttentionCause::ConfigDrift
        } else {
            CodexDisableAttentionCause::ReceiptCleanupFailed
        }
    }

    fn phase_publication_cause(
        &self,
        dir: &Path,
        config_image: config::CodexDisableTerminalConfigImage,
    ) -> CodexDisableAttentionCause {
        match self.terminal_cleanup_cause(dir, config_image) {
            CodexDisableAttentionCause::ConfigDrift => CodexDisableAttentionCause::ConfigDrift,
            _ => CodexDisableAttentionCause::ReceiptIo,
        }
    }

    fn clear_terminal(
        &self,
        dir: &Path,
        terminal_image: config::CodexDisableTerminalConfigImage,
    ) -> Result<(), CodexDisableAttentionCause> {
        let fence = codex_disable_config_fence(&self.record)
            .map_err(|_| CodexDisableAttentionCause::ReceiptIo)?;
        let cause = || self.terminal_cleanup_cause(dir, terminal_image);
        config::clear_codex_disable_operation_receipt(dir, &self.bytes, &fence, terminal_image)
            .map_err(|_| cause())?;
        if config::read_codex_disable_operation_receipt(dir)
            .map_err(|_| CodexDisableAttentionCause::ReceiptCleanupFailed)?
            .is_some()
        {
            return Err(CodexDisableAttentionCause::ReceiptCleanupFailed);
        }
        config::clear_codex_disable_operation_fence(dir, &fence, terminal_image)
            .map_err(|_| cause())?;
        Ok(())
    }

    fn clear_before(&self, dir: &Path) -> Result<(), CodexDisableAttentionCause> {
        self.clear_terminal(dir, config::CodexDisableTerminalConfigImage::Before)
    }

    fn clear_after(&self, dir: &Path) -> Result<(), CodexDisableAttentionCause> {
        self.clear_terminal(dir, config::CodexDisableTerminalConfigImage::After)
    }
}

fn require_no_codex_disable_receipt(dir: &Path) -> Result<(), CodexDisableCommandError> {
    match read_codex_disable_receipt_at(dir) {
        Ok(None) => match config::load_from(dir) {
            Ok(cfg) => match cfg.codex_disable_operation_fence() {
                Ok(None) => Ok(()),
                Ok(Some(fence)) => {
                    let fingerprint = codex_disable_config_fingerprint(&cfg).ok();
                    let cause = if fingerprint.as_deref()
                        == Some(fence.before_config_fingerprint.as_str())
                        || fingerprint.as_deref() == Some(fence.after_config_fingerprint.as_str())
                    {
                        "receipt_open"
                    } else {
                        "config_drift"
                    };
                    Err(CodexDisableCommandError::attention(
                        cause,
                        "attention",
                        !cfg.experimental_codex_enabled,
                    ))
                }
                Err(_) => Err(CodexDisableCommandError::attention(
                    "receipt_invalid",
                    "attention",
                    false,
                )),
            },
            Err(_) => Err(CodexDisableCommandError::attention(
                "receipt_io",
                "attention",
                false,
            )),
        },
        Ok(Some(open)) => Err(CodexDisableCommandError::attention(
            "receipt_open",
            open.record.phase.code(),
            codex_disable_current_config_state(dir, &open.record) == Some("disabled"),
        )),
        Err(_) => Err(CodexDisableCommandError::attention(
            "receipt_invalid",
            "attention",
            false,
        )),
    }
}

fn codex_disable_current_config_state(
    dir: &Path,
    receipt: &CodexDisableOperationReceipt,
) -> Option<&'static str> {
    let current = config::load_from(dir).ok()?;
    let fingerprint = codex_disable_config_fingerprint(&current).ok()?;
    if fingerprint == receipt.before_config_fingerprint {
        Some("unchanged")
    } else if fingerprint == receipt.after_config_fingerprint {
        Some("disabled")
    } else {
        None
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum RuntimeCommandError {
    Auth(CodexAuthCommandError),
    Disable(CodexDisableCommandError),
    Mutation(ConfigMutationCommandErrorV1),
    Message(String),
}

impl From<CodexAuthCommandError> for RuntimeCommandError {
    fn from(error: CodexAuthCommandError) -> Self {
        Self::Auth(error)
    }
}

impl From<CodexDisableCommandError> for RuntimeCommandError {
    fn from(error: CodexDisableCommandError) -> Self {
        Self::Disable(error)
    }
}

impl From<ConfigMutationCommandErrorV1> for RuntimeCommandError {
    fn from(error: ConfigMutationCommandErrorV1) -> Self {
        Self::Mutation(error)
    }
}

impl From<String> for RuntimeCommandError {
    fn from(error: String) -> Self {
        Self::Message(error)
    }
}

impl From<&str> for RuntimeCommandError {
    fn from(error: &str) -> Self {
        Self::Message(error.to_string())
    }
}

impl std::fmt::Display for RuntimeCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Disable(error) => formatter.write_str(error.safe_message()),
            Self::Mutation(error) => error.fmt(formatter),
            Self::Auth(error) => formatter.write_str(match error.code {
                "codex_login_required" => "Codex 尚未登录或本地认证记录不完整。",
                "codex_auth_busy" => "另一项 Codex 认证或启动操作正在进行。",
                _ => "Codex 认证状态暂不可用。",
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexAuthAction {
    LoginBrowser,
    Status,
    Logout,
}

struct ManagedAuthProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::process::ChildStdout,
    pending: Vec<u8>,
    identity: crate::commands::runtime::config_mutation::AuthSidecarIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidecarWaitFailure {
    Cancelled,
    Timeout,
    Protocol,
}

fn auth_error_from_sidecar_wait(error: SidecarWaitFailure) -> CodexAuthCommandError {
    match error {
        SidecarWaitFailure::Cancelled | SidecarWaitFailure::Timeout => {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::InteractionTimeout)
        }
        SidecarWaitFailure::Protocol => {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarProtocolError)
        }
    }
}

impl SidecarWaitFailure {
    #[cfg(test)]
    fn safe_message(self) -> &'static str {
        match self {
            Self::Cancelled => "Codex 认证检查已取消。",
            Self::Timeout => "Codex 认证 sidecar 超时，受管进程已结束。",
            Self::Protocol => "Codex 认证 sidecar 协议或进程状态无效。",
        }
    }
}

impl CodexAuthAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::LoginBrowser => "login-browser",
            Self::Status => "status",
            Self::Logout => "logout",
        }
    }

    fn timeout(self) -> Duration {
        match self {
            // Gateway's browser callback budget is five minutes. The outer
            // supervisor allows a small cleanup margin but never waits forever.
            Self::LoginBrowser => Duration::from_secs(5 * 60 + 15),
            Self::Status => Duration::from_secs(120),
            Self::Logout => Duration::from_secs(60),
        }
    }

    fn is_login(self) -> bool {
        self == Self::LoginBrowser
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthStatusView {
    authenticated: bool,
    reason: String,
    account_hash: Option<String>,
    expiry_state: String,
    expires_at: Option<i64>,
    auth_epoch: Option<String>,
    auth_generation: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum CodexPreflightTarget {
    ActiveProfile,
    Profile(String),
    NoProfile,
}

#[derive(PartialEq, Eq)]
struct SensitiveConfigSecret(String);

#[derive(PartialEq, Eq)]
struct CodexProfileLaunchSnapshot {
    id: String,
    template_id: String,
    api_format: String,
    base_url: String,
    model_catalog_snapshot: String,
    credential_source: crate::provider_contracts::CredentialSource,
    credential_ref: Option<String>,
    model_policy: crate::provider_contracts::ModelPolicy,
}

#[derive(PartialEq, Eq)]
struct CodexLaunchSnapshot {
    active_id: String,
    profile: Option<CodexProfileLaunchSnapshot>,
    experimental_codex_enabled: bool,
    codex_network: csswitch_codex_network::CodexNetworkSettings,
    proxy_port: u16,
    sandbox_port: u16,
    reuse_system_ssh: bool,
    mode: String,
    secret: SensitiveConfigSecret,
}

pub(crate) struct PreparedCodexAuth {
    target: CodexPreflightTarget,
    snapshot: CodexLaunchSnapshot,
    proof: CodexAuthReadyProof,
}

impl CodexLaunchSnapshot {
    fn capture(target: &CodexPreflightTarget) -> Result<Self, String> {
        Self::capture_from(&config::default_dir(), target)
    }

    fn capture_from(dir: &Path, target: &CodexPreflightTarget) -> Result<Self, String> {
        let cfg = config::load_from(dir).map_err(|error| error.to_string())?;
        Ok(Self::from_config(&cfg, target))
    }

    fn from_config(cfg: &config::Config, target: &CodexPreflightTarget) -> Self {
        let profile = match target {
            CodexPreflightTarget::ActiveProfile => cfg.active_profile(),
            CodexPreflightTarget::Profile(id) => cfg.profile_by_id(id),
            CodexPreflightTarget::NoProfile => None,
        }
        .map(|profile| CodexProfileLaunchSnapshot {
            id: profile.id.clone(),
            template_id: profile.template_id.clone(),
            api_format: profile.api_format.clone(),
            base_url: profile.base_url.clone(),
            model_catalog_snapshot: serde_json::to_string(&serde_json::json!({
                "model_catalog": profile.model_catalog,
                "default_model_route_id": profile.default_model_route_id,
                "role_bindings": profile.role_bindings,
            }))
            .unwrap_or_else(|_| "serialization-error".into()),
            credential_source: profile.credential_source,
            credential_ref: profile.credential_ref.clone(),
            model_policy: profile.model_policy,
        });
        Self {
            active_id: cfg.active_id.clone(),
            profile,
            experimental_codex_enabled: cfg.experimental_codex_enabled,
            codex_network: cfg.codex_network.clone(),
            proxy_port: cfg.proxy_port,
            sandbox_port: cfg.sandbox_port,
            reuse_system_ssh: cfg.reuse_system_ssh,
            mode: cfg.mode.clone(),
            secret: SensitiveConfigSecret(cfg.secret.clone()),
        }
    }
}

impl PreparedCodexAuth {
    pub(crate) fn proof(&self) -> &CodexAuthReadyProof {
        &self.proof
    }

    pub(crate) fn verify_unchanged(&self) -> Result<(), String> {
        self.verify_unchanged_from(&config::default_dir())
    }

    fn verify_unchanged_from(&self, dir: &Path) -> Result<(), String> {
        verify_launch_snapshot_unchanged(dir, &self.target, &self.snapshot)
    }
}

fn verify_launch_snapshot_unchanged(
    dir: &Path,
    target: &CodexPreflightTarget,
    expected: &CodexLaunchSnapshot,
) -> Result<(), String> {
    if CodexLaunchSnapshot::capture_from(dir, target)? == *expected {
        Ok(())
    } else {
        Err("config_changed_retry：Codex 启动配置在认证检查期间发生变化，请重试。".into())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarSuccess {
    schema_version: u32,
    ok: bool,
    command: String,
    status: AuthStatusView,
    #[serde(default)]
    warning: Option<LogoutWarningView>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LogoutWarningView {
    code: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarErrorView {
    code: String,
    message: String,
    retryable: bool,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    upstream_status: Option<u16>,
    #[serde(default)]
    response_kind: Option<String>,
    #[serde(default)]
    challenge_detected: Option<bool>,
    #[serde(default)]
    transport_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarError {
    schema_version: u32,
    ok: bool,
    command: Option<String>,
    error: SidecarErrorView,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SidecarEnvelope {
    Success(SidecarSuccess),
    Error(SidecarError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginSidecarEvent {
    schema_version: u32,
    operation_id: String,
    kind: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    disposition: Option<String>,
    #[serde(default)]
    authorization_digest: Option<String>,
    #[serde(default)]
    status: Option<AuthStatusView>,
    #[serde(default)]
    error: Option<LoginSidecarError>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LoginSidecarError {
    code: String,
    stage: String,
    retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge_detected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_kind: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackedProxyState {
    Absent,
    Running,
    Exited,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthRuntimeAction {
    Noop,
    PreserveOtherProvider,
    StopManagedCodex,
}

fn auth_runtime_terminal_state(action: AuthRuntimeAction) -> &'static str {
    match action {
        AuthRuntimeAction::StopManagedCodex => "stopped",
        AuthRuntimeAction::Noop | AuthRuntimeAction::PreserveOtherProvider => "preserved",
    }
}

fn retain_failed_login_receipt(
    mutation: &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
    runtime_action: AuthRuntimeAction,
    cause: &'static str,
) {
    let _ = mutation.finish(
        "attention",
        "before",
        auth_runtime_terminal_state(runtime_action),
        Some(cause),
        config::ConfigMutationTerminalConfigImage::Before,
    );
}

enum ExperimentalCodexDisablePlan {
    Noop,
    PreserveOtherProvider,
    StopManagedCodex(Box<CodexDisableStopPlan>),
}

struct CodexDisableScienceStop {
    runtime: crate::runtime::science::ScienceRuntimeIdentity,
    ownership: crate::runtime::science::ScienceManagedLaunchToken,
}

struct CodexDisableStopPlan {
    before_config: config::Config,
    receipt: CodexDisableOperationReceipt,
    science_owner: CodexScienceOwnerSnapshot,
    science: Option<CodexDisableScienceStop>,
    gateway: Option<GatewayStopClaim>,
}

enum DowngradeCommandOutcome {
    Committed(Value),
    SafeFailure(String),
    TerminalFailure(String),
}

fn downgrade_command_outcome(
    result: Result<Option<PathBuf>, config::DowngradeError>,
    success: Value,
) -> DowngradeCommandOutcome {
    match result {
        Ok(_) => DowngradeCommandOutcome::Committed(success),
        Err(error) if error.exit_required => DowngradeCommandOutcome::TerminalFailure(format!(
            "v2 配置发布后的持久化或回滚状态不确定；进程已锁存并强制退出，禁止再次读取配置：{}",
            error.message
        )),
        Err(error) => DowngradeCommandOutcome::SafeFailure(error.message),
    }
}

fn run_downgrade_mutation_at(
    dir: &Path,
    actions: &BTreeMap<String, config::CodexDowngradeAction>,
    destination: &Path,
    expected_fingerprint: &str,
    success: Value,
    stop_runtime: impl FnOnce() -> Result<(), String>,
) -> Result<DowngradeCommandOutcome, String> {
    let preflight = config::load_from(dir).map_err(|error| error.to_string())?;
    config::require_no_runtime_transaction(&preflight)?;
    stop_runtime()?;
    Ok(downgrade_command_outcome(
        config::downgrade_to_v2_and_latch(dir, actions, Some(destination), expected_fingerprint),
        success,
    ))
}

fn finish_downgrade_command(
    outcome: DowngradeCommandOutcome,
    exit: impl FnOnce(i32),
) -> Result<Value, String> {
    match outcome {
        DowngradeCommandOutcome::SafeFailure(error) => Err(error),
        DowngradeCommandOutcome::Committed(result) => {
            exit(0);
            Ok(result)
        }
        DowngradeCommandOutcome::TerminalFailure(error) => {
            exit(1);
            Err(error)
        }
    }
}

fn known_non_codex_provider(provider: &str) -> bool {
    matches!(
        provider,
        "deepseek" | "qwen" | "relay" | "openai-custom" | "openai-responses"
    )
}

fn decide_auth_runtime_action(
    provider: &str,
    tracked: TrackedProxyState,
    untracked_proxy_port_occupied: bool,
) -> Result<AuthRuntimeAction, String> {
    if matches!(
        tracked,
        TrackedProxyState::Absent | TrackedProxyState::Exited
    ) && untracked_proxy_port_occupied
    {
        return Err(
            "代理端口仍有 listener，但 CSSwitch 已没有可安全停止的 Child 句柄；未发送认证信息、未结束未知进程，Codex 操作已拒绝。"
                .into(),
        );
    }
    if provider == "codex" {
        return Ok(AuthRuntimeAction::StopManagedCodex);
    }
    if known_non_codex_provider(provider) {
        return Ok(AuthRuntimeAction::PreserveOtherProvider);
    }
    if matches!(
        tracked,
        TrackedProxyState::Running | TrackedProxyState::Unknown
    ) {
        return Err(
            "受管代理仍在运行，但无法确认其 provider 身份；为避免误停或在途认证变更，本次 Codex 操作已拒绝。"
                .into(),
        );
    }
    Ok(AuthRuntimeAction::Noop)
}

fn resolve_science_runtime_action(
    proxy_action: AuthRuntimeAction,
    active_profile_is_codex: bool,
    science_state: SandboxScienceState,
) -> Result<AuthRuntimeAction, String> {
    if proxy_action == AuthRuntimeAction::PreserveOtherProvider {
        return Ok(proxy_action);
    }
    if proxy_action == AuthRuntimeAction::Noop && !active_profile_is_codex {
        return Ok(proxy_action);
    }
    match science_state {
        SandboxScienceState::RunningHealthy => Ok(AuthRuntimeAction::StopManagedCodex),
        SandboxScienceState::Stopped => Ok(proxy_action),
        SandboxScienceState::Unknown => Err(
            "无法确认沙箱端口上的 Science binary/data-dir 身份；Codex 认证与实验开关均未变更。"
                .into(),
        ),
    }
}

fn codex_disable_requires_gateway_claim(
    action: AuthRuntimeAction,
    tracked: TrackedProxyState,
) -> Result<bool, String> {
    match (action, tracked) {
        (AuthRuntimeAction::StopManagedCodex, TrackedProxyState::Running) => Ok(true),
        (AuthRuntimeAction::StopManagedCodex, TrackedProxyState::Unknown) => Err(
            "受管 Codex Gateway 状态不确定，无法建立可持久化的精确停止计划；实验开关未变更。"
                .into(),
        ),
        _ => Ok(false),
    }
}

fn tracked_proxy_state(st: &mut AppState) -> TrackedProxyState {
    let Some(child) = st.proxy.as_mut() else {
        return TrackedProxyState::Absent;
    };
    match proc::poll_child_liveness(child) {
        ChildLiveness::Running => TrackedProxyState::Running,
        ChildLiveness::Exited(_) => TrackedProxyState::Exited,
        ChildLiveness::Unknown(_) => TrackedProxyState::Unknown,
    }
}

#[derive(Clone)]
struct CodexScienceOwnerSnapshot {
    generation: u64,
    runtime: Option<crate::runtime::science::ScienceRuntimeIdentity>,
    confirmed_stopped: Option<crate::runtime::science::ScienceRuntimeIdentity>,
    sandbox_child_pid: Option<u32>,
    sandbox_port: u16,
    sandbox_url: Option<String>,
}

impl CodexScienceOwnerSnapshot {
    fn claim(st: &AppState, generation: u64) -> Self {
        Self {
            generation,
            runtime: st.science_runtime.clone(),
            confirmed_stopped: st.science_confirmed_stopped.clone(),
            sandbox_child_pid: st.sandbox.as_ref().map(std::process::Child::id),
            sandbox_port: st.sandbox_port,
            sandbox_url: st.sandbox_url.clone(),
        }
    }

    fn still_owns(&self, st: &AppState, current_generation: u64) -> bool {
        self.generation == current_generation
            && self.runtime == st.science_runtime
            && self.confirmed_stopped == st.science_confirmed_stopped
            && self.sandbox_child_pid == st.sandbox.as_ref().map(std::process::Child::id)
            && self.sandbox_port == st.sandbox_port
            && self.sandbox_url == st.sandbox_url
    }
}

fn plan_experimental_codex_disable(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<ExperimentalCodexDisablePlan, CodexDisableCommandError> {
    require_no_codex_disable_receipt(dir)?;
    let cfg = config::load_from(dir)
        .map_err(|_| CodexDisableCommandError::failed("config_unavailable", "intent"))?;
    config::require_no_runtime_transaction(&cfg)
        .map_err(|_| CodexDisableCommandError::failed("mutation_conflict", "intent"))?;
    if !cfg.experimental_codex_enabled {
        return Ok(ExperimentalCodexDisablePlan::Noop);
    }
    let active_profile_is_codex = cfg
        .active_profile()
        .is_some_and(|profile| profile.template_id == "codex");
    let (provider, tracked, owner, version_cache) = {
        let mut current = lock(state);
        let provider = current.provider.clone();
        let tracked = tracked_proxy_state(&mut current);
        let owner = CodexScienceOwnerSnapshot::claim(&current, lifecycle.current_generation());
        (
            provider,
            tracked,
            owner,
            current.science_version_cache.clone(),
        )
    };
    let untracked_proxy_port_occupied = matches!(
        tracked,
        TrackedProxyState::Absent | TrackedProxyState::Exited
    ) && proc::loopback_port_in_use(cfg.proxy_port, 100);
    let proxy_action =
        decide_auth_runtime_action(&provider, tracked, untracked_proxy_port_occupied)
            .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?;
    if proxy_action == AuthRuntimeAction::PreserveOtherProvider {
        return Ok(ExperimentalCodexDisablePlan::PreserveOtherProvider);
    }
    if proxy_action == AuthRuntimeAction::Noop && !active_profile_is_codex {
        return Ok(ExperimentalCodexDisablePlan::Noop);
    }
    let remembered_runtime = owner
        .runtime
        .clone()
        .or_else(|| owner.confirmed_stopped.clone());
    let (science_state, detected_runtime) = match remembered_runtime {
        Some(runtime) => {
            let observed = ScienceHostAdapter::probe_known(cfg.sandbox_port, &runtime);
            let detected = (observed == SandboxScienceState::RunningHealthy).then_some(runtime);
            (observed, detected)
        }
        None => ScienceHostAdapter::probe_cached(cfg.sandbox_port, &version_cache)
            .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?,
    };
    let action =
        resolve_science_runtime_action(proxy_action, active_profile_is_codex, science_state)
            .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?;
    if action != AuthRuntimeAction::StopManagedCodex {
        return Ok(ExperimentalCodexDisablePlan::Noop);
    }

    let science = match detected_runtime {
        Some(runtime) => {
            let ownership = ScienceHostAdapter::managed_receipt(cfg.sandbox_port, &runtime)
                .ok_or_else(|| CodexDisableCommandError::failed("identity_unproven", "intent"))?;
            if !ScienceHostAdapter::receipt_is_current(&ownership, &runtime) {
                return Err(CodexDisableCommandError::failed(
                    "identity_unproven",
                    "intent",
                ));
            }
            Some(CodexDisableScienceStop { runtime, ownership })
        }
        None => None,
    };
    let requires_gateway_claim = codex_disable_requires_gateway_claim(proxy_action, tracked)
        .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?;
    let gateway = if requires_gateway_claim {
        GatewayController::claim_stop(state, lifecycle)
            .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?
            .ok_or_else(|| CodexDisableCommandError::failed("identity_unproven", "intent"))?
            .into()
    } else {
        None
    };
    if science.is_none() && gateway.is_none() {
        return Ok(ExperimentalCodexDisablePlan::Noop);
    }

    let science_plan = science
        .as_ref()
        .map(|science| {
            science
                .ownership
                .durable_prior_stop_recipe(&science.runtime, cfg.sandbox_port)
                .map(|prior| CodexDisableSciencePlan {
                    prior,
                    restore_launch_id: config::new_id(),
                })
        })
        .transpose()
        .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?;
    let gateway_plan = gateway
        .as_ref()
        .map(|claim: &GatewayStopClaim| CodexDisableGatewayPlan {
            profile_id: claim.profile_id().to_string(),
            identity: claim.durable_identity(),
            restore_launch_id: config::new_id(),
        });
    let before_config_fingerprint = codex_disable_config_fingerprint(&cfg)
        .map_err(|_| CodexDisableCommandError::failed("config_unavailable", "intent"))?;
    let after_config_fingerprint =
        codex_disable_config_fingerprint(&codex_disable_after_config(&cfg))
            .map_err(|_| CodexDisableCommandError::failed("config_unavailable", "intent"))?;
    let receipt = CodexDisableOperationReceipt {
        schema_version: CODEX_DISABLE_RECEIPT_SCHEMA_VERSION,
        operation_id: config::new_id(),
        operation: "experimental_codex_disable".into(),
        before_config_fingerprint,
        after_config_fingerprint,
        config_reference: CodexDisableConfigReference {
            schema_version: cfg.schema_version,
            active_profile_id: cfg.active_id.clone(),
            proxy_port: cfg.proxy_port,
            sandbox_port: cfg.sandbox_port,
        },
        plan: CodexDisableDurablePlan {
            owner_generation: owner.generation,
            science: science_plan,
            gateway: gateway_plan,
        },
        phase: CodexDisableReceiptPhase::Intent,
    };
    validate_codex_disable_receipt(&receipt)
        .map_err(|_| CodexDisableCommandError::failed("identity_unproven", "intent"))?;
    Ok(ExperimentalCodexDisablePlan::StopManagedCodex(Box::new(
        CodexDisableStopPlan {
            before_config: cfg,
            receipt,
            science_owner: owner,
            science,
            gateway,
        },
    )))
}

#[allow(clippy::result_large_err)]
fn execute_planned_codex_science_stop<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    owner: CodexScienceOwnerSnapshot,
    science: CodexDisableScienceStop,
) -> crate::runtime::science::ScienceStopOutcome {
    let expected = science.runtime.clone();
    let request = ScienceStopRequest::exact(
        &science.runtime,
        ScienceStopOwnershipReceipt::from_managed_launch(&science.ownership),
    );
    super::runtime::execute_process_local_science_stop_with(
        app,
        state,
        lifecycle,
        move |current, generation| {
            if !owner.still_owns(current, generation) {
                return Err(codex_science_owner_changed());
            }
            current.science_runtime = Some(expected.clone());
            Ok(())
        },
        move |_| Ok(request),
        |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
        |_| Ok(()),
    )
    .and_then(|verified| verified.require_exact_stop_of(&science.runtime))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexDisableComponentObservation {
    Original,
    Absent,
    Restored,
    Replacement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexDisableReplayDecision {
    Clear,
    Restore {
        science_stopped: bool,
        gateway_stopped: bool,
    },
    Attention(CodexDisableAttentionCause),
}

fn decide_codex_disable_before_image_replay(
    phase: CodexDisableReceiptPhase,
    science: Option<CodexDisableComponentObservation>,
    gateway: Option<CodexDisableComponentObservation>,
) -> CodexDisableReplayDecision {
    if let CodexDisableReceiptPhase::Attention { cause } = phase {
        return CodexDisableReplayDecision::Attention(cause);
    }
    if phase == CodexDisableReceiptPhase::ConfigCommitted {
        return CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::ConfigDrift);
    }
    if science == Some(CodexDisableComponentObservation::Replacement)
        || gateway == Some(CodexDisableComponentObservation::Replacement)
    {
        return CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::IdentityDrift);
    }
    let (science_stopped, gateway_stopped) = match phase {
        CodexDisableReceiptPhase::Intent => {
            let all_original = science
                .is_none_or(|value| value == CodexDisableComponentObservation::Original)
                && gateway.is_none_or(|value| value == CodexDisableComponentObservation::Original);
            if !all_original {
                return CodexDisableReplayDecision::Attention(
                    CodexDisableAttentionCause::IdentityDrift,
                );
            }
            (false, false)
        }
        CodexDisableReceiptPhase::Stopping {
            component,
            science_stopped,
            gateway_stopped,
        } => {
            let target = match component {
                CodexDisableComponent::Science => science,
                CodexDisableComponent::Gateway => gateway,
            };
            // `Stopping` is only a pre-effect WAL record.  Current absence
            // cannot prove that this operation executed the stop: a terminal
            // stop, an external kill, or a start->stop ABA has the same fresh
            // observation.  Only a later post-stop progress CAS
            // (`EffectsApplied`, or `Restoring` entered from the exact live
            // outcome) may grant inverse authority; this phase therefore fails
            // closed whenever the target is no longer the exact original.
            if target != Some(CodexDisableComponentObservation::Original) {
                return CodexDisableReplayDecision::Attention(
                    CodexDisableAttentionCause::StopUncertain,
                );
            }
            (science_stopped, gateway_stopped)
        }
        CodexDisableReceiptPhase::EffectsApplied {
            science_stopped,
            gateway_stopped,
        }
        | CodexDisableReceiptPhase::Restoring {
            science_stopped,
            gateway_stopped,
        }
        | CodexDisableReceiptPhase::Restored {
            science_stopped,
            gateway_stopped,
        } => (science_stopped, gateway_stopped),
        CodexDisableReceiptPhase::ConfigCommitted | CodexDisableReceiptPhase::Attention { .. } => {
            unreachable!()
        }
    };
    let consistent =
        |observation: Option<CodexDisableComponentObservation>, stopped: bool| match observation {
            None => !stopped,
            Some(CodexDisableComponentObservation::Original) => !stopped,
            Some(
                CodexDisableComponentObservation::Absent
                | CodexDisableComponentObservation::Restored,
            ) => stopped,
            Some(CodexDisableComponentObservation::Replacement) => false,
        };
    if !consistent(science, science_stopped) || !consistent(gateway, gateway_stopped) {
        return CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::IdentityDrift);
    }
    if !science_stopped && !gateway_stopped {
        CodexDisableReplayDecision::Clear
    } else {
        CodexDisableReplayDecision::Restore {
            science_stopped,
            gateway_stopped,
        }
    }
}

fn observe_codex_disable_science(
    plan: &CodexDisableSciencePlan,
) -> CodexDisableComponentObservation {
    let Ok(runtime) = crate::runtime::science::runtime_identity_from_prior_recipe(&plan.prior)
    else {
        return CodexDisableComponentObservation::Replacement;
    };
    let mut restored = runtime.clone();
    if crate::runtime::science::hydrate_runtime_from_v2_managed_launch(
        plan.prior.port,
        &mut restored,
        &plan.restore_launch_id,
    )
    .is_ok()
        && ScienceHostAdapter::probe_known(plan.prior.port, &restored)
            == SandboxScienceState::RunningHealthy
    {
        return CodexDisableComponentObservation::Restored;
    }
    if ScienceHostAdapter::probe_known(plan.prior.port, &runtime)
        == SandboxScienceState::RunningHealthy
    {
        return ScienceHostAdapter::managed_receipt(plan.prior.port, &runtime)
            .and_then(|token| token.durable_receipt_digest().ok())
            .filter(|digest| digest == &plan.prior.launch_receipt_digest)
            .map(|_| CodexDisableComponentObservation::Original)
            .unwrap_or(CodexDisableComponentObservation::Replacement);
    }
    if !proc::loopback_port_in_use(
        plan.prior.port,
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    ) && crate::runtime::science::prior_restart_receipt_is_absent(&plan.prior, &runtime)
    {
        CodexDisableComponentObservation::Absent
    } else {
        CodexDisableComponentObservation::Replacement
    }
}

fn restored_gateway_identity(
    plan: &CodexDisableGatewayPlan,
) -> config::GatewayRuntimeJournalIdentity {
    let mut identity = plan.identity.clone();
    identity.launch_id.clone_from(&plan.restore_launch_id);
    identity
}

fn observe_codex_disable_gateway(
    cfg: &config::Config,
    plan: &CodexDisableGatewayPlan,
) -> CodexDisableComponentObservation {
    match GatewayController::observe_durable_untracked(cfg.proxy_port, &cfg.secret, &plan.identity)
    {
        DurableGatewayObservation::Exact => CodexDisableComponentObservation::Original,
        DurableGatewayObservation::Absent => CodexDisableComponentObservation::Absent,
        DurableGatewayObservation::Replacement => {
            if GatewayController::observe_durable_untracked(
                cfg.proxy_port,
                &cfg.secret,
                &restored_gateway_identity(plan),
            ) == DurableGatewayObservation::Exact
            {
                CodexDisableComponentObservation::Restored
            } else {
                CodexDisableComponentObservation::Replacement
            }
        }
    }
}

fn retain_codex_disable_attention(
    dir: &Path,
    open: &mut OpenCodexDisableReceipt,
    cause: CodexDisableAttentionCause,
    config_committed: bool,
) -> CodexDisableCommandError {
    let phase = open.record.phase.code();
    if open
        .transition(dir, CodexDisableReceiptPhase::Attention { cause })
        .is_err()
    {
        // The only valid receipt-free error window is after exact receipt
        // unlink and before exact fence clear.  Preserve the self-describing
        // fence and the original typed cause instead of degrading Config drift
        // to generic receipt I/O; boot will make the same fail-closed decision.
        let fence_still_durable =
            codex_disable_config_fence(&open.record)
                .ok()
                .is_some_and(|expected| {
                    read_codex_disable_receipt_at(dir).ok().flatten().is_none()
                        && config::load_from(dir)
                            .ok()
                            .and_then(|cfg| cfg.codex_disable_operation_fence().ok().flatten())
                            .as_ref()
                            == Some(&expected)
                });
        if fence_still_durable {
            return CodexDisableCommandError::attention(cause.code(), phase, config_committed);
        }
        CodexDisableCommandError::attention("receipt_io", phase, config_committed)
    } else {
        CodexDisableCommandError::attention(cause.code(), phase, config_committed)
    }
}

/// Project attention without erasing replayable inverse progress.  Once a
/// post-stop phase grants inverse authority, a transient auth/launch/clear
/// failure must leave `EffectsApplied`, `Restoring`, or `Restored` durable so a
/// genuinely fresh process can adopt exact restored components and continue.
fn project_codex_disable_recovery_attention(
    open: &OpenCodexDisableReceipt,
    cause: CodexDisableAttentionCause,
    config_committed: bool,
) -> CodexDisableCommandError {
    CodexDisableCommandError::attention(cause.code(), open.record.phase.code(), config_committed)
}

fn codex_disable_phase_has_inverse_progress(phase: CodexDisableReceiptPhase) -> bool {
    match phase {
        CodexDisableReceiptPhase::Stopping {
            science_stopped,
            gateway_stopped,
            ..
        } => science_stopped || gateway_stopped,
        CodexDisableReceiptPhase::EffectsApplied { .. }
        | CodexDisableReceiptPhase::Restoring { .. }
        | CodexDisableReceiptPhase::Restored { .. } => true,
        CodexDisableReceiptPhase::Intent
        | CodexDisableReceiptPhase::ConfigCommitted
        | CodexDisableReceiptPhase::Attention { .. } => false,
    }
}

fn retain_or_project_codex_disable_attention(
    dir: &Path,
    open: &mut OpenCodexDisableReceipt,
    cause: CodexDisableAttentionCause,
    config_committed: bool,
) -> CodexDisableCommandError {
    if codex_disable_phase_has_inverse_progress(open.record.phase) {
        project_codex_disable_recovery_attention(open, cause, config_committed)
    } else {
        retain_codex_disable_attention(dir, open, cause, config_committed)
    }
}

fn codex_disable_config_is_exact_before(
    dir: &Path,
    receipt: &CodexDisableOperationReceipt,
) -> Result<config::Config, CodexDisableAttentionCause> {
    let cfg = config::load_from(dir).map_err(|_| CodexDisableAttentionCause::ReceiptIo)?;
    let expected_fence =
        codex_disable_config_fence(receipt).map_err(|_| CodexDisableAttentionCause::ReceiptIo)?;
    if cfg
        .codex_disable_operation_fence()
        .map_err(|_| CodexDisableAttentionCause::ReceiptIo)?
        .as_ref()
        != Some(&expected_fence)
    {
        return Err(CodexDisableAttentionCause::ConfigDrift);
    }
    let fingerprint = codex_disable_config_fingerprint(&cfg)
        .map_err(|_| CodexDisableAttentionCause::ReceiptIo)?;
    if fingerprint != receipt.before_config_fingerprint {
        return Err(CodexDisableAttentionCause::ConfigDrift);
    }
    Ok(cfg)
}

fn acquire_codex_disable_effect_lease(
    dir: &Path,
    receipt: &CodexDisableOperationReceipt,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<config::CodexDisableEffectLease, CodexDisableAttentionCause> {
    if lifecycle.current_generation() != receipt.plan.owner_generation {
        return Err(CodexDisableAttentionCause::IdentityDrift);
    }
    let fence =
        codex_disable_config_fence(receipt).map_err(|_| CodexDisableAttentionCause::ReceiptIo)?;
    let lease = config::acquire_codex_disable_effect_lease(dir, &fence)
        .map_err(|_| CodexDisableAttentionCause::ConfigDrift)?;
    let fingerprint = codex_disable_config_fingerprint(lease.config())
        .map_err(|_| CodexDisableAttentionCause::ReceiptIo)?;
    if fingerprint != receipt.before_config_fingerprint
        || !lease.config().experimental_codex_enabled
    {
        return Err(CodexDisableAttentionCause::ConfigDrift);
    }
    Ok(lease)
}

#[derive(Clone, Copy)]
enum CodexDisableRestoreOrigin {
    Live,
    Fresh,
}

fn restore_stopped_codex_components<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    dir: &Path,
    open: &mut OpenCodexDisableReceipt,
    science_stopped: bool,
    gateway_stopped: bool,
    origin: CodexDisableRestoreOrigin,
) -> Result<(), CodexDisableCommandError> {
    if matches!(origin, CodexDisableRestoreOrigin::Live)
        && !matches!(
            lifecycle.current_generation(),
            generation if generation == open.record.plan.owner_generation
                || generation == open.record.plan.owner_generation.saturating_add(1)
        )
    {
        return Err(project_codex_disable_recovery_attention(
            open,
            CodexDisableAttentionCause::IdentityDrift,
            false,
        ));
    }
    let cfg = match codex_disable_config_is_exact_before(dir, &open.record) {
        Ok(cfg) => cfg,
        Err(cause) => return Err(project_codex_disable_recovery_attention(open, cause, false)),
    };
    if science_stopped {
        let Some(plan) = open.record.plan.science.as_ref() else {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::IdentityDrift,
                false,
            ));
        };
        if !matches!(
            observe_codex_disable_science(plan),
            CodexDisableComponentObservation::Absent | CodexDisableComponentObservation::Restored
        ) {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::IdentityDrift,
                false,
            ));
        }
    }
    if gateway_stopped {
        let Some(plan) = open.record.plan.gateway.as_ref() else {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::IdentityDrift,
                false,
            ));
        };
        if !matches!(
            observe_codex_disable_gateway(&cfg, plan),
            CodexDisableComponentObservation::Absent | CodexDisableComponentObservation::Restored
        ) {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::IdentityDrift,
                false,
            ));
        }
    }
    if open
        .transition(
            dir,
            CodexDisableReceiptPhase::Restoring {
                science_stopped,
                gateway_stopped,
            },
        )
        .is_err()
    {
        let cause =
            open.phase_publication_cause(dir, config::CodexDisableTerminalConfigImage::Before);
        return Err(project_codex_disable_recovery_attention(open, cause, false));
    }

    let prepared = if gateway_stopped {
        let Some(plan) = open.record.plan.gateway.as_ref() else {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::IdentityDrift,
                false,
            ));
        };
        match prepare_provider_auth(
            app,
            "codex",
            CodexPreflightTarget::Profile(plan.profile_id.clone()),
        ) {
            Ok(proof) => proof,
            Err(_) => {
                return Err(project_codex_disable_recovery_attention(
                    open,
                    CodexDisableAttentionCause::RestoreFailed,
                    false,
                ))
            }
        }
    } else {
        None
    };

    if science_stopped {
        let Some(plan) = open.record.plan.science.as_ref() else {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::IdentityDrift,
                false,
            ));
        };
        match observe_codex_disable_science(plan) {
            CodexDisableComponentObservation::Absent
            | CodexDisableComponentObservation::Restored => {}
            CodexDisableComponentObservation::Original
            | CodexDisableComponentObservation::Replacement => {
                return Err(project_codex_disable_recovery_attention(
                    open,
                    CodexDisableAttentionCause::IdentityDrift,
                    false,
                ))
            }
        }
        let restore = crate::runtime::sandbox_session::restore_science_from_durable_recipe(
            app,
            state,
            lifecycle,
            prepared.as_ref().map(PreparedCodexAuth::proof),
            &plan.prior,
            &plan.restore_launch_id,
        );
        if let Err(error) = restore {
            let cause = match error {
                crate::runtime::sandbox_session::DurableScienceRestoreError::Failed => {
                    CodexDisableAttentionCause::RestoreFailed
                }
                crate::runtime::sandbox_session::DurableScienceRestoreError::Uncertain => {
                    CodexDisableAttentionCause::RestoreUncertain
                }
            };
            return Err(project_codex_disable_recovery_attention(open, cause, false));
        }
    }

    if gateway_stopped {
        let plan = open
            .record
            .plan
            .gateway
            .as_ref()
            .expect("validated Gateway restore plan");
        let restored_identity = restored_gateway_identity(plan);
        match observe_codex_disable_gateway(&cfg, plan) {
            CodexDisableComponentObservation::Absent => {}
            CodexDisableComponentObservation::Restored => {
                if GatewayController::clear_durable_untracked(
                    app,
                    cfg.proxy_port,
                    &cfg.secret,
                    &restored_identity,
                )
                .is_err()
                {
                    return Err(project_codex_disable_recovery_attention(
                        open,
                        CodexDisableAttentionCause::RestoreUncertain,
                        false,
                    ));
                }
            }
            CodexDisableComponentObservation::Original
            | CodexDisableComponentObservation::Replacement => {
                return Err(project_codex_disable_recovery_attention(
                    open,
                    CodexDisableAttentionCause::IdentityDrift,
                    false,
                ))
            }
        }
        let Some(profile) = cfg.profile_by_id(&plan.profile_id).cloned() else {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::ConfigDrift,
                false,
            ));
        };
        let science_runtime = lock(state).science_runtime.clone();
        if GatewayController::restore_for(
            app,
            state,
            lifecycle,
            &profile,
            science_runtime.as_ref(),
            prepared.as_ref().map(PreparedCodexAuth::proof),
            &plan.restore_launch_id,
        )
        .is_err()
            || GatewayController::observe_durable_untracked(
                cfg.proxy_port,
                &cfg.secret,
                &restored_identity,
            ) != DurableGatewayObservation::Exact
        {
            return Err(project_codex_disable_recovery_attention(
                open,
                CodexDisableAttentionCause::RestoreFailed,
                false,
            ));
        }
    }

    if science_stopped
        && open.record.plan.science.as_ref().is_none_or(|plan| {
            observe_codex_disable_science(plan) != CodexDisableComponentObservation::Restored
        })
    {
        return Err(project_codex_disable_recovery_attention(
            open,
            CodexDisableAttentionCause::RestoreUncertain,
            false,
        ));
    }
    if open
        .transition(
            dir,
            CodexDisableReceiptPhase::Restored {
                science_stopped,
                gateway_stopped,
            },
        )
        .is_err()
    {
        let cause =
            open.phase_publication_cause(dir, config::CodexDisableTerminalConfigImage::Before);
        return Err(project_codex_disable_recovery_attention(open, cause, false));
    }
    open.clear_before(dir)
        .map_err(|cause| project_codex_disable_recovery_attention(open, cause, false))
}

fn unrecorded_components_remain_original(
    dir: &Path,
    receipt: &CodexDisableOperationReceipt,
    science_stopped: bool,
    gateway_stopped: bool,
) -> bool {
    let Ok(cfg) = codex_disable_config_is_exact_before(dir, receipt) else {
        return false;
    };
    (!receipt.plan.science.as_ref().is_some_and(|plan| {
        !science_stopped
            && observe_codex_disable_science(plan) != CodexDisableComponentObservation::Original
    })) && (!receipt.plan.gateway.as_ref().is_some_and(|plan| {
        !gateway_stopped
            && observe_codex_disable_gateway(&cfg, plan)
                != CodexDisableComponentObservation::Original
    }))
}

fn handle_codex_disable_pre_effect_phase_failure(
    dir: &Path,
    open: &mut OpenCodexDisableReceipt,
) -> CodexDisableCommandError {
    if let Err(cause) = codex_disable_config_is_exact_before(dir, &open.record) {
        return retain_codex_disable_attention(dir, open, cause, false);
    }
    if unrecorded_components_remain_original(dir, &open.record, false, false) {
        match open.clear_before(dir) {
            Ok(()) => return CodexDisableCommandError::failed("receipt_io", "intent"),
            Err(cause) => return retain_codex_disable_attention(dir, open, cause, false),
        }
    }
    retain_codex_disable_attention(dir, open, CodexDisableAttentionCause::StopUncertain, false)
}

fn commit_codex_disable_config(
    dir: &Path,
    receipt: &CodexDisableOperationReceipt,
) -> Result<(), String> {
    let expected = receipt.before_config_fingerprint.clone();
    let fence = codex_disable_config_fence(receipt)?;
    config::update_codex_disable_operation(dir, &fence, move |cfg| {
        if codex_disable_config_fingerprint(cfg)? != expected || !cfg.experimental_codex_enabled {
            return Err("Codex disable config before-image 已变化；拒绝提交".into());
        }
        cfg.experimental_codex_enabled = false;
        Ok(((), true))
    })
}

fn execute_planned_codex_gateway_stop(
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    claim: GatewayStopClaim,
) -> Result<config::GatewayRuntimeJournalIdentity, String> {
    #[cfg(test)]
    if CODEX_DISABLE_GATEWAY_STOP_FAILURE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|thread| *thread == std::thread::current().id())
    {
        return Err("test-only Codex disable Gateway stop failure".into());
    }
    GatewayController::execute_claimed_stop(state, lifecycle, claim)
}

struct CodexScienceObservation {
    state: SandboxScienceState,
    detected_runtime: Option<crate::runtime::science::ScienceRuntimeIdentity>,
    owner: CodexScienceOwnerSnapshot,
}

fn codex_science_owner_changed() -> crate::runtime::science::ScienceStopFailure {
    crate::runtime::science::ScienceStopFailure::outcome_publication_failure(
        "Science stop claim 时 process-local owner 已变化；已保留 replacement runtime。",
    )
}

#[allow(clippy::result_large_err)]
fn stop_managed_codex_runtime_with<R, Claim, Execute, StopGateway>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    observation: CodexScienceObservation,
    claim_science: Claim,
    execute_science: Execute,
    mut stop_gateway: StopGateway,
) -> Result<(), String>
where
    R: tauri::Runtime,
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
    Execute: FnOnce(
        &tauri::AppHandle<R>,
        crate::runtime::science::ScienceStopRequest,
    ) -> (crate::runtime::science::ScienceStopOutcome, bool),
    StopGateway: FnMut(&mut AppState) -> crate::GatewayStopOutcome,
{
    let CodexScienceObservation {
        state: science_state,
        detected_runtime,
        owner,
    } = observation;
    match science_state {
        SandboxScienceState::RunningHealthy => {
            super::runtime::execute_process_local_science_stop_with(
                app,
                state,
                lifecycle,
                move |st, current_generation| {
                    if !owner.still_owns(st, current_generation) {
                        return Err(codex_science_owner_changed());
                    }
                    st.science_runtime = detected_runtime;
                    Ok(())
                },
                claim_science,
                execute_science,
                |st| {
                    lifecycle.bump_generation();
                    stop_gateway(st)
                        .require_stopped("停止受管 Codex Gateway 失败")
                        .map_err(
                            crate::runtime::science::ScienceStopFailure::outcome_publication_failure,
                        )
                },
            )
            .map_err(|error| {
                format!("停止受管 Codex Science 链路失败；认证未变更，实验开关也未关闭：{error}")
            })?;
            Ok(())
        }
        SandboxScienceState::Stopped => {
            let mut st = lock(state);
            if !owner.still_owns(&st, lifecycle.current_generation()) {
                return Err(format!(
                    "停止受管 Codex Science 链路失败；认证未变更，实验开关也未关闭：{}",
                    codex_science_owner_changed()
                ));
            }
            kill_child(&mut st.sandbox);
            st.sandbox_url = None;
            st.science_confirmed_stopped = owner.confirmed_stopped;
            st.science_runtime = None;
            lifecycle.bump_generation();
            stop_gateway(&mut st)
                .require_stopped("停止受管 Codex Gateway 失败；认证未变更，实验开关也未关闭")?;
            Ok(())
        }
        SandboxScienceState::Unknown => Err(
            "无法确认沙箱端口上的 Science binary/data-dir 身份；Codex 认证与实验开关均未变更。"
                .into(),
        ),
    }
}

/// Prepare for a CSSwitch-owned Codex credential mutation. Only a runtime whose
/// in-memory launch identity is exactly `codex` is stopped. Other known providers
/// remain untouched; an alive but unidentified managed child fails closed.
fn prepare_codex_auth_mutation<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<AuthRuntimeAction, String> {
    prepare_codex_auth_mutation_with_fence(app, state, lifecycle, None)
}

fn prepare_codex_auth_mutation_with_fence<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    expected_fence: Option<&config::ConfigMutationOperationFence>,
) -> Result<AuthRuntimeAction, String> {
    require_no_codex_disable_receipt(&config::default_dir())
        .map_err(|error| error.safe_message().to_string())?;
    let cfg = config::load_from(&config::default_dir()).map_err(|error| {
        format!("读取配置失败；为避免遗漏残留 Codex Science，认证未变更：{error}")
    })?;
    if let Some(expected_fence) = expected_fence {
        config::require_no_runtime_transaction_for_mutation(&cfg, expected_fence)?;
    } else {
        config::require_no_runtime_transaction(&cfg)?;
    }
    let active_profile_is_codex = cfg
        .active_profile()
        .is_some_and(|profile| profile.template_id == "codex");
    let (provider, tracked, owner, version_cache) = {
        let mut st = lock(state);
        let provider = st.provider.clone();
        let tracked = tracked_proxy_state(&mut st);
        let owner = CodexScienceOwnerSnapshot::claim(&st, lifecycle.current_generation());
        (provider, tracked, owner, st.science_version_cache.clone())
    };
    let remembered_runtime = owner
        .runtime
        .clone()
        .or_else(|| owner.confirmed_stopped.clone());
    let untracked_proxy_port_occupied = matches!(
        tracked,
        TrackedProxyState::Absent | TrackedProxyState::Exited
    ) && proc::loopback_port_in_use(cfg.proxy_port, 100);
    let proxy_action =
        decide_auth_runtime_action(&provider, tracked, untracked_proxy_port_occupied)?;
    if proxy_action == AuthRuntimeAction::PreserveOtherProvider
        || (proxy_action == AuthRuntimeAction::Noop && !active_profile_is_codex)
    {
        return Ok(proxy_action);
    }

    let (science_state, detected_runtime) = match remembered_runtime.clone() {
        Some(runtime) => {
            let science_state = ScienceHostAdapter::probe_known(cfg.sandbox_port, &runtime);
            let detected =
                (science_state == SandboxScienceState::RunningHealthy).then_some(runtime);
            (science_state, detected)
        }
        None => ScienceHostAdapter::probe_cached(cfg.sandbox_port, &version_cache)?,
    };
    let action =
        resolve_science_runtime_action(proxy_action, active_profile_is_codex, science_state)?;
    if action == AuthRuntimeAction::StopManagedCodex {
        stop_managed_codex_runtime_with(
            app,
            state,
            lifecycle,
            CodexScienceObservation {
                state: science_state,
                detected_runtime,
                owner,
            },
            ScienceHostAdapter::claim_stop,
            |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
            AppState::stop_proxy,
        )?;
    }
    Ok(action)
}

fn set_experimental_codex_enabled_at(
    dir: &Path,
    enabled: bool,
    before_disable: impl FnOnce() -> Result<(), String>,
) -> Result<Value, String> {
    require_no_codex_disable_receipt(dir).map_err(|error| error.safe_message().to_string())?;
    let preflight = config::load_from(dir).map_err(|error| error.to_string())?;
    config::require_no_runtime_transaction(&preflight)?;
    if !enabled {
        before_disable()?;
    }
    config::update_result(dir, move |cfg| {
        config::require_no_runtime_transaction(cfg)?;
        let changed = cfg.experimental_codex_enabled != enabled;
        cfg.experimental_codex_enabled = enabled;
        Ok(((), changed))
    })
    .map_err(|error| error.to_string())?;
    Ok(json!({ "experimental_codex_enabled": enabled }))
}

fn commit_experimental_codex_disable_without_runtime(
    dir: &Path,
) -> Result<Value, CodexDisableCommandError> {
    require_no_codex_disable_receipt(dir)?;
    let preflight = config::load_from(dir)
        .map_err(|_| CodexDisableCommandError::failed("config_unavailable", "intent"))?;
    config::require_no_runtime_transaction(&preflight)
        .map_err(|_| CodexDisableCommandError::failed("mutation_conflict", "intent"))?;
    let update = config::update_result(dir, |cfg| {
        config::require_no_runtime_transaction(cfg)?;
        let changed = cfg.experimental_codex_enabled;
        cfg.experimental_codex_enabled = false;
        Ok(((), changed))
    });
    if update.is_err() {
        if let Err(attention) = require_no_codex_disable_receipt(dir) {
            return Err(attention);
        }
        if config::load_from(dir)
            .ok()
            .is_some_and(|cfg| config::require_no_runtime_transaction(&cfg).is_err())
        {
            return Err(CodexDisableCommandError::failed(
                "mutation_conflict",
                "intent",
            ));
        }
        return Err(CodexDisableCommandError::failed(
            "config_commit_failed",
            "intent",
        ));
    }
    Ok(json!({ "experimental_codex_enabled": false }))
}

fn execute_experimental_codex_enabled<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    supervisor: &SharedCodexAuthSupervisor,
    enabled: bool,
) -> Result<Value, RuntimeCommandError> {
    let dir = config::default_dir();
    if enabled {
        return set_experimental_codex_enabled_at(&dir, true, || Ok(()))
            .map_err(RuntimeCommandError::from);
    }

    let mut mutation = Some(
        CodexAuthSupervisor::begin_mutation(supervisor)
            .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?,
    );
    let plan = plan_experimental_codex_disable(&dir, state, lifecycle)?;
    let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
        return commit_experimental_codex_disable_without_runtime(&dir)
            .map_err(RuntimeCommandError::from);
    };
    let CodexDisableStopPlan {
        before_config,
        receipt,
        science_owner,
        science,
        gateway,
    } = *plan;
    let mut open = OpenCodexDisableReceipt::publish_intent(&dir, &before_config, receipt)
        .map_err(|error| RuntimeCommandError::from(error.into_command_error()))?;
    let mut science_stopped = false;
    let mut gateway_stopped = false;

    if let Some(science) = science {
        if open
            .transition(
                &dir,
                CodexDisableReceiptPhase::Stopping {
                    component: CodexDisableComponent::Science,
                    science_stopped,
                    gateway_stopped,
                },
            )
            .is_err()
        {
            let cause =
                open.phase_publication_cause(&dir, config::CodexDisableTerminalConfigImage::Before);
            return Err(RuntimeCommandError::from(retain_codex_disable_attention(
                &dir, &mut open, cause, false,
            )));
        }
        let effect_lease = match acquire_codex_disable_effect_lease(&dir, &open.record, lifecycle) {
            Ok(lease) => lease,
            Err(cause) => {
                return Err(RuntimeCommandError::from(retain_codex_disable_attention(
                    &dir, &mut open, cause, false,
                )))
            }
        };
        let stop_result =
            execute_planned_codex_science_stop(app, state, lifecycle, science_owner, science);
        drop(effect_lease);
        if stop_result.is_err() {
            if !unrecorded_components_remain_original(
                &dir,
                &open.record,
                science_stopped,
                gateway_stopped,
            ) {
                return Err(RuntimeCommandError::from(
                    retain_or_project_codex_disable_attention(
                        &dir,
                        &mut open,
                        CodexDisableAttentionCause::StopUncertain,
                        false,
                    ),
                ));
            }
            open.clear_before(&dir).map_err(|cause| {
                RuntimeCommandError::from(retain_codex_disable_attention(
                    &dir, &mut open, cause, false,
                ))
            })?;
            return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
                "science_stop_failed",
                "stopping",
            )));
        }
        science_stopped = true;
        if open
            .transition(
                &dir,
                CodexDisableReceiptPhase::EffectsApplied {
                    science_stopped,
                    gateway_stopped,
                },
            )
            .is_err()
        {
            drop(mutation.take());
            restore_stopped_codex_components(
                app,
                state,
                lifecycle,
                &dir,
                &mut open,
                science_stopped,
                gateway_stopped,
                CodexDisableRestoreOrigin::Live,
            )?;
            return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
                "receipt_io",
                "restored",
            )));
        }
    }

    if let Some(gateway) = gateway {
        if open
            .transition(
                &dir,
                CodexDisableReceiptPhase::Stopping {
                    component: CodexDisableComponent::Gateway,
                    science_stopped,
                    gateway_stopped,
                },
            )
            .is_err()
        {
            drop(mutation.take());
            if science_stopped {
                restore_stopped_codex_components(
                    app,
                    state,
                    lifecycle,
                    &dir,
                    &mut open,
                    science_stopped,
                    gateway_stopped,
                    CodexDisableRestoreOrigin::Live,
                )?;
                return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
                    "receipt_io",
                    "restored",
                )));
            }
            return Err(RuntimeCommandError::from(
                handle_codex_disable_pre_effect_phase_failure(&dir, &mut open),
            ));
        }
        let effect_lease = match acquire_codex_disable_effect_lease(&dir, &open.record, lifecycle) {
            Ok(lease) => lease,
            Err(cause) => {
                drop(mutation.take());
                if science_stopped {
                    restore_stopped_codex_components(
                        app,
                        state,
                        lifecycle,
                        &dir,
                        &mut open,
                        science_stopped,
                        gateway_stopped,
                        CodexDisableRestoreOrigin::Live,
                    )?;
                    return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
                        cause.code(),
                        "restored",
                    )));
                }
                return Err(RuntimeCommandError::from(retain_codex_disable_attention(
                    &dir, &mut open, cause, false,
                )));
            }
        };
        let stop_result = execute_planned_codex_gateway_stop(state, lifecycle, gateway);
        drop(effect_lease);
        if stop_result.is_err() {
            if !unrecorded_components_remain_original(
                &dir,
                &open.record,
                science_stopped,
                gateway_stopped,
            ) {
                return Err(RuntimeCommandError::from(
                    retain_or_project_codex_disable_attention(
                        &dir,
                        &mut open,
                        CodexDisableAttentionCause::StopUncertain,
                        false,
                    ),
                ));
            }
            if science_stopped {
                drop(mutation.take());
                restore_stopped_codex_components(
                    app,
                    state,
                    lifecycle,
                    &dir,
                    &mut open,
                    science_stopped,
                    gateway_stopped,
                    CodexDisableRestoreOrigin::Live,
                )?;
            } else {
                open.clear_before(&dir).map_err(|cause| {
                    RuntimeCommandError::from(retain_codex_disable_attention(
                        &dir, &mut open, cause, false,
                    ))
                })?;
            }
            return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
                "gateway_stop_failed",
                if science_stopped {
                    "restored"
                } else {
                    "intent"
                },
            )));
        }
        gateway_stopped = true;
        if open
            .transition(
                &dir,
                CodexDisableReceiptPhase::EffectsApplied {
                    science_stopped,
                    gateway_stopped,
                },
            )
            .is_err()
        {
            drop(mutation.take());
            restore_stopped_codex_components(
                app,
                state,
                lifecycle,
                &dir,
                &mut open,
                science_stopped,
                gateway_stopped,
                CodexDisableRestoreOrigin::Live,
            )?;
            return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
                "receipt_io",
                "restored",
            )));
        }
    }
    lifecycle.bump_generation();

    if commit_codex_disable_config(&dir, &open.record).is_err() {
        drop(mutation.take());
        restore_stopped_codex_components(
            app,
            state,
            lifecycle,
            &dir,
            &mut open,
            science_stopped,
            gateway_stopped,
            CodexDisableRestoreOrigin::Live,
        )?;
        return Err(RuntimeCommandError::from(CodexDisableCommandError::failed(
            "config_commit_failed",
            "restored",
        )));
    }
    drop(mutation.take());
    if open
        .transition(&dir, CodexDisableReceiptPhase::ConfigCommitted)
        .is_err()
    {
        let cause = match open
            .terminal_cleanup_cause(&dir, config::CodexDisableTerminalConfigImage::After)
        {
            CodexDisableAttentionCause::ConfigDrift => CodexDisableAttentionCause::ConfigDrift,
            _ => CodexDisableAttentionCause::ReceiptIo,
        };
        return Err(RuntimeCommandError::from(retain_codex_disable_attention(
            &dir, &mut open, cause, true,
        )));
    }
    open.clear_after(&dir).map_err(|cause| {
        RuntimeCommandError::from(retain_codex_disable_attention(&dir, &mut open, cause, true))
    })?;
    Ok(json!({ "experimental_codex_enabled": false }))
}

fn replay_codex_disable_before_image<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    supervisor: &SharedCodexAuthSupervisor,
    dir: &Path,
    open: &mut OpenCodexDisableReceipt,
    cfg: &config::Config,
) -> Result<(), CodexDisableCommandError> {
    let science_observation = open
        .record
        .plan
        .science
        .as_ref()
        .map(observe_codex_disable_science);
    let gateway_observation = open
        .record
        .plan
        .gateway
        .as_ref()
        .map(|plan| observe_codex_disable_gateway(cfg, plan));
    let (science_stopped, gateway_stopped) = match decide_codex_disable_before_image_replay(
        open.record.phase,
        science_observation,
        gateway_observation,
    ) {
        CodexDisableReplayDecision::Clear => {
            return open
                .clear_before(dir)
                .map_err(|cause| retain_codex_disable_attention(dir, open, cause, false))
        }
        CodexDisableReplayDecision::Restore {
            science_stopped,
            gateway_stopped,
        } => (science_stopped, gateway_stopped),
        CodexDisableReplayDecision::Attention(cause) => {
            if codex_disable_phase_has_inverse_progress(open.record.phase) {
                return Err(project_codex_disable_recovery_attention(open, cause, false));
            }
            if matches!(
                open.record.phase,
                CodexDisableReceiptPhase::Attention { .. }
            ) {
                return Err(CodexDisableCommandError::attention(
                    cause.code(),
                    "attention",
                    false,
                ));
            }
            return Err(retain_codex_disable_attention(dir, open, cause, false));
        }
    };

    let mutation = CodexAuthSupervisor::begin_mutation(supervisor).map_err(|_| {
        project_codex_disable_recovery_attention(
            open,
            CodexDisableAttentionCause::RestoreFailed,
            false,
        )
    })?;
    if gateway_stopped {
        drop(mutation);
        restore_stopped_codex_components(
            app,
            state,
            lifecycle,
            dir,
            open,
            science_stopped,
            gateway_stopped,
            CodexDisableRestoreOrigin::Fresh,
        )
    } else {
        let outcome = restore_stopped_codex_components(
            app,
            state,
            lifecycle,
            dir,
            open,
            science_stopped,
            gateway_stopped,
            CodexDisableRestoreOrigin::Fresh,
        );
        drop(mutation);
        outcome
    }
}

/// Converge the dedicated Codex-disable receipt before ordinary boot chooses a
/// launch path.  `None` means there was no interrupted operation (or replay
/// safely finished); `Some` is a stable, credential-free boot attention DTO.
pub(crate) fn replay_interrupted_codex_disable<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Option<Value> {
    let dir = config::default_dir();
    match config::recover_orphan_codex_disable_operation_fence(&dir) {
        Ok(config::CodexDisableOrphanFenceRecovery::None)
        | Ok(config::CodexDisableOrphanFenceRecovery::Cleared) => {}
        Ok(config::CodexDisableOrphanFenceRecovery::ConfigDrift) => {
            let disabled = config::load_from(&dir)
                .map(|cfg| !cfg.experimental_codex_enabled)
                .unwrap_or(false);
            return Some(
                CodexDisableCommandError::attention("config_drift", "attention", disabled)
                    .project_boot_attention(),
            );
        }
        Err(_) => {
            return Some(
                CodexDisableCommandError::attention("receipt_io", "attention", false)
                    .project_boot_attention(),
            )
        }
    }
    let mut open = match read_codex_disable_receipt_at(&dir) {
        Ok(None) => return None,
        Ok(Some(open)) => open,
        Err(_) => {
            return Some(
                CodexDisableCommandError::attention("receipt_invalid", "attention", false)
                    .project_boot_attention(),
            )
        }
    };
    let cfg = match config::load_from(&dir) {
        Ok(cfg) => cfg,
        Err(_) => {
            return Some(
                retain_or_project_codex_disable_attention(
                    &dir,
                    &mut open,
                    CodexDisableAttentionCause::ReceiptIo,
                    false,
                )
                .project_boot_attention(),
            )
        }
    };
    let expected_fence = match codex_disable_config_fence(&open.record) {
        Ok(fence) => fence,
        Err(_) => {
            return Some(
                retain_or_project_codex_disable_attention(
                    &dir,
                    &mut open,
                    CodexDisableAttentionCause::ReceiptIo,
                    false,
                )
                .project_boot_attention(),
            )
        }
    };
    if cfg.codex_disable_operation_fence().ok().flatten().as_ref() != Some(&expected_fence) {
        return Some(
            retain_or_project_codex_disable_attention(
                &dir,
                &mut open,
                CodexDisableAttentionCause::ConfigDrift,
                false,
            )
            .project_boot_attention(),
        );
    }
    let fingerprint = match codex_disable_config_fingerprint(&cfg) {
        Ok(fingerprint) => fingerprint,
        Err(_) => {
            return Some(
                retain_or_project_codex_disable_attention(
                    &dir,
                    &mut open,
                    CodexDisableAttentionCause::ReceiptIo,
                    false,
                )
                .project_boot_attention(),
            )
        }
    };
    if fingerprint == open.record.after_config_fingerprint {
        if !matches!(open.record.phase, CodexDisableReceiptPhase::ConfigCommitted)
            && open
                .transition(&dir, CodexDisableReceiptPhase::ConfigCommitted)
                .is_err()
        {
            let cause = match open
                .terminal_cleanup_cause(&dir, config::CodexDisableTerminalConfigImage::After)
            {
                CodexDisableAttentionCause::ConfigDrift => CodexDisableAttentionCause::ConfigDrift,
                _ => CodexDisableAttentionCause::ReceiptIo,
            };
            return Some(
                retain_or_project_codex_disable_attention(&dir, &mut open, cause, true)
                    .project_boot_attention(),
            );
        }
        return match open.clear_after(&dir) {
            Ok(()) => None,
            Err(cause) => Some(
                retain_or_project_codex_disable_attention(&dir, &mut open, cause, true)
                    .project_boot_attention(),
            ),
        };
    }
    if fingerprint != open.record.before_config_fingerprint {
        return Some(
            retain_or_project_codex_disable_attention(
                &dir,
                &mut open,
                CodexDisableAttentionCause::ConfigDrift,
                false,
            )
            .project_boot_attention(),
        );
    }

    let state = app.state::<SharedAppState>().inner().clone();
    let lifecycle = app.state::<SharedLifecycle>().inner().clone();
    let supervisor = app.state::<SharedCodexAuthSupervisor>().inner().clone();
    let result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
        replay_codex_disable_before_image(
            app,
            &state,
            lifecycle.as_ref(),
            &supervisor,
            &dir,
            &mut open,
            &cfg,
        )
    });
    result.err().map(|error| error.project_boot_attention())
}

fn codex_network_requires_destructive_receipt(
    cfg: &config::Config,
    state: &SharedAppState,
) -> bool {
    let active_profile_is_codex = cfg
        .active_profile()
        .is_some_and(|profile| profile.template_id == "codex");
    let current = lock(state);
    let science = current.science_runtime.is_some() || current.sandbox.is_some();
    let gateway = current.proxy.is_some();
    (current.provider == "codex" && (science || gateway)) || (active_profile_is_codex && science)
}

fn set_codex_network_with_p2b<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    settings: csswitch_codex_network::CodexNetworkSettings,
    resolved: &csswitch_codex_network::ResolvedCodexNetworkRoute,
) -> Result<Value, RuntimeCommandError> {
    let dir = config::default_dir();
    let before =
        config::load_from(&dir).map_err(|error| RuntimeCommandError::from(error.to_string()))?;
    config::require_no_runtime_transaction(&before).map_err(RuntimeCommandError::from)?;
    let mode = settings.mode;
    if !codex_network_requires_destructive_receipt(&before, state) {
        return set_codex_network_at(&dir, settings, resolved, || Ok(())).map_err(Into::into);
    }
    let mut after = before.clone();
    after.codex_network = settings.clone();
    let (science_effect, gateway_effect) = {
        let current = lock(state);
        (
            current.science_runtime.is_some() || current.sandbox.is_some(),
            current.proxy.is_some(),
        )
    };
    let mut effects = Vec::new();
    if science_effect {
        effects
            .push(crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopScience);
    }
    if gateway_effect {
        effects
            .push(crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopGateway);
    }
    effects.push(crate::commands::runtime::config_mutation::ConfigMutationEffectKind::ConfigCommit);
    let mut operation = crate::commands::runtime::config_mutation::begin(
        &dir,
        crate::commands::runtime::config_mutation::ConfigMutationOperation::SetCodexNetwork,
        &before,
        Some(&after),
        crate::commands::runtime::config_mutation::MutationTarget {
            network_fingerprint: Some(
                crate::commands::runtime::config_mutation::network_fingerprint(&settings)
                    .map_err(RuntimeCommandError::from)?,
            ),
            ..Default::default()
        },
        crate::commands::runtime::config_mutation::RuntimePlan {
            owner_generation: lifecycle.current_generation(),
            ..Default::default()
        },
        effects,
        None,
        None,
    )
    .map_err(|error| RuntimeCommandError::Mutation(error))?;
    let runtime_effects = operation.receipt().effects.len().saturating_sub(1);
    for index in 0..runtime_effects {
        operation
            .checkpoint_effect_or_attention(
                index,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "runtime_stop_checkpoint_failed",
                "before",
                "unknown",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(RuntimeCommandError::Mutation)?;
    }
    let action = match prepare_codex_auth_mutation_with_fence(
        app,
        state,
        lifecycle,
        Some(operation.fence()),
    ) {
        Ok(action) => action,
        Err(error) => {
            return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                "runtime_stop_failed",
                "before",
                "unknown",
                config::ConfigMutationTerminalConfigImage::Before,
                error,
            )))
        }
    };
    let mut index = 0usize;
    for (present, code) in [
        (science_effect, "stopped_science"),
        (gateway_effect, "stopped_gateway"),
    ] {
        if present {
            operation
                .checkpoint_effect_or_attention(
                    index,
                    if action == AuthRuntimeAction::StopManagedCodex {
                        crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded
                    } else {
                        crate::commands::runtime::config_mutation::ConfigMutationEffectState::Skipped
                    },
                    Some(if action == AuthRuntimeAction::StopManagedCodex {
                        code
                    } else {
                        "preserved_other_provider"
                    }),
                    "runtime_stop_checkpoint_failed",
                    "before",
                    auth_runtime_terminal_state(action),
                    config::ConfigMutationTerminalConfigImage::Before,
                )
                .map_err(RuntimeCommandError::Mutation)?;
            index += 1;
        }
    }
    let before_fingerprint = operation.fence().before_config_fingerprint.clone();
    operation
        .checkpoint_effect_or_attention(
            index,
            crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
            None,
            "config_commit_checkpoint_failed",
            "before",
            auth_runtime_terminal_state(action),
            config::ConfigMutationTerminalConfigImage::Before,
        )
        .map_err(RuntimeCommandError::Mutation)?;
    if let Err(error) = operation.update_config(move |cfg| {
        if config::config_mutation_config_fingerprint(cfg).map_err(|error| error.to_string())?
            != before_fingerprint
        {
            return Err("Codex network Config before-image 已变化；拒绝提交".into());
        }
        cfg.codex_network = settings;
        Ok(((), true))
    }) {
        let attention = operation.finish(
            "attention",
            "before",
            "unknown",
            Some("config_commit_failed"),
            config::ConfigMutationTerminalConfigImage::Before,
        );
        return Err(match attention {
            Err(error) => RuntimeCommandError::Mutation(error),
            Ok(_) => RuntimeCommandError::from(error),
        });
    }
    operation
        .checkpoint_effect_or_attention(
            index,
            crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
            Some("committed"),
            "config_commit_checkpoint_failed",
            "after",
            auth_runtime_terminal_state(action),
            config::ConfigMutationTerminalConfigImage::After,
        )
        .map_err(RuntimeCommandError::Mutation)?;
    let outcome = operation
        .finish(
            "completed",
            "after",
            if action == AuthRuntimeAction::StopManagedCodex {
                "stopped"
            } else {
                "preserved"
            },
            None,
            config::ConfigMutationTerminalConfigImage::After,
        )
        .map_err(RuntimeCommandError::Mutation)?;
    let mut response = crate::commands::runtime::config_mutation::outcome_json(&outcome);
    if let Some(object) = response.as_object_mut() {
        object.insert(
            "mode".into(),
            Value::String(
                match mode {
                    csswitch_codex_network::CodexNetworkMode::Auto => "auto",
                    csswitch_codex_network::CodexNetworkMode::Custom => "custom",
                }
                .into(),
            ),
        );
        object.insert(
            "source".into(),
            Value::String(resolved.source.as_str().into()),
        );
        object.insert(
            "proxy_scheme".into(),
            resolved
                .proxy_scheme
                .as_ref()
                .map_or(Value::Null, |scheme| Value::String(scheme.to_string())),
        );
        object.insert("restarted".into(), Value::Bool(false));
    }
    Ok(response)
}

fn set_codex_network_at(
    dir: &Path,
    settings: csswitch_codex_network::CodexNetworkSettings,
    resolved: &csswitch_codex_network::ResolvedCodexNetworkRoute,
    before_commit: impl FnOnce() -> Result<(), String>,
) -> Result<Value, String> {
    let preflight = config::load_from(dir).map_err(|error| error.to_string())?;
    config::require_no_runtime_transaction(&preflight)?;
    before_commit()?;
    let mode = settings.mode;
    let changed = config::update_result(dir, move |cfg| {
        config::require_no_runtime_transaction(cfg)?;
        let changed = cfg.codex_network != settings;
        cfg.codex_network = settings;
        Ok((changed, changed))
    })
    .map_err(|error| error.to_string())?;
    let outcome = crate::commands::runtime::config_mutation::typed_intent_outcome(
        "set_codex_network",
        if changed { "committed" } else { "no_change" },
        "committed",
        None,
        None,
        Some("not_run"),
        Some(false),
    );
    let mut response = outcome;
    if let Some(object) = response.as_object_mut() {
        object.insert("mode".into(), json!(mode));
        object.insert("source".into(), json!(resolved.source));
        object.insert("proxy_scheme".into(), json!(resolved.proxy_scheme));
        object.insert("restarted".into(), Value::Bool(false));
    }
    Ok(response)
}

fn codex_downgrade_preview_for(cfg: &config::Config) -> Result<Value, String> {
    let profiles: Vec<Value> = cfg
        .profiles
        .iter()
        .filter(|profile| {
            profile.credential_source == crate::provider_contracts::CredentialSource::CsswitchOauth
        })
        .map(|profile| json!({ "id": profile.id, "name": profile.name }))
        .collect();
    let active_will_clear = profiles
        .iter()
        .any(|profile| profile["id"].as_str() == Some(cfg.active_id.as_str()));
    let actions = profiles
        .iter()
        .filter_map(|profile| profile["id"].as_str())
        .map(|id| {
            (
                id.to_string(),
                config::CodexDowngradeAction::ExportThenRemove,
            )
        })
        .collect();
    let prepared = config::prepare_downgrade_to_v2(cfg, &actions)?;
    let catalog_export_count = prepared
        .exports
        .iter()
        .filter(|value| value["kind"] == "saved_model_catalog")
        .count();
    Ok(json!({
        "schema_version": 1,
        "action": "export_then_remove_all",
        "profile_count": profiles.len(),
        "profiles": profiles,
        "active_will_clear": active_will_clear,
        "catalog_export_count": catalog_export_count,
        "preview_fingerprint": prepared.fingerprint,
        "credentials_unchanged": true,
        "app_exit_required": true,
    }))
}

fn downgrade_actions_for_expected(
    cfg: &config::Config,
    expected_profile_ids: &[String],
    expected_preview_fingerprint: &str,
) -> Result<BTreeMap<String, config::CodexDowngradeAction>, String> {
    let current: BTreeSet<String> = cfg
        .profiles
        .iter()
        .filter(|profile| {
            profile.credential_source == crate::provider_contracts::CredentialSource::CsswitchOauth
        })
        .map(|profile| profile.id.clone())
        .collect();
    let expected: BTreeSet<String> = expected_profile_ids.iter().cloned().collect();
    if current.is_empty() || expected.len() != expected_profile_ids.len() || current != expected {
        return Err(
            "Codex profile 列表已变化或确认参数不完整；未导出、未降级，请重新预览。".into(),
        );
    }
    let actions: BTreeMap<_, _> = current
        .into_iter()
        .map(|id| (id, config::CodexDowngradeAction::ExportThenRemove))
        .collect();
    let actual = config::prepare_downgrade_to_v2(cfg, &actions)?.fingerprint;
    if expected_preview_fingerprint.is_empty() || actual != expected_preview_fingerprint {
        return Err("配置或模型目录在预览后已变化；未导出、未降级，请重新预览并确认。".into());
    }
    Ok(actions)
}

fn stop_all_before_downgrade<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<(), String> {
    stop_all_before_downgrade_with(
        app,
        state,
        lifecycle,
        ScienceHostAdapter::claim_stop,
        |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
        AppState::stop_proxy,
    )
}

fn stop_all_before_downgrade_with<R, Claim, Execute, StopGateway>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    claim_science: Claim,
    execute_science: Execute,
    stop_gateway: StopGateway,
) -> Result<(), String>
where
    R: tauri::Runtime,
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
    Execute: FnOnce(
        &tauri::AppHandle<R>,
        crate::runtime::science::ScienceStopRequest,
    ) -> (crate::runtime::science::ScienceStopOutcome, bool),
    StopGateway: FnOnce(&mut AppState) -> crate::GatewayStopOutcome,
{
    lifecycle.bump_generation();
    // Downgrade keeps its terminal stop-all policy, but the bounded Science
    // stop/wait must not retain AppState. Publication is accepted only while
    // the bumped generation and complete process-local owner still match.
    let sandbox_result = super::runtime::execute_process_local_science_stop_with(
        app,
        state,
        lifecycle,
        |_st, _generation| Ok(()),
        claim_science,
        execute_science,
        |_st| Ok(()),
    );
    let gateway_result = stop_gateway(&mut lock(state))
        .require_stopped("降级前无法安全停止受管 Gateway；配置、导出和本地认证文件均未修改");
    match (sandbox_result, gateway_result) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(format!(
            "降级前无法安全停止受管 Science；配置、导出和本地认证文件均未修改：{error}"
        )),
        (Ok(_), Err(error)) => Err(error),
        (Err(science_error), Err(gateway_error)) => Err(format!(
            "{gateway_error}；同时无法安全停止受管 Science：{science_error}"
        )),
    }
}

fn production_home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| "HOME 不可用或不是绝对路径，无法访问 CSSwitch Codex 认证状态。".into())
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_status(status: &AuthStatusView) -> Result<(), String> {
    let valid_reason = matches!(
        status.reason.as_str(),
        "ready"
            | "state_missing"
            | "state_uncommitted"
            | "oauth_missing"
            | "thinking_missing"
            | "record_mismatch"
    );
    if !valid_reason {
        return Err("Codex 认证 sidecar 返回了未知的状态原因。".into());
    }
    if !matches!(
        status.expiry_state.as_str(),
        "missing" | "unknown" | "expired" | "expiring" | "valid"
    ) {
        return Err("Codex 认证 sidecar 返回了未知的过期状态。".into());
    }
    if status
        .account_hash
        .as_deref()
        .is_some_and(|value| !is_lower_hex(value, 32))
    {
        return Err("Codex 认证 sidecar 返回了非法账号指纹。".into());
    }
    if status
        .auth_epoch
        .as_deref()
        .is_some_and(|value| !is_lower_hex(value, 32))
    {
        return Err("Codex 认证 sidecar 返回了非法认证代次。".into());
    }
    if status.authenticated {
        if status.reason != "ready"
            || status.account_hash.is_none()
            || status.auth_epoch.is_none()
            || status.auth_generation == 0
            || status.expiry_state == "missing"
            || !matches!(
                (status.expiry_state.as_str(), status.expires_at),
                ("unknown", None) | ("expired" | "expiring" | "valid", Some(_))
            )
        {
            return Err("Codex 认证 sidecar 返回了不一致的已登录状态。".into());
        }
    } else if status.reason == "ready"
        || status.account_hash.is_some()
        || status.expires_at.is_some()
        || status.expiry_state != "missing"
    {
        return Err("Codex 认证 sidecar 返回了不一致的未登录状态。".into());
    }
    if !status.authenticated {
        match status.reason.as_str() {
            "state_missing" if status.auth_epoch.is_none() && status.auth_generation == 0 => {}
            "state_uncommitted" if status.auth_epoch.is_some() => {}
            "oauth_missing" | "thinking_missing" | "record_mismatch"
                if status.auth_epoch.is_some() && status.auth_generation > 0 => {}
            _ => return Err("Codex 认证 sidecar 返回了不一致的状态原因。".into()),
        }
    }
    Ok(())
}

fn allowed_error_code(code: &str) -> bool {
    expected_error_exit_code(code).is_some()
}

fn expected_error_exit_code(code: &str) -> Option<i32> {
    match code {
        "not_authenticated" => Some(3),
        "browser_open_failed" | "oauth_denied" => Some(4),
        "callback_timeout" => Some(5),
        "auth_busy"
        | "auth_changed"
        | "auth_state_invalid"
        | "callback_unavailable"
        | "keychain_unavailable"
        | "auth_storage_error"
        | "unsupported_platform" => Some(6),
        "oauth_network_error"
        | "oauth_protocol_error"
        | "oauth_unexpected_content_type"
        | "oauth_challenge_response"
        | "proxy_connect_failed"
        | "tls_failed"
        | "auth_cancelled" => Some(7),
        "identity_mismatch" | "internal_error" => Some(8),
        _ => None,
    }
}

fn safe_error_message(code: &str) -> &'static str {
    match code {
        "auth_busy" => "另一项 Codex 认证操作正在进行，请稍后重试。",
        "auth_changed" => "Codex 认证状态在操作期间发生变化，请重试。",
        "auth_state_invalid" => "CSSwitch 的 Codex 认证状态无效，需要重新登录。",
        "browser_open_failed" => "无法打开系统浏览器完成 Codex 登录。",
        "callback_timeout" => "等待 Codex 登录回调超时，请重试。",
        "callback_unavailable" => "Codex 登录回调端口不可用，请关闭占用后重试。",
        "keychain_unavailable" => "旧版 CSSwitch 本地认证存储不可用。",
        "not_authenticated" => "CSSwitch 尚未登录 Codex。",
        "oauth_denied" => "Codex 登录未获授权。",
        "oauth_network_error" => "Codex 认证网络请求失败，请稍后重试。",
        "oauth_protocol_error" => "Codex 认证服务返回了无法识别的响应。",
        "oauth_unexpected_content_type" => "Codex 认证服务返回了意外的内容类型。",
        "oauth_challenge_response" => "Codex 认证请求遇到上游安全挑战。",
        "proxy_connect_failed" => "Codex 认证无法连接所选代理。",
        "tls_failed" => "Codex 认证 TLS 连接失败。",
        "auth_cancelled" => "Codex 登录已取消。",
        "auth_storage_error" => "CSSwitch 无法安全保存 Codex 认证状态。",
        "unsupported_platform" => "当前平台不支持 CSSwitch Codex 本地认证存储。",
        "identity_mismatch" => "安装包内 Gateway 与 Desktop 不匹配。",
        _ => "Codex 认证 sidecar 发生内部错误。",
    }
}

fn allowed_stage(stage: &str) -> bool {
    matches!(
        stage,
        "identity_check"
            | "proxy_config"
            | "browser_open"
            | "callback_wait"
            | "token_exchange"
            | "refresh"
            | "revoke"
            | "credential_commit"
            | "cancelled"
    )
}

fn allowed_response_kind(kind: &str) -> bool {
    matches!(kind, "json" | "html" | "empty" | "other" | "unknown")
}

fn allowed_transport_kind(kind: &str) -> bool {
    matches!(
        kind,
        "timeout" | "dns_connect" | "proxy_connect" | "tls" | "http" | "unknown"
    )
}

fn validate_diagnostic_fields(
    stage: Option<&str>,
    response_kind: Option<&str>,
    transport_kind: Option<&str>,
) -> bool {
    stage.is_none_or(allowed_stage)
        && response_kind.is_none_or(allowed_response_kind)
        && transport_kind.is_none_or(allowed_transport_kind)
}

fn parse_sidecar_output(
    bytes: &[u8],
    action: CodexAuthAction,
    exit_code: Option<i32>,
) -> Result<Value, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "Codex 认证 sidecar 输出不是 UTF-8。".to_string())?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() || line.contains(['\r', '\n']) {
        return Err("Codex 认证 sidecar 必须且只能返回一行 JSON。".into());
    }
    let envelope: SidecarEnvelope = serde_json::from_str(line)
        .map_err(|_| "Codex 认证 sidecar 返回了非法 JSON 协议。".to_string())?;
    match envelope {
        SidecarEnvelope::Success(success) => {
            if success.schema_version != AUTH_SCHEMA_VERSION
                || !success.ok
                || success.command != action.as_str()
                || exit_code != Some(0)
            {
                return Err("Codex 认证 sidecar 成功响应与进程状态不一致。".into());
            }
            validate_status(&success.status)?;
            if success.warning.as_ref().is_some_and(|warning| {
                action != CodexAuthAction::Logout
                    || warning.code != "revoke_skipped"
                    || warning.reason != "proxy_config_invalid"
            }) {
                return Err("Codex logout sidecar warning 非法。".into());
            }
            serde_json::to_value(success.status)
                .map(|status| {
                    json!({
                        "schema_version": AUTH_SCHEMA_VERSION,
                        "ok": true,
                        "command": action.as_str(),
                        "status": status,
                        "warning": success.warning,
                    })
                })
                .map_err(|_| "无法编码 Codex 认证状态。".into())
        }
        SidecarEnvelope::Error(error) => {
            if error.schema_version != AUTH_SCHEMA_VERSION
                || error.ok
                || error.command.as_deref() != Some(action.as_str())
                || !allowed_error_code(&error.error.code)
                || exit_code != expected_error_exit_code(&error.error.code)
                || error.error.message.is_empty()
                || error.error.message.len() > 512
                || !validate_diagnostic_fields(
                    error.error.stage.as_deref(),
                    error.error.response_kind.as_deref(),
                    error.error.transport_kind.as_deref(),
                )
            {
                return Err("Codex 认证 sidecar 错误响应与进程状态不一致。".into());
            }
            Ok(json!({
                "schema_version": AUTH_SCHEMA_VERSION,
                "ok": false,
                "command": action.as_str(),
                "error": {
                    "code": error.error.code,
                    "message": safe_error_message(&error.error.code),
                    "retryable": error.error.retryable,
                    "stage": error.error.stage,
                    "upstream_status": error.error.upstream_status,
                    "response_kind": error.error.response_kind,
                    "challenge_detected": error.error.challenge_detected,
                    "transport_kind": error.error.transport_kind,
                }
            }))
        }
    }
}

#[cfg(unix)]
fn set_nonblocking_stdout(stdout: &std::process::ChildStdout) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("无法为 Codex 认证 sidecar 建立有界输出通道。".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking_stdout(_stdout: &std::process::ChildStdout) -> Result<(), String> {
    Err("当前平台不支持有界 Codex 认证 sidecar 输出。".into())
}

fn stop_auth_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
fn run_codex_auth_sidecar_at(
    binary: &Path,
    home: &Path,
    action: CodexAuthAction,
) -> Result<Value, String> {
    run_codex_auth_sidecar_at_with_timeout(binary, home, action, action.timeout())
}

#[cfg(test)]
fn run_codex_auth_sidecar_at_with_timeout(
    binary: &Path,
    home: &Path,
    action: CodexAuthAction,
    timeout: Duration,
) -> Result<Value, String> {
    let process = spawn_codex_auth_sidecar_at(binary, home, action, None, None, false)?;
    wait_for_single_sidecar_response(process, action, timeout)
}

fn spawn_codex_auth_sidecar_at(
    binary: &Path,
    home: &Path,
    action: CodexAuthAction,
    route: Option<&csswitch_codex_network::ResolvedCodexNetworkRoute>,
    operation_id: Option<&str>,
    skip_revoke: bool,
) -> Result<ManagedAuthProcess, String> {
    let binary_metadata = std::fs::symlink_metadata(binary).ok();
    if !binary.is_absolute()
        || binary_metadata
            .as_ref()
            .is_none_or(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err("Codex 认证 sidecar 路径无效。".into());
    }
    if !home.is_absolute() {
        return Err("Codex 认证 HOME 必须是绝对路径。".into());
    }
    let executable_fingerprint = auth_sidecar_executable_fingerprint(binary)?;
    let controlled_stream =
        action.is_login() || (action == CodexAuthAction::Logout && operation_id.is_some());
    let mut command = Command::new(binary);
    command
        .arg("codex-auth")
        .arg(action.as_str())
        .env_clear()
        .env("HOME", home)
        .stdin(if controlled_stream {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if controlled_stream {
        let operation_id = operation_id
            .filter(|value| is_lower_hex(value, 32))
            .ok_or_else(|| "Codex 认证 operation ID 非法。".to_string())?;
        command.env("CSSWITCH_CODEX_AUTH_OPERATION_ID", operation_id);
        command.env(
            "CSSWITCH_CODEX_AUTH_START_DIGEST",
            auth_start_authorization_digest(operation_id),
        );
    } else if operation_id.is_some() {
        return Err("只读 sidecar 不得携带 operation ID。".into());
    }
    if skip_revoke {
        if action != CodexAuthAction::Logout {
            return Err("只有 logout sidecar 可以跳过 revoke。".into());
        }
        command.env("CSSWITCH_CODEX_LOGOUT_SKIP_REVOKE", "proxy_config_invalid");
    }
    if let Some(route) = route {
        let encoded = csswitch_codex_network::encode_route(route)
            .map_err(|_| "无法编码 Codex 网络路由。".to_string())?;
        command.env(csswitch_codex_network::ROUTE_ENV, encoded);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "无法启动 Codex 认证 sidecar。".to_string())?;
    let identity = match capture_auth_sidecar_identity(child.id(), &executable_fingerprint) {
        Ok(identity) => identity,
        Err(error) => {
            stop_auth_child(&mut child);
            return Err(error);
        }
    };
    if auth_sidecar_executable_fingerprint(binary).as_deref() != Ok(executable_fingerprint.as_str())
    {
        stop_auth_child(&mut child);
        return Err("Codex 认证 sidecar executable identity 已漂移。".into());
    }
    let stdin = child.stdin.take();
    if controlled_stream && stdin.is_none() {
        stop_auth_child(&mut child);
        return Err("无法建立 Codex 认证 sidecar 取消通道。".into());
    }
    let Some(stdout) = child.stdout.take() else {
        stop_auth_child(&mut child);
        return Err("无法读取 Codex 认证 sidecar 输出。".into());
    };
    if let Err(error) = set_nonblocking_stdout(&stdout) {
        stop_auth_child(&mut child);
        return Err(error);
    }

    Ok(ManagedAuthProcess {
        child,
        stdin,
        stdout,
        pending: Vec::new(),
        identity,
    })
}

fn auth_sidecar_executable_fingerprint(binary: &Path) -> Result<String, String> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    const MAX_SIDECAR_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
    let named_before = binary
        .symlink_metadata()
        .map_err(|_| "Codex 认证 sidecar executable identity 不可读。".to_string())?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(binary)
        .map_err(|_| "Codex 认证 sidecar executable identity 不可读。".to_string())?;
    let opened_before = file
        .metadata()
        .map_err(|_| "Codex 认证 sidecar executable identity 不可读。".to_string())?;
    if !opened_before.file_type().is_file()
        || opened_before.permissions().mode() & 0o111 == 0
        || opened_before.len() > MAX_SIDECAR_EXECUTABLE_BYTES
        || opened_before.dev() != named_before.dev()
        || opened_before.ino() != named_before.ino()
    {
        return Err("Codex 认证 sidecar executable identity 非法。".into());
    }
    let mut content = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "Codex 认证 sidecar executable identity 读取失败。".to_string())?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .filter(|total| *total <= MAX_SIDECAR_EXECUTABLE_BYTES)
            .ok_or_else(|| "Codex 认证 sidecar executable 超过上限。".to_string())?;
        content.update(&buffer[..read]);
    }
    let opened_after = file
        .metadata()
        .map_err(|_| "Codex 认证 sidecar executable identity 不可读。".to_string())?;
    let named_after = binary
        .symlink_metadata()
        .map_err(|_| "Codex 认证 sidecar executable identity 不可读。".to_string())?;
    let identity = |metadata: &std::fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.mode(),
        )
    };
    if total != opened_before.len()
        || identity(&opened_before) != identity(&opened_after)
        || identity(&opened_after) != identity(&named_after)
    {
        return Err("Codex 认证 sidecar executable identity 已漂移。".into());
    }
    let mut digest = Sha256::new();
    digest.update(b"csswitch-codex-auth-sidecar-executable-v1\0");
    digest.update(opened_after.dev().to_be_bytes());
    digest.update(opened_after.ino().to_be_bytes());
    digest.update(opened_after.len().to_be_bytes());
    digest.update(opened_after.mtime().to_be_bytes());
    digest.update(opened_after.mtime_nsec().to_be_bytes());
    digest.update(opened_after.mode().to_be_bytes());
    digest.update(content.finalize());
    Ok(format!("{:x}", digest.finalize()))
}

fn auth_start_authorization_digest(operation_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"csswitch-p2b-auth-start-v1\0");
    digest.update(operation_id.as_bytes());
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
fn wait_for_single_sidecar_response(
    process: ManagedAuthProcess,
    action: CodexAuthAction,
    timeout: Duration,
) -> Result<Value, String> {
    wait_for_single_sidecar_response_controlled(process, action, timeout, None)
        .map_err(|error| error.safe_message().to_string())
}

fn wait_for_single_sidecar_response_controlled(
    mut process: ManagedAuthProcess,
    action: CodexAuthAction,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<Value, SidecarWaitFailure> {
    if action.is_login() || process.stdin.is_some() {
        stop_auth_child(&mut process.child);
        return Err(SidecarWaitFailure::Protocol);
    }
    let ManagedAuthProcess {
        ref mut child,
        stdin: _,
        ref mut stdout,
        pending,
        identity: _,
    } = process;

    let deadline = Instant::now() + timeout;
    let mut bytes = pending;
    let mut output_eof = false;
    let mut exit_status = None;
    let mut chunk = [0_u8; 8192];
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            stop_auth_child(child);
            return Err(SidecarWaitFailure::Cancelled);
        }
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    output_eof = true;
                    break;
                }
                Ok(read) => {
                    bytes.extend_from_slice(&chunk[..read]);
                    if bytes.len() as u64 > MAX_AUTH_OUTPUT_BYTES {
                        stop_auth_child(child);
                        return Err(SidecarWaitFailure::Protocol);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    stop_auth_child(child);
                    return Err(SidecarWaitFailure::Protocol);
                }
            }
        }
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(_) => {
                    stop_auth_child(child);
                    return Err(SidecarWaitFailure::Protocol);
                }
            }
        }
        if exit_status.is_some() && output_eof {
            break;
        }
        if Instant::now() >= deadline {
            stop_auth_child(child);
            return Err(SidecarWaitFailure::Timeout);
        }
        std::thread::sleep(AUTH_POLL_INTERVAL);
    }
    parse_sidecar_output(&bytes, action, exit_status.and_then(|status| status.code()))
        .map_err(|_| SidecarWaitFailure::Protocol)
}

fn validate_login_sidecar_error(error: &LoginSidecarError) -> bool {
    allowed_error_code(&error.code)
        && allowed_stage(&error.stage)
        && validate_diagnostic_fields(
            Some(&error.stage),
            error.response_kind.as_deref(),
            error.transport_kind.as_deref(),
        )
}

fn send_cancel_to_sidecar(
    stdin: &mut Option<std::process::ChildStdin>,
    operation_id: &str,
) -> Result<(), String> {
    let mut input = stdin
        .take()
        .ok_or_else(|| "Codex 认证 sidecar 取消通道不可用。".to_string())?;
    let line = serde_json::to_vec(&json!({
        "schema_version": AUTH_SCHEMA_VERSION,
        "operation_id": operation_id,
        "command": "cancel",
    }))
    .map_err(|_| "无法编码 Codex 认证取消请求。".to_string())?;
    if line.len() >= MAX_AUTH_LINE_BYTES {
        return Err("Codex 认证取消请求超过协议上限。".into());
    }
    input
        .write_all(&line)
        .and_then(|_| input.write_all(b"\n"))
        .and_then(|_| input.flush())
        .map_err(|_| "无法向 Codex 认证 sidecar 发送取消请求。".to_string())
}

fn send_start_to_sidecar(
    stdin: &mut Option<std::process::ChildStdin>,
    operation_id: &str,
    authorization_digest: &str,
) -> Result<(), String> {
    let input = stdin
        .as_mut()
        .ok_or_else(|| "Codex 认证 sidecar 启动授权通道不可用。".to_string())?;
    let line = serde_json::to_vec(&json!({
        "schema_version": AUTH_SCHEMA_VERSION,
        "operation_id": operation_id,
        "command": "start",
        "authorization_digest": authorization_digest,
    }))
    .map_err(|_| "无法编码 Codex 认证启动授权。".to_string())?;
    if line.len() >= MAX_AUTH_LINE_BYTES {
        return Err("Codex 认证启动授权超过协议上限。".into());
    }
    input
        .write_all(&line)
        .and_then(|_| input.write_all(b"\n"))
        .and_then(|_| input.flush())
        .map_err(|_| "无法向 Codex 认证 sidecar 发送启动授权。".to_string())
}

fn wait_for_login_start_ack(
    process: ManagedAuthProcess,
    operation_id: &str,
    authorization_digest: &str,
    supervisor: &CodexAuthSupervisor,
) -> Result<ManagedAuthProcess, String> {
    wait_for_sidecar_start_ack(process, operation_id, authorization_digest, |stdin| {
        supervisor.authorize_login_start(operation_id, || {
            send_start_to_sidecar(stdin, operation_id, authorization_digest)
        })
    })
}

fn wait_for_sidecar_start_ack(
    mut process: ManagedAuthProcess,
    operation_id: &str,
    authorization_digest: &str,
    authorize: impl FnOnce(&mut Option<std::process::ChildStdin>) -> Result<(), String>,
) -> Result<ManagedAuthProcess, String> {
    if let Err(error) = authorize(&mut process.stdin) {
        stop_auth_child(&mut process.child);
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pending = std::mem::take(&mut process.pending);
    let mut chunk = [0_u8; 8192];
    loop {
        let mut output_eof = false;
        loop {
            match process.stdout.read(&mut chunk) {
                Ok(0) => {
                    output_eof = true;
                    break;
                }
                Ok(read) => {
                    pending.extend_from_slice(&chunk[..read]);
                    if pending.len() > MAX_AUTH_OUTPUT_BYTES as usize {
                        stop_auth_child(&mut process.child);
                        return Err("Codex 认证启动授权响应超过 64 KiB。".into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    stop_auth_child(&mut process.child);
                    return Err("读取 Codex 认证启动授权响应失败。".into());
                }
            }
        }
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let event: LoginSidecarEvent = match serde_json::from_slice(&line) {
                Ok(event) => event,
                Err(_) => {
                    stop_auth_child(&mut process.child);
                    return Err("Codex 认证启动授权响应不是合法 NDJSON。".into());
                }
            };
            if event.schema_version != AUTH_SCHEMA_VERSION || event.operation_id != operation_id {
                stop_auth_child(&mut process.child);
                return Err("Codex 认证启动授权 operation 不匹配。".into());
            }
            if event.kind != "start_ack"
                || event.state.is_some()
                || event.disposition.is_some()
                || event.status.is_some()
                || event.error.is_some()
                || event.authorization_digest.as_deref() != Some(authorization_digest)
            {
                stop_auth_child(&mut process.child);
                return Err("Codex 认证 sidecar 在 start_ack 前发送了非法事件。".into());
            }
            process.pending = pending;
            return Ok(process);
        }
        if output_eof {
            stop_auth_child(&mut process.child);
            return Err("Codex 认证 sidecar 在匹配 start_ack 前退出。".into());
        }
        match process.child.try_wait() {
            Ok(Some(_)) => {
                stop_auth_child(&mut process.child);
                return Err("Codex 认证 sidecar 未返回匹配的 start_ack。".into());
            }
            Ok(None) => {}
            Err(_) => {
                stop_auth_child(&mut process.child);
                return Err("无法确认 Codex 认证 sidecar 启动状态。".into());
            }
        }
        if Instant::now() >= deadline {
            stop_auth_child(&mut process.child);
            return Err("等待 Codex 认证 start_ack 超时。".into());
        }
        std::thread::sleep(AUTH_POLL_INTERVAL);
    }
}

fn wait_for_login_sidecar(
    mut process: ManagedAuthProcess,
    action: CodexAuthAction,
    operation_id: &str,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(&LoginSidecarEvent),
    mut on_cancel_ack: impl FnMut(&str),
) -> Result<Value, String> {
    if !matches!(
        action,
        CodexAuthAction::LoginBrowser | CodexAuthAction::Logout
    ) || !is_lower_hex(operation_id, 32)
    {
        stop_auth_child(&mut process.child);
        return Err("Codex 认证流式协议参数非法。".into());
    }
    let deadline = Instant::now() + action.timeout();
    let mut pending = std::mem::take(&mut process.pending);
    let mut total = 0_u64;
    let mut output_eof = false;
    let mut exit_status = None;
    let mut terminal: Option<Value> = None;
    let mut terminal_error_code: Option<String> = None;
    let mut cancel_sent = false;
    let mut accepted_at: Option<Instant> = None;
    let mut chunk = [0_u8; 8192];

    loop {
        if cancel.load(Ordering::SeqCst) && !cancel_sent {
            if let Err(error) = send_cancel_to_sidecar(&mut process.stdin, operation_id) {
                stop_auth_child(&mut process.child);
                return Err(error);
            }
            cancel_sent = true;
        }
        loop {
            if !pending.contains(&b'\n') {
                match process.stdout.read(&mut chunk) {
                    Ok(0) => {
                        output_eof = true;
                        break;
                    }
                    Ok(read) => {
                        total = total.saturating_add(read as u64);
                        if total > MAX_AUTH_OUTPUT_BYTES {
                            stop_auth_child(&mut process.child);
                            return Err("Codex 认证 sidecar 输出超过 64 KiB。".into());
                        }
                        pending.extend_from_slice(&chunk[..read]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        stop_auth_child(&mut process.child);
                        return Err("Codex 认证 sidecar 输出读取失败。".into());
                    }
                }
            }
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let mut line = pending.drain(..=newline).collect::<Vec<_>>();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() || line.len() > MAX_AUTH_LINE_BYTES {
                    stop_auth_child(&mut process.child);
                    return Err("Codex 认证 sidecar NDJSON 行非法。".into());
                }
                let event: LoginSidecarEvent = match serde_json::from_slice(&line) {
                    Ok(event) => event,
                    Err(_) => {
                        stop_auth_child(&mut process.child);
                        return Err("Codex 认证 sidecar 返回了非法 NDJSON。".into());
                    }
                };
                if event.schema_version != AUTH_SCHEMA_VERSION || event.operation_id != operation_id
                {
                    stop_auth_child(&mut process.child);
                    return Err("Codex 认证 sidecar operation 不匹配。".into());
                }
                match event.kind.as_str() {
                    "progress" => {
                        if terminal.is_some()
                            || event.status.is_some()
                            || event.error.is_some()
                            || event.disposition.is_some()
                            || event.authorization_digest.is_some()
                        {
                            stop_auth_child(&mut process.child);
                            return Err("Codex 认证 progress 字段非法。".into());
                        }
                        let state = event.state.as_deref().unwrap_or_default();
                        if !matches!(state, "waiting" | "exchanging" | "committing") {
                            stop_auth_child(&mut process.child);
                            return Err("Codex 认证 progress 状态非法。".into());
                        }
                        on_progress(&event);
                    }
                    "cancel_ack" => {
                        if !cancel_sent
                            || event.state.is_some()
                            || event.status.is_some()
                            || event.error.is_some()
                            || event.authorization_digest.is_some()
                        {
                            stop_auth_child(&mut process.child);
                            return Err("Codex 认证 cancel ack 字段非法。".into());
                        }
                        let disposition = event.disposition.as_deref().unwrap_or_default();
                        if !matches!(
                            disposition,
                            "accepted" | "commit_in_progress" | "already_terminal"
                        ) {
                            stop_auth_child(&mut process.child);
                            return Err("Codex 认证 cancel ack 结果非法。".into());
                        }
                        if disposition == "accepted" {
                            accepted_at = Some(Instant::now());
                        }
                        on_cancel_ack(disposition);
                    }
                    "terminal" => {
                        if terminal.is_some()
                            || event.disposition.is_some()
                            || event.authorization_digest.is_some()
                        {
                            stop_auth_child(&mut process.child);
                            return Err("Codex 认证 terminal 字段非法。".into());
                        }
                        let state = event.state.as_deref().unwrap_or_default();
                        match state {
                            "succeeded" => {
                                let Some(status) = event.status.as_ref() else {
                                    stop_auth_child(&mut process.child);
                                    return Err("Codex 认证成功终态缺少状态。".into());
                                };
                                if event.error.is_some() {
                                    stop_auth_child(&mut process.child);
                                    return Err("Codex 认证成功终态包含错误。".into());
                                }
                                if let Err(error) = validate_status(status) {
                                    stop_auth_child(&mut process.child);
                                    return Err(error);
                                }
                                let valid_success = match action {
                                    CodexAuthAction::LoginBrowser => status.authenticated,
                                    CodexAuthAction::Logout => {
                                        !status.authenticated
                                            && status.reason == "state_uncommitted"
                                            && status.account_hash.is_none()
                                            && status.expires_at.is_none()
                                            && status.auth_epoch.is_some()
                                            && status.auth_generation > 0
                                    }
                                    CodexAuthAction::Status => false,
                                };
                                if !valid_success {
                                    stop_auth_child(&mut process.child);
                                    return Err(match action {
                                        CodexAuthAction::LoginBrowser => {
                                            "Codex 认证成功终态必须包含已登录状态。"
                                        }
                                        CodexAuthAction::Logout => {
                                            "Codex logout 成功终态必须是 exact logged-out generation。"
                                        }
                                        CodexAuthAction::Status => {
                                            "Codex 认证成功终态与 action 语义不匹配。"
                                        }
                                    }
                                    .into());
                                }
                                terminal = Some(json!({
                                    "ok": true,
                                    "state": "succeeded",
                                    "status": status,
                                }));
                            }
                            "failed" | "cancelled" => {
                                let Some(error) = event.error.as_ref() else {
                                    stop_auth_child(&mut process.child);
                                    return Err("Codex 认证失败终态缺少错误。".into());
                                };
                                if event.status.is_some()
                                    || !validate_login_sidecar_error(error)
                                    || (state == "cancelled" && error.code != "auth_cancelled")
                                {
                                    stop_auth_child(&mut process.child);
                                    return Err("Codex 认证失败终态字段非法。".into());
                                }
                                terminal_error_code = Some(error.code.clone());
                                terminal = Some(json!({
                                    "ok": false,
                                    "state": state,
                                    "error": error,
                                }));
                            }
                            _ => {
                                stop_auth_child(&mut process.child);
                                return Err("Codex 认证 terminal 状态非法。".into());
                            }
                        }
                    }
                    _ => {
                        stop_auth_child(&mut process.child);
                        return Err("Codex 认证 sidecar 事件类型非法。".into());
                    }
                }
            }
            if pending.len() > MAX_AUTH_LINE_BYTES {
                stop_auth_child(&mut process.child);
                return Err("Codex 认证 sidecar NDJSON 行超过 8 KiB。".into());
            }
        }
        if exit_status.is_none() {
            exit_status = match process.child.try_wait() {
                Ok(status) => status,
                Err(_) => {
                    stop_auth_child(&mut process.child);
                    return Err("无法确认 Codex 认证 sidecar 退出状态。".into());
                }
            };
        }
        if exit_status.is_some() && output_eof {
            break;
        }
        if accepted_at.is_some_and(|at| at.elapsed() >= ACCEPTED_CANCEL_WATCHDOG) {
            stop_auth_child(&mut process.child);
            return Ok(json!({
                "ok": false,
                "state": "cancelled",
                "error": {
                    "code": "auth_cancelled",
                    "stage": "cancelled",
                    "retryable": true,
                }
            }));
        }
        if Instant::now() >= deadline && !cancel_sent {
            cancel.store(true, Ordering::SeqCst);
        }
        std::thread::sleep(AUTH_POLL_INTERVAL);
    }
    if !pending.is_empty() || terminal.is_none() {
        return Err("Codex 认证 sidecar 未返回完整终态。".into());
    }
    let exit_code = exit_status.and_then(|status| status.code());
    if let Some(code) = terminal_error_code {
        if exit_code != expected_error_exit_code(&code) {
            return Err("Codex 认证终态与进程退出码不一致。".into());
        }
    } else if exit_code != Some(0) {
        return Err("Codex 认证成功终态与进程退出码不一致。".into());
    }
    terminal.ok_or_else(|| "Codex 认证 sidecar 未返回终态。".into())
}

fn run_codex_auth_preflight_sidecar<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    reservation: &AuthPreflightReservation,
    route: &csswitch_codex_network::ResolvedCodexNetworkRoute,
) -> Result<Value, CodexAuthCommandError> {
    let binary = codex_gateway_bin(app)?;
    run_codex_auth_preflight_sidecar_at(
        &binary,
        &production_home().map_err(|_| {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed)
        })?,
        reservation,
        route,
    )
}

fn run_codex_auth_preflight_sidecar_at(
    binary: &Path,
    home: &Path,
    reservation: &AuthPreflightReservation,
    route: &csswitch_codex_network::ResolvedCodexNetworkRoute,
) -> Result<Value, CodexAuthCommandError> {
    let mut process = spawn_codex_auth_sidecar_at(
        binary,
        home,
        CodexAuthAction::Status,
        Some(route),
        None,
        false,
    )
    .map_err(|_| {
        CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed)
    })?;
    let identity = match auth_sidecar_identity(&process) {
        Ok(identity) => identity,
        Err(_) => {
            stop_auth_child(&mut process.child);
            return Err(CodexAuthCommandError::unavailable(
                CodexAuthUnavailableCause::SidecarProtocolError,
            ));
        }
    };
    if reservation
        .set_process_identity(
            identity.pid,
            &identity.process_start,
            &identity.executable_fingerprint,
            identity.process_group_id,
        )
        .is_err()
    {
        stop_auth_child(&mut process.child);
        return Err(CodexAuthCommandError::busy());
    }
    let result = wait_for_single_sidecar_response_controlled(
        process,
        CodexAuthAction::Status,
        CodexAuthAction::Status.timeout(),
        Some(reservation.cancel_flag()),
    );
    reservation.clear_pid();
    result.map_err(auth_error_from_sidecar_wait)
}

fn codex_gateway_bin<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<PathBuf, CodexAuthCommandError> {
    let binary = gateway_bin_path(app).ok_or_else(|| {
        CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed)
    })?;
    Ok(binary)
}

fn spawn_codex_auth_sidecar<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    action: CodexAuthAction,
    operation_id: &str,
    route: &csswitch_codex_network::ResolvedCodexNetworkRoute,
) -> Result<ManagedAuthProcess, CodexAuthCommandError> {
    let binary = codex_gateway_bin(app)?;
    spawn_codex_auth_sidecar_at(
        &binary,
        &production_home().map_err(|_| {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed)
        })?,
        action,
        Some(route),
        Some(operation_id),
        false,
    )
    .map_err(|_| CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed))
}

fn register_login_process(
    supervisor: &CodexAuthSupervisor,
    operation_id: &str,
    mut process: ManagedAuthProcess,
) -> Result<ManagedAuthProcess, RuntimeCommandError> {
    if supervisor
        .set_pid(operation_id, process.child.id())
        .is_err()
    {
        stop_auth_child(&mut process.child);
        return Err(RuntimeCommandError::from(CodexAuthCommandError::busy()));
    }
    Ok(process)
}

fn auth_sidecar_identity(
    process: &ManagedAuthProcess,
) -> Result<crate::commands::runtime::config_mutation::AuthSidecarIdentity, String> {
    Ok(process.identity.clone())
}

fn capture_auth_sidecar_identity(
    pid: u32,
    executable_fingerprint: &str,
) -> Result<crate::commands::runtime::config_mutation::AuthSidecarIdentity, String> {
    let process_start = crate::runtime::science::process_start_identity_digest(pid)
        .ok_or_else(|| "Codex 认证 sidecar process-start identity 不可确认。".to_string())?;
    let process_group_id = {
        #[cfg(unix)]
        {
            let value = unsafe { libc::getpgid(pid as libc::pid_t) };
            if value > 0 {
                value
            } else {
                return Err("Codex 认证 sidecar process group identity 不可确认。".into());
            }
        }
        #[cfg(not(unix))]
        {
            pid as i32
        }
    };
    if process_group_id != i32::try_from(pid).unwrap_or_default() {
        return Err("Codex 认证 sidecar process group 未隔离。".into());
    }
    Ok(
        crate::commands::runtime::config_mutation::AuthSidecarIdentity {
            pid,
            process_start,
            executable_fingerprint: executable_fingerprint.to_string(),
            process_group_id,
        },
    )
}

fn run_codex_logout_sidecar_p2b<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    mutation: &CodexMutationLease,
    operation: &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
) -> Result<Value, CodexAuthCommandError> {
    let binary = codex_gateway_bin(app)?;
    let (route, skip_revoke) = match resolve_codex_network_route() {
        Ok(route) => (route, false),
        Err(_) => (csswitch_codex_network::direct_route(), true),
    };
    let auth_operation_id = operation
        .receipt()
        .auth_operation
        .as_ref()
        .map(|auth| auth.auth_operation_id.clone())
        .ok_or_else(|| {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::AuthStateChanged)
        })?;
    let mut value = run_codex_logout_sidecar_at_with_identity(
        &binary,
        &production_home().map_err(|_| {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed)
        })?,
        &route,
        skip_revoke,
        mutation,
        Some(&auth_operation_id),
        |identity| checkpoint_logout_sidecar_start(operation, identity),
    )?;
    if skip_revoke {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "warning".into(),
                json!({"code": "revoke_skipped", "reason": "proxy_config_invalid"}),
            );
        }
    }
    Ok(value)
}

fn checkpoint_logout_sidecar_start(
    operation: &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
    identity: &crate::commands::runtime::config_mutation::AuthSidecarIdentity,
) -> Result<(), String> {
    if !operation.receipt().effects.get(2).is_some_and(|effect| {
        effect.kind
            == crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthSidecar
            && effect.state
                == crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress
            && effect.attempt_id.is_some()
    }) {
        return Err("Codex logout sidecar effect identity 不匹配".into());
    }
    let start_digest = operation
        .receipt()
        .auth_operation
        .as_ref()
        .map(|auth| auth_start_authorization_digest(&auth.auth_operation_id))
        .ok_or_else(|| "Codex logout auth operation identity 缺失".to_string())?;
    operation.update_receipt(|receipt| {
        if let Some(auth) = receipt.auth_operation.as_mut() {
            auth.state = "start_prepared".into();
            auth.start_authorization_digest = Some(start_digest);
            auth.sidecar = Some(identity.clone());
        }
    })
}

fn run_codex_logout_sidecar_at(
    binary: &Path,
    home: &Path,
    route: &csswitch_codex_network::ResolvedCodexNetworkRoute,
    skip_revoke: bool,
    mutation: &CodexMutationLease,
) -> Result<Value, CodexAuthCommandError> {
    run_codex_logout_sidecar_at_with_identity(
        binary,
        home,
        route,
        skip_revoke,
        mutation,
        None,
        |_| Ok(()),
    )
}

fn run_codex_logout_sidecar_at_with_identity(
    binary: &Path,
    home: &Path,
    route: &csswitch_codex_network::ResolvedCodexNetworkRoute,
    skip_revoke: bool,
    mutation: &CodexMutationLease,
    operation_id: Option<&str>,
    on_identity: impl FnOnce(
        &crate::commands::runtime::config_mutation::AuthSidecarIdentity,
    ) -> Result<(), String>,
) -> Result<Value, CodexAuthCommandError> {
    let mut process = spawn_codex_auth_sidecar_at(
        binary,
        home,
        CodexAuthAction::Logout,
        Some(route),
        operation_id,
        skip_revoke,
    )
    .map_err(|_| {
        CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarSpawnFailed)
    })?;
    let identity = match auth_sidecar_identity(&process) {
        Ok(identity) => identity,
        Err(_) => {
            stop_auth_child(&mut process.child);
            return Err(CodexAuthCommandError::unavailable(
                CodexAuthUnavailableCause::SidecarProtocolError,
            ));
        }
    };
    if mutation
        .set_process_identity(
            identity.pid,
            &identity.process_start,
            &identity.executable_fingerprint,
            identity.process_group_id,
        )
        .is_err()
    {
        stop_auth_child(&mut process.child);
        return Err(CodexAuthCommandError::unavailable(
            CodexAuthUnavailableCause::AuthStateChanged,
        ));
    }
    if on_identity(&identity).is_err() {
        stop_auth_child(&mut process.child);
        mutation.clear_pid();
        return Err(CodexAuthCommandError::unavailable(
            CodexAuthUnavailableCause::AuthStateChanged,
        ));
    }
    let result: Result<Value, CodexAuthCommandError> = if let Some(operation_id) = operation_id {
        let digest = auth_start_authorization_digest(operation_id);
        let process = match wait_for_sidecar_start_ack(process, operation_id, &digest, |stdin| {
            send_start_to_sidecar(stdin, operation_id, &digest)
        }) {
            Ok(process) => process,
            Err(_) => {
                mutation.clear_pid();
                return Err(CodexAuthCommandError::unavailable(
                    CodexAuthUnavailableCause::AuthStateChanged,
                ));
            }
        };
        let cancel = AtomicBool::new(false);
        wait_for_login_sidecar(
            process,
            CodexAuthAction::Logout,
            operation_id,
            &cancel,
            |_| {},
            |_| {},
        )
        .map(|stream| {
            json!({
                "schema_version": AUTH_SCHEMA_VERSION,
                "ok": stream.get("ok").cloned().unwrap_or(Value::Bool(false)),
                "command": "logout",
                "status": stream.get("status").cloned().unwrap_or(Value::Null),
                "error": stream.get("error").cloned().unwrap_or(Value::Null),
            })
        })
        .map_err(|_| {
            CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::SidecarProtocolError)
        })
    } else {
        wait_for_single_sidecar_response_controlled(
            process,
            CodexAuthAction::Logout,
            CodexAuthAction::Logout.timeout(),
            None,
        )
        .map_err(auth_error_from_sidecar_wait)
    };
    mutation.clear_pid();
    result
}

fn resolve_codex_network_route() -> Result<csswitch_codex_network::ResolvedCodexNetworkRoute, String>
{
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    csswitch_codex_network::resolve_from_process(&cfg.codex_network)
        .map_err(|_| "proxy_config_invalid：Codex 网络代理配置非法。".to_string())
}

fn require_authenticated_status_typed(value: &Value) -> Result<(), CodexAuthCommandError> {
    match value.get("ok").and_then(Value::as_bool) {
        Some(false) => {
            let code = value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            Err(match code {
                "auth_busy" => CodexAuthCommandError::busy(),
                "keychain_unavailable" => CodexAuthCommandError::unavailable(
                    CodexAuthUnavailableCause::KeychainUnavailable,
                ),
                "auth_state_invalid" => {
                    CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::AuthStateInvalid)
                }
                "auth_storage_error" => CodexAuthCommandError::unavailable(
                    CodexAuthUnavailableCause::StorageUnavailable,
                ),
                "unsupported_platform" => CodexAuthCommandError::unavailable(
                    CodexAuthUnavailableCause::UnsupportedPlatform,
                ),
                "auth_changed" => {
                    CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::AuthStateChanged)
                }
                "identity_mismatch" => {
                    CodexAuthCommandError::unavailable(CodexAuthUnavailableCause::IdentityMismatch)
                }
                _ => CodexAuthCommandError::unavailable(
                    CodexAuthUnavailableCause::SidecarProtocolError,
                ),
            })
        }
        Some(true) => {
            let authenticated = value
                .pointer("/status/authenticated")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    CodexAuthCommandError::unavailable(
                        CodexAuthUnavailableCause::SidecarProtocolError,
                    )
                })?;
            if authenticated {
                Ok(())
            } else {
                let reason = value
                    .pointer("/status/reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        CodexAuthCommandError::unavailable(
                            CodexAuthUnavailableCause::SidecarProtocolError,
                        )
                    })?;
                Err(CodexAuthCommandError::login_required(reason))
            }
        }
        None => Err(CodexAuthCommandError::unavailable(
            CodexAuthUnavailableCause::SidecarProtocolError,
        )),
    }
}

fn record_last_auth_status(supervisor: &CodexAuthSupervisor, value: &Value) {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        let authenticated = value
            .pointer("/status/authenticated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reason = value.pointer("/status/reason").and_then(Value::as_str);
        supervisor.record_auth_status(
            if authenticated {
                "ready"
            } else {
                "not_authenticated"
            },
            reason,
            None,
        );
        return;
    }
    let error = require_authenticated_status_typed(value).unwrap_err();
    supervisor.record_auth_status("unavailable", None, error.cause);
}

fn record_login_terminal_auth_status(
    supervisor: &CodexAuthSupervisor,
    outcome: &Result<Value, String>,
) {
    match outcome {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            record_last_auth_status(supervisor, value);
        }
        Ok(value) => {
            let cause = (value.pointer("/error/code").and_then(Value::as_str)
                == Some("identity_mismatch"))
            .then_some("identity_mismatch");
            supervisor.record_auth_status("unavailable", None, cause);
        }
        Err(_) => {
            supervisor.record_auth_status("unavailable", None, Some("sidecar_protocol_error"))
        }
    }
}

/// Runs the only interactive status sidecar for a top-level Codex user action.
/// The returned proof owns the Codex use lease and is borrowed by nested scratch,
/// formal Gateway, and Science startup paths without repeating auth preflight.
pub(crate) fn prepare_provider_auth<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    adapter: &str,
    target: CodexPreflightTarget,
) -> Result<Option<PreparedCodexAuth>, RuntimeCommandError> {
    prepare_provider_auth_inner(app, adapter, target, run_codex_auth_preflight_sidecar)
}

fn prepare_provider_auth_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    adapter: &str,
    target: CodexPreflightTarget,
    run_preflight: impl FnOnce(
        &tauri::AppHandle<R>,
        &AuthPreflightReservation,
        &csswitch_codex_network::ResolvedCodexNetworkRoute,
    ) -> Result<Value, CodexAuthCommandError>,
) -> Result<Option<PreparedCodexAuth>, RuntimeCommandError> {
    if adapter != "codex" {
        return Ok(None);
    }
    let snapshot = CodexLaunchSnapshot::capture(&target).map_err(RuntimeCommandError::from)?;
    let route = resolve_codex_network_route().map_err(RuntimeCommandError::from)?;
    let supervisor = app.state::<SharedCodexAuthSupervisor>().inner().clone();
    let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor)
        .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
    let value = match run_preflight(app, &reservation, &route) {
        Ok(value) => value,
        Err(error) => {
            supervisor.record_auth_status("unavailable", None, error.cause);
            return Err(RuntimeCommandError::from(error));
        }
    };
    record_last_auth_status(&supervisor, &value);
    require_authenticated_status_typed(&value).map_err(RuntimeCommandError::from)?;
    let proof = reservation
        .promote_to_ready_proof()
        .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
    Ok(Some(PreparedCodexAuth {
        target,
        snapshot,
        proof,
    }))
}

pub(crate) fn require_provider_auth_proof(
    adapter: &str,
    proof: Option<&CodexAuthReadyProof>,
) -> Result<(), String> {
    if adapter != "codex" {
        return Ok(());
    }
    let proof = proof
        .ok_or_else(|| "CODEX_AUTH_UNAVAILABLE：缺少本次 Codex 操作的认证 proof。".to_string())?;
    proof.ensure_active()
}

/// Doctor reports only the last user-initiated in-memory observation. It never
/// starts an interactive status sidecar and never treats stale data as current.
pub(crate) fn codex_auth_diagnostic_summary<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> String {
    let supervisor = app.state::<SharedCodexAuthSupervisor>();
    let Some(snapshot) = supervisor.last_auth_status() else {
        return "auth=not_checked".into();
    };
    let age_seconds = crate::config::now_ms()
        .saturating_sub(snapshot.checked_at_ms)
        .max(0)
        / 1_000;
    let mut fields = vec![
        format!("auth=last_known_{}", snapshot.status),
        format!("age_seconds={age_seconds}"),
    ];
    if let Some(reason) = snapshot.reason {
        fields.push(format!("reason={reason}"));
    }
    if let Some(cause) = snapshot.cause {
        fields.push(format!("cause={cause}"));
    }
    fields.join(" ")
}

#[tauri::command]
pub(crate) async fn codex_auth_status(app: tauri::AppHandle) -> Result<Value, RuntimeCommandError> {
    crate::run_blocking_typed(move || {
        let route = resolve_codex_network_route().map_err(RuntimeCommandError::from)?;
        let supervisor = app.state::<SharedCodexAuthSupervisor>().inner().clone();
        let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor)
            .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
        let value = match run_codex_auth_preflight_sidecar(&app, &reservation, &route) {
            Ok(value) => value,
            Err(error) => {
                supervisor.record_auth_status("unavailable", None, error.cause);
                return Err(RuntimeCommandError::from(error));
            }
        };
        record_last_auth_status(&supervisor, &value);
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(RuntimeCommandError::from(
                require_authenticated_status_typed(&value).unwrap_err(),
            ));
        }
        Ok(value)
    })
    .await
}

#[tauri::command]
pub(crate) async fn codex_auth_start<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    supervisor: State<'_, SharedCodexAuthSupervisor>,
    method: Option<String>,
) -> Result<Value, RuntimeCommandError> {
    reject_legacy_login_method(method.as_deref()).map_err(RuntimeCommandError::from)?;
    let action = CodexAuthAction::LoginBrowser;
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    let worker_app = app.clone();
    let worker_lifecycle = lifecycle.clone();
    let worker_supervisor = supervisor.clone();
    let (reservation, process, mutation, runtime_action) = crate::run_blocking_typed(move || {
        start_codex_login_p2b_inner(
            &app,
            &state,
            lifecycle.as_ref(),
            &supervisor,
            action,
            spawn_codex_auth_sidecar,
        )
    })
    .await?;
    let response = serde_json::to_value(&reservation.snapshot)
        .map_err(|_| RuntimeCommandError::from("无法编码 Codex 登录 operation。"))?;
    let operation_id = reservation.operation_id.clone();
    let cancel = reservation.cancel.clone();
    let _worker = tauri::async_runtime::spawn_blocking(move || {
        complete_login_operation_p2b(
            worker_app,
            worker_lifecycle,
            worker_supervisor,
            operation_id,
            cancel,
            process,
            action,
            mutation,
            runtime_action,
        );
    });
    Ok(response)
}

fn start_codex_login_p2b_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    supervisor: &SharedCodexAuthSupervisor,
    action: CodexAuthAction,
    spawn_sidecar: impl FnOnce(
        &tauri::AppHandle<R>,
        CodexAuthAction,
        &str,
        &csswitch_codex_network::ResolvedCodexNetworkRoute,
    ) -> Result<ManagedAuthProcess, CodexAuthCommandError>,
) -> Result<
    (
        LoginReservation,
        ManagedAuthProcess,
        crate::commands::runtime::config_mutation::OpenConfigMutation,
        AuthRuntimeAction,
    ),
    RuntimeCommandError,
> {
    lifecycle.with_mutation(
        RuntimeMutationDomain::Destructive,
        |_| -> Result<_, RuntimeCommandError> {
            let dir = config::default_dir();
            let before = config::load_from(&dir)
                .map_err(|error| RuntimeCommandError::from(error.to_string()))?;
            config::require_no_runtime_transaction(&before).map_err(RuntimeCommandError::from)?;
            config::require_template_enabled(&before, "codex")
                .map_err(RuntimeCommandError::from)?;
            let route = csswitch_codex_network::resolve_from_process(&before.codex_network)
                .map_err(|_| RuntimeCommandError::from("proxy_config_invalid：Codex 网络代理配置非法。"))?;
            let reservation = supervisor
                .begin_login()
                .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
            let auth_operation_id = reservation.operation_id.clone();
            let operation = crate::commands::runtime::config_mutation::begin(
                &dir,
                crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthStart,
                &before,
                None,
                crate::commands::runtime::config_mutation::MutationTarget::default(),
                crate::commands::runtime::config_mutation::RuntimePlan {
                    owner_generation: lifecycle.current_generation(),
                    ..Default::default()
                },
                vec![
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopScience,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopGateway,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthSidecar,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthGenerationCommit,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::ProfileEnsure,
                ],
                Some(crate::commands::runtime::config_mutation::AuthOperationReceipt {
                    auth_operation_id: auth_operation_id.clone(),
                    supervisor_sequence: reservation.snapshot.sequence,
                    state: "reserved".into(),
                    start_authorization_digest: None,
                    sidecar: None,
                    terminal_auth_epoch: None,
                    terminal_auth_generation: None,
                    terminal_account_hash: None,
                }),
                None,
            );
            let mut operation = match operation {
                Ok(operation) => operation,
                Err(error) => {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(error));
                }
            };
            let attached = match supervisor
                .attach_config_mutation_operation(&auth_operation_id, operation.operation_id())
            {
                Ok(attached) => attached,
                Err(error) => {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                        "auth_reservation_attach_failed",
                        "before",
                        "unknown",
                        config::ConfigMutationTerminalConfigImage::Before,
                        error,
                    )));
                }
            };
            let mut reservation = reservation;
            reservation.snapshot = attached;

            let (science_present, gateway_present) = {
                let current = lock(state);
                (
                    current.science_runtime.is_some() || current.sandbox.is_some(),
                    current.proxy.is_some(),
                )
            };
            for index in 0..=1 {
                if let Err(error) = operation.checkpoint_effect_or_attention(
                    index,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                    None,
                    "runtime_stop_checkpoint_failed",
                    "before",
                    "unknown",
                    config::ConfigMutationTerminalConfigImage::Before,
                ) {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(error));
                }
            }
            let runtime_action = match prepare_codex_auth_mutation_with_fence(
                app,
                state,
                lifecycle,
                Some(operation.fence()),
            ) {
                Ok(action) => action,
                Err(error) => {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                        "runtime_preflight_failed",
                        "before",
                        "unknown",
                        config::ConfigMutationTerminalConfigImage::Before,
                        error,
                    )));
                }
            };
            for (index, present) in [(0, science_present), (1, gateway_present)] {
                let state = if runtime_action == AuthRuntimeAction::StopManagedCodex && present {
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded
                } else {
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Skipped
                };
                if let Err(error) = operation.checkpoint_effect_or_attention(
                    index,
                    state,
                    Some(if state == crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded { "stopped" } else { "not_present" }),
                    "runtime_stop_checkpoint_failed",
                    "before",
                    auth_runtime_terminal_state(runtime_action),
                    config::ConfigMutationTerminalConfigImage::Before,
                ) {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(error));
                }
            }

            if let Err(error) = operation.checkpoint_effect_or_attention(
                2,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "sidecar_effect_checkpoint_failed",
                "before",
                auth_runtime_terminal_state(runtime_action),
                config::ConfigMutationTerminalConfigImage::Before,
            ) {
                supervisor.abort_login_start(&auth_operation_id);
                return Err(RuntimeCommandError::Mutation(error));
            }

            let process = match spawn_sidecar(app, action, &auth_operation_id, &route)
                .map_err(RuntimeCommandError::from)
                .and_then(|process| register_login_process(supervisor, &auth_operation_id, process))
            {
                Ok(process) => process,
                Err(error) => {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                        "sidecar_spawn_failed",
                        "before",
                        auth_runtime_terminal_state(runtime_action),
                        config::ConfigMutationTerminalConfigImage::Before,
                        error.to_string(),
                    )));
                }
            };
            let sidecar_identity = match auth_sidecar_identity(&process) {
                Ok(identity) => identity,
                Err(error) => {
                    let mut process = process;
                    stop_auth_child(&mut process.child);
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                        "sidecar_identity_unconfirmed",
                        "before",
                        auth_runtime_terminal_state(runtime_action),
                        config::ConfigMutationTerminalConfigImage::Before,
                        error,
                    )));
                }
            };
            if let Err(error) = supervisor.bind_login_process_identity(
                &auth_operation_id,
                sidecar_identity.pid,
                &sidecar_identity.process_start,
                &sidecar_identity.executable_fingerprint,
                sidecar_identity.process_group_id,
            ) {
                let mut process = process;
                stop_auth_child(&mut process.child);
                supervisor.abort_login_start(&auth_operation_id);
                return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                    "sidecar_identity_registration_failed",
                    "before",
                    auth_runtime_terminal_state(runtime_action),
                    config::ConfigMutationTerminalConfigImage::Before,
                    error,
                )));
            }
            if let Err(error) = operation.update_receipt(|receipt| {
                if let Some(auth) = receipt.auth_operation.as_mut() {
                    auth.state = "spawned_inert".into();
                    auth.sidecar = Some(sidecar_identity.clone());
                }
            }) {
                let mut process = process;
                stop_auth_child(&mut process.child);
                supervisor.abort_login_start(&auth_operation_id);
                return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                    "sidecar_identity_checkpoint_failed",
                    "before",
                    auth_runtime_terminal_state(runtime_action),
                    config::ConfigMutationTerminalConfigImage::Before,
                    error,
                )));
            }
            let start_digest = auth_start_authorization_digest(&auth_operation_id);
            if let Err(error) = operation.update_receipt(|receipt| {
                if let Some(auth) = receipt.auth_operation.as_mut() {
                    auth.state = "start_prepared".into();
                    auth.start_authorization_digest = Some(start_digest.clone());
                }
            }) {
                let mut process = process;
                stop_auth_child(&mut process.child);
                supervisor.abort_login_start(&auth_operation_id);
                return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                    "start_authorization_checkpoint_failed",
                    "before",
                    auth_runtime_terminal_state(runtime_action),
                    config::ConfigMutationTerminalConfigImage::Before,
                    error,
                )));
            }
            if let Err(error) = operation.checkpoint_effect_or_attention(
                3,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "auth_generation_checkpoint_failed",
                "before",
                auth_runtime_terminal_state(runtime_action),
                config::ConfigMutationTerminalConfigImage::Before,
            ) {
                let mut process = process;
                stop_auth_child(&mut process.child);
                supervisor.abort_login_start(&auth_operation_id);
                return Err(RuntimeCommandError::Mutation(error));
            }
            let process = match wait_for_login_start_ack(
                process,
                &auth_operation_id,
                &start_digest,
                supervisor,
            ) {
                Ok(process) => process,
                Err(error) => {
                    supervisor.abort_login_start(&auth_operation_id);
                    return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                        "start_ack_failed",
                        "before",
                        auth_runtime_terminal_state(runtime_action),
                        config::ConfigMutationTerminalConfigImage::Before,
                        error,
                    )));
                }
            };
            if let Err(error) = operation.update_receipt(|receipt| {
                if let Some(auth) = receipt.auth_operation.as_mut() {
                    auth.state = "start_authorized".into();
                }
            }) {
                let mut process = process;
                stop_auth_child(&mut process.child);
                supervisor.abort_login_start(&auth_operation_id);
                return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                    "start_authorization_checkpoint_failed",
                    "before",
                    auth_runtime_terminal_state(runtime_action),
                    config::ConfigMutationTerminalConfigImage::Before,
                    error,
                )));
            }
            Ok((reservation, process, operation, runtime_action))
        },
    )
}

fn start_codex_login_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    supervisor: &SharedCodexAuthSupervisor,
    action: CodexAuthAction,
    spawn_sidecar: impl FnOnce(
        &tauri::AppHandle<R>,
        CodexAuthAction,
        &str,
        &csswitch_codex_network::ResolvedCodexNetworkRoute,
    ) -> Result<ManagedAuthProcess, CodexAuthCommandError>,
) -> Result<(LoginReservation, ManagedAuthProcess), RuntimeCommandError> {
    lifecycle.with_mutation(
        RuntimeMutationDomain::Destructive,
        |_| -> Result<_, RuntimeCommandError> {
            let cfg = config::load_from(&config::default_dir())
                .map_err(|error| RuntimeCommandError::from(error.to_string()))?;
            config::require_no_runtime_transaction(&cfg).map_err(RuntimeCommandError::from)?;
            config::require_template_enabled(&cfg, "codex").map_err(RuntimeCommandError::from)?;
            let route =
                csswitch_codex_network::resolve_from_process(&cfg.codex_network).map_err(|_| {
                    RuntimeCommandError::from("proxy_config_invalid：Codex 网络代理配置非法。")
                })?;
            let reservation = supervisor
                .begin_login()
                .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
            let operation_id = reservation.operation_id.clone();
            let process = (|| -> Result<_, RuntimeCommandError> {
                prepare_codex_auth_mutation(app, state, lifecycle)
                    .map_err(RuntimeCommandError::from)?;
                let current = config::load_from(&config::default_dir())
                    .map_err(|error| RuntimeCommandError::from(error.to_string()))?;
                config::require_no_runtime_transaction(&current)
                    .map_err(RuntimeCommandError::from)?;
                let process = spawn_sidecar(app, action, &operation_id, &route)
                    .map_err(RuntimeCommandError::from)?;
                register_login_process(supervisor, &operation_id, process)
            })();
            if process.is_err() {
                supervisor.abort_login_start(&operation_id);
            }
            process.map(|process| (reservation, process))
        },
    )
}

fn reject_legacy_login_method(method: Option<&str>) -> Result<(), String> {
    if method.is_some() {
        return Err(
            "login_method_removed：Codex 只支持浏览器登录，请更新调用方并移除 method 参数。".into(),
        );
    }
    Ok(())
}

fn operation_error_from_envelope(value: &Value) -> OperationErrorView {
    let code = value
        .pointer("/error/code")
        .and_then(Value::as_str)
        .filter(|code| allowed_error_code(code))
        .unwrap_or("internal_error")
        .to_string();
    let stage = value
        .pointer("/error/stage")
        .and_then(Value::as_str)
        .filter(|stage| allowed_stage(stage))
        .unwrap_or("token_exchange");
    OperationErrorView {
        code,
        stage: stage.into(),
        retryable: value
            .pointer("/error/retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        upstream_status: value
            .pointer("/error/upstream_status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok()),
        response_kind: value
            .pointer("/error/response_kind")
            .and_then(Value::as_str)
            .filter(|kind| allowed_response_kind(kind))
            .map(str::to_string),
        challenge_detected: value
            .pointer("/error/challenge_detected")
            .and_then(Value::as_bool),
        transport_kind: value
            .pointer("/error/transport_kind")
            .and_then(Value::as_str)
            .filter(|kind| allowed_transport_kind(kind))
            .map(str::to_string),
    }
}

fn durable_mutation_attention_error(stage: &str) -> OperationErrorView {
    OperationErrorView {
        code: "config_mutation_attention".into(),
        stage: stage.into(),
        retryable: false,
        upstream_status: None,
        response_kind: None,
        challenge_detected: None,
        transport_kind: Some("durable_receipt".into()),
    }
}

fn emit_operation_snapshot<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    snapshot: &OperationSnapshot,
) {
    let _ = app.emit("codex-auth://operation", snapshot);
}

fn complete_login_operation_p2b<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    lifecycle: SharedLifecycle,
    supervisor: SharedCodexAuthSupervisor,
    operation_id: String,
    cancel: std::sync::Arc<AtomicBool>,
    process: ManagedAuthProcess,
    action: CodexAuthAction,
    mutation: crate::commands::runtime::config_mutation::OpenConfigMutation,
    runtime_action: AuthRuntimeAction,
) {
    let progress_app = app.clone();
    let progress_supervisor = supervisor.clone();
    let progress_operation_id = operation_id.clone();
    let ack_supervisor = supervisor.clone();
    let ack_operation_id = operation_id.clone();
    let snapshot = complete_login_operation_p2b_inner(
        &supervisor,
        &lifecycle,
        &operation_id,
        cancel.as_ref(),
        process,
        action,
        mutation,
        runtime_action,
        move |event| {
            let Some(state) = event.state.as_deref() else {
                return;
            };
            if let Ok(snapshot) = progress_supervisor.update_progress(&progress_operation_id, state)
            {
                emit_operation_snapshot(&progress_app, &snapshot);
            }
        },
        move |disposition| {
            ack_supervisor.record_cancel_disposition(&ack_operation_id, disposition);
        },
    )
    .or_else(|error| {
        supervisor
            .finish(
                &operation_id,
                "failed",
                Some(OperationErrorView {
                    code: "config_mutation_attention".into(),
                    stage: "terminal".into(),
                    retryable: false,
                    upstream_status: None,
                    response_kind: None,
                    challenge_detected: None,
                    transport_kind: Some("durable_receipt".into()),
                }),
            )
            .map_err(|finish_error| format!("{error}; {finish_error}"))
    });
    if let Ok(snapshot) = snapshot {
        emit_operation_snapshot(&app, &snapshot);
    }
}

#[allow(clippy::too_many_arguments)]
fn fail_login_with_retained_attention(
    supervisor: &CodexAuthSupervisor,
    operation_id: &str,
    mutation: &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
    cause: &str,
    config_state: &str,
    runtime_state: &str,
    terminal_image: config::ConfigMutationTerminalConfigImage,
    message: impl Into<String>,
) -> Result<OperationSnapshot, String> {
    let _ = mutation.retain_attention(cause, config_state, runtime_state, terminal_image, message);
    supervisor.finish(
        operation_id,
        "failed",
        Some(durable_mutation_attention_error("terminal")),
    )
}

fn complete_login_operation_p2b_inner(
    supervisor: &SharedCodexAuthSupervisor,
    lifecycle: &SharedLifecycle,
    operation_id: &str,
    cancel: &AtomicBool,
    process: ManagedAuthProcess,
    action: CodexAuthAction,
    mut mutation: crate::commands::runtime::config_mutation::OpenConfigMutation,
    runtime_action: AuthRuntimeAction,
    on_progress: impl FnMut(&LoginSidecarEvent),
    on_cancel_ack: impl FnMut(&str),
) -> Result<OperationSnapshot, String> {
    let terminal_runtime_state = auth_runtime_terminal_state(runtime_action);
    let outcome = wait_for_login_sidecar(
        process,
        action,
        operation_id,
        cancel,
        on_progress,
        on_cancel_ack,
    );
    // The Child has been waited/reaped on every return from the bounded sidecar
    // reader.  Clear the supervisor's signal target before any later durable
    // write can fail so native-exit cleanup can never signal a PID-reuse
    // replacement for an already-terminal sidecar.
    supervisor.clear_login_pid(operation_id);
    record_login_terminal_auth_status(supervisor, &outcome);
    match outcome {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            let status: AuthStatusView = match value
                .get("status")
                .cloned()
                .and_then(|status| serde_json::from_value(status).ok())
            {
                Some(status) => status,
                None => {
                    return fail_login_with_retained_attention(
                        supervisor,
                        operation_id,
                        &mut mutation,
                        "auth_terminal_invalid",
                        "before",
                        terminal_runtime_state,
                        config::ConfigMutationTerminalConfigImage::Before,
                        "Codex 登录成功终态 status 不可用于 durable 引用。",
                    )
                }
            };
            let account_hash = status.account_hash.as_deref().map(|account| {
                let mut digest = Sha256::new();
                digest.update(b"csswitch-p2b-auth-account-v1\0");
                digest.update(account.as_bytes());
                format!("{:x}", digest.finalize())
            });
            if let Err(error) = mutation.update_receipt(|receipt| {
                if let Some(auth) = receipt.auth_operation.as_mut() {
                    auth.state = "terminal".into();
                    auth.terminal_auth_epoch = status.auth_epoch.clone();
                    auth.terminal_auth_generation = Some(status.auth_generation);
                    auth.terminal_account_hash = account_hash.clone();
                }
                receipt.target.auth_generation = Some(status.auth_generation);
                receipt.target.auth_epoch = status.auth_epoch.clone();
                receipt.target.auth_account_hash = account_hash.clone();
            }) {
                return fail_login_with_retained_attention(
                    supervisor,
                    operation_id,
                    &mut mutation,
                    "auth_terminal_checkpoint_failed",
                    "before",
                    terminal_runtime_state,
                    config::ConfigMutationTerminalConfigImage::Before,
                    error,
                );
            }
            for (index, outcome_code) in [(2, "sidecar_terminal"), (3, "auth_generation_committed")]
            {
                if let Err(error) = mutation.checkpoint_effect_or_attention(
                    index,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                    Some(outcome_code),
                    "auth_terminal_checkpoint_failed",
                    "before",
                    terminal_runtime_state,
                    config::ConfigMutationTerminalConfigImage::Before,
                ) {
                    return supervisor
                        .finish(
                            operation_id,
                            "failed",
                            Some(durable_mutation_attention_error("terminal")),
                        )
                        .map_err(|supervisor_error| format!("{error}; {supervisor_error}"));
                }
            }
            if let Err(error) = mutation.checkpoint_effect_or_attention(
                4,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "profile_ensure_checkpoint_failed",
                "before",
                terminal_runtime_state,
                config::ConfigMutationTerminalConfigImage::Before,
            ) {
                return supervisor
                    .finish(
                        operation_id,
                        "failed",
                        Some(durable_mutation_attention_error("profile_ensure")),
                    )
                    .map_err(|supervisor_error| format!("{error}; {supervisor_error}"));
            }
            let ensure = lifecycle.with_mutation(RuntimeMutationDomain::Intent, |_| {
                crate::runtime::profile::ensure_codex_profile_with_mutation(
                    &config::default_dir(),
                    mutation.fence(),
                    mutation.receipt_bytes(),
                )
            });
            let (ensure, after_config_fingerprint) = match ensure {
                Ok(ensure) => ensure,
                Err(error) => {
                    let _ = mutation.checkpoint_effect(
                        4,
                        crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                        Some("profile_ensure_failed"),
                    );
                    let _ = mutation.retain_attention(
                        "profile_ensure_failed",
                        "before",
                        terminal_runtime_state,
                        config::ConfigMutationTerminalConfigImage::Before,
                        error,
                    );
                    return supervisor.finish(
                        operation_id,
                        "failed",
                        Some(durable_mutation_attention_error("profile_ensure")),
                    );
                }
            };
            if let Err(error) = mutation.bind_after_config_fingerprint(after_config_fingerprint) {
                return fail_login_with_retained_attention(
                    supervisor,
                    operation_id,
                    &mut mutation,
                    "profile_after_bind_failed",
                    "unknown",
                    terminal_runtime_state,
                    config::ConfigMutationTerminalConfigImage::Before,
                    error,
                );
            }
            if let Err(error) = mutation.update_receipt(|receipt| {
                receipt.target.profile_id = Some(ensure.profile_id.clone());
            }) {
                return fail_login_with_retained_attention(
                    supervisor,
                    operation_id,
                    &mut mutation,
                    "profile_reference_checkpoint_failed",
                    "after",
                    terminal_runtime_state,
                    config::ConfigMutationTerminalConfigImage::After,
                    error,
                );
            }
            if let Err(error) = mutation.checkpoint_effect_or_attention(
                4,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some(match ensure.disposition {
                    crate::runtime::profile::EnsureCodexProfileDisposition::Created => "created",
                    crate::runtime::profile::EnsureCodexProfileDisposition::Existing => "existing",
                }),
                "profile_ensure_checkpoint_failed",
                "after",
                terminal_runtime_state,
                config::ConfigMutationTerminalConfigImage::After,
            ) {
                return supervisor
                    .finish(
                        operation_id,
                        "failed",
                        Some(durable_mutation_attention_error("profile_ensure")),
                    )
                    .map_err(|supervisor_error| format!("{error}; {supervisor_error}"));
            }
            let terminal = mutation.finish(
                "completed",
                "after",
                terminal_runtime_state,
                None,
                config::ConfigMutationTerminalConfigImage::After,
            );
            match terminal {
                Ok(_) => supervisor.finish(operation_id, "succeeded", None),
                Err(_) => supervisor.finish(
                    operation_id,
                    "failed",
                    Some(durable_mutation_attention_error("terminal")),
                ),
            }
        }
        Ok(value) => {
            let state = if value.get("state").and_then(Value::as_str) == Some("cancelled") {
                "cancelled"
            } else {
                "failed"
            };
            let receipt_checkpoint = mutation.update_receipt(|receipt| {
                if let Some(auth) = receipt.auth_operation.as_mut() {
                    auth.state = if state == "cancelled" {
                        "cancelled".into()
                    } else {
                        "terminal".into()
                    };
                }
            });
            if receipt_checkpoint.is_ok() {
                let _ = mutation.checkpoint_effect(
                    2,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                    Some("sidecar_terminal"),
                );
                let _ = mutation.checkpoint_effect(
                    3,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Failed,
                    Some(if state == "cancelled" {
                        "auth_cancelled"
                    } else {
                        "auth_not_committed"
                    }),
                );
            }
            retain_failed_login_receipt(
                &mut mutation,
                runtime_action,
                if state == "cancelled" {
                    "auth_cancelled"
                } else {
                    "auth_failed"
                },
            );
            supervisor.finish(
                operation_id,
                state,
                Some(durable_mutation_attention_error("terminal")),
            )
        }
        Err(_) => {
            let _ = mutation.update_receipt(|receipt| {
                if let Some(auth) = receipt.auth_operation.as_mut() {
                    auth.state = "terminal".into();
                }
            });
            let _ = mutation.checkpoint_effect(
                2,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                Some("sidecar_protocol_error"),
            );
            let _ = mutation.checkpoint_effect(
                3,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                Some("auth_generation_unknown"),
            );
            retain_failed_login_receipt(&mut mutation, runtime_action, "sidecar_protocol_error");
            supervisor.finish(
                operation_id,
                "failed",
                Some(durable_mutation_attention_error("terminal")),
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn complete_login_operation_inner(
    supervisor: &SharedCodexAuthSupervisor,
    lifecycle: &SharedLifecycle,
    operation_id: &str,
    cancel: &AtomicBool,
    process: ManagedAuthProcess,
    action: CodexAuthAction,
    on_progress: impl FnMut(&LoginSidecarEvent),
    on_cancel_ack: impl FnMut(&str),
    ensure_profile: impl FnOnce() -> Result<crate::runtime::profile::EnsureCodexProfileResult, String>,
) -> Result<OperationSnapshot, String> {
    let outcome = wait_for_login_sidecar(
        process,
        action,
        operation_id,
        cancel,
        on_progress,
        on_cancel_ack,
    );
    record_login_terminal_auth_status(supervisor, &outcome);
    finalize_login_operation(supervisor, lifecycle, operation_id, outcome, ensure_profile)
}

fn finalize_login_operation(
    supervisor: &SharedCodexAuthSupervisor,
    lifecycle: &SharedLifecycle,
    operation_id: &str,
    outcome: Result<Value, String>,
    ensure_profile: impl FnOnce() -> Result<crate::runtime::profile::EnsureCodexProfileResult, String>,
) -> Result<OperationSnapshot, String> {
    match outcome {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            match lifecycle.with_mutation(RuntimeMutationDomain::Intent, |_| ensure_profile()) {
                Ok(_) => supervisor.finish(operation_id, "succeeded", None),
                Err(_) => supervisor.finish(
                    operation_id,
                    "failed",
                    Some(OperationErrorView {
                        code: "profile_ensure_failed".into(),
                        stage: "profile_ensure".into(),
                        retryable: true,
                        upstream_status: None,
                        response_kind: None,
                        challenge_detected: None,
                        transport_kind: None,
                    }),
                ),
            }
        }
        Ok(value) => {
            let state = if value.get("state").and_then(Value::as_str) == Some("cancelled") {
                "cancelled"
            } else {
                "failed"
            };
            supervisor.finish(
                operation_id,
                state,
                Some(operation_error_from_envelope(&value)),
            )
        }
        Err(_) => supervisor.finish(
            operation_id,
            "failed",
            Some(OperationErrorView {
                code: "internal_error".into(),
                stage: "token_exchange".into(),
                retryable: true,
                upstream_status: None,
                response_kind: None,
                challenge_detected: None,
                transport_kind: Some("unknown".into()),
            }),
        ),
    }
}

#[tauri::command]
pub(crate) fn codex_auth_operation_status(
    supervisor: State<'_, SharedCodexAuthSupervisor>,
) -> Result<Option<OperationSnapshot>, String> {
    Ok(supervisor.snapshot())
}

#[tauri::command]
pub(crate) fn codex_auth_cancel(
    supervisor: State<'_, SharedCodexAuthSupervisor>,
    operation_id: String,
) -> Result<Value, String> {
    let disposition = supervisor.cancel(&operation_id)?;
    Ok(json!({ "disposition": disposition }))
}

#[tauri::command]
pub(crate) async fn codex_ensure_profile<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    lifecycle: State<'_, SharedLifecycle>,
    _supervisor: State<'_, SharedCodexAuthSupervisor>,
) -> Result<Value, RuntimeCommandError> {
    let lifecycle = lifecycle.inner().clone();
    crate::run_blocking_typed(move || {
        ensure_codex_profile_command_inner(
            lifecycle.as_ref(),
            || prepare_provider_auth(&app, "codex", CodexPreflightTarget::NoProfile),
            || ensure_codex_profile_authenticated(&config::default_dir()),
        )
    })
    .await
}

fn ensure_codex_profile_command_inner(
    lifecycle: &crate::lifecycle::Lifecycle,
    prepare_auth: impl FnOnce() -> Result<Option<PreparedCodexAuth>, RuntimeCommandError>,
    ensure_profile: impl FnOnce() -> Result<Value, String>,
) -> Result<Value, RuntimeCommandError> {
    let prepared =
        prepare_auth()?.ok_or_else(|| RuntimeCommandError::from("Codex preflight 未建立。"))?;
    lifecycle
        .with_mutation(RuntimeMutationDomain::Intent, |_| -> Result<_, String> {
            prepared.verify_unchanged()?;
            ensure_profile()
        })
        .map_err(RuntimeCommandError::from)
}

fn ensure_codex_profile_authenticated(dir: &Path) -> Result<Value, String> {
    let result = crate::runtime::profile::ensure_codex_profile_inner(dir).map_err(|_| {
        "profile_ensure_failed：授权已保存，但无法创建 Codex 配置；请重试。".to_string()
    })?;
    let disposition = match result.disposition {
        crate::runtime::profile::EnsureCodexProfileDisposition::Created => "created",
        crate::runtime::profile::EnsureCodexProfileDisposition::Existing => "existing",
    };
    let mut outcome = crate::commands::runtime::config_mutation::typed_intent_outcome(
        "codex_ensure_profile",
        disposition,
        "committed",
        None,
        Some(result.profile_id.clone()),
        Some("accepted"),
        None,
    );
    if let Some(object) = outcome.as_object_mut() {
        object.insert(
            "disposition".into(),
            serde_json::Value::String(disposition.into()),
        );
        object.insert(
            "profile_id".into(),
            serde_json::Value::String(result.profile_id),
        );
    }
    Ok(outcome)
}

#[tauri::command]
pub(crate) async fn codex_auth_logout(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    supervisor: State<'_, SharedCodexAuthSupervisor>,
) -> Result<Value, RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let completion_lifecycle = lifecycle.clone();
    let supervisor = supervisor.inner().clone();
    let logout_supervisor = supervisor.clone();
    let logout_app = app.clone();
    let (mutation, operation, runtime_action) = crate::run_blocking_typed(move || {
        prepare_codex_logout_p2b_inner(&app, &state, lifecycle.as_ref(), &supervisor)
    })
    .await?;
    let operation_id = operation.operation_id().to_string();
    crate::run_blocking_typed(move || {
        completion_lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
            let mut response = complete_codex_logout_p2b_inner(
                &logout_supervisor,
                operation,
                mutation,
                runtime_action,
                |mutation, operation| {
                    run_codex_logout_sidecar_p2b(&logout_app, mutation, operation)
                },
            )?;
            if let Some(object) = response.as_object_mut() {
                object.insert(
                    "config_mutation_operation_id".into(),
                    Value::String(operation_id),
                );
            }
            Ok(response)
        })
    })
    .await
}

fn prepare_codex_logout_p2b_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    supervisor: &SharedCodexAuthSupervisor,
) -> Result<
    (
        CodexMutationLease,
        crate::commands::runtime::config_mutation::OpenConfigMutation,
        AuthRuntimeAction,
    ),
    RuntimeCommandError,
> {
    lifecycle.with_mutation(
        RuntimeMutationDomain::Destructive,
        |_| -> Result<_, RuntimeCommandError> {
            let dir = config::default_dir();
            let before = config::load_from(&dir)
                .map_err(|error| RuntimeCommandError::from(error.to_string()))?;
            config::require_no_runtime_transaction(&before).map_err(RuntimeCommandError::from)?;
            let mutation = CodexAuthSupervisor::begin_mutation(supervisor)
                .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
            let auth_operation_id = config::new_id();
            let mut operation = crate::commands::runtime::config_mutation::begin(
                &dir,
                crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthLogout,
                &before,
                Some(&before),
                crate::commands::runtime::config_mutation::MutationTarget::default(),
                crate::commands::runtime::config_mutation::RuntimePlan {
                    owner_generation: lifecycle.current_generation(),
                    ..Default::default()
                },
                vec![
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopScience,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopGateway,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthSidecar,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthGenerationCommit,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthSecretCleanupObservation,
                ],
                Some(crate::commands::runtime::config_mutation::AuthOperationReceipt {
                    auth_operation_id,
                    supervisor_sequence: 1,
                    state: "reserved".into(),
                    start_authorization_digest: None,
                    sidecar: None,
                    terminal_auth_epoch: None,
                    terminal_auth_generation: None,
                    terminal_account_hash: None,
                }),
                None,
            )
            .map_err(RuntimeCommandError::Mutation)?;
            let (science_present, gateway_present) = {
                let current = lock(state);
                (
                    current.science_runtime.is_some() || current.sandbox.is_some(),
                    current.proxy.is_some(),
                )
            };
            for index in 0..=1 {
                operation
                    .checkpoint_effect_or_attention(
                        index,
                        crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                        None,
                        "runtime_stop_checkpoint_failed",
                        "before",
                        "unknown",
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(RuntimeCommandError::Mutation)?;
            }
            let action = match prepare_codex_auth_mutation_with_fence(
                app,
                state,
                lifecycle,
                Some(operation.fence()),
            ) {
                Ok(action) => action,
                Err(error) => {
                    return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                        "runtime_preflight_failed",
                        "before",
                        "unknown",
                        config::ConfigMutationTerminalConfigImage::Before,
                        error,
                    )));
                }
            };
            for (index, present) in [(0, science_present), (1, gateway_present)] {
                let state = if action == AuthRuntimeAction::StopManagedCodex && present {
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded
                } else {
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Skipped
                };
                operation
                    .checkpoint_effect_or_attention(
                        index,
                        state,
                        Some(if state == crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded { "stopped" } else { "not_present" }),
                        "runtime_stop_checkpoint_failed",
                        "before",
                        auth_runtime_terminal_state(action),
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(RuntimeCommandError::Mutation)?;
            }
            for index in 2..=4 {
                operation
                    .checkpoint_effect_or_attention(
                        index,
                        crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                        None,
                        "logout_effect_checkpoint_failed",
                        "before",
                        auth_runtime_terminal_state(action),
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(RuntimeCommandError::Mutation)?;
            }
            Ok((mutation, operation, action))
        },
    )
}

fn complete_codex_logout_p2b_inner(
    supervisor: &SharedCodexAuthSupervisor,
    mut operation: crate::commands::runtime::config_mutation::OpenConfigMutation,
    mutation: CodexMutationLease,
    runtime_action: AuthRuntimeAction,
    run_sidecar: impl FnOnce(
        &CodexMutationLease,
        &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
    ) -> Result<Value, CodexAuthCommandError>,
) -> Result<Value, RuntimeCommandError> {
    let value = match run_sidecar(&mutation, &mut operation) {
        Ok(value) => value,
        Err(error) => {
            supervisor.record_auth_status("unavailable", None, error.cause);
            for index in 2..=4 {
                let _ = operation.checkpoint_effect(
                    index,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                    Some("logout_sidecar_uncertain"),
                );
            }
            return Err(RuntimeCommandError::Mutation(operation.retain_attention(
                error.cause.unwrap_or("logout_failed"),
                "before",
                auth_runtime_terminal_state(runtime_action),
                config::ConfigMutationTerminalConfigImage::Before,
                format!("Codex logout 失败：{}", error.cause.unwrap_or(error.code)),
            )));
        }
    };
    record_last_auth_status(supervisor, &value);
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let error = require_authenticated_status_typed(&value).unwrap_err();
        for index in 2..=4 {
            let _ = operation.checkpoint_effect(
                index,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                Some("logout_sidecar_failed"),
            );
        }
        return Err(RuntimeCommandError::Mutation(operation.retain_attention(
            error.cause.unwrap_or("logout_failed"),
            "before",
            auth_runtime_terminal_state(runtime_action),
            config::ConfigMutationTerminalConfigImage::Before,
            format!("Codex logout 失败：{}", error.cause.unwrap_or(error.code)),
        )));
    }
    let status = value
        .get("status")
        .cloned()
        .ok_or(())
        .and_then(|status| serde_json::from_value::<AuthStatusView>(status).map_err(|_| ()))
        .map_err(|_| {
            for index in 2..=4 {
                let _ = operation.checkpoint_effect(
                    index,
                    crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                    Some("logout_terminal_mismatch"),
                );
            }
            RuntimeCommandError::Mutation(operation.retain_attention(
                "logout_terminal_mismatch",
                "before",
                auth_runtime_terminal_state(runtime_action),
                config::ConfigMutationTerminalConfigImage::Before,
                "Codex logout 成功终态 status 不可用于 durable 引用",
            ))
        })?;
    if status.authenticated
        || status.reason != "state_uncommitted"
        || status.account_hash.is_some()
        || status.expires_at.is_some()
        || status.auth_epoch.is_none()
        || status.auth_generation == 0
    {
        for index in 2..=4 {
            let _ = operation.checkpoint_effect(
                index,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
                Some("logout_terminal_mismatch"),
            );
        }
        return Err(RuntimeCommandError::Mutation(operation.retain_attention(
            "logout_terminal_mismatch",
            "before",
            auth_runtime_terminal_state(runtime_action),
            config::ConfigMutationTerminalConfigImage::Before,
            "Codex logout 成功终态不是 exact logged-out generation",
        )));
    }
    let account_hash = status.account_hash.as_deref().map(|account| {
        let mut digest = Sha256::new();
        digest.update(b"csswitch-p2b-auth-account-v1\0");
        digest.update(account.as_bytes());
        format!("{:x}", digest.finalize())
    });
    if let Err(error) = operation.update_receipt(|receipt| {
        if let Some(auth) = receipt.auth_operation.as_mut() {
            auth.state = "terminal".into();
            auth.terminal_auth_epoch = status.auth_epoch.clone();
            auth.terminal_auth_generation = Some(status.auth_generation);
            auth.terminal_account_hash = account_hash.clone();
        }
        receipt.target.auth_epoch = status.auth_epoch.clone();
        receipt.target.auth_generation = Some(status.auth_generation);
        receipt.target.auth_account_hash = account_hash.clone();
    }) {
        return Err(RuntimeCommandError::Mutation(operation.retain_attention(
            "logout_terminal_checkpoint_failed",
            "before",
            auth_runtime_terminal_state(runtime_action),
            config::ConfigMutationTerminalConfigImage::Before,
            error,
        )));
    }
    for (index, code) in [
        (2, "logout_sidecar"),
        (3, "auth_generation_committed"),
        (4, "logged_out_readback"),
    ] {
        operation
            .checkpoint_effect_or_attention(
                index,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some(code),
                "logout_terminal_checkpoint_failed",
                "before",
                auth_runtime_terminal_state(runtime_action),
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(RuntimeCommandError::Mutation)?;
    }
    operation
        .finish(
            "completed",
            "after",
            auth_runtime_terminal_state(runtime_action),
            None,
            config::ConfigMutationTerminalConfigImage::After,
        )
        .map_err(RuntimeCommandError::Mutation)?;
    Ok(value)
}

fn prepare_codex_logout_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    supervisor: &SharedCodexAuthSupervisor,
) -> Result<CodexMutationLease, RuntimeCommandError> {
    lifecycle.with_mutation(
        RuntimeMutationDomain::Destructive,
        |_| -> Result<_, RuntimeCommandError> {
            let mutation = CodexAuthSupervisor::begin_mutation(supervisor)
                .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
            prepare_codex_auth_mutation(app, state, lifecycle)
                .map_err(RuntimeCommandError::from)?;
            Ok(mutation)
        },
    )
}

fn complete_codex_logout_inner(
    supervisor: &SharedCodexAuthSupervisor,
    mutation: CodexMutationLease,
    run_sidecar: impl FnOnce(&CodexMutationLease) -> Result<Value, CodexAuthCommandError>,
) -> Result<Value, RuntimeCommandError> {
    let value = match run_sidecar(&mutation) {
        Ok(value) => value,
        Err(error) => {
            supervisor.record_auth_status("unavailable", None, error.cause);
            return Err(RuntimeCommandError::from(error));
        }
    };
    record_last_auth_status(supervisor, &value);
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(RuntimeCommandError::from(
            require_authenticated_status_typed(&value).unwrap_err(),
        ));
    }
    Ok(value)
}

#[tauri::command]
pub(crate) async fn set_experimental_codex_enabled(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    supervisor: State<'_, SharedCodexAuthSupervisor>,
    enabled: bool,
) -> Result<Value, RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    crate::run_blocking_typed(move || {
        lifecycle.with_mutation(
            RuntimeMutationDomain::Destructive,
            |_| -> Result<_, RuntimeCommandError> {
                execute_experimental_codex_enabled(
                    &app,
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    enabled,
                )
            },
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn set_codex_network(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    supervisor: State<'_, SharedCodexAuthSupervisor>,
    settings: csswitch_codex_network::CodexNetworkSettings,
) -> Result<Value, RuntimeCommandError> {
    let resolved = csswitch_codex_network::resolve_from_process(&settings)
        .map_err(|_| RuntimeCommandError::from("proxy_config_invalid：Codex 网络代理配置非法。"))?;
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    crate::run_blocking_typed(move || {
        lifecycle.with_mutation(
            RuntimeMutationDomain::Destructive,
            |_| -> Result<_, RuntimeCommandError> {
                let _mutation = CodexAuthSupervisor::begin_mutation(&supervisor)
                    .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
                set_codex_network_with_p2b(&app, &state, lifecycle.as_ref(), settings, &resolved)
            },
        )
    })
    .await
}

#[tauri::command]
pub(crate) fn codex_downgrade_preview() -> Result<Value, String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    codex_downgrade_preview_for(&cfg)
}

/// Export metadata for every currently confirmed Codex profile, remove those
/// profiles, and atomically commit a v2 config. The picker happens before any
/// runtime/config mutation. The frontend must stop status polling and exit this
/// source build immediately after success so it cannot migrate v2 back to v3.
#[tauri::command]
pub(crate) async fn codex_downgrade_export_all(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    expected_profile_ids: Vec<String>,
    expected_preview_fingerprint: String,
) -> Result<Value, String> {
    let exit_app = app.clone();
    let picker_app = app.clone();
    let selected = run_blocking(move || {
        Ok(picker_app
            .dialog()
            .file()
            .set_title("导出 Codex 配置元数据并降级到 v2")
            .set_file_name("csswitch-codex-profiles-export-v1.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file())
    })
    .await?;
    let Some(selected) = selected else {
        return Ok(json!({
            "schema_version": 1,
            "status": "CANCELLED",
            "credentials_unchanged": true,
        }));
    };
    let destination = selected
        .into_path()
        .map_err(|_| "Codex export 选择结果不是本地文件路径。".to_string())?;
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let outcome = run_blocking(move || {
        lifecycle.with_mutation(RuntimeMutationDomain::Terminal, |_| {
            let dir = config::default_dir();
            let cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
            let actions = downgrade_actions_for_expected(
                &cfg,
                &expected_profile_ids,
                &expected_preview_fingerprint,
            )?;
            run_downgrade_mutation_at(
                &dir,
                &actions,
                &destination,
                &expected_preview_fingerprint,
                json!({
                    "schema_version": 1,
                    "status": "DOWNGRADED_EXIT_REQUIRED",
                    "profile_count": actions.len(),
                    "exported": true,
                    "credentials_unchanged": true,
                    "app_exit_required": true,
                }),
                || stop_all_before_downgrade(&app, &state, lifecycle.as_ref()),
            )
        })
    })
    .await?;
    // The managed runtime was already stopped before the v2 commit. Do not use
    // generic quit_app: it may reload config to rediscover a stopped sandbox
    // and migrate v2 back to v3. Post-publish uncertainty uses the same direct
    // terminal path with exit code one.
    finish_downgrade_command(outcome, move |code| exit_app.exit(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::env;
    use std::fs;
    use std::net::TcpListener;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static R0_CODEX_PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "csswitch-codex-command-{name}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn script(&self, body: &str) -> PathBuf {
            self.named_script("fake-sidecar", body)
        }

        fn named_script(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn success_json(command: &str) -> String {
        format!(
            "{{\"schema_version\":3,\"ok\":true,\"command\":\"{command}\",\"status\":{{\"authenticated\":true,\"reason\":\"ready\",\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":7}}}}",
            "ab".repeat(16),
            "cd".repeat(16)
        )
    }

    fn bind_test_login_process(
        supervisor: &CodexAuthSupervisor,
        operation_id: &str,
        process: ManagedAuthProcess,
    ) -> ManagedAuthProcess {
        let process = register_login_process(supervisor, operation_id, process).unwrap();
        let identity = auth_sidecar_identity(&process).unwrap();
        supervisor
            .bind_login_process_identity(
                operation_id,
                identity.pid,
                &identity.process_start,
                &identity.executable_fingerprint,
                identity.process_group_id,
            )
            .unwrap();
        process
    }

    fn p2a_receipt_fixture(dir: &Path) -> CodexDisableOperationReceipt {
        let cfg = config::load_from(dir).unwrap();
        let before_config_fingerprint = codex_disable_config_fingerprint(&cfg).unwrap();
        let after_config_fingerprint =
            codex_disable_config_fingerprint(&codex_disable_after_config(&cfg)).unwrap();
        CodexDisableOperationReceipt {
            schema_version: CODEX_DISABLE_RECEIPT_SCHEMA_VERSION,
            operation_id: "11".repeat(16),
            operation: "experimental_codex_disable".into(),
            before_config_fingerprint,
            after_config_fingerprint,
            config_reference: CodexDisableConfigReference {
                schema_version: cfg.schema_version,
                active_profile_id: cfg.active_id.clone(),
                proxy_port: cfg.proxy_port,
                sandbox_port: cfg.sandbox_port,
            },
            plan: CodexDisableDurablePlan {
                owner_generation: 7,
                science: Some(CodexDisableSciencePlan {
                    prior: config::RuntimePriorScienceRecipe {
                        port: cfg.sandbox_port,
                        runtime_path: PathBuf::from("/bin/sh"),
                        runtime_source: "explicit".into(),
                        runtime_version: None,
                        runtime_fingerprint: "22".repeat(32),
                        runtime_adoption_attempt_id: Some("55".repeat(16)),
                        launch_receipt_digest: "33".repeat(32),
                    },
                    restore_launch_id: "44".repeat(16),
                }),
                gateway: None,
            },
            phase: CodexDisableReceiptPhase::Intent,
        }
    }

    fn p2a_config_dir(name: &str) -> TempDir {
        let temp = TempDir::new(name);
        let (proxy_port, sandbox_port) = r0_distinct_ports();
        let cfg = config::Config {
            experimental_codex_enabled: true,
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        config::save_to(&temp.0, &cfg).unwrap();
        temp
    }

    #[test]
    fn p2a_codex_disable_receipt_phase_matrix_is_validated() {
        let temp = p2a_config_dir("p2a-receipt-phases");
        let mut receipt = p2a_receipt_fixture(&temp.0);
        for phase in [
            CodexDisableReceiptPhase::Intent,
            CodexDisableReceiptPhase::Stopping {
                component: CodexDisableComponent::Science,
                science_stopped: false,
                gateway_stopped: false,
            },
            CodexDisableReceiptPhase::EffectsApplied {
                science_stopped: true,
                gateway_stopped: false,
            },
            CodexDisableReceiptPhase::Restoring {
                science_stopped: true,
                gateway_stopped: false,
            },
            CodexDisableReceiptPhase::Restored {
                science_stopped: true,
                gateway_stopped: false,
            },
            CodexDisableReceiptPhase::ConfigCommitted,
            CodexDisableReceiptPhase::Attention {
                cause: CodexDisableAttentionCause::RestoreUncertain,
            },
        ] {
            receipt.phase = phase;
            let encoded = encode_codex_disable_receipt(&receipt).unwrap();
            let decoded: CodexDisableOperationReceipt = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, receipt);
            let text = String::from_utf8(encoded).unwrap();
            for forbidden in ["api_key", "oauth", "account", "secret", "credential_ref"] {
                assert!(!text.contains(forbidden), "receipt leaked {forbidden}");
            }
        }
        receipt.phase = CodexDisableReceiptPhase::EffectsApplied {
            science_stopped: false,
            gateway_stopped: false,
        };
        assert!(encode_codex_disable_receipt(&receipt).is_err());
        receipt.phase = CodexDisableReceiptPhase::EffectsApplied {
            science_stopped: false,
            gateway_stopped: true,
        };
        assert!(encode_codex_disable_receipt(&receipt).is_err());
    }

    #[test]
    fn p2a_codex_disable_intent_config_race_without_receipt_is_retryable() {
        let temp = p2a_config_dir("p2a-intent-config-race");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let mut raced = before.clone();
        raced.reuse_system_ssh = !raced.reuse_system_ssh;
        config::save_to(&temp.0, &raced).unwrap();

        let error = match OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt) {
            Ok(_) => panic!("a stale before-image must not publish an intent"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            CodexDisableIntentPublishError::Retryable {
                cause: "config_drift"
            }
        );
        let projected = error.into_command_error();
        assert_eq!(projected.code, "codex_disable_failed");
        assert_eq!(projected.cause, "config_drift");
        assert!(projected.retryable);
        assert!(!projected.attention_required);
        assert!(config::read_codex_disable_operation_receipt(&temp.0)
            .unwrap()
            .is_none());
        assert!(config::load_from(&temp.0)
            .unwrap()
            .codex_disable_operation_fence()
            .unwrap()
            .is_none());
    }

    #[test]
    fn p2a_codex_disable_config_commit_is_exact_before_image() {
        let temp = p2a_config_dir("p2a-config-before-image");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let mut open =
            OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt.clone()).unwrap();
        open.transition(
            &temp.0,
            CodexDisableReceiptPhase::EffectsApplied {
                science_stopped: true,
                gateway_stopped: false,
            },
        )
        .unwrap();
        commit_codex_disable_config(&temp.0, &receipt).unwrap();
        assert!(
            !config::load_from(&temp.0)
                .unwrap()
                .experimental_codex_enabled
        );
        open.transition(&temp.0, CodexDisableReceiptPhase::ConfigCommitted)
            .unwrap();
        open.clear_after(&temp.0).unwrap();

        let temp = p2a_config_dir("p2a-config-drift");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let open =
            OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt.clone()).unwrap();
        let mut drifted = config::load_from(&temp.0).unwrap();
        drifted.reuse_system_ssh = true;
        config::test_save_to_without_history_authority_guard(&temp.0, &drifted).unwrap();
        assert!(commit_codex_disable_config(&temp.0, &receipt).is_err());
        let after = config::load_from(&temp.0).unwrap();
        assert!(after.experimental_codex_enabled);
        assert!(after.reuse_system_ssh);
        assert_eq!(
            open.clear_before(&temp.0).unwrap_err(),
            CodexDisableAttentionCause::ConfigDrift
        );
        assert!(read_codex_disable_receipt_at(&temp.0).unwrap().is_some());
        let mut restored = config::load_from(&temp.0).unwrap();
        restored.reuse_system_ssh = false;
        config::test_save_to_without_history_authority_guard(&temp.0, &restored).unwrap();
        open.clear_before(&temp.0).unwrap();

        let temp = p2a_config_dir("p2a-post-commit-terminal-drift");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let mut open =
            OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt.clone()).unwrap();
        open.transition(
            &temp.0,
            CodexDisableReceiptPhase::EffectsApplied {
                science_stopped: true,
                gateway_stopped: false,
            },
        )
        .unwrap();
        commit_codex_disable_config(&temp.0, &receipt).unwrap();
        let mut drifted = config::load_from(&temp.0).unwrap();
        drifted.reuse_system_ssh = true;
        config::test_save_to_without_history_authority_guard(&temp.0, &drifted).unwrap();
        assert!(open
            .transition(&temp.0, CodexDisableReceiptPhase::ConfigCommitted)
            .is_err());
        assert_eq!(
            open.clear_after(&temp.0).unwrap_err(),
            CodexDisableAttentionCause::ConfigDrift
        );
        assert!(matches!(
            read_codex_disable_receipt_at(&temp.0)
                .unwrap()
                .unwrap()
                .record
                .phase,
            CodexDisableReceiptPhase::EffectsApplied {
                science_stopped: true,
                gateway_stopped: false
            }
        ));
        assert!(config::load_from(&temp.0)
            .unwrap()
            .codex_disable_operation_fence()
            .unwrap()
            .is_some());
        let mut exact_after = config::load_from(&temp.0).unwrap();
        exact_after.reuse_system_ssh = false;
        config::test_save_to_without_history_authority_guard(&temp.0, &exact_after).unwrap();
        open.transition(&temp.0, CodexDisableReceiptPhase::ConfigCommitted)
            .unwrap();
        open.clear_after(&temp.0).unwrap();

        let temp = p2a_config_dir("p2a-restored-terminal-drift");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let mut open = OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt).unwrap();
        open.transition(
            &temp.0,
            CodexDisableReceiptPhase::Restored {
                science_stopped: true,
                gateway_stopped: false,
            },
        )
        .unwrap();
        let mut drifted = config::load_from(&temp.0).unwrap();
        drifted.reuse_system_ssh = true;
        config::test_save_to_without_history_authority_guard(&temp.0, &drifted).unwrap();
        assert_eq!(
            open.clear_before(&temp.0).unwrap_err(),
            CodexDisableAttentionCause::ConfigDrift
        );
        assert!(matches!(
            read_codex_disable_receipt_at(&temp.0)
                .unwrap()
                .unwrap()
                .record
                .phase,
            CodexDisableReceiptPhase::Restored {
                science_stopped: true,
                gateway_stopped: false
            }
        ));
        let mut exact_before = config::load_from(&temp.0).unwrap();
        exact_before.reuse_system_ssh = false;
        config::test_save_to_without_history_authority_guard(&temp.0, &exact_before).unwrap();
        open.clear_before(&temp.0).unwrap();

        let temp = p2a_config_dir("p2a-receipt-unlink-terminal-drift");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let mut open =
            OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt.clone()).unwrap();
        open.transition(
            &temp.0,
            CodexDisableReceiptPhase::EffectsApplied {
                science_stopped: true,
                gateway_stopped: false,
            },
        )
        .unwrap();
        commit_codex_disable_config(&temp.0, &receipt).unwrap();
        open.transition(&temp.0, CodexDisableReceiptPhase::ConfigCommitted)
            .unwrap();
        let fence = codex_disable_config_fence(&open.record).unwrap();
        config::clear_codex_disable_operation_receipt(
            &temp.0,
            &open.bytes,
            &fence,
            config::CodexDisableTerminalConfigImage::After,
        )
        .unwrap();
        let mut drifted = config::load_from(&temp.0).unwrap();
        drifted.reuse_system_ssh = true;
        config::test_save_to_without_history_authority_guard(&temp.0, &drifted).unwrap();
        assert_eq!(
            open.clear_after(&temp.0).unwrap_err(),
            CodexDisableAttentionCause::ConfigDrift
        );
        let attention = retain_codex_disable_attention(
            &temp.0,
            &mut open,
            CodexDisableAttentionCause::ConfigDrift,
            true,
        );
        assert_eq!(attention.code, "codex_disable_attention");
        assert_eq!(attention.cause, "config_drift");
        assert!(attention.attention_required);
        let blocked = require_no_codex_disable_receipt(&temp.0).unwrap_err();
        assert_eq!(blocked.cause, "config_drift");
        assert!(blocked.attention_required);
        assert!(config::read_codex_disable_operation_receipt(&temp.0)
            .unwrap()
            .is_none());
        assert_eq!(
            config::recover_orphan_codex_disable_operation_fence(&temp.0).unwrap(),
            config::CodexDisableOrphanFenceRecovery::ConfigDrift
        );
        let mut exact_after = config::load_from(&temp.0).unwrap();
        exact_after.reuse_system_ssh = false;
        config::test_save_to_without_history_authority_guard(&temp.0, &exact_after).unwrap();
        assert_eq!(
            config::recover_orphan_codex_disable_operation_fence(&temp.0).unwrap(),
            config::CodexDisableOrphanFenceRecovery::Cleared
        );
    }

    #[test]
    fn p2a_open_receipt_blocks_conflicting_toggle() {
        let temp = p2a_config_dir("p2a-open-receipt");
        let receipt = p2a_receipt_fixture(&temp.0);
        let before = config::load_from(&temp.0).unwrap();
        let open = OpenCodexDisableReceipt::publish_intent(&temp.0, &before, receipt).unwrap();
        let called = std::cell::Cell::new(false);
        let result = set_experimental_codex_enabled_at(&temp.0, true, || {
            called.set(true);
            Ok(())
        });
        assert!(result.unwrap_err().contains("durable receipt"));
        assert!(!called.get());
        assert!(
            config::load_from(&temp.0)
                .unwrap()
                .experimental_codex_enabled
        );
        open.clear_before(&temp.0).unwrap();
    }

    #[test]
    fn p2a_replay_decision_preserves_replacement_and_is_idempotent() {
        use CodexDisableComponentObservation::{Absent, Original, Replacement, Restored};
        assert_eq!(
            decide_codex_disable_before_image_replay(
                CodexDisableReceiptPhase::Intent,
                Some(Original),
                Some(Original),
            ),
            CodexDisableReplayDecision::Clear,
        );
        assert_eq!(
            decide_codex_disable_before_image_replay(
                CodexDisableReceiptPhase::Intent,
                Some(Absent),
                Some(Original),
            ),
            CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::IdentityDrift),
        );
        assert_eq!(
            decide_codex_disable_before_image_replay(
                CodexDisableReceiptPhase::Stopping {
                    component: CodexDisableComponent::Science,
                    science_stopped: false,
                    gateway_stopped: false,
                },
                Some(Absent),
                Some(Original),
            ),
            CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::StopUncertain),
        );
        assert_eq!(
            decide_codex_disable_before_image_replay(
                CodexDisableReceiptPhase::Stopping {
                    component: CodexDisableComponent::Science,
                    science_stopped: false,
                    gateway_stopped: false,
                },
                Some(Original),
                Some(Original),
            ),
            CodexDisableReplayDecision::Clear,
        );
        for phase in [
            CodexDisableReceiptPhase::EffectsApplied {
                science_stopped: true,
                gateway_stopped: true,
            },
            CodexDisableReceiptPhase::Restoring {
                science_stopped: true,
                gateway_stopped: true,
            },
            CodexDisableReceiptPhase::Restored {
                science_stopped: true,
                gateway_stopped: true,
            },
        ] {
            assert_eq!(
                decide_codex_disable_before_image_replay(phase, Some(Restored), Some(Restored),),
                CodexDisableReplayDecision::Restore {
                    science_stopped: true,
                    gateway_stopped: true,
                },
            );
        }
        assert_eq!(
            decide_codex_disable_before_image_replay(
                CodexDisableReceiptPhase::Intent,
                Some(Replacement),
                Some(Original),
            ),
            CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::IdentityDrift),
        );
        assert_eq!(
            decide_codex_disable_before_image_replay(
                CodexDisableReceiptPhase::ConfigCommitted,
                None,
                None,
            ),
            CodexDisableReplayDecision::Attention(CodexDisableAttentionCause::ConfigDrift),
        );
    }

    fn status_json(reason: &str, authenticated: bool, generation: u64) -> String {
        let ready = authenticated;
        let account = if ready {
            format!("\"{}\"", "ab".repeat(16))
        } else {
            "null".into()
        };
        let epoch = if reason == "state_missing" {
            "null".into()
        } else {
            format!("\"{}\"", "cd".repeat(16))
        };
        let expiry_state = if ready { "valid" } else { "missing" };
        let expires_at = if ready { "2000000000" } else { "null" };
        format!(
            "{{\"schema_version\":3,\"ok\":true,\"command\":\"status\",\"status\":{{\"authenticated\":{authenticated},\"reason\":\"{reason}\",\"account_hash\":{account},\"expiry_state\":\"{expiry_state}\",\"expires_at\":{expires_at},\"auth_epoch\":{epoch},\"auth_generation\":{generation}}}}}"
        )
    }

    #[test]
    fn legacy_method_is_rejected_by_the_real_tauri_invoke_handler() {
        let app = tauri::test::mock_builder()
            .manage(Arc::new(Mutex::new(AppState::default())) as SharedAppState)
            .manage(Arc::new(crate::lifecycle::Lifecycle::new()) as SharedLifecycle)
            .manage(Arc::new(CodexAuthSupervisor::default()) as SharedCodexAuthSupervisor)
            .invoke_handler(tauri::generate_handler![codex_auth_start])
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let error = tauri::test::get_ipc_response(
            &webview,
            tauri::webview::InvokeRequest {
                cmd: "codex_auth_start".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::Json(json!({"method": "device"})),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.into(),
            },
        )
        .unwrap_err();
        assert!(error
            .as_str()
            .is_some_and(|message| message.starts_with("login_method_removed：")));
    }

    #[test]
    fn runtime_decision_stops_only_confirmed_codex_and_fails_closed_on_unknown_live_child() {
        assert_eq!(
            decide_auth_runtime_action("codex", TrackedProxyState::Running, false).unwrap(),
            AuthRuntimeAction::StopManagedCodex
        );
        assert_eq!(
            decide_auth_runtime_action("relay", TrackedProxyState::Running, false).unwrap(),
            AuthRuntimeAction::PreserveOtherProvider
        );
        assert_eq!(
            decide_auth_runtime_action("", TrackedProxyState::Absent, false).unwrap(),
            AuthRuntimeAction::Noop
        );
        assert!(decide_auth_runtime_action("", TrackedProxyState::Running, false).is_err());
        assert!(decide_auth_runtime_action("mystery", TrackedProxyState::Unknown, false).is_err());
        assert_eq!(
            decide_auth_runtime_action("mystery", TrackedProxyState::Exited, false).unwrap(),
            AuthRuntimeAction::Noop
        );
        assert!(decide_auth_runtime_action("", TrackedProxyState::Absent, true).is_err());
        assert!(decide_auth_runtime_action("codex", TrackedProxyState::Exited, true).is_err());

        assert!(codex_disable_requires_gateway_claim(
            AuthRuntimeAction::StopManagedCodex,
            TrackedProxyState::Unknown,
        )
        .is_err());
        assert!(codex_disable_requires_gateway_claim(
            resolve_science_runtime_action(
                AuthRuntimeAction::StopManagedCodex,
                false,
                SandboxScienceState::Stopped,
            )
            .unwrap(),
            TrackedProxyState::Running,
        )
        .unwrap());
        assert!(codex_disable_requires_gateway_claim(
            resolve_science_runtime_action(
                AuthRuntimeAction::StopManagedCodex,
                false,
                SandboxScienceState::RunningHealthy,
            )
            .unwrap(),
            TrackedProxyState::Unknown,
        )
        .is_err());

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let occupied = proc::loopback_port_in_use(port, 100);
        assert!(occupied);
        assert!(decide_auth_runtime_action("codex", TrackedProxyState::Absent, occupied).is_err());

        assert_eq!(
            resolve_science_runtime_action(
                AuthRuntimeAction::Noop,
                true,
                SandboxScienceState::RunningHealthy,
            )
            .unwrap(),
            AuthRuntimeAction::StopManagedCodex
        );
        assert_eq!(
            resolve_science_runtime_action(
                AuthRuntimeAction::Noop,
                true,
                SandboxScienceState::Stopped,
            )
            .unwrap(),
            AuthRuntimeAction::Noop
        );
        assert!(resolve_science_runtime_action(
            AuthRuntimeAction::Noop,
            true,
            SandboxScienceState::Unknown,
        )
        .is_err());
    }

    #[test]
    fn p2b_auth_terminal_runtime_state_matches_the_actual_preflight_action() {
        assert_eq!(
            auth_runtime_terminal_state(AuthRuntimeAction::StopManagedCodex),
            "stopped"
        );
        assert_eq!(
            auth_runtime_terminal_state(AuthRuntimeAction::PreserveOtherProvider),
            "preserved"
        );
        assert_eq!(
            auth_runtime_terminal_state(AuthRuntimeAction::Noop),
            "preserved"
        );
    }

    #[test]
    fn experimental_toggle_commits_only_after_disable_precondition_succeeds() {
        let temp = TempDir::new("toggle-order");
        config::update(&temp.0, |cfg| cfg.experimental_codex_enabled = true).unwrap();

        let failure = set_experimental_codex_enabled_at(&temp.0, false, || {
            Err("managed Codex Science stop failed".into())
        });
        assert!(failure.is_err());
        assert!(
            config::load_from(&temp.0)
                .unwrap()
                .experimental_codex_enabled
        );

        let disabled = set_experimental_codex_enabled_at(&temp.0, false, || Ok(())).unwrap();
        assert_eq!(disabled["experimental_codex_enabled"], false);
        assert!(
            !config::load_from(&temp.0)
                .unwrap()
                .experimental_codex_enabled
        );

        let enabled = set_experimental_codex_enabled_at(&temp.0, true, || {
            panic!("enable must not run the disable precondition")
        })
        .unwrap();
        assert_eq!(enabled["experimental_codex_enabled"], true);
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn r3_codex_mutation_wait_releases_read_model_and_stale_result_preserves_replacement() {
        let temp = TempDir::new("r3-codex-mutation-owner-cas");
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o700)).unwrap();
        let prior_binary = temp.0.join("prior-science");
        let replacement_binary = temp.0.join("replacement-science");
        fs::write(&prior_binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::write(&replacement_binary, b"#!/bin/sh\nexit 0\n# replacement\n").unwrap();
        fs::set_permissions(&prior_binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&replacement_binary, fs::Permissions::from_mode(0o700)).unwrap();
        let prior =
            crate::runtime::science::test_runtime_identity(prior_binary.canonicalize().unwrap());
        let replacement = crate::runtime::science::test_runtime_identity(
            replacement_binary.canonicalize().unwrap(),
        );

        for (case, replace_identity, bump_generation) in [
            ("generation-only", false, true),
            ("identity-only", true, false),
        ] {
            let config_dir = temp.0.join(format!("config-{case}"));
            fs::create_dir_all(&config_dir).unwrap();
            fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
            config::save_to(
                &config_dir,
                &config::Config {
                    experimental_codex_enabled: true,
                    sandbox_port: 18765,
                    proxy_port: 18000,
                    ..Default::default()
                },
            )
            .unwrap();
            let config_before = fs::read(config_dir.join("config.json")).unwrap();

            let (state, proxy_pid) = r0_proxy_state("codex");
            {
                let mut authority = lock(&state);
                authority.science_runtime = Some(prior.clone());
                authority.sandbox_port = 18765;
                authority.sandbox_url = Some("http://127.0.0.1:18765/prior".into());
            }
            let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
            let generation_before = lifecycle.current_generation();
            let observed_owner = CodexScienceOwnerSnapshot::claim(&lock(&state), generation_before);
            let app = tauri::test::mock_builder()
                .build(tauri::test::mock_context(tauri::test::noop_assets()))
                .unwrap();
            let handle = app.handle().clone();

            let (stop_started_tx, stop_started_rx) = std::sync::mpsc::channel();
            let (release_stop_tx, release_stop_rx) = std::sync::mpsc::channel();
            let worker_state = state.clone();
            let worker_lifecycle = lifecycle.clone();
            let worker_prior = prior.clone();
            let worker_config_dir = config_dir.clone();
            let worker = std::thread::spawn(move || {
                let claim_prior = worker_prior.clone();
                set_experimental_codex_enabled_at(&worker_config_dir, false, || {
                    stop_managed_codex_runtime_with(
                        &handle,
                        &worker_state,
                        worker_lifecycle.as_ref(),
                        CodexScienceObservation {
                            state: SandboxScienceState::RunningHealthy,
                            detected_runtime: Some(worker_prior.clone()),
                            owner: observed_owner,
                        },
                        |runtime| {
                            assert_eq!(runtime, Some(&claim_prior));
                            Ok(crate::runtime::science::ScienceStopRequest::recover(
                                runtime,
                            ))
                        },
                        move |_, _| {
                            stop_started_tx.send(()).unwrap();
                            release_stop_rx.recv().unwrap();
                            (
                                Ok(crate::runtime::science::VerifiedScienceStop {
                                    runtime: Some(worker_prior),
                                    ownership_was_proven: true,
                                }),
                                true,
                            )
                        },
                        AppState::stop_proxy,
                    )
                })
            });

            stop_started_rx.recv().unwrap();
            {
                let mut read_model = state
                    .try_lock()
                    .expect("Codex mutation Science stop wait must not retain AppState");
                assert_eq!(read_model.science_runtime.as_ref(), Some(&prior), "{case}");
                if replace_identity {
                    read_model.science_runtime = Some(replacement.clone());
                    read_model.sandbox_url = Some("http://127.0.0.1:18765/replacement".into());
                }
            }
            if bump_generation {
                lifecycle.bump_generation();
            }
            release_stop_tx.send(()).unwrap();

            let changed = worker.join().unwrap();
            assert!(
                changed
                    .as_ref()
                    .is_err_and(|error| error.contains("process-local owner 已变化")),
                "{case}: {changed:?}"
            );
            assert_eq!(
                fs::read(config_dir.join("config.json")).unwrap(),
                config_before,
                "{case}"
            );
            assert_eq!(
                lifecycle.current_generation(),
                generation_before + u64::from(bump_generation),
                "{case}"
            );
            let current = lock(&state);
            let expected_runtime = if replace_identity {
                &replacement
            } else {
                &prior
            };
            let expected_url = if replace_identity {
                "http://127.0.0.1:18765/replacement"
            } else {
                "http://127.0.0.1:18765/prior"
            };
            assert_eq!(
                current.science_runtime.as_ref(),
                Some(expected_runtime),
                "{case}"
            );
            assert!(current.science_confirmed_stopped.is_none(), "{case}");
            assert_eq!(current.sandbox_url.as_deref(), Some(expected_url), "{case}");
            assert!(current.proxy.is_some(), "{case}");
            assert!(r0_process_is_running(proxy_pid), "{case}");
            drop(current);
            let _ = lock(&state).stop_proxy();
        }
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn r3_codex_mutation_rejects_preclaim_identity_drift_without_overwriting_replacement() {
        let temp = TempDir::new("r3-codex-mutation-preclaim-cas");
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o700)).unwrap();
        let prior_binary = temp.0.join("prior-science");
        let replacement_binary = temp.0.join("replacement-science");
        fs::write(&prior_binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::write(&replacement_binary, b"#!/bin/sh\nexit 0\n# replacement\n").unwrap();
        fs::set_permissions(&prior_binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&replacement_binary, fs::Permissions::from_mode(0o700)).unwrap();
        let prior =
            crate::runtime::science::test_runtime_identity(prior_binary.canonicalize().unwrap());
        let replacement = crate::runtime::science::test_runtime_identity(
            replacement_binary.canonicalize().unwrap(),
        );
        let config_dir = temp.0.join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        config::save_to(
            &config_dir,
            &config::Config {
                experimental_codex_enabled: true,
                sandbox_port: 18765,
                proxy_port: 18000,
                ..Default::default()
            },
        )
        .unwrap();
        let config_before = fs::read(config_dir.join("config.json")).unwrap();

        let (state, proxy_pid) = r0_proxy_state("codex");
        {
            let mut authority = lock(&state);
            authority.science_runtime = Some(prior.clone());
            authority.sandbox_port = 18765;
            authority.sandbox_url = Some("http://127.0.0.1:18765/prior".into());
        }
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let observed_owner =
            CodexScienceOwnerSnapshot::claim(&lock(&state), lifecycle.current_generation());
        {
            let mut authority = lock(&state);
            authority.science_runtime = Some(replacement.clone());
            authority.sandbox_url = Some("http://127.0.0.1:18765/replacement".into());
        }
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();

        let changed = set_experimental_codex_enabled_at(&config_dir, false, || {
            stop_managed_codex_runtime_with(
                app.handle(),
                &state,
                lifecycle.as_ref(),
                CodexScienceObservation {
                    state: SandboxScienceState::RunningHealthy,
                    detected_runtime: Some(prior.clone()),
                    owner: observed_owner,
                },
                |_| panic!("pre-claim owner drift must reject before typed stop claim"),
                |_, _| panic!("pre-claim owner drift must reject before stop execution"),
                AppState::stop_proxy,
            )
        });
        assert!(
            changed
                .as_ref()
                .is_err_and(|error| error.contains("stop claim 时 process-local owner 已变化")),
            "{changed:?}"
        );
        assert_eq!(
            fs::read(config_dir.join("config.json")).unwrap(),
            config_before
        );
        let current = lock(&state);
        assert_eq!(current.science_runtime.as_ref(), Some(&replacement));
        assert_eq!(
            current.sandbox_url.as_deref(),
            Some("http://127.0.0.1:18765/replacement")
        );
        assert!(current.proxy.is_some());
        assert!(r0_process_is_running(proxy_pid));
        drop(current);
        let _ = lock(&state).stop_proxy();
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn downgrade_cleanup_wait_releases_read_model_and_stale_result_preserves_replacement() {
        let temp = TempDir::new("downgrade-stop-owner-cas");
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o700)).unwrap();
        let prior_binary = temp.0.join("prior-science");
        let replacement_binary = temp.0.join("replacement-science");
        fs::write(&prior_binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::write(&replacement_binary, b"#!/bin/sh\nexit 0\n# replacement\n").unwrap();
        fs::set_permissions(&prior_binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&replacement_binary, fs::Permissions::from_mode(0o700)).unwrap();
        let prior =
            crate::runtime::science::test_runtime_identity(prior_binary.canonicalize().unwrap());
        let replacement = crate::runtime::science::test_runtime_identity(
            replacement_binary.canonicalize().unwrap(),
        );

        for (case, replace_identity, bump_generation) in [
            ("generation-only", false, true),
            ("identity-only", true, false),
        ] {
            let (state, proxy_pid) = r0_proxy_state("codex");
            {
                let mut authority = lock(&state);
                authority.science_runtime = Some(prior.clone());
                authority.sandbox_port = 18765;
                authority.sandbox_url = Some("http://127.0.0.1:18765/prior".into());
            }
            let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
            let generation_before = lifecycle.current_generation();
            let app = tauri::test::mock_builder()
                .build(tauri::test::mock_context(tauri::test::noop_assets()))
                .unwrap();
            let handle = app.handle().clone();

            let (stop_started_tx, stop_started_rx) = std::sync::mpsc::channel();
            let (release_stop_tx, release_stop_rx) = std::sync::mpsc::channel();
            let worker_state = state.clone();
            let worker_lifecycle = lifecycle.clone();
            let worker_prior = prior.clone();
            let worker = std::thread::spawn(move || {
                let claim_prior = worker_prior.clone();
                worker_lifecycle.with_mutation(RuntimeMutationDomain::Terminal, |_| {
                    stop_all_before_downgrade_with(
                        &handle,
                        &worker_state,
                        worker_lifecycle.as_ref(),
                        |runtime| {
                            assert_eq!(runtime, Some(&claim_prior));
                            Ok(crate::runtime::science::ScienceStopRequest::recover(
                                runtime,
                            ))
                        },
                        move |_, _| {
                            stop_started_tx.send(()).unwrap();
                            release_stop_rx.recv().unwrap();
                            (
                                Ok(crate::runtime::science::VerifiedScienceStop {
                                    runtime: Some(worker_prior),
                                    ownership_was_proven: true,
                                }),
                                true,
                            )
                        },
                        AppState::stop_proxy,
                    )
                })
            });

            stop_started_rx.recv().unwrap();
            {
                let mut read_model = state
                    .try_lock()
                    .expect("downgrade Science stop wait must not retain AppState");
                assert_eq!(read_model.science_runtime.as_ref(), Some(&prior), "{case}");
                if replace_identity {
                    read_model.science_runtime = Some(replacement.clone());
                    read_model.sandbox_url = Some("http://127.0.0.1:18765/replacement".into());
                }
            }
            if bump_generation {
                lifecycle.bump_generation();
            }
            release_stop_tx.send(()).unwrap();

            let stopped = worker.join().unwrap();
            assert!(
                stopped
                    .as_ref()
                    .is_err_and(|error| error.contains("process-local owner 已变化")),
                "{case}: {stopped:?}"
            );
            assert_eq!(
                lifecycle.current_generation(),
                generation_before + 1 + u64::from(bump_generation),
                "{case}"
            );
            let current = lock(&state);
            let expected_runtime = if replace_identity {
                &replacement
            } else {
                &prior
            };
            let expected_url = if replace_identity {
                "http://127.0.0.1:18765/replacement"
            } else {
                "http://127.0.0.1:18765/prior"
            };
            assert_eq!(
                current.science_runtime.as_ref(),
                Some(expected_runtime),
                "{case}"
            );
            assert!(current.science_confirmed_stopped.is_none(), "{case}");
            assert_eq!(current.sandbox_url.as_deref(), Some(expected_url), "{case}");
            assert!(current.proxy.is_none(), "{case}");
            assert!(!r0_process_is_running(proxy_pid), "{case}");
        }
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn r3_codex_mutation_gateway_uncertainty_fails_before_config_commit() {
        let temp = TempDir::new("r3-codex-gateway-stop-uncertain");
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o700)).unwrap();
        config::save_to(
            &temp.0,
            &config::Config {
                experimental_codex_enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let config_before = fs::read(temp.0.join("config.json")).unwrap();
        let (state, proxy_pid) = r0_proxy_state("codex");
        let lifecycle = crate::lifecycle::Lifecycle::new();
        let owner = CodexScienceOwnerSnapshot::claim(&lock(&state), lifecycle.current_generation());
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();

        let changed = set_experimental_codex_enabled_at(&temp.0, false, || {
            stop_managed_codex_runtime_with(
                app.handle(),
                &state,
                &lifecycle,
                CodexScienceObservation {
                    state: SandboxScienceState::Stopped,
                    detected_runtime: None,
                    owner,
                },
                |_| panic!("stopped Science must not claim a stop request"),
                |_, _| panic!("stopped Science must not execute a stop request"),
                |_| crate::GatewayStopOutcome::Uncertain {
                    owned_count: 1,
                    reason: "injected retained Gateway child".into(),
                },
            )
        });

        let error = changed.unwrap_err();
        assert!(error.contains("认证未变更"), "{error}");
        assert!(error.contains("退出未确认"), "{error}");
        assert_eq!(fs::read(temp.0.join("config.json")).unwrap(), config_before);
        assert!(lock(&state).proxy.is_some());
        assert!(r0_process_is_running(proxy_pid));
        let _ = lock(&state).stop_proxy();
    }

    #[test]
    fn launch_snapshot_ignores_unrelated_config_but_detects_every_launch_boundary() {
        let temp = TempDir::new("launch-snapshot");
        let codex = config::Profile {
            id: "codex-profile".into(),
            name: "Codex display name".into(),
            template_id: "codex".into(),
            api_format: "openai_responses".into(),
            base_url: String::new(),
            model: String::new(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            notes: Some("ignored note".into()),
            ..Default::default()
        };
        let other = config::Profile {
            id: "other-profile".into(),
            name: "Other".into(),
            template_id: "glm".into(),
            api_format: "anthropic".into(),
            base_url: "https://example.test/anthropic".into(),
            model: "glm-test".into(),
            api_key: "ignored-api-key".into(),
            notes: Some("ignored".into()),
            ..Default::default()
        };
        config::update(&temp.0, |cfg| {
            cfg.profiles = vec![codex, other];
            cfg.active_id = "codex-profile".into();
            cfg.experimental_codex_enabled = true;
            cfg.secret = "private-path-secret".into();
        })
        .unwrap();
        let target = CodexPreflightTarget::ActiveProfile;
        let base = config::load_from(&temp.0).unwrap();
        let baseline = CodexLaunchSnapshot::from_config(&base, &target);
        let mut unrelated = base.clone();
        unrelated.pending_notice = Some("ignored notice".into());
        unrelated.profiles[0].name = "Renamed Codex".into();
        unrelated.profiles[0].notes = Some("changed note".into());
        unrelated.profiles[1].name = "Renamed other".into();
        unrelated.profiles[1].api_key = "changed-ignored-key".into();
        assert!(CodexLaunchSnapshot::from_config(&unrelated, &target) == baseline);

        type SnapshotMutation = Box<dyn Fn(&mut config::Config)>;
        let mutations: Vec<(&str, SnapshotMutation)> = vec![
            (
                "active_id",
                Box::new(|cfg| cfg.active_id = "other-profile".into()),
            ),
            (
                "profile.id",
                Box::new(|cfg| cfg.profiles[0].id = "changed-id".into()),
            ),
            (
                "profile.template_id",
                Box::new(|cfg| cfg.profiles[0].template_id = "changed-template".into()),
            ),
            (
                "profile.api_format",
                Box::new(|cfg| cfg.profiles[0].api_format = "changed-format".into()),
            ),
            (
                "profile.base_url",
                Box::new(|cfg| cfg.profiles[0].base_url = "https://changed.test".into()),
            ),
            (
                "profile.model_catalog",
                Box::new(|cfg| {
                    cfg.profiles[0]
                        .model_catalog
                        .push(crate::model_catalog::ModelRoute {
                            selector_id: "claude-csswitch-test-extra-0123456789ab".into(),
                            display_name: "Extra".into(),
                            upstream_model: "extra".into(),
                            supports_tools: Some(true),
                            ..Default::default()
                        })
                }),
            ),
            (
                "profile.default_model_route_id",
                Box::new(|cfg| cfg.profiles[0].default_model_route_id = "changed-route".into()),
            ),
            (
                "profile.role_bindings",
                Box::new(|cfg| cfg.profiles[0].role_bindings.sonnet = "changed-role".into()),
            ),
            (
                "profile.credential_source",
                Box::new(|cfg| {
                    cfg.profiles[0].credential_source =
                        crate::provider_contracts::CredentialSource::ApiKey
                }),
            ),
            (
                "profile.credential_ref",
                Box::new(|cfg| cfg.profiles[0].credential_ref = None),
            ),
            (
                "profile.model_policy",
                Box::new(|cfg| {
                    cfg.profiles[0].model_policy =
                        crate::provider_contracts::ModelPolicy::SavedCatalog
                }),
            ),
            (
                "experimental_codex_enabled",
                Box::new(|cfg| cfg.experimental_codex_enabled = false),
            ),
            (
                "codex_network",
                Box::new(|cfg| {
                    cfg.codex_network.mode = csswitch_codex_network::CodexNetworkMode::Custom;
                    cfg.codex_network.proxy_url = "http://127.0.0.1:8080".into();
                }),
            ),
            ("proxy_port", Box::new(|cfg| cfg.proxy_port += 1)),
            ("sandbox_port", Box::new(|cfg| cfg.sandbox_port += 1)),
            (
                "reuse_system_ssh",
                Box::new(|cfg| cfg.reuse_system_ssh = true),
            ),
            ("mode", Box::new(|cfg| cfg.mode = "changed-mode".into())),
            (
                "path_secret",
                Box::new(|cfg| cfg.secret = "changed-private-path-secret".into()),
            ),
        ];
        for (field, mutate) in mutations {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert!(
                CodexLaunchSnapshot::from_config(&changed, &target) != baseline,
                "launch snapshot missed {field}"
            );
        }

        let mut changed = base;
        changed.proxy_port += 1;
        config::save_to(&temp.0, &changed).unwrap();
        let error = verify_launch_snapshot_unchanged(&temp.0, &target, &baseline).unwrap_err();
        assert!(error.starts_with("config_changed_retry："));
    }

    #[test]
    fn sidecar_runner_uses_exact_args_clean_env_and_returns_safe_success() {
        let temp = TempDir::new("success");
        let output = success_json("status");
        let script = temp.script(&format!(
            "[ \"$#\" -eq 2 ]\n[ \"$1\" = \"codex-auth\" ]\n[ \"$2\" = \"status\" ]\n[ \"$HOME\" = \"{}\" ]\n[ -z \"${{CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE:-}}\" ]\n[ -z \"${{OPENAI_API_KEY:-}}\" ]\nprintf '%s\\n' '{}'",
            temp.0.display(),
            output
        ));
        let value = run_codex_auth_sidecar_at(&script, &temp.0, CodexAuthAction::Status).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "status");
        assert_eq!(value["status"]["authenticated"], true);
        let encoded = value.to_string();
        assert!(!encoded.contains("access_token"));
        assert!(!encoded.contains("refresh_token"));
    }

    #[test]
    fn sidecar_runner_returns_typed_failure_but_discards_untrusted_message_and_stderr() {
        let temp = TempDir::new("failure");
        let script = temp.script(
            "printf '%s\\n' 'secret-stderr' >&2\nprintf '%s\\n' '{\"schema_version\":3,\"ok\":false,\"command\":\"logout\",\"error\":{\"code\":\"oauth_denied\",\"message\":\"attacker supplied secret\",\"retryable\":false}}'\nexit 4",
        );
        let value = run_codex_auth_sidecar_at(&script, &temp.0, CodexAuthAction::Logout).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "oauth_denied");
        assert!(!value.to_string().contains("attacker supplied secret"));
        assert!(!value.to_string().contains("secret-stderr"));

        let identity = br#"{"schema_version":3,"ok":false,"command":"status","error":{"code":"identity_mismatch","message":"Gateway code identity mismatch.","retryable":false,"stage":"identity_check"}}"#;
        let identity = parse_sidecar_output(identity, CodexAuthAction::Status, Some(8)).unwrap();
        let typed = require_authenticated_status_typed(&identity).unwrap_err();
        assert_eq!(typed.code, "codex_auth_unavailable");
        assert_eq!(typed.cause, Some("identity_mismatch"));
        assert!(!typed.retryable);
    }

    #[test]
    fn sidecar_protocol_rejects_multiline_mismatch_unknown_fields_and_oversize() {
        let success = success_json("status");
        assert!(parse_sidecar_output(
            format!("{success}\n{success}\n").as_bytes(),
            CodexAuthAction::Status,
            Some(0)
        )
        .is_err());
        assert!(parse_sidecar_output(
            success_json("logout").as_bytes(),
            CodexAuthAction::Status,
            Some(0)
        )
        .is_err());
        let missing_reason = success.replacen(",\"reason\":\"ready\"", "", 1);
        assert!(
            parse_sidecar_output(missing_reason.as_bytes(), CodexAuthAction::Status, Some(0))
                .is_err()
        );
        let v2_with_v3_status = success.replacen("\"schema_version\":3", "\"schema_version\":2", 1);
        assert!(parse_sidecar_output(
            v2_with_v3_status.as_bytes(),
            CodexAuthAction::Status,
            Some(0)
        )
        .is_err());
        let extra = success.replacen(
            "\"schema_version\":3",
            "\"schema_version\":3,\"token\":\"must-reject\"",
            1,
        );
        assert!(parse_sidecar_output(extra.as_bytes(), CodexAuthAction::Status, Some(0)).is_err());

        let denied = br#"{"schema_version":3,"ok":false,"command":"logout","error":{"code":"oauth_denied","message":"denied","retryable":false}}"#;
        assert!(parse_sidecar_output(denied, CodexAuthAction::Logout, Some(7)).is_err());
        assert!(parse_sidecar_output(denied, CodexAuthAction::Logout, None).is_err());
        assert!(parse_sidecar_output(denied, CodexAuthAction::Logout, Some(4)).is_ok());

        let temp = TempDir::new("oversize");
        let script = temp.script(
            "i=0\nwhile [ \"$i\" -lt 70000 ]; do printf x; i=$((i + 1)); done\nprintf '\\n'",
        );
        assert!(run_codex_auth_sidecar_at(&script, &temp.0, CodexAuthAction::Status).is_err());
    }

    #[test]
    fn sidecar_v3_round_trips_every_status_reason_and_rejects_illegal_combinations() {
        for (reason, authenticated, generation) in [
            ("ready", true, 7),
            ("state_missing", false, 0),
            ("state_uncommitted", false, 0),
            ("oauth_missing", false, 7),
            ("thinking_missing", false, 7),
            ("record_mismatch", false, 7),
        ] {
            let encoded = status_json(reason, authenticated, generation);
            let parsed = parse_sidecar_output(encoded.as_bytes(), CodexAuthAction::Status, Some(0))
                .unwrap_or_else(|error| panic!("reason {reason} rejected: {error}"));
            assert_eq!(parsed["status"]["reason"], reason);
        }

        let ready = status_json("ready", true, 7);
        let invalid = [
            status_json("oauth_missing", true, 7),
            status_json("ready", false, 7),
            status_json("oauth_missing", false, 0),
            status_json("state_missing", false, 1),
            ready.replacen("\"expires_at\":2000000000", "\"expires_at\":null", 1),
            ready.replacen("\"reason\":\"ready\"", "\"reason\":\"unknown\"", 1),
        ];
        for encoded in invalid {
            assert!(
                parse_sidecar_output(encoded.as_bytes(), CodexAuthAction::Status, Some(0),)
                    .is_err()
            );
        }
    }

    #[test]
    fn all_v2_login_event_kinds_and_error_envelopes_fail_closed() {
        let operation_id = "ad".repeat(16);
        for (name, line) in [
            (
                "progress",
                format!("{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"progress\",\"state\":\"waiting\"}}"),
            ),
            (
                "terminal",
                format!("{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":true,\"reason\":\"ready\",\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":1}}}}", "ab".repeat(16), "cd".repeat(16)),
            ),
            (
                "error-terminal",
                format!("{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"failed\",\"error\":{{\"code\":\"oauth_denied\",\"stage\":\"token_exchange\",\"retryable\":false}}}}"),
            ),
        ] {
            let temp = TempDir::new(name);
            let script = temp.script(&format!("printf '%s\\n' '{line}'"));
            let process = spawn_codex_auth_sidecar_at(
                &script,
                &temp.0,
                CodexAuthAction::LoginBrowser,
                None,
                Some(&operation_id),
                false,
            )
            .unwrap();
            let cancel = AtomicBool::new(false);
            assert!(wait_for_login_sidecar(
                process,
                CodexAuthAction::LoginBrowser,
                &operation_id,
                &cancel,
                |_| {},
                |_| {},
            )
            .is_err());
        }

        let cancel_temp = TempDir::new("cancel-ack");
        let cancel_script = cancel_temp.script(&format!(
            "IFS= read -r cancel\nprintf '%s\\n' '{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"cancel_ack\",\"disposition\":\"accepted\"}}'"
        ));
        let cancel_process = spawn_codex_auth_sidecar_at(
            &cancel_script,
            &cancel_temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let cancel = AtomicBool::new(true);
        assert!(wait_for_login_sidecar(
            cancel_process,
            CodexAuthAction::LoginBrowser,
            &operation_id,
            &cancel,
            |_| {},
            |_| {},
        )
        .is_err());

        let v2_error = br#"{"schema_version":2,"ok":false,"command":"status","error":{"code":"auth_storage_error","message":"safe","retryable":true}}"#;
        assert!(parse_sidecar_output(v2_error, CodexAuthAction::Status, Some(6)).is_err());
    }

    #[test]
    fn login_terminal_rejects_missing_and_unknown_status_reason() {
        let operation_id = "ae".repeat(16);
        for (name, status) in [
            (
                "missing-reason",
                format!("{{\"authenticated\":true,\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":1}}", "ab".repeat(16), "cd".repeat(16)),
            ),
            (
                "unknown-reason",
                format!("{{\"authenticated\":true,\"reason\":\"future\",\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":1}}", "ab".repeat(16), "cd".repeat(16)),
            ),
        ] {
            let temp = TempDir::new(name);
            let line = format!("{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{status}}}");
            let script = temp.script(&format!("printf '%s\\n' '{line}'"));
            let process = spawn_codex_auth_sidecar_at(
                &script,
                &temp.0,
                CodexAuthAction::LoginBrowser,
                None,
                Some(&operation_id),
                false,
            )
            .unwrap();
            let cancel = AtomicBool::new(false);
            assert!(wait_for_login_sidecar(
                process,
                CodexAuthAction::LoginBrowser,
                &operation_id,
                &cancel,
                |_| {},
                |_| {},
            )
            .is_err());
        }
    }

    #[test]
    fn sidecar_supervisor_times_out_running_process_and_inherited_stdout() {
        let temp = TempDir::new("timeout-running");
        let running = temp.script("exec /bin/sleep 2");
        let started = Instant::now();
        assert!(run_codex_auth_sidecar_at_with_timeout(
            &running,
            &temp.0,
            CodexAuthAction::Status,
            Duration::from_millis(75),
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));

        let inherited = TempDir::new("timeout-inherited-stdout");
        let output = success_json("status");
        let script = inherited.script(&format!(
            "(/bin/sleep 2) &\nprintf '%s\\n' '{}'\nexit 0",
            output
        ));
        let started = Instant::now();
        assert!(run_codex_auth_sidecar_at_with_timeout(
            &script,
            &inherited.0,
            CodexAuthAction::Status,
            Duration::from_millis(75),
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn interactive_status_cancellation_reaps_the_child_promptly() {
        let temp = TempDir::new("status-cancel");
        let script = temp.script("exec /bin/sleep 5");
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::Status,
            None,
            None,
            false,
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_writer = cancel.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel_writer.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert_eq!(
            wait_for_single_sidecar_response_controlled(
                process,
                CodexAuthAction::Status,
                Duration::from_secs(120),
                Some(cancel.as_ref()),
            ),
            Err(SidecarWaitFailure::Cancelled)
        );
        cancel_thread.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[cfg(unix)]
    fn login_shutdown_race_reaps_child_when_pid_registration_is_rejected() {
        let temp = TempDir::new("login-register-shutdown");
        let script = temp.script("exec /bin/sleep 5");
        let operation_id = "af".repeat(16);
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let pid = process.child.id();
        let supervisor = CodexAuthSupervisor::default();
        let reservation = supervisor.begin_login().unwrap();
        assert!(supervisor.cancel_for_exit().is_empty());
        let started = Instant::now();
        assert!(register_login_process(&supervisor, &reservation.operation_id, process).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        supervisor.abort_login_start(&reservation.operation_id);
    }

    #[test]
    fn login_sidecar_ndjson_replays_progress_and_one_terminal() {
        let temp = TempDir::new("login-ndjson");
        let operation_id = "ab".repeat(16);
        let script = temp.script(&format!(
            "[ \"$CSSWITCH_CODEX_AUTH_OPERATION_ID\" = \"{operation_id}\" ]\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"progress\",\"state\":\"waiting\"}}'\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"progress\",\"state\":\"exchanging\"}}'\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":true,\"reason\":\"ready\",\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":1}}}}'",
            "ab".repeat(16),
            "cd".repeat(16),
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut states = Vec::new();
        let value = wait_for_login_sidecar(
            process,
            CodexAuthAction::LoginBrowser,
            &operation_id,
            &cancel,
            |event| states.push(event.state.clone().unwrap()),
            |_| {},
        )
        .unwrap();
        assert_eq!(states, vec!["waiting", "exchanging"]);
        assert_eq!(value["ok"], true);
        assert_eq!(value["state"], "succeeded");
    }

    #[test]
    fn login_success_terminal_requires_authenticated_status() {
        let temp = TempDir::new("login-success-requires-authenticated");
        let operation_id = "bc".repeat(16);
        let script = temp.script(&format!(
            "printf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":false,\"reason\":\"state_missing\",\"account_hash\":null,\"expiry_state\":\"missing\",\"expires_at\":null,\"auth_epoch\":null,\"auth_generation\":0}}}}'"
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let error = wait_for_login_sidecar(
            process,
            CodexAuthAction::LoginBrowser,
            &operation_id,
            &cancel,
            |_| {},
            |_| {},
        )
        .unwrap_err();
        assert_eq!(error, "Codex 认证成功终态必须包含已登录状态。");
    }

    #[test]
    fn login_finalization_requires_profile_ready_before_succeeded() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let reservation = supervisor.begin_login().unwrap();
        let snapshot = finalize_login_operation(
            &supervisor,
            &lifecycle,
            &reservation.operation_id,
            Ok(json!({ "ok": true, "state": "succeeded" })),
            || {
                Ok(crate::runtime::profile::EnsureCodexProfileResult {
                    disposition: crate::runtime::profile::EnsureCodexProfileDisposition::Created,
                    profile_id: "cd".repeat(16),
                })
            },
        )
        .unwrap();
        assert_eq!(snapshot.state, "succeeded");
        assert!(snapshot.error.is_none());

        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = supervisor.begin_login().unwrap();
        let snapshot = finalize_login_operation(
            &supervisor,
            &lifecycle,
            &reservation.operation_id,
            Ok(json!({ "ok": true, "state": "succeeded" })),
            || Err("simulated config commit failure".into()),
        )
        .unwrap();
        assert_eq!(snapshot.state, "failed");
        let error = snapshot.error.unwrap();
        assert_eq!(error.code, "profile_ensure_failed");
        assert_eq!(error.stage, "profile_ensure");
        assert!(error.retryable);
    }

    #[test]
    fn failed_or_cancelled_login_never_runs_profile_ensure() {
        for (state, code) in [("failed", "oauth_denied"), ("cancelled", "auth_cancelled")] {
            let supervisor = Arc::new(CodexAuthSupervisor::default());
            let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
            let reservation = supervisor.begin_login().unwrap();
            let called = Arc::new(AtomicBool::new(false));
            let called_by_ensure = called.clone();
            let snapshot = finalize_login_operation(
                &supervisor,
                &lifecycle,
                &reservation.operation_id,
                Ok(json!({
                    "ok": false,
                    "state": state,
                    "error": {
                        "code": code,
                        "stage": if state == "cancelled" { "cancelled" } else { "browser_open" },
                        "retryable": true
                    }
                })),
                move || {
                    called_by_ensure.store(true, Ordering::SeqCst);
                    Err("must not run".into())
                },
            )
            .unwrap();
            assert_eq!(snapshot.state, state);
            assert!(!called.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn terminal_ensure_races_repair_manual_create_logout_and_disable_without_deadlock() {
        let temp = TempDir::new("profile-concurrency");
        let active = crate::runtime::profile::create_profile_inner(
            &temp.0,
            "glm",
            "当前 GLM",
            Some("gk"),
            None,
            Some("glm-5.2"),
        )
        .unwrap();
        config::update(&temp.0, |cfg| {
            cfg.active_id = active.clone();
            cfg.experimental_codex_enabled = true;
        })
        .unwrap();

        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let reservation = supervisor.begin_login().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(6));
        let (sender, receiver) = std::sync::mpsc::channel();
        let ensured = Arc::new(Mutex::new(None));
        let mut workers = Vec::new();

        {
            let dir = temp.0.clone();
            let supervisor = supervisor.clone();
            let lifecycle = lifecycle.clone();
            let operation_id = reservation.operation_id.clone();
            let barrier = barrier.clone();
            let sender = sender.clone();
            let ensured = ensured.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let snapshot = finalize_login_operation(
                    &supervisor,
                    &lifecycle,
                    &operation_id,
                    Ok(json!({ "ok": true, "state": "succeeded" })),
                    || {
                        let result = crate::runtime::profile::ensure_codex_profile_inner(&dir)?;
                        *ensured.lock().unwrap_or_else(|error| error.into_inner()) =
                            Some(result.clone());
                        Ok(result)
                    },
                );
                let state = snapshot
                    .map(|snapshot| snapshot.state)
                    .unwrap_or_else(|_| "error".into());
                sender.send(format!("terminal:{state}")).unwrap();
            }));
        }

        {
            let dir = temp.0.clone();
            let lifecycle = lifecycle.clone();
            let barrier = barrier.clone();
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let result = lifecycle.with_serialized(|| {
                    crate::runtime::profile::create_profile_inner(
                        &dir,
                        "codex",
                        "手工 Codex",
                        None,
                        None,
                        None,
                    )
                });
                sender
                    .send(format!(
                        "manual:{}",
                        if result.is_ok() { "ok" } else { "busy" }
                    ))
                    .unwrap();
            }));
        }

        for action in ["repair", "logout", "disable"] {
            let dir = temp.0.clone();
            let supervisor = supervisor.clone();
            let lifecycle = lifecycle.clone();
            let barrier = barrier.clone();
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let result =
                    lifecycle.with_serialized(|| {
                        let _mutation = CodexAuthSupervisor::begin_mutation(&supervisor)?;
                        match action {
                            "repair" => crate::runtime::profile::ensure_codex_profile_inner(&dir)
                                .map(|_| ()),
                            "disable" => set_experimental_codex_enabled_at(&dir, false, || Ok(()))
                                .map(|_| ()),
                            _ => Ok(()),
                        }
                    });
                sender
                    .send(format!(
                        "{action}:{}",
                        if result.is_ok() { "ok" } else { "busy" }
                    ))
                    .unwrap();
            }));
        }

        barrier.wait();
        drop(sender);
        let mut outcomes = Vec::new();
        for _ in 0..5 {
            outcomes.push(
                receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("concurrent Codex mutation deadlocked"),
            );
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(outcomes
            .iter()
            .any(|outcome| outcome == "terminal:succeeded"));
        assert!(ensured
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some());
        let cfg = config::load_from(&temp.0).unwrap();
        assert_eq!(cfg.active_id, active);
        assert!(cfg.profiles.iter().any(|profile| {
            profile.template_id == "codex"
                && profile.credential_source
                    == crate::provider_contracts::CredentialSource::CsswitchOauth
        }));
    }

    #[test]
    fn accepted_cancel_watchdog_reaps_only_after_sidecar_ack() {
        let temp = TempDir::new("login-cancel-watchdog");
        let operation_id = "ef".repeat(16);
        let script = temp.script(&format!(
            "IFS= read -r cancel\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"cancel_ack\",\"disposition\":\"accepted\"}}'\nexec /bin/sleep 30"
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let mut ack = String::new();
        let value = wait_for_login_sidecar(
            process,
            CodexAuthAction::LoginBrowser,
            &operation_id,
            &cancel,
            |_| {},
            |value| ack = value.to_string(),
        )
        .unwrap();
        assert_eq!(ack, "accepted");
        assert_eq!(value["state"], "cancelled");
        let elapsed = started.elapsed();
        assert_eq!(ACCEPTED_CANCEL_WATCHDOG, Duration::from_secs(2));
        assert!(elapsed >= ACCEPTED_CANCEL_WATCHDOG);
        // The fixture exits naturally after 30 seconds. Finishing well before
        // that proves the acknowledged-cancel watchdog reaped it, while the
        // margin avoids mistaking full-suite scheduler pauses for a product
        // failure. The exact product budget remains asserted above.
        assert!(elapsed < Duration::from_secs(10));
    }

    #[test]
    fn status_consistency_rejects_hash_or_login_state_anomalies() {
        let missing_hash =
            success_json("status").replace(&format!("\"{}\"", "ab".repeat(16)), "null");
        assert!(
            parse_sidecar_output(missing_hash.as_bytes(), CodexAuthAction::Status, Some(0))
                .is_err()
        );
        let uppercase_hash = success_json("status").replace(&"ab".repeat(16), &"AB".repeat(16));
        assert!(
            parse_sidecar_output(uppercase_hash.as_bytes(), CodexAuthAction::Status, Some(0))
                .is_err()
        );
    }

    #[test]
    fn backend_readiness_rejects_unauthenticated_status_before_any_launch() {
        let authenticated: Value = serde_json::from_str(&success_json("status")).unwrap();
        assert!(require_authenticated_status_typed(&authenticated).is_ok());
        let unauthenticated = json!({
            "schema_version": 3,
            "ok": true,
            "command": "status",
            "status": {
                "authenticated": false,
                "reason": "state_missing",
                "account_hash": null,
                "expiry_state": "missing",
                "expires_at": null,
                "auth_epoch": null,
                "auth_generation": 0
            }
        });
        let error = require_authenticated_status_typed(&unauthenticated).unwrap_err();
        assert_eq!(error.code, "codex_login_required");
        assert_eq!(error.reason.as_deref(), Some("state_missing"));
    }

    #[test]
    fn structured_auth_errors_have_exact_reason_cause_and_retryability_contracts() {
        for reason in [
            "state_missing",
            "state_uncommitted",
            "oauth_missing",
            "thinking_missing",
            "record_mismatch",
        ] {
            let value = serde_json::to_value(RuntimeCommandError::from(
                CodexAuthCommandError::login_required(reason),
            ))
            .unwrap();
            assert_eq!(
                value,
                json!({"code":"codex_login_required","reason":reason,"retryable":false})
            );
        }
        for (cause, cause_name, retryable) in [
            (
                CodexAuthUnavailableCause::KeychainUnavailable,
                "keychain_unavailable",
                true,
            ),
            (
                CodexAuthUnavailableCause::InteractionTimeout,
                "interaction_timeout",
                true,
            ),
            (
                CodexAuthUnavailableCause::SidecarSpawnFailed,
                "sidecar_spawn_failed",
                false,
            ),
            (
                CodexAuthUnavailableCause::SidecarProtocolError,
                "sidecar_protocol_error",
                false,
            ),
            (
                CodexAuthUnavailableCause::IdentityMismatch,
                "identity_mismatch",
                false,
            ),
            (
                CodexAuthUnavailableCause::AuthStateInvalid,
                "auth_state_invalid",
                false,
            ),
            (
                CodexAuthUnavailableCause::StorageUnavailable,
                "storage_unavailable",
                true,
            ),
            (
                CodexAuthUnavailableCause::UnsupportedPlatform,
                "unsupported_platform",
                false,
            ),
            (
                CodexAuthUnavailableCause::AuthStateChanged,
                "auth_state_changed",
                true,
            ),
        ] {
            let value = serde_json::to_value(RuntimeCommandError::from(
                CodexAuthCommandError::unavailable(cause),
            ))
            .unwrap();
            assert_eq!(
                value,
                json!({"code":"codex_auth_unavailable","cause":cause_name,"retryable":retryable})
            );
        }
        assert_eq!(
            serde_json::to_value(RuntimeCommandError::from(CodexAuthCommandError::busy())).unwrap(),
            json!({"code":"codex_auth_busy","retryable":true})
        );
        assert_eq!(
            serde_json::to_value(RuntimeCommandError::from("ordinary failure")).unwrap(),
            json!("ordinary failure")
        );

        for failure in [SidecarWaitFailure::Cancelled, SidecarWaitFailure::Timeout] {
            let error = auth_error_from_sidecar_wait(failure);
            assert_eq!(error.cause, Some("interaction_timeout"));
            assert!(error.retryable);
        }
        let protocol = auth_error_from_sidecar_wait(SidecarWaitFailure::Protocol);
        assert_eq!(protocol.cause, Some("sidecar_protocol_error"));
        assert!(!protocol.retryable);
    }

    #[test]
    fn login_and_logout_replace_the_last_known_auth_observation() {
        let supervisor = CodexAuthSupervisor::default();
        let login = Ok(serde_json::from_str(&success_json("status")).unwrap());
        record_login_terminal_auth_status(&supervisor, &login);
        let ready = supervisor.last_auth_status().unwrap();
        assert_eq!(ready.status, "ready");
        assert_eq!(ready.reason.as_deref(), Some("ready"));
        assert_eq!(ready.cause, None);

        let logout = json!({
            "schema_version": 3,
            "ok": true,
            "command": "logout",
            "status": {
                "authenticated": false,
                "reason": "state_uncommitted",
                "account_hash": null,
                "expiry_state": "missing",
                "expires_at": null,
                "auth_epoch": "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
                "auth_generation": 8
            }
        });
        record_last_auth_status(&supervisor, &logout);
        let logged_out = supervisor.last_auth_status().unwrap();
        assert_eq!(logged_out.status, "not_authenticated");
        assert_eq!(logged_out.reason.as_deref(), Some("state_uncommitted"));
        assert_eq!(logged_out.cause, None);

        record_login_terminal_auth_status(&supervisor, &Err("protocol".into()));
        let unavailable = supervisor.last_auth_status().unwrap();
        assert_eq!(unavailable.status, "unavailable");
        assert_eq!(unavailable.reason, None);
        assert_eq!(unavailable.cause.as_deref(), Some("sidecar_protocol_error"));
    }

    #[test]
    fn profile_repair_requires_authentication_and_is_idempotent() {
        let temp = TempDir::new("profile-repair");
        config::update(&temp.0, |cfg| cfg.experimental_codex_enabled = true).unwrap();
        let unauthenticated = json!({
            "ok": true,
            "status": {
                "authenticated": false,
                "reason": "state_missing",
                "account_hash": null,
                "expiry_state": "missing",
                "expires_at": null,
                "auth_epoch": null,
                "auth_generation": 0
            }
        });
        assert!(require_authenticated_status_typed(&unauthenticated).is_err());
        assert!(config::load_from(&temp.0).unwrap().profiles.is_empty());

        let authenticated: Value = serde_json::from_str(&success_json("status")).unwrap();
        require_authenticated_status_typed(&authenticated).unwrap();
        let created = ensure_codex_profile_authenticated(&temp.0).unwrap();
        assert_eq!(created["disposition"], "created");
        assert!(created["profile_id"]
            .as_str()
            .is_some_and(|id| is_lower_hex(id, 32)));
        let existing = ensure_codex_profile_authenticated(&temp.0).unwrap();
        assert_eq!(existing["disposition"], "existing");
        assert_eq!(existing["profile_id"], created["profile_id"]);
    }

    #[test]
    fn profile_repair_exposes_only_safe_failure_when_config_is_unwritable() {
        let temp = TempDir::new("profile-repair-save-failure");
        config::update(&temp.0, |cfg| cfg.experimental_codex_enabled = true).unwrap();
        let config_path = temp.0.join("config.json");
        let preserved = temp.0.join("preserved-config.json");
        fs::rename(&config_path, &preserved).unwrap();
        std::os::unix::fs::symlink(&preserved, &config_path).unwrap();
        let error = ensure_codex_profile_authenticated(&temp.0).unwrap_err();
        assert_eq!(
            error,
            "profile_ensure_failed：授权已保存，但无法创建 Codex 配置；请重试。"
        );
        assert!(!error.contains(preserved.to_string_lossy().as_ref()));
    }

    #[test]
    fn diagnostic_summary_uses_only_last_known_in_memory_status() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let app = tauri::test::mock_builder()
            .manage(supervisor.clone() as SharedCodexAuthSupervisor)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        assert_eq!(
            codex_auth_diagnostic_summary(app.handle()),
            "auth=not_checked"
        );

        supervisor.record_auth_status("ready", Some("ready"), None);
        let ready = codex_auth_diagnostic_summary(app.handle());
        assert!(ready.starts_with("auth=last_known_ready age_seconds="));
        assert!(ready.ends_with("reason=ready"));

        supervisor.record_auth_status("unavailable", None, Some("keychain_unavailable"));
        let unavailable = codex_auth_diagnostic_summary(app.handle());
        assert!(unavailable.starts_with("auth=last_known_unavailable age_seconds="));
        assert!(unavailable.ends_with("cause=keychain_unavailable"));
        for secret in [
            "account",
            "epoch",
            "generation",
            "token",
            "person@example.test",
        ] {
            assert!(!unavailable.contains(secret));
        }
    }

    #[test]
    fn downgrade_preview_and_confirmation_are_complete_and_secret_free() {
        let codex = config::Profile {
            id: "codex-1".into(),
            name: "My Codex".into(),
            template_id: "codex".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            ..Default::default()
        };
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::new_profile_catalog(
                "deepseek",
                "anthropic",
                Some("deepseek-v4-pro"),
            )
            .unwrap();
        let api = config::Profile {
            id: "api-1".into(),
            name: "DeepSeek".into(),
            template_id: "deepseek".into(),
            api_format: "anthropic".into(),
            api_key: "must-never-appear".into(),
            model: "deepseek-v4-pro".into(),
            model_catalog,
            default_model_route_id,
            role_bindings,
            model_policy: crate::provider_contracts::ModelPolicy::SavedCatalog,
            ..Default::default()
        };
        let preview_cfg = config::Config {
            profiles: vec![api.clone(), codex.clone()],
            active_id: codex.id.clone(),
            ..Default::default()
        };
        let preview = codex_downgrade_preview_for(&preview_cfg).unwrap();
        assert_eq!(preview["profile_count"], 1);
        assert_eq!(preview["active_will_clear"], true);
        assert_eq!(preview["credentials_unchanged"], true);
        let encoded = preview.to_string();
        assert!(!encoded.contains("must-never-appear"));
        assert!(!encoded.contains("credential_ref"));
        assert_eq!(preview["catalog_export_count"], 1);
        let fingerprint = preview["preview_fingerprint"].as_str().unwrap().to_string();

        let cfg = config::Config {
            profiles: vec![api, codex],
            active_id: "codex-1".into(),
            ..Default::default()
        };
        let actions =
            downgrade_actions_for_expected(&cfg, &["codex-1".into()], &fingerprint).unwrap();
        assert_eq!(
            actions.get("codex-1"),
            Some(&config::CodexDowngradeAction::ExportThenRemove)
        );
        assert!(downgrade_actions_for_expected(&cfg, &[], &fingerprint).is_err());
        assert!(downgrade_actions_for_expected(
            &cfg,
            &["codex-1".into(), "codex-1".into()],
            &fingerprint,
        )
        .is_err());
        assert!(downgrade_actions_for_expected(&cfg, &["other".into()], &fingerprint).is_err());
        assert!(downgrade_actions_for_expected(&cfg, &["codex-1".into()], "stale").is_err());
    }

    fn run_exact_ignored_codex_characterization(case: &str) {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let child_name = "commands::codex::tests::isolated_r0_codex_mutation_command_contract";
        let output = Command::new(env::current_exe().unwrap())
            .arg("--exact")
            .arg(child_name)
            .arg("--ignored")
            .arg("--nocapture")
            .env("CSSWITCH_TEST_R0_F_CASE", case)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success()
                && stdout.lines().any(|line| line == "running 1 test")
                && stdout
                    .lines()
                    .any(|line| line == format!("test {child_name} ... ok")),
            "isolated R0-F {case} characterization failed:\nstdout={}\nstderr={}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn r0_listener() -> TcpListener {
        loop {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            if listener.local_addr().unwrap().port() != 8765 {
                return listener;
            }
        }
    }

    fn r0_distinct_ports() -> (u16, u16) {
        let proxy = r0_listener();
        let sandbox = r0_listener();
        (
            proxy.local_addr().unwrap().port(),
            sandbox.local_addr().unwrap().port(),
        )
    }

    fn r0_codex_config(home: &Path) -> PathBuf {
        env::set_var("HOME", home);
        let dir = config::default_dir();
        let (proxy_port, sandbox_port) = r0_distinct_ports();
        let mut cfg = config::Config {
            experimental_codex_enabled: true,
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        let profile = config::Profile {
            id: "codex-r0-f".into(),
            name: "Codex R0-F".into(),
            template_id: "codex".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            ..Default::default()
        };
        cfg.active_id = profile.id.clone();
        cfg.profiles.push(profile);
        config::save_to(&dir, &cfg).unwrap();
        dir
    }

    fn o1_e1_codex_compensation_marker() -> config::RuntimeCompensationJournal {
        config::RuntimeCompensationJournal {
            schema_version: config::RUNTIME_COMPENSATION_SCHEMA_VERSION_V1,
            compensation_id: "o1-e1-codex-guard".into(),
            target_profile_id: "codex-r0-f".into(),
            runtime_fingerprint: "d".repeat(64),
            snapshot_ticket: config::RuntimeSnapshotTicket::verified(
                ".one-click-rollback-fedcba9876543210fedcba9876543210".into(),
            )
            .unwrap(),
            state: config::RuntimeCompensationState::InProgress,
            steps: Vec::new(),
            science_adoption_attempt_ids: Vec::new(),
        }
    }

    fn r0_proxy_state(provider: &str) -> (SharedAppState, u32) {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated R0-F Gateway fixture");
        let pid = child.id();
        let mut state = AppState::default();
        state.proxy = Some(child);
        state.provider = provider.into();
        (Arc::new(Mutex::new(state)), pid)
    }

    fn r0_process_is_running(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn p2a_terminate_exact_uncommitted_science(port: u16, pid: u32, process_start: &str) {
        assert!(pid > 1, "isolated fake Science PID must be signal-safe");
        assert_eq!(
            crate::runtime::science::test_unique_listener_pid(port),
            Some(pid),
            "test cleanup may signal only the captured isolated listener"
        );
        assert_eq!(
            crate::runtime::science::test_process_start_identity_for_pid(pid).as_deref(),
            Some(process_start),
            "test cleanup requires the captured process-start identity"
        );
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
        for _ in 0..100 {
            if crate::runtime::science::test_unique_listener_pid(port) != Some(pid)
                && crate::runtime::science::test_process_start_identity_for_pid(pid).as_deref()
                    != Some(process_start)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if crate::runtime::science::test_unique_listener_pid(port) == Some(pid)
            && crate::runtime::science::test_process_start_identity_for_pid(pid).as_deref()
                == Some(process_start)
        {
            assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        }
        for _ in 0..100 {
            if crate::runtime::science::test_unique_listener_pid(port) != Some(pid)
                && crate::runtime::science::test_process_start_identity_for_pid(pid).as_deref()
                    != Some(process_start)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("captured isolated fake Science process survived exact test cleanup");
    }

    fn run_exact_p2a_codex_disable_case(case: &str) {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let child_name =
            "commands::codex::tests::isolated_p2a_codex_disable_durable_mutation_receipt";
        let output = Command::new(env::current_exe().unwrap())
            .arg("--exact")
            .arg(child_name)
            .arg("--ignored")
            .arg("--nocapture")
            .env("CSSWITCH_TEST_P2A_CASE", case)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success()
                && stdout.lines().any(|line| line == "running 1 test")
                && stdout
                    .lines()
                    .any(|line| line == format!("test {child_name} ... ok")),
            "isolated P2-A {case} failed:\nstdout={}\nstderr={}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn p2a_install_gateway_wrapper(temp: &TempDir, real_gateway: &Path) -> PathBuf {
        let wrapper = temp.named_script(
            "p2a-gateway-wrapper",
            &format!(
                r#"if [ "$1" = "codex-auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"schema_version":3,"ok":true,"command":"status","status":{{"authenticated":true,"reason":"ready","account_hash":"abababababababababababababababab","expiry_state":"valid","expires_at":2000000000,"auth_epoch":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","auth_generation":1}}}}'
  exit 0
fi
exec '{}' "$@""#,
                real_gateway.display()
            ),
        );
        let wrapper = wrapper.canonicalize().unwrap();
        env::set_var("CSSWITCH_GATEWAY_BIN", &wrapper);
        wrapper
    }

    fn p2a_start_gateway<R: tauri::Runtime>(
        app: &tauri::AppHandle<R>,
        state: &SharedAppState,
        lifecycle: &crate::lifecycle::Lifecycle,
    ) -> (u32, String) {
        let prepared = prepare_provider_auth(app, "codex", CodexPreflightTarget::ActiveProfile)
            .unwrap()
            .unwrap();
        let cfg = config::load_from(&config::default_dir()).unwrap();
        let profile = cfg.active_profile().unwrap().clone();
        GatewayController::start_for(
            app,
            state,
            lifecycle,
            &profile,
            None,
            None,
            Some(prepared.proof()),
        )
        .unwrap();
        let current = lock(state);
        (
            current.proxy.as_ref().unwrap().id(),
            current.launch_id.clone(),
        )
    }

    fn p2a_stop_test_gateway(state: &SharedAppState) {
        let outcome = lock(state).stop_proxy();
        assert!(matches!(outcome, crate::GatewayStopOutcome::Stopped));
    }

    fn p2a_stop_test_science<R: tauri::Runtime>(app: &tauri::AppHandle<R>, state: &SharedAppState) {
        let runtime = lock(state)
            .science_runtime
            .clone()
            .expect("test cleanup requires tracked Science runtime");
        let cfg = config::load_from(&config::default_dir()).unwrap();
        let token = ScienceHostAdapter::managed_receipt(cfg.sandbox_port, &runtime)
            .expect("test cleanup requires exact managed Science receipt");
        let request = ScienceStopRequest::exact(
            &runtime,
            ScienceStopOwnershipReceipt::from_managed_launch(&token),
        );
        let outcome = ScienceHostAdapter::execute_stop(app, request)
            .into_parts()
            .0;
        assert!(
            outcome.is_ok(),
            "managed Science cleanup failed: {outcome:?}"
        );
        let mut current = lock(state);
        current.science_runtime = None;
        current.science_confirmed_stopped = Some(runtime);
        current.sandbox_url = None;
    }

    #[test]
    fn p2a_codex_disable_managed_runtime_contract() {
        for case in [
            "success",
            "science-success",
            "science-replay-stop",
            "science-stopping-terminal",
            "science-replay-both",
            "science-partial-restore",
            "science-restore-uncertain",
            "science-replacement",
            "science-config-drift",
            "science-generation-drift",
            "open-conflicts",
            "stop-failure",
            "config-restore",
            "restore-attention",
            "other-provider",
            "replay-intent",
            "replay-stop",
            "gateway-phase-drift",
            "gateway-stopping-aba",
            "replay-commit",
            "replay-replacement",
        ] {
            run_exact_p2a_codex_disable_case(case);
        }
    }

    #[test]
    #[ignore = "source-gate parent executes exact isolated P2-A Codex disable receipt cases with temp HOME, managed local Gateway, fake auth sidecar, and dynamic loopback ports"]
    fn isolated_p2a_codex_disable_durable_mutation_receipt() {
        let requested = env::var("CSSWITCH_TEST_P2A_CASE").unwrap_or_default();
        assert!(matches!(
            requested.as_str(),
            "success"
                | "science-success"
                | "science-replay-stop"
                | "science-stopping-terminal"
                | "science-replay-both"
                | "science-partial-restore"
                | "science-restore-uncertain"
                | "science-replacement"
                | "science-config-drift"
                | "science-generation-drift"
                | "open-conflicts"
                | "stop-failure"
                | "config-restore"
                | "restore-attention"
                | "other-provider"
                | "replay-intent"
                | "replay-stop"
                | "gateway-phase-drift"
                | "gateway-stopping-aba"
                | "replay-commit"
                | "replay-replacement"
        ));
        let temp = TempDir::new(&format!("p2a-{requested}"));
        let home = temp.0.canonicalize().unwrap().join("home");
        fs::create_dir_all(&home).unwrap();
        let config_dir = r0_codex_config(&home);
        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone() as SharedLifecycle)
            .manage(supervisor.clone() as SharedCodexAuthSupervisor)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let real_gateway = gateway_bin_path(app.handle()).expect("local test Gateway must exist");
        let wrapper = p2a_install_gateway_wrapper(&temp, &real_gateway);

        if requested == "open-conflicts" {
            let receipt = p2a_receipt_fixture(&config_dir);
            let before = config::load_from(&config_dir).unwrap();
            let open =
                OpenCodexDisableReceipt::publish_intent(&config_dir, &before, receipt).unwrap();
            let one_click =
                match crate::runtime::sandbox_session::OneClickEntryPreflight::capture(&state) {
                    Ok(_) => panic!("open Codex disable fence must block one-click preflight"),
                    Err(error) => error,
                };
            assert_eq!(
                one_click.kind(),
                crate::runtime::failure::OneClickFailureKind::Prepare
            );
            assert!(
                crate::runtime::profile::ensure_codex_profile_inner(&config_dir)
                    .unwrap_err()
                    .contains("codex_disable_operation_in_progress")
            );
            let writer = config::update(&config_dir, |current| {
                current.codex_network = csswitch_codex_network::CodexNetworkSettings::default();
                current.mode = "official".into();
            });
            assert!(writer
                .unwrap_err()
                .to_string()
                .contains("Codex disable operation"));
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            open.clear_before(&config_dir).unwrap();
            return;
        }

        if requested.starts_with("science-") {
            let bin_dir = temp.0.join("bin");
            let fake_science = crate::commands::runtime::tests::write_test_bins(&bin_dir)
                .canonicalize()
                .unwrap();
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            env::set_var("SCIENCE_BIN", &fake_science);
            env::set_var("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
            env::set_var("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
            env::set_var("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
            env::set_var("CSSWITCH_REPO", root);
            env::set_var(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    bin_dir.to_string_lossy()
                ),
            );
            let prepared =
                prepare_provider_auth(app.handle(), "codex", CodexPreflightTarget::ActiveProfile)
                    .unwrap()
                    .unwrap();
            let _catalog = crate::runtime::sandbox_session::test_arm_gateway_catalog_bypass(
                config::load_from(&config_dir).unwrap().proxy_port,
            );
            lifecycle
                .with_mutation(RuntimeMutationDomain::Destructive, |_| {
                    crate::runtime::sandbox_session::one_click_login(
                        app.handle().clone(),
                        state.clone(),
                        lifecycle.as_ref(),
                        None,
                        Some(prepared.proof()),
                    )
                })
                .unwrap();
            drop(prepared);
            let prior_runtime = lock(&state)
                .science_runtime
                .clone()
                .expect("fake managed Science must be tracked");
            let sandbox_port = config::load_from(&config_dir).unwrap().sandbox_port;
            let prior_token = ScienceHostAdapter::managed_receipt(sandbox_port, &prior_runtime)
                .expect("fake managed Science receipt must exist");
            assert!(ScienceHostAdapter::receipt_is_current(
                &prior_token,
                &prior_runtime
            ));
            let (prior_gateway_pid, prior_gateway_launch_id) = {
                let current = lock(&state);
                (
                    current.proxy.as_ref().unwrap().id(),
                    current.launch_id.clone(),
                )
            };

            match requested.as_str() {
                "science-success" => {
                    let result =
                        lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                            execute_experimental_codex_enabled(
                                app.handle(),
                                &state,
                                lifecycle.as_ref(),
                                &supervisor,
                                false,
                            )
                        });
                    assert_eq!(result.unwrap()["experimental_codex_enabled"], false);
                    assert!(lock(&state).science_runtime.is_none());
                    assert_eq!(
                        ScienceHostAdapter::probe_known(sandbox_port, &prior_runtime),
                        SandboxScienceState::Stopped
                    );
                    assert!(read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .is_none());
                }
                "science-replay-stop" => {
                    let plan =
                        plan_experimental_codex_disable(&config_dir, &state, &lifecycle).unwrap();
                    let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
                        panic!("managed Science and Gateway must produce a stop plan")
                    };
                    let CodexDisableStopPlan {
                        before_config,
                        receipt,
                        science_owner,
                        science,
                        gateway,
                    } = *plan;
                    let science_plan = receipt.plan.science.clone().unwrap();
                    let mut open = OpenCodexDisableReceipt::publish_intent(
                        &config_dir,
                        &before_config,
                        receipt,
                    )
                    .unwrap();
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::Stopping {
                            component: CodexDisableComponent::Science,
                            science_stopped: false,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    let proof =
                        acquire_codex_disable_effect_lease(&config_dir, &open.record, &lifecycle)
                            .unwrap();
                    execute_planned_codex_science_stop(
                        app.handle(),
                        &state,
                        &lifecycle,
                        science_owner,
                        science.unwrap(),
                    )
                    .unwrap();
                    drop(proof);
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::EffectsApplied {
                            science_stopped: true,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    drop(gateway);
                    let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                    let fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let fresh_app = tauri::test::mock_builder()
                        .manage(fresh_state.clone())
                        .manage(fresh_lifecycle.clone() as SharedLifecycle)
                        .manage(fresh_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    assert!(replay_interrupted_codex_disable(fresh_app.handle()).is_none());
                    assert!(replay_interrupted_codex_disable(fresh_app.handle()).is_none());
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Restored
                    );
                    assert!(r0_process_is_running(prior_gateway_pid));
                    assert_eq!(lock(&state).launch_id, prior_gateway_launch_id);
                    p2a_stop_test_science(fresh_app.handle(), &fresh_state);
                    p2a_stop_test_gateway(&state);
                }
                "science-stopping-terminal" => {
                    let plan =
                        plan_experimental_codex_disable(&config_dir, &state, &lifecycle).unwrap();
                    let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
                        panic!("managed Science and Gateway must produce a stop plan")
                    };
                    let CodexDisableStopPlan {
                        before_config,
                        receipt,
                        science,
                        gateway,
                        ..
                    } = *plan;
                    assert!(science.is_some());
                    assert!(gateway.is_some());
                    let mut open = OpenCodexDisableReceipt::publish_intent(
                        &config_dir,
                        &before_config,
                        receipt,
                    )
                    .unwrap();
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::Stopping {
                            component: CodexDisableComponent::Science,
                            science_stopped: false,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    drop(science);
                    drop(gateway);

                    // The existing terminal cleanup path is intentionally
                    // available without a runtime journal.  Its stop must not
                    // be misattributed to the open P2-A pre-effect WAL.
                    crate::commands::runtime::stop_all_inner_cmd_for_test(
                        app.handle().clone(),
                        state.clone(),
                        lifecycle.clone(),
                    )
                    .unwrap();
                    assert!(!r0_process_is_running(prior_gateway_pid));
                    assert_eq!(
                        ScienceHostAdapter::probe_known(sandbox_port, &prior_runtime),
                        SandboxScienceState::Stopped
                    );

                    let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                    let fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let fresh_app = tauri::test::mock_builder()
                        .manage(fresh_state.clone())
                        .manage(fresh_lifecycle.clone() as SharedLifecycle)
                        .manage(fresh_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    for _ in 0..2 {
                        let attention = replay_interrupted_codex_disable(fresh_app.handle())
                            .expect("terminal stop after pre-effect WAL must retain attention");
                        assert_eq!(attention["status"], "attention");
                        assert_eq!(attention["error"]["cause"], "stop_uncertain");
                    }
                    assert!(lock(&fresh_state).science_runtime.is_none());
                    assert!(lock(&fresh_state).proxy.is_none());
                    assert!(
                        config::load_from(&config_dir)
                            .unwrap()
                            .experimental_codex_enabled
                    );
                    open = read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .expect("uncertain terminal stop retains receipt");
                    assert!(matches!(
                        open.record.phase,
                        CodexDisableReceiptPhase::Attention {
                            cause: CodexDisableAttentionCause::StopUncertain
                        }
                    ));
                    open.clear_before(&config_dir).unwrap();
                }
                "science-replay-both" => {
                    let plan =
                        plan_experimental_codex_disable(&config_dir, &state, &lifecycle).unwrap();
                    let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
                        panic!("managed Science and Gateway must produce a stop plan")
                    };
                    let CodexDisableStopPlan {
                        before_config,
                        receipt,
                        science_owner,
                        science,
                        gateway,
                    } = *plan;
                    let science_plan = receipt.plan.science.clone().unwrap();
                    let gateway_plan = receipt.plan.gateway.clone().unwrap();
                    let mut open = OpenCodexDisableReceipt::publish_intent(
                        &config_dir,
                        &before_config,
                        receipt,
                    )
                    .unwrap();
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::Stopping {
                            component: CodexDisableComponent::Science,
                            science_stopped: false,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    let proof =
                        acquire_codex_disable_effect_lease(&config_dir, &open.record, &lifecycle)
                            .unwrap();
                    execute_planned_codex_science_stop(
                        app.handle(),
                        &state,
                        &lifecycle,
                        science_owner,
                        science.unwrap(),
                    )
                    .unwrap();
                    drop(proof);
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::EffectsApplied {
                            science_stopped: true,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::Stopping {
                            component: CodexDisableComponent::Gateway,
                            science_stopped: true,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    let proof =
                        acquire_codex_disable_effect_lease(&config_dir, &open.record, &lifecycle)
                            .unwrap();
                    execute_planned_codex_gateway_stop(&state, &lifecycle, gateway.unwrap())
                        .unwrap();
                    drop(proof);
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::EffectsApplied {
                            science_stopped: true,
                            gateway_stopped: true,
                        },
                    )
                    .unwrap();
                    let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                    let fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let fresh_app = tauri::test::mock_builder()
                        .manage(fresh_state.clone())
                        .manage(fresh_lifecycle.clone() as SharedLifecycle)
                        .manage(fresh_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    assert!(replay_interrupted_codex_disable(fresh_app.handle()).is_none());
                    assert!(replay_interrupted_codex_disable(fresh_app.handle()).is_none());
                    let restored_cfg = config::load_from(&config_dir).unwrap();
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Restored
                    );
                    assert_eq!(
                        observe_codex_disable_gateway(&restored_cfg, &gateway_plan),
                        CodexDisableComponentObservation::Restored
                    );
                    assert!(lock(&fresh_state).proxy.is_some());
                    p2a_stop_test_science(fresh_app.handle(), &fresh_state);
                    p2a_stop_test_gateway(&fresh_state);
                }
                "science-partial-restore" => {
                    let failing_gateway = temp.named_script(
                        "p2a-gateway-restore-fails-after-auth",
                        r#"if [ "$1" = "codex-auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"schema_version":3,"ok":true,"command":"status","status":{"authenticated":true,"reason":"ready","account_hash":"abababababababababababababababab","expiry_state":"valid","expires_at":2000000000,"auth_epoch":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","auth_generation":1}}'
  exit 0
fi
exit 23"#,
                    );
                    env::set_var(
                        "CSSWITCH_GATEWAY_BIN",
                        failing_gateway.canonicalize().unwrap(),
                    );
                    let restore_auth_probe = prepare_provider_auth(
                        app.handle(),
                        "codex",
                        CodexPreflightTarget::ActiveProfile,
                    )
                    .expect("Gateway restore failure wrapper must pass auth preflight")
                    .expect("Codex auth preflight must return proof");
                    drop(restore_auth_probe);
                    let prior_science_pid =
                        crate::runtime::science::test_unique_listener_pid(sandbox_port)
                            .expect("prior fake Science listener must be unique");
                    let fault = config::test_arm_update_commit_failure(config_dir.clone());
                    let result =
                        lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                            execute_experimental_codex_enabled(
                                app.handle(),
                                &state,
                                lifecycle.as_ref(),
                                &supervisor,
                                false,
                            )
                        });
                    drop(fault);
                    assert!(matches!(
                        result,
                        Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                            code: "codex_disable_attention",
                            cause: "restore_failed",
                            phase: "restoring",
                            attention_required: true,
                            ..
                        }))
                    ));
                    assert!(
                        config::load_from(&config_dir)
                            .unwrap()
                            .experimental_codex_enabled
                    );
                    assert!(!r0_process_is_running(prior_gateway_pid));
                    let open = read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .expect("partial inverse must retain replayable receipt progress");
                    assert!(matches!(
                        open.record.phase,
                        CodexDisableReceiptPhase::Restoring {
                            science_stopped: true,
                            gateway_stopped: true
                        }
                    ));
                    let science_plan = open.record.plan.science.clone().unwrap();
                    let gateway_plan = open.record.plan.gateway.clone().unwrap();
                    let restored_science_pid =
                        crate::runtime::science::test_unique_listener_pid(sandbox_port)
                            .unwrap_or_else(|| {
                                panic!(
                                    "partial inverse must leave exact restored Science running: observation={:?}, tracked={}, prior_running={}, port_in_use={}",
                                    observe_codex_disable_science(&science_plan),
                                    lock(&state).science_runtime.is_some(),
                                    r0_process_is_running(prior_science_pid),
                                    proc::loopback_port_in_use(sandbox_port, 100)
                                )
                            });
                    assert_ne!(restored_science_pid, prior_science_pid);
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Restored
                    );
                    assert_eq!(
                        observe_codex_disable_gateway(
                            &config::load_from(&config_dir).unwrap(),
                            &gateway_plan
                        ),
                        CodexDisableComponentObservation::Absent
                    );

                    let first_fresh_state: SharedAppState =
                        Arc::new(Mutex::new(AppState::default()));
                    let first_fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let first_fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let first_fresh_app = tauri::test::mock_builder()
                        .manage(first_fresh_state.clone())
                        .manage(first_fresh_lifecycle.clone() as SharedLifecycle)
                        .manage(first_fresh_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    let first_attention =
                        replay_interrupted_codex_disable(first_fresh_app.handle())
                            .expect("fresh replay must retain partial inverse progress");
                    assert_eq!(first_attention["error"]["cause"], "restore_failed");
                    assert_eq!(
                        crate::runtime::science::test_unique_listener_pid(sandbox_port),
                        Some(restored_science_pid),
                        "fresh replay must adopt, not duplicate, exact restored Science"
                    );
                    assert!(lock(&first_fresh_state).science_runtime.is_some());
                    assert!(lock(&first_fresh_state).proxy.is_none());
                    assert!(matches!(
                        read_codex_disable_receipt_at(&config_dir)
                            .unwrap()
                            .unwrap()
                            .record
                            .phase,
                        CodexDisableReceiptPhase::Restoring {
                            science_stopped: true,
                            gateway_stopped: true
                        }
                    ));

                    let before_drift = config::load_from(&config_dir).unwrap().reuse_system_ssh;
                    let mut drifted = config::load_from(&config_dir).unwrap();
                    drifted.reuse_system_ssh = !before_drift;
                    config::test_save_to_without_history_authority_guard(&config_dir, &drifted)
                        .unwrap();
                    let drift_attention =
                        replay_interrupted_codex_disable(first_fresh_app.handle())
                            .expect("Config drift must project attention without erasing progress");
                    assert_eq!(drift_attention["error"]["cause"], "config_drift");
                    assert!(matches!(
                        read_codex_disable_receipt_at(&config_dir)
                            .unwrap()
                            .unwrap()
                            .record
                            .phase,
                        CodexDisableReceiptPhase::Restoring {
                            science_stopped: true,
                            gateway_stopped: true
                        }
                    ));
                    assert_eq!(
                        crate::runtime::science::test_unique_listener_pid(sandbox_port),
                        Some(restored_science_pid),
                        "Config drift must not stop the exact partial restore"
                    );
                    let mut exact = config::load_from(&config_dir).unwrap();
                    exact.reuse_system_ssh = before_drift;
                    config::test_save_to_without_history_authority_guard(&config_dir, &exact)
                        .unwrap();

                    env::set_var("CSSWITCH_GATEWAY_BIN", &wrapper);
                    let second_fresh_state: SharedAppState =
                        Arc::new(Mutex::new(AppState::default()));
                    let second_fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let second_fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let second_fresh_app = tauri::test::mock_builder()
                        .manage(second_fresh_state.clone())
                        .manage(second_fresh_lifecycle.clone() as SharedLifecycle)
                        .manage(second_fresh_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    assert!(replay_interrupted_codex_disable(second_fresh_app.handle()).is_none());
                    assert!(replay_interrupted_codex_disable(second_fresh_app.handle()).is_none());
                    assert_eq!(
                        crate::runtime::science::test_unique_listener_pid(sandbox_port),
                        Some(restored_science_pid),
                        "second fresh replay must converge around the same restored Science"
                    );
                    assert!(lock(&second_fresh_state).science_runtime.is_some());
                    assert!(lock(&second_fresh_state).proxy.is_some());
                    assert!(read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .is_none());
                    p2a_stop_test_science(second_fresh_app.handle(), &second_fresh_state);
                    p2a_stop_test_gateway(&second_fresh_state);
                }
                "science-restore-uncertain" => {
                    let prior_science_pid =
                        crate::runtime::science::test_unique_listener_pid(sandbox_port)
                            .expect("prior fake Science listener must be unique");
                    let uncertainty =
                        crate::runtime::sandbox_session::test_arm_prior_restart_post_spawn_uncertain(
                            sandbox_port,
                        );
                    let fault = config::test_arm_update_commit_failure(config_dir.clone());
                    let result =
                        lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                            execute_experimental_codex_enabled(
                                app.handle(),
                                &state,
                                lifecycle.as_ref(),
                                &supervisor,
                                false,
                            )
                        });
                    drop(fault);
                    assert!(matches!(
                        result,
                        Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                            code: "codex_disable_attention",
                            cause: "restore_uncertain",
                            phase: "restoring",
                            attention_required: true,
                            ..
                        }))
                    ));
                    let (uncertain_pid, uncertain_start) =
                        crate::runtime::sandbox_session::test_prior_restart_post_spawn_identity()
                            .expect("uncertain restore must expose its isolated fake identity");
                    assert_ne!(uncertain_pid, prior_science_pid);
                    assert!(r0_process_is_running(uncertain_pid));
                    assert!(!r0_process_is_running(prior_gateway_pid));
                    let open = read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .expect("uncertain inverse must retain replayable receipt progress");
                    assert!(matches!(
                        open.record.phase,
                        CodexDisableReceiptPhase::Restoring {
                            science_stopped: true,
                            gateway_stopped: true
                        }
                    ));
                    let science_plan = open.record.plan.science.clone().unwrap();
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Replacement,
                        "uncommitted candidate must never be claimed as a managed restore"
                    );

                    let blocked_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                    let blocked_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let blocked_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let blocked_app = tauri::test::mock_builder()
                        .manage(blocked_state.clone())
                        .manage(blocked_lifecycle.clone() as SharedLifecycle)
                        .manage(blocked_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    let blocked = replay_interrupted_codex_disable(blocked_app.handle())
                        .expect("unproven Science candidate must keep replay fail closed");
                    assert_eq!(blocked["error"]["cause"], "identity_drift");
                    assert_eq!(
                        crate::runtime::science::test_unique_listener_pid(sandbox_port),
                        Some(uncertain_pid),
                        "replay must preserve an unproven replacement"
                    );
                    assert!(lock(&blocked_state).science_runtime.is_none());
                    assert!(lock(&blocked_state).proxy.is_none());

                    drop(uncertainty);
                    p2a_terminate_exact_uncommitted_science(
                        sandbox_port,
                        uncertain_pid,
                        &uncertain_start,
                    );
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Absent
                    );

                    let recovered_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                    let recovered_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let recovered_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let recovered_app = tauri::test::mock_builder()
                        .manage(recovered_state.clone())
                        .manage(recovered_lifecycle.clone() as SharedLifecycle)
                        .manage(recovered_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    assert!(replay_interrupted_codex_disable(recovered_app.handle()).is_none());
                    assert!(replay_interrupted_codex_disable(recovered_app.handle()).is_none());
                    let recovered_pid =
                        crate::runtime::science::test_unique_listener_pid(sandbox_port)
                            .expect("fresh replay must deterministically restore Science");
                    assert_ne!(recovered_pid, uncertain_pid);
                    assert!(lock(&recovered_state).science_runtime.is_some());
                    assert!(lock(&recovered_state).proxy.is_some());
                    assert!(read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .is_none());
                    p2a_stop_test_science(recovered_app.handle(), &recovered_state);
                    p2a_stop_test_gateway(&recovered_state);
                }
                "science-replacement" => {
                    let plan =
                        plan_experimental_codex_disable(&config_dir, &state, &lifecycle).unwrap();
                    let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
                        panic!("managed Science and Gateway must produce a stop plan")
                    };
                    let CodexDisableStopPlan {
                        before_config,
                        receipt,
                        science_owner,
                        science,
                        gateway,
                    } = *plan;
                    let science_plan = receipt.plan.science.clone().unwrap();
                    let mut open = OpenCodexDisableReceipt::publish_intent(
                        &config_dir,
                        &before_config,
                        receipt,
                    )
                    .unwrap();
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::Stopping {
                            component: CodexDisableComponent::Science,
                            science_stopped: false,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    let proof =
                        acquire_codex_disable_effect_lease(&config_dir, &open.record, &lifecycle)
                            .unwrap();
                    execute_planned_codex_science_stop(
                        app.handle(),
                        &state,
                        &lifecycle,
                        science_owner,
                        science.unwrap(),
                    )
                    .unwrap();
                    drop(proof);
                    drop(gateway);
                    let replacement_launch_id = "aa".repeat(16);
                    crate::runtime::sandbox_session::restore_science_from_durable_recipe(
                        app.handle(),
                        &state,
                        &lifecycle,
                        None,
                        &science_plan.prior,
                        &replacement_launch_id,
                    )
                    .unwrap();
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Replacement
                    );
                    let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                    let fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                    let fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                    let fresh_app = tauri::test::mock_builder()
                        .manage(fresh_state)
                        .manage(fresh_lifecycle.clone() as SharedLifecycle)
                        .manage(fresh_supervisor as SharedCodexAuthSupervisor)
                        .build(tauri::test::mock_context(tauri::test::noop_assets()))
                        .unwrap();
                    assert_eq!(
                        replay_interrupted_codex_disable(fresh_app.handle()).unwrap()["status"],
                        "attention"
                    );
                    assert_eq!(
                        replay_interrupted_codex_disable(fresh_app.handle()).unwrap()["status"],
                        "attention"
                    );
                    assert_eq!(
                        observe_codex_disable_science(&science_plan),
                        CodexDisableComponentObservation::Replacement
                    );
                    assert!(r0_process_is_running(prior_gateway_pid));
                    open = read_codex_disable_receipt_at(&config_dir)
                        .unwrap()
                        .expect("replacement attention retains receipt");
                    open.clear_before(&config_dir).unwrap();
                    p2a_stop_test_science(app.handle(), &state);
                    p2a_stop_test_gateway(&state);
                }
                "science-config-drift" | "science-generation-drift" => {
                    let plan =
                        plan_experimental_codex_disable(&config_dir, &state, &lifecycle).unwrap();
                    let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
                        panic!("managed Science and Gateway must produce a stop plan")
                    };
                    let CodexDisableStopPlan {
                        before_config,
                        receipt,
                        science,
                        gateway,
                        ..
                    } = *plan;
                    let mut open = OpenCodexDisableReceipt::publish_intent(
                        &config_dir,
                        &before_config,
                        receipt,
                    )
                    .unwrap();
                    open.transition(
                        &config_dir,
                        CodexDisableReceiptPhase::Stopping {
                            component: CodexDisableComponent::Science,
                            science_stopped: false,
                            gateway_stopped: false,
                        },
                    )
                    .unwrap();
                    let expected_cause = if requested == "science-config-drift" {
                        let mut drifted = config::load_from(&config_dir).unwrap();
                        drifted.reuse_system_ssh = true;
                        config::test_save_to_without_history_authority_guard(&config_dir, &drifted)
                            .unwrap();
                        CodexDisableAttentionCause::ConfigDrift
                    } else {
                        lifecycle.bump_generation();
                        CodexDisableAttentionCause::IdentityDrift
                    };
                    assert!(acquire_codex_disable_effect_lease(
                        &config_dir,
                        &open.record,
                        &lifecycle,
                    )
                    .is_err());
                    let error = retain_codex_disable_attention(
                        &config_dir,
                        &mut open,
                        expected_cause,
                        false,
                    );
                    assert!(error.attention_required);
                    assert_eq!(
                        ScienceHostAdapter::probe_known(sandbox_port, &prior_runtime),
                        SandboxScienceState::RunningHealthy
                    );
                    assert!(r0_process_is_running(prior_gateway_pid));
                    assert_eq!(lock(&state).launch_id, prior_gateway_launch_id);
                    drop(science);
                    drop(gateway);
                    if requested == "science-config-drift" {
                        assert_eq!(
                            open.clear_before(&config_dir).unwrap_err(),
                            CodexDisableAttentionCause::ConfigDrift
                        );
                        let mut exact = config::load_from(&config_dir).unwrap();
                        exact.reuse_system_ssh = false;
                        config::test_save_to_without_history_authority_guard(&config_dir, &exact)
                            .unwrap();
                    }
                    open.clear_before(&config_dir).unwrap();
                    p2a_stop_test_science(app.handle(), &state);
                    p2a_stop_test_gateway(&state);
                }
                _ => unreachable!(),
            }
            return;
        }

        if requested == "other-provider" {
            let (other_state, other_pid) = r0_proxy_state("deepseek");
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let failed = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &other_state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            drop(fault);
            assert!(matches!(
                failed,
                Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                    code: "codex_disable_failed",
                    cause: "config_commit_failed",
                    phase: "intent",
                    ..
                }))
            ));
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(r0_process_is_running(other_pid));
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            let result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &other_state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            assert_eq!(result.unwrap()["experimental_codex_enabled"], false);
            assert!(r0_process_is_running(other_pid));
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            p2a_stop_test_gateway(&other_state);
            return;
        }

        let (prior_pid, prior_launch_id) = p2a_start_gateway(app.handle(), &state, &lifecycle);
        assert!(r0_process_is_running(prior_pid));

        if requested == "success" {
            let result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            assert_eq!(result.unwrap()["experimental_codex_enabled"], false);
            assert!(
                !config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(lock(&state).proxy.is_none());
            assert!(!r0_process_is_running(prior_pid));
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());

            let enabled = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    true,
                )
            });
            assert_eq!(enabled.unwrap()["experimental_codex_enabled"], true);
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let failed_noop = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            drop(fault);
            assert!(matches!(
                failed_noop,
                Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                    code: "codex_disable_failed",
                    cause: "config_commit_failed",
                    phase: "intent",
                    ..
                }))
            ));
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            let noop = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            assert_eq!(noop.unwrap()["experimental_codex_enabled"], false);
            assert!(lock(&state).proxy.is_none());
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            return;
        }

        if requested == "stop-failure" {
            let _fault = test_arm_codex_disable_gateway_stop_failure();
            let result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            assert!(matches!(
                result,
                Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                    code: "codex_disable_failed",
                    cause: "gateway_stop_failed",
                    ..
                }))
            ));
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(r0_process_is_running(prior_pid));
            assert_eq!(lock(&state).launch_id, prior_launch_id);
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            p2a_stop_test_gateway(&state);
            return;
        }

        if requested == "gateway-phase-drift" {
            let exact_failure = test_arm_codex_disable_gateway_phase_failure(
                CodexDisablePhaseFailureMode::ExactBeforeImage,
            );
            let exact_result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            drop(exact_failure);
            assert!(matches!(
                exact_result,
                Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                    code: "codex_disable_failed",
                    cause: "receipt_io",
                    phase: "intent",
                    attention_required: false,
                    ..
                }))
            ));
            assert!(r0_process_is_running(prior_pid));
            assert_eq!(lock(&state).launch_id, prior_launch_id);
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());

            let before_drift = config::load_from(&config_dir).unwrap().reuse_system_ssh;
            let drift_failure = test_arm_codex_disable_gateway_phase_failure(
                CodexDisablePhaseFailureMode::ConfigDrift,
            );
            let drift_result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            drop(drift_failure);
            assert!(matches!(
                drift_result,
                Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                    code: "codex_disable_attention",
                    cause: "config_drift",
                    phase: "intent",
                    attention_required: true,
                    ..
                }))
            ));
            let drifted = config::load_from(&config_dir).unwrap();
            assert!(drifted.experimental_codex_enabled);
            assert_ne!(drifted.reuse_system_ssh, before_drift);
            assert!(r0_process_is_running(prior_pid));
            assert_eq!(lock(&state).launch_id, prior_launch_id);
            let open = read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .expect("Config drift after phase failure must retain receipt attention");
            assert!(matches!(
                open.record.phase,
                CodexDisableReceiptPhase::Attention {
                    cause: CodexDisableAttentionCause::ConfigDrift
                }
            ));
            assert_eq!(
                open.clear_before(&config_dir).unwrap_err(),
                CodexDisableAttentionCause::ConfigDrift
            );
            let mut exact = config::load_from(&config_dir).unwrap();
            exact.reuse_system_ssh = before_drift;
            config::test_save_to_without_history_authority_guard(&config_dir, &exact).unwrap();
            open.clear_before(&config_dir).unwrap();
            p2a_stop_test_gateway(&state);
            return;
        }

        if matches!(requested.as_str(), "config-restore" | "restore-attention") {
            if requested == "restore-attention" {
                let failing = temp.named_script(
                    "p2a-failing-gateway",
                    "printf '%s\\n' 'invalid-sidecar-response'\nexit 23",
                );
                env::set_var("CSSWITCH_GATEWAY_BIN", failing);
            }
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let result = lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
                execute_experimental_codex_enabled(
                    app.handle(),
                    &state,
                    lifecycle.as_ref(),
                    &supervisor,
                    false,
                )
            });
            drop(fault);
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(!r0_process_is_running(prior_pid));
            if requested == "config-restore" {
                assert!(matches!(
                    result,
                    Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                        code: "codex_disable_failed",
                        cause: "config_commit_failed",
                        ..
                    }))
                ));
                let current = lock(&state);
                assert!(current.proxy.is_some());
                assert_ne!(current.launch_id, prior_launch_id);
                drop(current);
                assert!(read_codex_disable_receipt_at(&config_dir)
                    .unwrap()
                    .is_none());
                p2a_stop_test_gateway(&state);
            } else {
                assert!(matches!(
                    result,
                    Err(RuntimeCommandError::Disable(CodexDisableCommandError {
                        code: "codex_disable_attention",
                        cause: "restore_failed",
                        phase: "restoring",
                        attention_required: true,
                        ..
                    }))
                ));
                assert!(lock(&state).proxy.is_none());
                let open = read_codex_disable_receipt_at(&config_dir)
                    .unwrap()
                    .expect("failed restore retains receipt");
                assert!(matches!(
                    open.record.phase,
                    CodexDisableReceiptPhase::Restoring {
                        science_stopped: false,
                        gateway_stopped: true
                    }
                ));
                env::set_var("CSSWITCH_GATEWAY_BIN", &wrapper);
                let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
                let fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
                let fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
                let fresh_app = tauri::test::mock_builder()
                    .manage(fresh_state.clone())
                    .manage(fresh_lifecycle.clone() as SharedLifecycle)
                    .manage(fresh_supervisor as SharedCodexAuthSupervisor)
                    .build(tauri::test::mock_context(tauri::test::noop_assets()))
                    .unwrap();
                assert!(replay_interrupted_codex_disable(fresh_app.handle()).is_none());
                assert!(replay_interrupted_codex_disable(fresh_app.handle()).is_none());
                assert!(lock(&fresh_state).proxy.is_some());
                assert!(read_codex_disable_receipt_at(&config_dir)
                    .unwrap()
                    .is_none());
                p2a_stop_test_gateway(&fresh_state);
            }
            return;
        }

        let plan = plan_experimental_codex_disable(&config_dir, &state, &lifecycle).unwrap();
        let ExperimentalCodexDisablePlan::StopManagedCodex(plan) = plan else {
            panic!("running managed Codex Gateway must create a stop plan")
        };
        let CodexDisableStopPlan {
            before_config,
            receipt,
            science,
            gateway,
            ..
        } = *plan;
        assert!(science.is_none());
        let mut open =
            OpenCodexDisableReceipt::publish_intent(&config_dir, &before_config, receipt).unwrap();

        if requested == "replay-intent" {
            drop(gateway);
            assert!(replay_interrupted_codex_disable(app.handle()).is_none());
            assert!(replay_interrupted_codex_disable(app.handle()).is_none());
            assert!(r0_process_is_running(prior_pid));
            assert_eq!(lock(&state).launch_id, prior_launch_id);
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            p2a_stop_test_gateway(&state);
            return;
        }

        open.transition(
            &config_dir,
            CodexDisableReceiptPhase::Stopping {
                component: CodexDisableComponent::Gateway,
                science_stopped: false,
                gateway_stopped: false,
            },
        )
        .unwrap();
        let effect_lease =
            acquire_codex_disable_effect_lease(&config_dir, &open.record, &lifecycle).unwrap();
        execute_planned_codex_gateway_stop(&state, &lifecycle, gateway.unwrap()).unwrap();
        drop(effect_lease);
        assert!(!r0_process_is_running(prior_pid));
        assert!(lock(&state).proxy.is_none());

        if requested == "gateway-stopping-aba" {
            let (replacement_pid, replacement_launch_id) =
                p2a_start_gateway(app.handle(), &state, &lifecycle);
            assert_ne!(replacement_launch_id, prior_launch_id);
            assert!(r0_process_is_running(replacement_pid));
            p2a_stop_test_gateway(&state);
            assert!(!r0_process_is_running(replacement_pid));

            let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
            let fresh_lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
            let fresh_supervisor = Arc::new(CodexAuthSupervisor::default());
            let fresh_app = tauri::test::mock_builder()
                .manage(fresh_state.clone())
                .manage(fresh_lifecycle.clone() as SharedLifecycle)
                .manage(fresh_supervisor as SharedCodexAuthSupervisor)
                .build(tauri::test::mock_context(tauri::test::noop_assets()))
                .unwrap();
            for _ in 0..2 {
                let attention = replay_interrupted_codex_disable(fresh_app.handle())
                    .expect("start-stop ABA after pre-effect WAL must retain attention");
                assert_eq!(attention["status"], "attention");
                assert_eq!(attention["error"]["cause"], "stop_uncertain");
            }
            assert!(lock(&fresh_state).proxy.is_none());
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            open = read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .expect("Gateway ABA uncertainty retains receipt");
            assert!(matches!(
                open.record.phase,
                CodexDisableReceiptPhase::Attention {
                    cause: CodexDisableAttentionCause::StopUncertain
                }
            ));
            open.clear_before(&config_dir).unwrap();
            return;
        }

        if requested == "replay-commit" {
            open.transition(
                &config_dir,
                CodexDisableReceiptPhase::EffectsApplied {
                    science_stopped: false,
                    gateway_stopped: true,
                },
            )
            .unwrap();
            commit_codex_disable_config(&config_dir, &open.record).unwrap();
            assert!(replay_interrupted_codex_disable(app.handle()).is_none());
            assert!(
                !config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(lock(&state).proxy.is_none());
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            return;
        }

        if requested == "replay-stop" {
            open.transition(
                &config_dir,
                CodexDisableReceiptPhase::EffectsApplied {
                    science_stopped: false,
                    gateway_stopped: true,
                },
            )
            .unwrap();
            assert!(replay_interrupted_codex_disable(app.handle()).is_none());
            assert!(replay_interrupted_codex_disable(app.handle()).is_none());
            assert!(
                config::load_from(&config_dir)
                    .unwrap()
                    .experimental_codex_enabled
            );
            assert!(lock(&state).proxy.is_some());
            assert!(read_codex_disable_receipt_at(&config_dir)
                .unwrap()
                .is_none());
            p2a_stop_test_gateway(&state);
            return;
        }

        assert_eq!(requested, "replay-replacement");
        let (replacement_pid, replacement_launch_id) =
            p2a_start_gateway(app.handle(), &state, &lifecycle);
        assert_ne!(replacement_launch_id, prior_launch_id);
        let first =
            replay_interrupted_codex_disable(app.handle()).expect("replacement requires attention");
        assert_eq!(first["status"], "attention");
        assert!(r0_process_is_running(replacement_pid));
        assert_eq!(lock(&state).launch_id, replacement_launch_id);
        let second = replay_interrupted_codex_disable(app.handle())
            .expect("attention replay remains fail closed");
        assert_eq!(second["status"], "attention");
        assert!(r0_process_is_running(replacement_pid));
        assert_eq!(lock(&state).launch_id, replacement_launch_id);
        p2a_stop_test_gateway(&state);
        open = read_codex_disable_receipt_at(&config_dir)
            .unwrap()
            .expect("replacement attention retains receipt");
        open.clear_before(&config_dir).unwrap();
        env::set_var("CSSWITCH_GATEWAY_BIN", wrapper);
    }

    #[test]
    fn r0_codex_login_prepare_failure_preserves_other_provider_and_stopped_codex_state() {
        for case in [
            "login-prepare",
            "login-terminal-failure",
            "login-profile-failure",
        ] {
            run_exact_ignored_codex_characterization(case);
        }
    }

    #[test]
    fn r0_cancel_writes_exact_ndjson_and_write_failure_is_safely_terminalized() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("r0-f-cancel-wire");
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = supervisor.begin_login().unwrap();
        let operation_id = reservation.operation_id.clone();
        let line_path = temp.0.join("cancel-line");
        let script = temp.script(&format!(
            "IFS= read -r cancel\nprintf '%s' \"$cancel\" > \"$HOME/cancel-line\"\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"cancel_ack\",\"disposition\":\"accepted\"}}'\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"cancelled\",\"error\":{{\"code\":\"auth_cancelled\",\"stage\":\"cancelled\",\"retryable\":true}}}}'\nexit 7"
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let waiter_supervisor = supervisor.clone();
        let waiter_operation_id = operation_id.clone();
        let cancel = reservation.cancel.clone();
        let waiter = std::thread::spawn(move || {
            let result = wait_for_login_sidecar(
                process,
                CodexAuthAction::LoginBrowser,
                &waiter_operation_id,
                cancel.as_ref(),
                |_| {},
                |disposition| {
                    waiter_supervisor.record_cancel_disposition(&waiter_operation_id, disposition)
                },
            );
            waiter_supervisor
                .finish(&waiter_operation_id, "cancelled", None)
                .unwrap();
            result
        });
        for _ in 0..2 {
            let disposition = supervisor.cancel(&operation_id).unwrap();
            assert!(
                matches!(disposition, "accepted" | "already_terminal"),
                "cancel may race the worker's valid terminal transition: {disposition}"
            );
        }
        assert_eq!(waiter.join().unwrap().unwrap()["state"], "cancelled");
        assert_eq!(
            supervisor.cancel(&operation_id).unwrap(),
            "already_terminal"
        );
        let expected = json!({
            "schema_version": AUTH_SCHEMA_VERSION,
            "operation_id": operation_id,
            "command": "cancel",
        });
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(line_path).unwrap()).unwrap(),
            expected
        );

        let failing = supervisor.begin_login().unwrap();
        let failing_id = failing.operation_id.clone();
        let failing_script =
            temp.script("exec 0<&-\n: > \"$HOME/cancel-closed\"\nexec /bin/sleep 1");
        let failing_process = spawn_codex_auth_sidecar_at(
            &failing_script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&failing_id),
            false,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.0.join("cancel-closed").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(temp.0.join("cancel-closed").exists());
        let failing_supervisor = supervisor.clone();
        let waiter_id = failing_id.clone();
        let failing_cancel = failing.cancel.clone();
        let failing_waiter = std::thread::spawn(move || {
            let result = wait_for_login_sidecar(
                failing_process,
                CodexAuthAction::LoginBrowser,
                &waiter_id,
                failing_cancel.as_ref(),
                |_| {},
                |_| {},
            );
            assert_eq!(
                failing_supervisor.snapshot().unwrap().state,
                "starting",
                "write failure is observed before the login worker terminalizes the operation"
            );
            failing_supervisor
                .finish(&waiter_id, "failed", None)
                .unwrap();
            result
        });
        assert_eq!(supervisor.cancel(&failing_id).unwrap(), "already_terminal");
        assert!(failing_waiter
            .join()
            .unwrap()
            .unwrap_err()
            .contains("发送取消请求"));
        assert_eq!(supervisor.snapshot().unwrap().state, "failed");
    }

    #[test]
    fn r0_codex_logout_failure_leaves_confirmed_codex_runtime_stopped() {
        run_exact_ignored_codex_characterization("logout-failure");
    }

    #[test]
    fn r0_codex_ensure_profile_rechecks_auth_inside_lifecycle() {
        run_exact_ignored_codex_characterization("ensure-drift");
    }

    #[test]
    fn r0_codex_disable_config_failure_preserves_other_provider_and_does_not_restart_codex() {
        run_exact_ignored_codex_characterization("disable-config");
    }

    #[test]
    fn r0_codex_network_commit_failure_leaves_codex_stopped_and_other_provider_untouched() {
        run_exact_ignored_codex_characterization("network-config");
    }

    #[test]
    fn r0_downgrade_safe_failure_retains_stopped_runtime() {
        run_exact_ignored_codex_characterization("downgrade-safe");
    }

    #[test]
    fn r0_downgrade_post_publish_uncertainty_is_terminal() {
        run_exact_ignored_codex_characterization("downgrade-uncertain");
    }

    #[test]
    #[ignore = "source-gate parents execute exact isolated R0-F Codex mutation cases with temp HOME, fake processes, fake sidecar, and dynamic loopback ports"]
    fn isolated_r0_codex_mutation_command_contract() {
        let requested = env::var("CSSWITCH_TEST_R0_F_CASE").unwrap_or_default();
        assert!(
            matches!(
                requested.as_str(),
                "login-prepare"
                    | "login-terminal-failure"
                    | "login-profile-failure"
                    | "logout-failure"
                    | "ensure-drift"
                    | "disable-config"
                    | "network-config"
                    | "downgrade-safe"
                    | "downgrade-uncertain"
            ),
            "unknown isolated R0-F case"
        );
        let temp = TempDir::new(&format!("r0-f-{requested}"));
        let home = temp.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let config_dir = r0_codex_config(&home);
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let app = tauri::test::mock_builder()
            .manage(supervisor.clone() as SharedCodexAuthSupervisor)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());

        if requested == "login-prepare" {
            config::update(&config_dir, |cfg| {
                cfg.runtime_compensation = Some(o1_e1_codex_compensation_marker())
            })
            .unwrap();
            let (guarded_state, guarded_pid) = r0_proxy_state("codex");
            let guarded = match start_codex_login_inner(
                app.handle(),
                &guarded_state,
                lifecycle.as_ref(),
                &supervisor,
                CodexAuthAction::LoginBrowser,
                |_, _, _, _| panic!("open compensation marker must reject before sidecar spawn"),
            ) {
                Ok(_) => panic!("open compensation marker must reject Codex login"),
                Err(error) => error,
            };
            assert!(guarded
                .to_string()
                .contains("runtime_transaction_in_progress"));
            assert!(r0_process_is_running(guarded_pid));
            assert!(lock(&guarded_state).proxy.is_some());
            assert!(supervisor.snapshot().is_none());
            let _ = lock(&guarded_state).stop_proxy();
            config::update(&config_dir, |cfg| cfg.runtime_compensation = None).unwrap();

            let (other_state, other_pid) = r0_proxy_state("deepseek");
            let other_result = start_codex_login_inner(
                app.handle(),
                &other_state,
                lifecycle.as_ref(),
                &supervisor,
                CodexAuthAction::LoginBrowser,
                |_, action, operation_id, route| {
                    spawn_codex_auth_sidecar_at(
                        &temp.0.join("missing-sidecar"),
                        &home,
                        action,
                        Some(route),
                        Some(operation_id),
                        false,
                    )
                    .map_err(|_| {
                        CodexAuthCommandError::unavailable(
                            CodexAuthUnavailableCause::SidecarSpawnFailed,
                        )
                    })
                },
            );
            assert!(matches!(other_result, Err(RuntimeCommandError::Auth(_))));
            assert!(supervisor.snapshot().is_none());
            assert!(r0_process_is_running(other_pid));
            let _ = lock(&other_state).stop_proxy();

            let (codex_state, codex_pid) = r0_proxy_state("codex");
            let failed = start_codex_login_inner(
                app.handle(),
                &codex_state,
                lifecycle.as_ref(),
                &supervisor,
                CodexAuthAction::LoginBrowser,
                |_, action, operation_id, route| {
                    spawn_codex_auth_sidecar_at(
                        &temp.0.join("missing-sidecar"),
                        &home,
                        action,
                        Some(route),
                        Some(operation_id),
                        false,
                    )
                    .map_err(|_| {
                        CodexAuthCommandError::unavailable(
                            CodexAuthUnavailableCause::SidecarSpawnFailed,
                        )
                    })
                },
            );
            assert!(failed.is_err());
            assert!(supervisor.snapshot().is_none());
            assert!(lock(&codex_state).proxy.is_none());
            assert!(!r0_process_is_running(codex_pid));
        }

        if matches!(
            requested.as_str(),
            "login-terminal-failure" | "login-profile-failure"
        ) {
            let (state, other_pid) = r0_proxy_state("deepseek");
            let terminal_failure = requested == "login-terminal-failure";
            let (reservation, process) = start_codex_login_inner(
                app.handle(),
                &state,
                lifecycle.as_ref(),
                &supervisor,
                CodexAuthAction::LoginBrowser,
                |_, action, operation_id, route| {
                    let terminal = if terminal_failure {
                        format!(
                            "{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"failed\",\"error\":{{\"code\":\"oauth_denied\",\"stage\":\"browser_open\",\"retryable\":false}}}}"
                        )
                    } else {
                        format!(
                            "{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":true,\"reason\":\"ready\",\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":7}}}}",
                            "ab".repeat(16),
                            "cd".repeat(16)
                        )
                    };
                    let sidecar = temp.script(&format!(
                        "printf '%s\\n' '{terminal}'\nexit {}",
                        if terminal_failure { 4 } else { 0 }
                    ));
                    spawn_codex_auth_sidecar_at(
                        &sidecar,
                        &home,
                        action,
                        Some(route),
                        Some(operation_id),
                        false,
                    )
                    .map_err(|_| {
                        CodexAuthCommandError::unavailable(
                            CodexAuthUnavailableCause::SidecarSpawnFailed,
                        )
                    })
                },
            )
            .unwrap();
            let ensure_called = Arc::new(AtomicBool::new(false));
            let called = ensure_called.clone();
            let snapshot = complete_login_operation_inner(
                &supervisor,
                &lifecycle,
                &reservation.operation_id,
                reservation.cancel.as_ref(),
                process,
                CodexAuthAction::LoginBrowser,
                |_| {},
                |_| {},
                move || {
                    called.store(true, Ordering::SeqCst);
                    Err("simulated profile commit failure".into())
                },
            )
            .unwrap();
            assert_eq!(snapshot.state, "failed");
            let error = snapshot.error.unwrap();
            if terminal_failure {
                assert_eq!(error.code, "oauth_denied");
                assert_eq!(error.stage, "browser_open");
                assert!(!ensure_called.load(Ordering::SeqCst));
            } else {
                assert_eq!(error.code, "profile_ensure_failed");
                assert_eq!(error.stage, "profile_ensure");
                assert!(ensure_called.load(Ordering::SeqCst));
            }
            assert!(r0_process_is_running(other_pid));
            let _ = lock(&state).stop_proxy();
        }

        if requested == "logout-failure" {
            let (codex_state, codex_pid) = r0_proxy_state("codex");
            config::update(&config_dir, |cfg| {
                cfg.runtime_compensation = Some(o1_e1_codex_compensation_marker())
            })
            .unwrap();
            let guarded = match prepare_codex_logout_inner(
                app.handle(),
                &codex_state,
                lifecycle.as_ref(),
                &supervisor,
            ) {
                Ok(_) => panic!("open compensation marker must reject Codex logout"),
                Err(error) => error,
            };
            assert!(guarded
                .to_string()
                .contains("runtime_transaction_in_progress"));
            assert!(r0_process_is_running(codex_pid));
            assert!(lock(&codex_state).proxy.is_some());
            config::update(&config_dir, |cfg| cfg.runtime_compensation = None).unwrap();
            let mutation = prepare_codex_logout_inner(
                app.handle(),
                &codex_state,
                lifecycle.as_ref(),
                &supervisor,
            )
            .unwrap();
            let sidecar = temp.script(
                "printf '%s\\n' '{\"schema_version\":3,\"ok\":false,\"command\":\"logout\",\"error\":{\"code\":\"keychain_unavailable\",\"message\":\"fixture detail must not escape\",\"retryable\":true}}'\nexit 6",
            );
            let result = complete_codex_logout_inner(&supervisor, mutation, |mutation| {
                run_codex_logout_sidecar_at(
                    &sidecar,
                    &home,
                    &csswitch_codex_network::direct_route(),
                    false,
                    mutation,
                )
            });
            assert!(matches!(
                result,
                Err(RuntimeCommandError::Auth(CodexAuthCommandError {
                    cause: Some("keychain_unavailable"),
                    ..
                }))
            ));
            assert!(lock(&codex_state).proxy.is_none());
            assert!(!r0_process_is_running(codex_pid));
        }

        if requested == "ensure-drift" {
            config::update(&config_dir, |cfg| {
                cfg.profiles.clear();
                cfg.active_id.clear();
            })
            .unwrap();
            let sidecar = temp.script(&format!("printf '%s\\n' '{}'", success_json("status")));
            let worker_app = app.handle().clone();
            let worker_lifecycle = lifecycle.clone();
            let worker_home = home.clone();
            let (preflight_sender, preflight_receiver) = std::sync::mpsc::channel();
            let worker = lifecycle.with_serialized(|| {
                let worker = std::thread::spawn(move || {
                    ensure_codex_profile_command_inner(
                        worker_lifecycle.as_ref(),
                        || {
                            prepare_provider_auth_inner(
                                &worker_app,
                                "codex",
                                CodexPreflightTarget::NoProfile,
                                |_, reservation, route| {
                                    let value = run_codex_auth_preflight_sidecar_at(
                                        &sidecar,
                                        &worker_home,
                                        reservation,
                                        route,
                                    )?;
                                    preflight_sender.send(()).unwrap();
                                    Ok(value)
                                },
                            )
                        },
                        || ensure_codex_profile_authenticated(&config::default_dir()),
                    )
                });
                preflight_receiver
                    .recv_timeout(Duration::from_secs(5))
                    .expect("fake auth status must finish before lifecycle drift");
                config::update(&config_dir, |cfg| cfg.reuse_system_ssh = true).unwrap();
                worker
            });
            let result = worker.join().unwrap();
            let error = result.unwrap_err();
            assert!(matches!(
                error,
                RuntimeCommandError::Message(message)
                    if message.starts_with("config_changed_retry：")
            ));
            let after = config::load_from(&config_dir).unwrap();
            assert!(
                after.profiles.is_empty(),
                "drifted ensure must not commit a profile"
            );
            assert!(
                after.reuse_system_ssh,
                "test drift itself must be committed"
            );
        }

        if requested == "disable-config" {
            config::update(&config_dir, |cfg| {
                cfg.runtime_compensation = Some(o1_e1_codex_compensation_marker())
            })
            .unwrap();
            let guarded_before = fs::read(config_dir.join("config.json")).unwrap();
            let side_effect_called = std::cell::Cell::new(false);
            let guarded = set_experimental_codex_enabled_at(&config_dir, false, || {
                side_effect_called.set(true);
                Ok(())
            })
            .unwrap_err();
            assert!(
                guarded.contains("runtime_transaction_in_progress"),
                "{guarded}"
            );
            assert!(!side_effect_called.get());
            assert_eq!(
                fs::read(config_dir.join("config.json")).unwrap(),
                guarded_before
            );
            config::update(&config_dir, |cfg| cfg.runtime_compensation = None).unwrap();

            let before = fs::read(config_dir.join("config.json")).unwrap();
            let (other_state, other_pid) = r0_proxy_state("deepseek");
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let result = set_experimental_codex_enabled_at(&config_dir, false, || {
                prepare_codex_auth_mutation(app.handle(), &other_state, &lifecycle).map(|_| ())
            });
            drop(fault);
            assert!(result
                .unwrap_err()
                .contains("test-only config update commit failure"));
            assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
            assert!(r0_process_is_running(other_pid));
            let _ = lock(&other_state).stop_proxy();

            let (codex_state, codex_pid) = r0_proxy_state("codex");
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let result = set_experimental_codex_enabled_at(&config_dir, false, || {
                prepare_codex_auth_mutation(app.handle(), &codex_state, &lifecycle).map(|_| ())
            });
            drop(fault);
            assert!(result
                .unwrap_err()
                .contains("test-only config update commit failure"));
            assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
            assert!(lock(&codex_state).proxy.is_none());
            assert!(!r0_process_is_running(codex_pid));
        }

        if requested == "network-config" {
            config::update(&config_dir, |cfg| {
                cfg.runtime_compensation = Some(o1_e1_codex_compensation_marker())
            })
            .unwrap();
            let guarded_before = fs::read(config_dir.join("config.json")).unwrap();
            let side_effect_called = std::cell::Cell::new(false);
            let settings = csswitch_codex_network::CodexNetworkSettings::default();
            let resolved = csswitch_codex_network::direct_route();
            let guarded = set_codex_network_at(&config_dir, settings, &resolved, || {
                side_effect_called.set(true);
                Ok(())
            })
            .unwrap_err();
            assert!(
                guarded.contains("runtime_transaction_in_progress"),
                "{guarded}"
            );
            assert!(!side_effect_called.get());
            assert_eq!(
                fs::read(config_dir.join("config.json")).unwrap(),
                guarded_before
            );
            config::update(&config_dir, |cfg| cfg.runtime_compensation = None).unwrap();

            config::update(&config_dir, |cfg| {
                cfg.codex_network.mode = csswitch_codex_network::CodexNetworkMode::Custom;
                cfg.codex_network.proxy_url = "http://127.0.0.1:8080".into();
            })
            .unwrap();
            let before = fs::read(config_dir.join("config.json")).unwrap();
            let settings = csswitch_codex_network::CodexNetworkSettings::default();
            let resolved = csswitch_codex_network::direct_route();
            let (other_state, other_pid) = r0_proxy_state("deepseek");
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let result = set_codex_network_at(&config_dir, settings.clone(), &resolved, || {
                prepare_codex_auth_mutation(app.handle(), &other_state, &lifecycle).map(|_| ())
            });
            drop(fault);
            assert!(result
                .unwrap_err()
                .contains("test-only config update commit failure"));
            assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
            assert!(r0_process_is_running(other_pid));
            let _ = lock(&other_state).stop_proxy();

            let (codex_state, codex_pid) = r0_proxy_state("codex");
            let fault = config::test_arm_update_commit_failure(config_dir.clone());
            let result = set_codex_network_at(&config_dir, settings, &resolved, || {
                prepare_codex_auth_mutation(app.handle(), &codex_state, &lifecycle).map(|_| ())
            });
            drop(fault);
            assert!(result
                .unwrap_err()
                .contains("test-only config update commit failure"));
            assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
            assert!(lock(&codex_state).proxy.is_none());
            assert!(!r0_process_is_running(codex_pid));
        }

        if matches!(requested.as_str(), "downgrade-safe" | "downgrade-uncertain") {
            let cfg = config::load_from(&config_dir).unwrap();
            let actions = BTreeMap::from([(
                "codex-r0-f".into(),
                config::CodexDowngradeAction::ExportThenRemove,
            )]);
            let fingerprint = config::prepare_downgrade_to_v2(&cfg, &actions)
                .unwrap()
                .fingerprint;
            let destination = home.join("codex-export.json");
            let (codex_state, codex_pid) = r0_proxy_state("codex");

            let _commit_fault = (requested == "downgrade-uncertain")
                .then(|| config::test_arm_downgrade_commit_failure(config_dir.clone(), true));
            if requested == "downgrade-safe" {
                let backup = config_dir.join("config.json.bak");
                let backup = std::ffi::CString::new(backup.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(backup.as_ptr(), 0o600) }, 0);
            }
            let outcome = run_downgrade_mutation_at(
                &config_dir,
                &actions,
                &destination,
                &fingerprint,
                json!({"status": "DOWNGRADED_EXIT_REQUIRED"}),
                || stop_all_before_downgrade(app.handle(), &codex_state, &lifecycle),
            )
            .unwrap();
            assert!(lock(&codex_state).proxy.is_none());
            assert!(!r0_process_is_running(codex_pid));
            assert!(
                destination.exists(),
                "export must precede the injected failure"
            );

            let exit_code = AtomicI32::new(-1);
            let result =
                finish_downgrade_command(outcome, |code| exit_code.store(code, Ordering::SeqCst));
            if requested == "downgrade-safe" {
                assert!(result.unwrap_err().contains("滚动备份失败"));
                assert_eq!(exit_code.load(Ordering::SeqCst), -1);
                assert_eq!(config::load_from(&config_dir).unwrap().schema_version, 4);
            } else {
                let error = result.unwrap_err();
                assert_eq!(exit_code.load(Ordering::SeqCst), 1);
                assert!(error.contains("禁止再次读取配置"));
                assert!(error.contains("test-only downgrade commit failure"));
                assert!(config::load_from(&config_dir)
                    .unwrap_err()
                    .to_string()
                    .contains("终态退出"));
            }
        }
    }

    #[test]
    fn p2b_codex_auth_start_inert_sidecar_identity_and_dual_id_matrix() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = supervisor.begin_login().unwrap();
        let config_operation_id = config::new_id();
        let snapshot = supervisor
            .attach_config_mutation_operation(&reservation.operation_id, &config_operation_id)
            .unwrap();
        assert_eq!(snapshot.operation_id, reservation.operation_id);
        assert_eq!(
            snapshot.config_mutation_operation_id.as_deref(),
            Some(config_operation_id.as_str())
        );
        assert!(auth_start_authorization_digest(&reservation.operation_id).len() == 64);
        assert!(auth_start_authorization_digest(&reservation.operation_id)
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert!(supervisor
            .attach_config_mutation_operation(&reservation.operation_id, "not-an-id")
            .is_err());
    }

    #[test]
    fn p2b_auth_sidecar_receipt_identity_binds_live_process_and_executable() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("p2b-sidecar-identity");
        let operation_id = "ad".repeat(16);
        let script = temp.script("IFS= read -r control\nexit 0");
        let expected_fingerprint = auth_sidecar_executable_fingerprint(&script).unwrap();
        let mut process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();

        let identity = auth_sidecar_identity(&process).unwrap();

        assert_eq!(identity.pid, process.child.id());
        assert_eq!(identity.process_group_id, identity.pid as i32);
        assert_eq!(identity.executable_fingerprint, expected_fingerprint);
        assert_ne!(identity.process_start, format!("pid:{}", identity.pid));
        assert_eq!(
            crate::runtime::science::process_start_identity_digest(identity.pid).as_deref(),
            Some(identity.process_start.as_str())
        );
        stop_auth_child(&mut process.child);
    }

    #[test]
    fn auth_sidecar_identity_is_captured_before_fast_status_exit() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("auth-status-fast-exit-identity");
        let script = temp.script("exit 0");
        let expected_fingerprint = auth_sidecar_executable_fingerprint(&script).unwrap();
        let mut process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::Status,
            None,
            None,
            false,
        )
        .unwrap();
        assert!(process.child.wait().unwrap().success());

        let identity = auth_sidecar_identity(&process).unwrap();

        assert_eq!(identity.pid, process.child.id());
        assert_eq!(identity.process_group_id, identity.pid as i32);
        assert_eq!(identity.executable_fingerprint, expected_fingerprint);
        assert_ne!(identity.process_start, format!("pid:{}", identity.pid));
    }

    #[test]
    fn p2b_start_authorization_keeps_cancel_channel_until_terminal() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("p2b-start-cancel-channel");
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = supervisor.begin_login().unwrap();
        let operation_id = reservation.operation_id.clone();
        let digest = auth_start_authorization_digest(&operation_id);
        let script = temp.script(&format!(
            "IFS= read -r start\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"start_ack\",\"authorization_digest\":\"{digest}\"}}'\nIFS= read -r cancel\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"cancel_ack\",\"disposition\":\"accepted\"}}'\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"cancelled\",\"error\":{{\"code\":\"auth_cancelled\",\"stage\":\"cancelled\",\"retryable\":true}}}}'\nexit 7"
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let process = bind_test_login_process(&supervisor, &operation_id, process);
        let process =
            wait_for_login_start_ack(process, &operation_id, &digest, &supervisor).unwrap();
        assert!(process.stdin.is_some());

        let cancel = AtomicBool::new(true);
        let terminal = wait_for_login_sidecar(
            process,
            CodexAuthAction::LoginBrowser,
            &operation_id,
            &cancel,
            |_| {},
            |_| {},
        )
        .unwrap();
        assert_eq!(terminal["state"], "cancelled");
        supervisor.clear_login_pid(&operation_id);
        supervisor.abort_login_start(&operation_id);
    }

    #[test]
    fn p2b_protocol_error_reaps_sidecar_before_pid_is_forgotten() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("p2b-protocol-reap");
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = supervisor.begin_login().unwrap();
        let operation_id = reservation.operation_id.clone();
        let digest = auth_start_authorization_digest(&operation_id);
        let script = temp.script(&format!(
            "IFS= read -r start\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"start_ack\",\"authorization_digest\":\"{digest}\"}}'\nprintf '%s\\n' 'not-json'\nexec /bin/sleep 30"
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginBrowser,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let process = bind_test_login_process(&supervisor, &operation_id, process);
        let process =
            wait_for_login_start_ack(process, &operation_id, &digest, &supervisor).unwrap();
        let pid = process.child.id();

        let error = wait_for_login_sidecar(
            process,
            CodexAuthAction::LoginBrowser,
            &operation_id,
            &AtomicBool::new(false),
            |_| {},
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains("非法 NDJSON"), "unexpected error: {error}");
        assert!(crate::runtime::science::process_start_identity_digest(pid).is_none());
        supervisor.clear_login_pid(&operation_id);
        supervisor.abort_login_start(&operation_id);
    }

    #[test]
    fn p2b_receipt_admission_failure_releases_login_reservation_without_spawn() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("p2b-admission-release");
        let prior_home = env::var_os("HOME");
        env::set_var("HOME", &temp.0);
        let config_dir = config::default_dir();
        let (proxy_port, sandbox_port) = r0_distinct_ports();
        let cfg = config::Config {
            experimental_codex_enabled: true,
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        config::save_to(&config_dir, &cfg).unwrap();
        let conflicting_receipt = config_dir.join(config::CONFIG_MUTATION_OPERATION_RECEIPT_FILE);
        fs::write(&conflicting_receipt, b"foreign-receipt").unwrap();
        fs::set_permissions(&conflicting_receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let state = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = crate::lifecycle::Lifecycle::new();
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();

        let result = start_codex_login_p2b_inner(
            app.handle(),
            &state,
            &lifecycle,
            &supervisor,
            CodexAuthAction::LoginBrowser,
            |_, _, _, _| panic!("conflicting receipt must reject before sidecar spawn"),
        );

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("conflicting receipt unexpectedly admitted Codex login"),
        };
        assert!(matches!(error, RuntimeCommandError::Mutation(_)));
        assert!(supervisor.snapshot().is_none());
        let next = supervisor.begin_login().unwrap();
        supervisor.abort_login_start(&next.operation_id);
        match prior_home {
            Some(home) => env::set_var("HOME", home),
            None => env::remove_var("HOME"),
        }
    }

    #[test]
    fn p2b_terminal_receipt_failure_clears_pid_and_finishes_supervisor() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("p2b-terminal-receipt-failure");
        let prior_home = env::var_os("HOME");
        env::set_var("HOME", &temp.0);
        let config_dir = config::default_dir();
        let (proxy_port, sandbox_port) = r0_distinct_ports();
        config::save_to(
            &config_dir,
            &config::Config {
                experimental_codex_enabled: true,
                proxy_port,
                sandbox_port,
                ..Default::default()
            },
        )
        .unwrap();
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let state = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let (reservation, process, mutation, runtime_action) = start_codex_login_p2b_inner(
            app.handle(),
            &state,
            lifecycle.as_ref(),
            &supervisor,
            CodexAuthAction::LoginBrowser,
            |_, action, operation_id, route| {
                let digest = auth_start_authorization_digest(operation_id);
                let script = temp.script(&format!(
                    "IFS= read -r start\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"start_ack\",\"authorization_digest\":\"{digest}\"}}'\n/bin/sleep 1\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":true,\"reason\":\"ready\",\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":7}}}}'",
                    "ab".repeat(16),
                    "cd".repeat(16),
                ));
                spawn_codex_auth_sidecar_at(
                    &script,
                    &temp.0,
                    action,
                    Some(route),
                    Some(operation_id),
                    false,
                )
                .map_err(|_| {
                    CodexAuthCommandError::unavailable(
                        CodexAuthUnavailableCause::SidecarSpawnFailed,
                    )
                })
            },
        )
        .unwrap();
        let receipt = config_dir.join(config::CONFIG_MUTATION_OPERATION_RECEIPT_FILE);
        fs::rename(&receipt, config_dir.join("preserved-auth-receipt.json")).unwrap();
        fs::write(&receipt, b"replacement-receipt").unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();

        complete_login_operation_p2b(
            app.handle().clone(),
            lifecycle,
            supervisor.clone(),
            reservation.operation_id.clone(),
            reservation.cancel,
            process,
            CodexAuthAction::LoginBrowser,
            mutation,
            runtime_action,
        );

        let terminal = supervisor.snapshot().unwrap();
        assert_eq!(terminal.state, "failed");
        assert_eq!(terminal.error.unwrap().code, "config_mutation_attention");
        assert!(supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
        let next = supervisor.begin_login().unwrap();
        supervisor.abort_login_start(&next.operation_id);
        match prior_home {
            Some(home) => env::set_var("HOME", home),
            None => env::remove_var("HOME"),
        }
    }

    #[test]
    fn p2b_codex_auth_start_generation_crash_matrix() {
        let dir = TempDir::new("p2b-auth-start-crash");
        let cfg = config::Config::default();
        config::save_to(&dir.0, &cfg).unwrap();
        let mut operation = crate::commands::runtime::config_mutation::begin(
            &dir.0,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthStart,
            &cfg,
            None,
            crate::commands::runtime::config_mutation::MutationTarget::default(),
            crate::commands::runtime::config_mutation::RuntimePlan {
                owner_generation: 0,
                ..Default::default()
            },
            vec![crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthGenerationCommit],
            None,
            None,
        )
        .unwrap();
        operation
            .checkpoint_effect(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
            )
            .unwrap();
        assert!(config::read_config_mutation_operation_receipt(&dir.0)
            .unwrap()
            .is_some());
        let attention = operation
            .finish(
                "attention",
                "before",
                "unknown",
                Some("auth_generation_unknown"),
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .unwrap_err();
        assert!(attention.attention_required);
        assert!(config::read_config_mutation_operation_receipt(&dir.0)
            .unwrap()
            .is_some());

        for (action_index, runtime_action) in [
            AuthRuntimeAction::Noop,
            AuthRuntimeAction::PreserveOtherProvider,
            AuthRuntimeAction::StopManagedCodex,
        ]
        .into_iter()
        .enumerate()
        {
            for (cause_index, cause) in ["auth_failed", "auth_cancelled", "sidecar_protocol_error"]
                .into_iter()
                .enumerate()
            {
                let terminal_dir =
                    TempDir::new(&format!("p2b-auth-terminal-{action_index}-{cause_index}"));
                config::save_to(&terminal_dir.0, &cfg).unwrap();
                let mut terminal = crate::commands::runtime::config_mutation::begin(
                    &terminal_dir.0,
                    crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthStart,
                    &cfg,
                    None,
                    crate::commands::runtime::config_mutation::MutationTarget::default(),
                    crate::commands::runtime::config_mutation::RuntimePlan {
                        owner_generation: 0,
                        ..Default::default()
                    },
                    vec![crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthGenerationCommit],
                    None,
                    None,
                )
                .unwrap();
                retain_failed_login_receipt(&mut terminal, runtime_action, cause);
                let bytes = config::read_config_mutation_operation_receipt(&terminal_dir.0)
                    .unwrap()
                    .unwrap();
                let receipt: crate::commands::runtime::config_mutation::ConfigMutationReceipt =
                    serde_json::from_slice(&bytes).unwrap();
                assert_eq!(receipt.terminal.state, "attention");
                assert_eq!(
                    receipt.terminal.runtime_state,
                    auth_runtime_terminal_state(runtime_action)
                );
                assert_eq!(receipt.terminal.cause.as_deref(), Some(cause));
            }
        }

        let dynamic_dir = TempDir::new("p2b-auth-dynamic-after-drift");
        config::save_to(&dynamic_dir.0, &cfg).unwrap();
        let mut dynamic = crate::commands::runtime::config_mutation::begin(
            &dynamic_dir.0,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthStart,
            &cfg,
            None,
            crate::commands::runtime::config_mutation::MutationTarget::default(),
            crate::commands::runtime::config_mutation::RuntimePlan::default(),
            vec![
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::ProfileEnsure,
            ],
            None,
            None,
        )
        .unwrap();
        let ((), after_fingerprint) =
            config::update_config_mutation_operation_with_after_fingerprint(
                &dynamic_dir.0,
                dynamic.fence(),
                dynamic.receipt_bytes(),
                |current| {
                    current.reuse_system_ssh = !current.reuse_system_ssh;
                    Ok(((), true))
                },
            )
            .unwrap();
        dynamic
            .bind_after_config_fingerprint(after_fingerprint)
            .unwrap();
        let mut drifted = config::load_from(&dynamic_dir.0).unwrap();
        drifted.reuse_system_ssh = !drifted.reuse_system_ssh;
        config::test_save_to_without_history_authority_guard(&dynamic_dir.0, &drifted).unwrap();
        let drift = dynamic
            .finish(
                "completed",
                "after",
                "preserved",
                None,
                config::ConfigMutationTerminalConfigImage::After,
            )
            .unwrap_err();
        assert!(drift.attention_required);
        assert!(
            config::read_config_mutation_operation_receipt(&dynamic_dir.0)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn p2b_codex_network_non_destructive_returns_intent_identity() {
        let dir = TempDir::new("p2b-network-intent");
        let mut cfg = config::Config::default();
        cfg.codex_network.mode = csswitch_codex_network::CodexNetworkMode::Custom;
        cfg.codex_network.proxy_url = "http://127.0.0.1:8080".into();
        config::save_to(&dir.0, &cfg).unwrap();
        let settings = csswitch_codex_network::CodexNetworkSettings::default();
        let resolved = csswitch_codex_network::direct_route();

        let committed =
            set_codex_network_at(&dir.0, settings.clone(), &resolved, || Ok(())).unwrap();
        assert_eq!(committed.get("schema_version"), Some(&json!(1)));
        assert_eq!(
            committed.get("operation"),
            Some(&json!("set_codex_network"))
        );
        assert_eq!(committed.get("disposition"), Some(&json!("committed")));
        assert_eq!(committed.get("config_state"), Some(&json!("committed")));
        assert_eq!(committed.get("validation"), Some(&json!("not_run")));
        assert_eq!(committed.get("science_running"), Some(&json!(false)));
        assert!(committed.get("operation_id").is_none());
        assert!(config::read_config_mutation_operation_receipt(&dir.0)
            .unwrap()
            .is_none());
        let committed_intent = committed.get("intent_id").and_then(Value::as_str).unwrap();
        assert_eq!(committed_intent.len(), 32);
        assert!(committed_intent
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));

        let unchanged = set_codex_network_at(&dir.0, settings, &resolved, || Ok(())).unwrap();
        assert_eq!(unchanged.get("disposition"), Some(&json!("no_change")));
        let unchanged_intent = unchanged.get("intent_id").and_then(Value::as_str).unwrap();
        assert_eq!(unchanged_intent.len(), 32);
        assert_ne!(unchanged_intent, committed_intent);
        assert!(unchanged.get("operation_id").is_none());
        assert!(config::read_config_mutation_operation_receipt(&dir.0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn p2b_codex_logout_never_reactivates_committed_logout() {
        let _serial = R0_CODEX_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = TempDir::new("p2b-logout-terminal");
        let cfg = config::Config::default();
        config::save_to(&dir.0, &cfg).unwrap();
        let operation = crate::commands::runtime::config_mutation::begin(
            &dir.0,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthLogout,
            &cfg,
            Some(&cfg),
            crate::commands::runtime::config_mutation::MutationTarget {
                auth_generation: Some(7),
                ..Default::default()
            },
            crate::commands::runtime::config_mutation::RuntimePlan {
                owner_generation: 0,
                ..Default::default()
            },
            vec![crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthGenerationCommit],
            None,
            None,
        )
        .unwrap();
        let mut operation = operation;
        operation
            .checkpoint_effect(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
            )
            .unwrap();
        operation
            .checkpoint_effect(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some("logged_out_generation_7"),
            )
            .unwrap();
        let outcome = operation
            .finish(
                "completed",
                "after",
                "stopped",
                None,
                config::ConfigMutationTerminalConfigImage::After,
            )
            .unwrap();
        assert_eq!(outcome.disposition, "completed");
        assert!(config::read_config_mutation_operation_receipt(&dir.0)
            .unwrap()
            .is_none());

        let identity_dir = TempDir::new("p2b-logout-sidecar-identity");
        config::save_to(&identity_dir.0, &cfg).unwrap();
        let mut identity_operation = crate::commands::runtime::config_mutation::begin(
            &identity_dir.0,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthLogout,
            &cfg,
            Some(&cfg),
            crate::commands::runtime::config_mutation::MutationTarget::default(),
            crate::commands::runtime::config_mutation::RuntimePlan::default(),
            vec![
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopScience,
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopGateway,
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::AuthSidecar,
            ],
            Some(
                crate::commands::runtime::config_mutation::AuthOperationReceipt {
                    auth_operation_id: "11".repeat(16),
                    supervisor_sequence: 1,
                    state: "reserved".into(),
                    start_authorization_digest: None,
                    sidecar: None,
                    terminal_auth_epoch: None,
                    terminal_auth_generation: None,
                    terminal_account_hash: None,
                },
            ),
            None,
        )
        .unwrap();
        let identity = crate::commands::runtime::config_mutation::AuthSidecarIdentity {
            pid: 123,
            process_start: "process-start-123".into(),
            executable_fingerprint: "22".repeat(32),
            process_group_id: 123,
        };
        identity_operation
            .checkpoint_effect(
                2,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
            )
            .unwrap();
        checkpoint_logout_sidecar_start(&mut identity_operation, &identity).unwrap();
        let bytes = config::read_config_mutation_operation_receipt(&identity_dir.0)
            .unwrap()
            .unwrap();
        let persisted: crate::commands::runtime::config_mutation::ConfigMutationReceipt =
            serde_json::from_slice(&bytes).unwrap();
        let auth = persisted.auth_operation.unwrap();
        let sidecar = auth.sidecar.unwrap();
        assert_eq!(auth.state, "start_prepared");
        assert_eq!(
            auth.start_authorization_digest.as_deref(),
            Some(auth_start_authorization_digest(&"11".repeat(16)).as_str())
        );
        assert_eq!(sidecar.pid, identity.pid);
        assert_eq!(sidecar.process_start, identity.process_start);
        assert_eq!(
            sidecar.executable_fingerprint,
            identity.executable_fingerprint
        );
        assert_eq!(sidecar.process_group_id, identity.process_group_id);
        assert_eq!(
            persisted.effects[2].state,
            crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress
        );

        let sidecar_home = TempDir::new("p2b-controlled-logout");
        let auth_operation_id = "44".repeat(16);
        let start_digest = auth_start_authorization_digest(&auth_operation_id);
        let script = sidecar_home.script(&format!(
            "IFS= read -r start\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{auth_operation_id}\",\"kind\":\"start_ack\",\"authorization_digest\":\"{start_digest}\"}}'\nprintf '%s\\n' '{{\"schema_version\":3,\"operation_id\":\"{auth_operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":false,\"reason\":\"state_uncommitted\",\"account_hash\":null,\"expiry_state\":\"missing\",\"expires_at\":null,\"auth_epoch\":\"{}\",\"auth_generation\":9}}}}'",
            "55".repeat(16),
        ));
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let mutation = CodexAuthSupervisor::begin_mutation(&supervisor).unwrap();
        let identity_seen = Cell::new(false);
        let logged_out = run_codex_logout_sidecar_at_with_identity(
            &script,
            &sidecar_home.0,
            &csswitch_codex_network::direct_route(),
            false,
            &mutation,
            Some(&auth_operation_id),
            |identity| {
                assert_ne!(identity.process_start, format!("pid:{}", identity.pid));
                identity_seen.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(identity_seen.get());
        assert_eq!(logged_out["status"]["authenticated"], false);
        assert_eq!(logged_out["status"]["reason"], "state_uncommitted");
        assert_eq!(logged_out["status"]["auth_generation"], 9);

        let drift_dir = TempDir::new("p2b-logout-after-drift");
        config::save_to(&drift_dir.0, &cfg).unwrap();
        let mut drift_operation = crate::commands::runtime::config_mutation::begin(
            &drift_dir.0,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::CodexAuthLogout,
            &cfg,
            Some(&cfg),
            crate::commands::runtime::config_mutation::MutationTarget::default(),
            crate::commands::runtime::config_mutation::RuntimePlan::default(),
            vec![],
            None,
            None,
        )
        .unwrap();
        let mut drifted = config::load_from(&drift_dir.0).unwrap();
        drifted.reuse_system_ssh = !drifted.reuse_system_ssh;
        config::test_save_to_without_history_authority_guard(&drift_dir.0, &drifted).unwrap();
        let drift = drift_operation
            .finish(
                "completed",
                "after",
                "stopped",
                None,
                config::ConfigMutationTerminalConfigImage::After,
            )
            .unwrap_err();
        assert!(drift.attention_required);
        assert!(config::read_config_mutation_operation_receipt(&drift_dir.0)
            .unwrap()
            .is_some());
    }
}
