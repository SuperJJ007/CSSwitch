use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::codex_auth_supervisor::{
    AuthPreflightReservation, CodexAuthReadyProof, CodexAuthSupervisor, CodexMutationLease,
    OperationErrorView, OperationSnapshot, SharedCodexAuthSupervisor,
};
use crate::proc::ChildLiveness;
use crate::runtime::proxy_lifecycle::gateway_bin_path;
use crate::runtime::science::{
    probe_known_runtime, probe_sandbox_runtime_cached, SandboxScienceState,
};
use crate::runtime::system::kill_child;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

const AUTH_SCHEMA_VERSION: u32 = 3;
const MAX_AUTH_LINE_BYTES: usize = 8 * 1024;
const MAX_AUTH_OUTPUT_BYTES: u64 = 64 * 1024;
const AUTH_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ACCEPTED_CANCEL_WATCHDOG: Duration = Duration::from_secs(2);
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

    fn unavailable(cause: &'static str) -> Self {
        Self {
            code: "codex_auth_unavailable",
            reason: None,
            cause: Some(cause),
            retryable: matches!(
                cause,
                "keychain_unavailable"
                    | "interaction_timeout"
                    | "storage_unavailable"
                    | "auth_state_changed"
            ),
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

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum RuntimeCommandError {
    Auth(CodexAuthCommandError),
    Message(String),
}

impl From<CodexAuthCommandError> for RuntimeCommandError {
    fn from(error: CodexAuthCommandError) -> Self {
        Self::Auth(error)
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
            CodexAuthCommandError::unavailable("interaction_timeout")
        }
        SidecarWaitFailure::Protocol => {
            CodexAuthCommandError::unavailable("sidecar_protocol_error")
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

enum DowngradeCommandOutcome {
    Committed(Value),
    SafeFailure(String),
    TerminalFailure(String),
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

/// Prepare for a CSSwitch-owned Codex credential mutation. Only a runtime whose
/// in-memory launch identity is exactly `codex` is stopped. Other known providers
/// remain untouched; an alive but unidentified managed child fails closed.
fn prepare_codex_auth_mutation<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<AuthRuntimeAction, String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| {
        format!("读取配置失败；为避免遗漏残留 Codex Science，认证未变更：{error}")
    })?;
    let active_profile_is_codex = cfg
        .active_profile()
        .is_some_and(|profile| profile.template_id == "codex");
    let (provider, tracked, remembered_runtime, version_cache) = {
        let mut st = lock(state);
        let provider = st.provider.clone();
        let tracked = tracked_proxy_state(&mut st);
        (
            provider,
            tracked,
            st.science_runtime
                .clone()
                .or_else(|| st.science_confirmed_stopped.clone()),
            st.science_version_cache.clone(),
        )
    };
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
            let science_state = probe_known_runtime(cfg.sandbox_port, &runtime);
            let detected =
                (science_state == SandboxScienceState::RunningHealthy).then_some(runtime);
            (science_state, detected)
        }
        None => probe_sandbox_runtime_cached(cfg.sandbox_port, &version_cache)?,
    };
    let action =
        resolve_science_runtime_action(proxy_action, active_profile_is_codex, science_state)?;
    if action == AuthRuntimeAction::StopManagedCodex {
        let mut st = lock(state);
        if science_state == SandboxScienceState::RunningHealthy {
            st.science_runtime = detected_runtime;
            super::runtime::stop_sandbox_state(app, &mut st).map_err(|error| {
                format!("停止受管 Codex Science 链路失败；认证未变更，实验开关也未关闭：{error}")
            })?;
        } else {
            kill_child(&mut st.sandbox);
            st.sandbox_url = None;
            st.science_confirmed_stopped = remembered_runtime;
            st.science_runtime = None;
        }
        lifecycle.bump_generation();
        st.stop_proxy();
    }
    Ok(action)
}

fn set_experimental_codex_enabled_at(
    dir: &Path,
    enabled: bool,
    before_disable: impl FnOnce() -> Result<(), String>,
) -> Result<Value, String> {
    if !enabled {
        before_disable()?;
    }
    config::update(dir, move |cfg| {
        cfg.experimental_codex_enabled = enabled;
    })
    .map_err(|error| error.to_string())?;
    Ok(json!({ "experimental_codex_enabled": enabled }))
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

fn stop_all_before_downgrade(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<(), String> {
    lifecycle.bump_generation();
    let mut app_state = lock(state);
    let sandbox_result = super::runtime::stop_sandbox_state(app, &mut app_state);
    app_state.stop_proxy();
    sandbox_result.map_err(|error| {
        format!("降级前无法安全停止受管 Science；配置、导出和本地认证文件均未修改：{error}")
    })
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
    let mut command = Command::new(binary);
    command
        .arg("codex-auth")
        .arg(action.as_str())
        .env_clear()
        .env("HOME", home)
        .stdin(if action.is_login() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if action.is_login() {
        let operation_id = operation_id
            .filter(|value| is_lower_hex(value, 32))
            .ok_or_else(|| "Codex 登录 operation ID 非法。".to_string())?;
        command.env("CSSWITCH_CODEX_AUTH_OPERATION_ID", operation_id);
    } else if operation_id.is_some() {
        return Err("非登录 sidecar 不得携带 operation ID。".into());
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
    let stdin = child.stdin.take();
    if action.is_login() && stdin.is_none() {
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
    })
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
    } = process;

    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
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

fn wait_for_login_sidecar(
    mut process: ManagedAuthProcess,
    action: CodexAuthAction,
    operation_id: &str,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(&LoginSidecarEvent),
    mut on_cancel_ack: impl FnMut(&str),
) -> Result<Value, String> {
    if !action.is_login() || !is_lower_hex(operation_id, 32) {
        stop_auth_child(&mut process.child);
        return Err("Codex 登录流式协议参数非法。".into());
    }
    let deadline = Instant::now() + action.timeout();
    let mut pending = Vec::new();
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
            send_cancel_to_sidecar(&mut process.stdin, operation_id)?;
            cancel_sent = true;
        }
        loop {
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
                        let event: LoginSidecarEvent = serde_json::from_slice(&line)
                            .map_err(|_| "Codex 认证 sidecar 返回了非法 NDJSON。".to_string())?;
                        if event.schema_version != AUTH_SCHEMA_VERSION
                            || event.operation_id != operation_id
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
                                if terminal.is_some() || event.disposition.is_some() {
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
                                        validate_status(status)?;
                                        if !status.authenticated {
                                            stop_auth_child(&mut process.child);
                                            return Err(
                                                "Codex 认证成功终态必须包含已登录状态。".into()
                                            );
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
                                            || (state == "cancelled"
                                                && error.code != "auth_cancelled")
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
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    stop_auth_child(&mut process.child);
                    return Err("Codex 认证 sidecar 输出读取失败。".into());
                }
            }
        }
        if exit_status.is_none() {
            exit_status = process
                .child
                .try_wait()
                .map_err(|_| "无法确认 Codex 认证 sidecar 退出状态。".to_string())?;
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
    let mut process = spawn_codex_auth_sidecar_at(
        &binary,
        &production_home()
            .map_err(|_| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))?,
        CodexAuthAction::Status,
        Some(route),
        None,
        false,
    )
    .map_err(|_| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))?;
    if reservation.set_pid(process.child.id()).is_err() {
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
    let binary = gateway_bin_path(app)
        .ok_or_else(|| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))?;
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
        &production_home()
            .map_err(|_| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))?,
        action,
        Some(route),
        Some(operation_id),
        false,
    )
    .map_err(|_| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))
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

fn run_codex_logout_sidecar<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    mutation: &CodexMutationLease,
) -> Result<Value, CodexAuthCommandError> {
    let binary = codex_gateway_bin(app)?;
    let (route, skip_revoke) = match resolve_codex_network_route() {
        Ok(route) => (route, false),
        Err(_) => (csswitch_codex_network::direct_route(), true),
    };
    let mut process = spawn_codex_auth_sidecar_at(
        &binary,
        &production_home()
            .map_err(|_| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))?,
        CodexAuthAction::Logout,
        Some(&route),
        None,
        skip_revoke,
    )
    .map_err(|_| CodexAuthCommandError::unavailable("sidecar_spawn_failed"))?;
    if mutation.set_pid(process.child.id()).is_err() {
        stop_auth_child(&mut process.child);
        return Err(CodexAuthCommandError::unavailable("auth_state_changed"));
    }
    let result = wait_for_single_sidecar_response_controlled(
        process,
        CodexAuthAction::Logout,
        CodexAuthAction::Logout.timeout(),
        None,
    );
    mutation.clear_pid();
    result.map_err(auth_error_from_sidecar_wait)
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
                "keychain_unavailable" => {
                    CodexAuthCommandError::unavailable("keychain_unavailable")
                }
                "auth_state_invalid" => CodexAuthCommandError::unavailable("auth_state_invalid"),
                "auth_storage_error" => CodexAuthCommandError::unavailable("storage_unavailable"),
                "unsupported_platform" => {
                    CodexAuthCommandError::unavailable("unsupported_platform")
                }
                "auth_changed" => CodexAuthCommandError::unavailable("auth_state_changed"),
                "identity_mismatch" => CodexAuthCommandError::unavailable("identity_mismatch"),
                _ => CodexAuthCommandError::unavailable("sidecar_protocol_error"),
            })
        }
        Some(true) => {
            let authenticated = value
                .pointer("/status/authenticated")
                .and_then(Value::as_bool)
                .ok_or_else(|| CodexAuthCommandError::unavailable("sidecar_protocol_error"))?;
            if authenticated {
                Ok(())
            } else {
                let reason = value
                    .pointer("/status/reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| CodexAuthCommandError::unavailable("sidecar_protocol_error"))?;
                Err(CodexAuthCommandError::login_required(reason))
            }
        }
        None => Err(CodexAuthCommandError::unavailable("sidecar_protocol_error")),
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
    if adapter != "codex" {
        return Ok(None);
    }
    let snapshot = CodexLaunchSnapshot::capture(&target).map_err(RuntimeCommandError::from)?;
    let route = resolve_codex_network_route().map_err(RuntimeCommandError::from)?;
    let supervisor = app.state::<SharedCodexAuthSupervisor>().inner().clone();
    let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor)
        .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
    let value = match run_codex_auth_preflight_sidecar(app, &reservation, &route) {
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
    let (reservation, process) = crate::run_blocking_typed(move || {
        lifecycle.with_serialized(|| -> Result<_, RuntimeCommandError> {
            let cfg = config::load_from(&config::default_dir())
                .map_err(|error| RuntimeCommandError::from(error.to_string()))?;
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
                prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())
                    .map_err(RuntimeCommandError::from)?;
                let process = spawn_codex_auth_sidecar(&app, action, &operation_id, &route)
                    .map_err(RuntimeCommandError::from)?;
                register_login_process(&supervisor, &operation_id, process)
            })();
            if process.is_err() {
                supervisor.abort_login_start(&operation_id);
            }
            process.map(|process| (reservation, process))
        })
    })
    .await?;
    let response = serde_json::to_value(&reservation.snapshot)
        .map_err(|_| RuntimeCommandError::from("无法编码 Codex 登录 operation。"))?;
    let operation_id = reservation.operation_id.clone();
    let cancel = reservation.cancel.clone();
    let _worker = tauri::async_runtime::spawn_blocking(move || {
        complete_login_operation(
            worker_app,
            worker_lifecycle,
            worker_supervisor,
            operation_id,
            cancel,
            process,
            action,
        );
    });
    Ok(response)
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

fn emit_operation_snapshot<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    snapshot: &OperationSnapshot,
) {
    let _ = app.emit("codex-auth://operation", snapshot);
}

fn complete_login_operation<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    lifecycle: SharedLifecycle,
    supervisor: SharedCodexAuthSupervisor,
    operation_id: String,
    cancel: std::sync::Arc<AtomicBool>,
    process: ManagedAuthProcess,
    action: CodexAuthAction,
) {
    let progress_app = app.clone();
    let progress_supervisor = supervisor.clone();
    let progress_operation_id = operation_id.clone();
    let ack_supervisor = supervisor.clone();
    let ack_operation_id = operation_id.clone();
    let outcome = wait_for_login_sidecar(
        process,
        action,
        &operation_id,
        cancel.as_ref(),
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
    );
    record_login_terminal_auth_status(&supervisor, &outcome);
    let snapshot =
        finalize_login_operation(&supervisor, &lifecycle, &operation_id, outcome, || {
            crate::runtime::profile::ensure_codex_profile_inner(&config::default_dir())
        });
    if let Ok(snapshot) = snapshot {
        emit_operation_snapshot(&app, &snapshot);
    }
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
            match lifecycle.with_serialized(ensure_profile) {
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
        let prepared = prepare_provider_auth(&app, "codex", CodexPreflightTarget::NoProfile)?
            .ok_or_else(|| RuntimeCommandError::from("Codex preflight 未建立。"))?;
        lifecycle
            .with_serialized(|| -> Result<_, String> {
                prepared.verify_unchanged()?;
                ensure_codex_profile_authenticated(&config::default_dir())
            })
            .map_err(RuntimeCommandError::from)
    })
    .await
}

fn ensure_codex_profile_authenticated(dir: &Path) -> Result<Value, String> {
    let result = crate::runtime::profile::ensure_codex_profile_inner(dir).map_err(|_| {
        "profile_ensure_failed：授权已保存，但无法创建 Codex 配置；请重试。".to_string()
    })?;
    let disposition = match result.disposition {
        crate::runtime::profile::EnsureCodexProfileDisposition::Created => "created",
        crate::runtime::profile::EnsureCodexProfileDisposition::Existing => "existing",
    };
    Ok(json!({
        "disposition": disposition,
        "profile_id": result.profile_id,
    }))
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
    let supervisor = supervisor.inner().clone();
    let logout_supervisor = supervisor.clone();
    let logout_app = app.clone();
    let mutation: CodexMutationLease = crate::run_blocking_typed(move || {
        lifecycle.with_serialized(|| -> Result<_, RuntimeCommandError> {
            let mutation = CodexAuthSupervisor::begin_mutation(&supervisor)
                .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
            prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())
                .map_err(RuntimeCommandError::from)?;
            Ok(mutation)
        })
    })
    .await?;
    crate::run_blocking_typed(move || {
        let mutation = mutation;
        let value = match run_codex_logout_sidecar(&logout_app, &mutation) {
            Ok(value) => value,
            Err(error) => {
                logout_supervisor.record_auth_status("unavailable", None, error.cause);
                return Err(RuntimeCommandError::from(error));
            }
        };
        record_last_auth_status(&logout_supervisor, &value);
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
        lifecycle.with_serialized(|| -> Result<_, RuntimeCommandError> {
            let _mutation = if enabled {
                None
            } else {
                Some(
                    CodexAuthSupervisor::begin_mutation(&supervisor)
                        .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?,
                )
            };
            set_experimental_codex_enabled_at(&config::default_dir(), enabled, || {
                prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref()).map(|_| ())
            })
            .map_err(RuntimeCommandError::from)
        })
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
    let mode = settings.mode;
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    crate::run_blocking_typed(move || {
        lifecycle.with_serialized(|| -> Result<_, RuntimeCommandError> {
            let _mutation = CodexAuthSupervisor::begin_mutation(&supervisor)
                .map_err(|_| RuntimeCommandError::from(CodexAuthCommandError::busy()))?;
            prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())
                .map_err(RuntimeCommandError::from)?;
            config::update(&config::default_dir(), move |cfg| {
                cfg.codex_network = settings;
            })
            .map_err(|error| RuntimeCommandError::from(error.to_string()))?;
            Ok(json!({
                "mode": mode,
                "source": resolved.source,
                "proxy_scheme": resolved.proxy_scheme,
                "restarted": false,
            }))
        })
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
        lifecycle.with_serialized(|| {
            let dir = config::default_dir();
            let cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
            let actions = downgrade_actions_for_expected(
                &cfg,
                &expected_profile_ids,
                &expected_preview_fingerprint,
            )?;
            stop_all_before_downgrade(&app, &state, lifecycle.as_ref())?;
            Ok(match config::downgrade_to_v2_and_latch(
                &dir,
                &actions,
                Some(&destination),
                &expected_preview_fingerprint,
            ) {
                Ok(_) => DowngradeCommandOutcome::Committed(json!({
                    "schema_version": 1,
                    "status": "DOWNGRADED_EXIT_REQUIRED",
                    "profile_count": actions.len(),
                    "exported": true,
                    "credentials_unchanged": true,
                    "app_exit_required": true,
                })),
                Err(error) if error.exit_required => {
                    DowngradeCommandOutcome::TerminalFailure(format!(
                        "v2 配置发布后的持久化或回滚状态不确定；进程已锁存并强制退出，禁止再次读取配置：{}",
                        error.message
                    ))
                }
                Err(error) => DowngradeCommandOutcome::SafeFailure(error.message),
            })
        })
    })
    .await?;
    match outcome {
        DowngradeCommandOutcome::SafeFailure(error) => Err(error),
        DowngradeCommandOutcome::Committed(result) => {
            // The managed runtime was already stopped before the v2 commit. Do
            // not use generic quit_app: it may reload config to rediscover a
            // stopped sandbox and migrate v2 back to v3.
            exit_app.exit(0);
            Ok(result)
        }
        DowngradeCommandOutcome::TerminalFailure(error) => {
            // Even an error is terminal once rename publication cannot be
            // proven rolled back. The latch rejects every config caller during
            // the short interval before this direct exit.
            exit_app.exit(1);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

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
            let path = self.0.join("fake-sidecar");
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
        for (cause, retryable) in [
            ("keychain_unavailable", true),
            ("interaction_timeout", true),
            ("sidecar_spawn_failed", false),
            ("sidecar_protocol_error", false),
            ("identity_mismatch", false),
            ("auth_state_invalid", false),
            ("storage_unavailable", true),
            ("unsupported_platform", false),
            ("auth_state_changed", true),
        ] {
            let value = serde_json::to_value(RuntimeCommandError::from(
                CodexAuthCommandError::unavailable(cause),
            ))
            .unwrap();
            assert_eq!(
                value,
                json!({"code":"codex_auth_unavailable","cause":cause,"retryable":retryable})
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
}
