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
    CodexAuthSupervisor, CodexMutationLease, CodexUseLease, OperationErrorView, OperationSnapshot,
    SharedCodexAuthSupervisor,
};
use crate::proc::ChildLiveness;
use crate::runtime::proxy_lifecycle::gateway_bin_path;
use crate::runtime::science::{
    probe_known_runtime, probe_sandbox_runtime_cached, SandboxScienceState,
};
use crate::runtime::system::kill_child;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

const AUTH_SCHEMA_VERSION: u32 = 2;
const MAX_AUTH_LINE_BYTES: usize = 8 * 1024;
const MAX_AUTH_OUTPUT_BYTES: u64 = 64 * 1024;
const AUTH_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(feature = "acceptance-keychain"))]
const EXPECTED_CODEX_KEYCHAIN_SERVICE: &str = "com.csswitch.codex.oauth.v1";
#[cfg(feature = "acceptance-keychain")]
const EXPECTED_CODEX_KEYCHAIN_SERVICE: &str = "com.csswitch.acceptance.codex.oauth.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexAuthAction {
    LoginDevice,
    LoginBrowser,
    Status,
    Logout,
}

struct ManagedAuthProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::process::ChildStdout,
}

impl CodexAuthAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::LoginDevice => "login-device",
            Self::LoginBrowser => "login-browser",
            Self::Status => "status",
            Self::Logout => "logout",
        }
    }

    fn timeout(self) -> Duration {
        match self {
            // Gateway's browser callback budget is five minutes. The outer
            // supervisor allows a small cleanup margin but never waits forever.
            Self::LoginDevice => Duration::from_secs(15 * 60 + 15),
            Self::LoginBrowser => Duration::from_secs(5 * 60 + 15),
            Self::Status => Duration::from_secs(15),
            Self::Logout => Duration::from_secs(60),
        }
    }

    fn is_login(self) -> bool {
        matches!(self, Self::LoginDevice | Self::LoginBrowser)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthStatusView {
    authenticated: bool,
    account_hash: Option<String>,
    expiry_state: String,
    expires_at: Option<i64>,
    auth_epoch: Option<String>,
    auth_generation: u64,
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
    verification_url: Option<String>,
    #[serde(default)]
    user_code: Option<String>,
    #[serde(default)]
    expires_at_ms: Option<i64>,
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
fn prepare_codex_auth_mutation(
    app: &tauri::AppHandle,
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

fn codex_downgrade_preview_for(cfg: &config::Config) -> Value {
    let profiles: Vec<Value> = cfg
        .profiles
        .iter()
        .filter(|profile| {
            profile.credential_source == crate::provider_contracts::CredentialSource::KeychainOauth
        })
        .map(|profile| json!({ "id": profile.id, "name": profile.name }))
        .collect();
    let active_will_clear = profiles
        .iter()
        .any(|profile| profile["id"].as_str() == Some(cfg.active_id.as_str()));
    json!({
        "schema_version": 1,
        "action": "export_then_remove_all",
        "profile_count": profiles.len(),
        "profiles": profiles,
        "active_will_clear": active_will_clear,
        "keychain_unchanged": true,
        "app_exit_required": true,
    })
}

fn downgrade_actions_for_expected(
    cfg: &config::Config,
    expected_profile_ids: &[String],
) -> Result<BTreeMap<String, config::CodexDowngradeAction>, String> {
    let current: BTreeSet<String> = cfg
        .profiles
        .iter()
        .filter(|profile| {
            profile.credential_source == crate::provider_contracts::CredentialSource::KeychainOauth
        })
        .map(|profile| profile.id.clone())
        .collect();
    let expected: BTreeSet<String> = expected_profile_ids.iter().cloned().collect();
    if current.is_empty() || expected.len() != expected_profile_ids.len() || current != expected {
        return Err(
            "Codex profile 列表已变化或确认参数不完整；未导出、未降级，请重新预览。".into(),
        );
    }
    Ok(current
        .into_iter()
        .map(|id| (id, config::CodexDowngradeAction::ExportThenRemove))
        .collect())
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
        format!("降级前无法安全停止受管 Science；配置、导出和 Keychain 均未修改：{error}")
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
        if status.account_hash.is_none()
            || status.auth_epoch.is_none()
            || status.expiry_state == "missing"
        {
            return Err("Codex 认证 sidecar 返回了不一致的已登录状态。".into());
        }
    } else if status.account_hash.is_some() || status.expiry_state != "missing" {
        return Err("Codex 认证 sidecar 返回了不一致的未登录状态。".into());
    }
    Ok(())
}

fn allowed_error_code(code: &str) -> bool {
    expected_error_exit_code(code).is_some()
}

fn summarize_auth_for_diagnostics(value: &Value) -> String {
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => {
            let Some(status) = value.get("status") else {
                return "auth=protocol_error".into();
            };
            let authenticated = status
                .get("authenticated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let expiry = status
                .get("expiry_state")
                .and_then(Value::as_str)
                .filter(|state| {
                    matches!(
                        *state,
                        "missing" | "unknown" | "expired" | "expiring" | "valid"
                    )
                });
            match (authenticated, expiry) {
                (true, Some(state)) if state != "missing" => {
                    format!("auth=authenticated expiry={state}")
                }
                (false, Some("missing")) => "auth=unauthenticated expiry=missing".into(),
                _ => "auth=protocol_error".into(),
            }
        }
        Some(false) => value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .filter(|code| allowed_error_code(code))
            .map(|code| format!("auth=error code={code}"))
            .unwrap_or_else(|| "auth=protocol_error".into()),
        None => "auth=protocol_error".into(),
    }
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
        | "device_auth_unavailable"
        | "auth_cancelled" => Some(7),
        "internal_error" => Some(8),
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
        "keychain_unavailable" => "无法访问 CSSwitch 专用的 macOS 钥匙串项目。",
        "not_authenticated" => "CSSwitch 尚未登录 Codex。",
        "oauth_denied" => "Codex 登录未获授权。",
        "oauth_network_error" => "Codex 认证网络请求失败，请稍后重试。",
        "oauth_protocol_error" => "Codex 认证服务返回了无法识别的响应。",
        "oauth_unexpected_content_type" => "Codex 认证服务返回了意外的内容类型。",
        "oauth_challenge_response" => "Codex 认证请求遇到上游安全挑战。",
        "proxy_connect_failed" => "Codex 认证无法连接所选代理。",
        "tls_failed" => "Codex 认证 TLS 连接失败。",
        "device_auth_unavailable" => "当前服务未启用设备码登录，请改用浏览器登录。",
        "auth_cancelled" => "Codex 登录已取消。",
        "auth_storage_error" => "CSSwitch 无法安全保存 Codex 认证状态。",
        "unsupported_platform" => "当前平台不支持 CSSwitch Codex 钥匙串认证。",
        _ => "Codex 认证 sidecar 发生内部错误。",
    }
}

fn allowed_stage(stage: &str) -> bool {
    matches!(
        stage,
        "proxy_config"
            | "device_code_request"
            | "device_wait"
            | "browser_open"
            | "callback_wait"
            | "token_exchange"
            | "refresh"
            | "revoke"
            | "keychain_commit"
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
        .env(
            "CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE",
            EXPECTED_CODEX_KEYCHAIN_SERVICE,
        )
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

fn wait_for_single_sidecar_response(
    mut process: ManagedAuthProcess,
    action: CodexAuthAction,
    timeout: Duration,
) -> Result<Value, String> {
    if action.is_login() || process.stdin.is_some() {
        stop_auth_child(&mut process.child);
        return Err("登录 sidecar 必须使用流式协议读取。".into());
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
                        return Err("Codex 认证 sidecar 输出读取失败或超过 64 KiB。".into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    stop_auth_child(child);
                    return Err("Codex 认证 sidecar 输出读取失败或超过 64 KiB。".into());
                }
            }
        }
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(_) => {
                    stop_auth_child(child);
                    return Err("无法确认 Codex 认证 sidecar 退出状态。".into());
                }
            }
        }
        if exit_status.is_some() && output_eof {
            break;
        }
        if Instant::now() >= deadline {
            stop_auth_child(child);
            return Err("Codex 认证 sidecar 超时，受管进程已结束。".into());
        }
        std::thread::sleep(AUTH_POLL_INTERVAL);
    }
    parse_sidecar_output(&bytes, action, exit_status.and_then(|status| status.code()))
}

fn valid_device_user_code(value: &str) -> bool {
    let Some((left, right)) = value.split_once('-') else {
        return false;
    };
    (4..=8).contains(&left.len())
        && (4..=8).contains(&right.len())
        && left
            .chars()
            .chain(right.chars())
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
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
                                if !matches!(
                                    state,
                                    "verification_required"
                                        | "waiting"
                                        | "exchanging"
                                        | "committing"
                                ) {
                                    stop_auth_child(&mut process.child);
                                    return Err("Codex 认证 progress 状态非法。".into());
                                }
                                if state == "verification_required" {
                                    if action != CodexAuthAction::LoginDevice
                                        || event.verification_url.as_deref()
                                            != Some("https://auth.openai.com/codex/device")
                                        || event
                                            .user_code
                                            .as_deref()
                                            .is_none_or(|code| !valid_device_user_code(code))
                                        || event.expires_at_ms.is_none()
                                    {
                                        stop_auth_child(&mut process.child);
                                        return Err("Codex 设备码 progress 字段非法。".into());
                                    }
                                } else if event.verification_url.is_some()
                                    || event.user_code.is_some()
                                    || event.expires_at_ms.is_some()
                                {
                                    stop_auth_child(&mut process.child);
                                    return Err("Codex 认证 progress 携带了多余字段。".into());
                                }
                                on_progress(&event);
                            }
                            "cancel_ack" => {
                                if !cancel_sent
                                    || event.state.is_some()
                                    || event.status.is_some()
                                    || event.error.is_some()
                                    || event.verification_url.is_some()
                                    || event.user_code.is_some()
                                    || event.expires_at_ms.is_some()
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
                                    || event.verification_url.is_some()
                                    || event.user_code.is_some()
                                    || event.expires_at_ms.is_some()
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
                                        validate_status(status)?;
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
        if accepted_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(2)) {
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

fn run_codex_auth_sidecar<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    action: CodexAuthAction,
) -> Result<Value, String> {
    let binary = gateway_bin_path(app).ok_or("找不到受管 csswitch-gateway sidecar。")?;
    let route = resolve_codex_network_route()?;
    let process = spawn_codex_auth_sidecar_at(
        &binary,
        &production_home()?,
        action,
        Some(&route),
        None,
        false,
    )?;
    wait_for_single_sidecar_response(process, action, action.timeout())
}

fn spawn_codex_auth_sidecar<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    action: CodexAuthAction,
    operation_id: &str,
    route: &csswitch_codex_network::ResolvedCodexNetworkRoute,
) -> Result<ManagedAuthProcess, String> {
    let binary = gateway_bin_path(app).ok_or("找不到受管 csswitch-gateway sidecar。")?;
    spawn_codex_auth_sidecar_at(
        &binary,
        &production_home()?,
        action,
        Some(route),
        Some(operation_id),
        false,
    )
}

fn run_codex_logout_sidecar<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Value, String> {
    let binary = gateway_bin_path(app).ok_or("找不到受管 csswitch-gateway sidecar。")?;
    let (route, skip_revoke) = match resolve_codex_network_route() {
        Ok(route) => (route, false),
        Err(_) => (csswitch_codex_network::direct_route(), true),
    };
    let process = spawn_codex_auth_sidecar_at(
        &binary,
        &production_home()?,
        CodexAuthAction::Logout,
        Some(&route),
        None,
        skip_revoke,
    )?;
    wait_for_single_sidecar_response(
        process,
        CodexAuthAction::Logout,
        CodexAuthAction::Logout.timeout(),
    )
}

fn resolve_codex_network_route() -> Result<csswitch_codex_network::ResolvedCodexNetworkRoute, String>
{
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    csswitch_codex_network::resolve_from_process(&cfg.codex_network)
        .map_err(|_| "proxy_config_invalid：Codex 网络代理配置非法。".to_string())
}

fn require_authenticated_status(value: &Value) -> Result<(), String> {
    if value
        .pointer("/status/authenticated")
        .and_then(Value::as_bool)
        == Some(true)
    {
        Ok(())
    } else {
        Err("CODEX_LOGIN_REQUIRED：请先在 CSSwitch 中登录 Codex，再启动或验证该连接。".into())
    }
}

/// Backend authorization boundary shared by formal proxy, scratch/model discovery,
/// and Science auto-boot. It checks only CSSwitch-owned Keychain state through the
/// managed sidecar and never reads or mutates native Codex CLI credentials.
pub(crate) fn ensure_provider_auth_ready<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    adapter: &str,
) -> Result<Option<CodexUseLease>, String> {
    if adapter != "codex" {
        return Ok(None);
    }
    let supervisor = app.state::<SharedCodexAuthSupervisor>().inner().clone();
    let lease = CodexAuthSupervisor::acquire_use(&supervisor)?;
    let value = run_codex_auth_sidecar(app, CodexAuthAction::Status)
        .map_err(|error| format!("CODEX_AUTH_UNAVAILABLE：{error}"))?;
    require_authenticated_status(&value)?;
    Ok(Some(lease))
}

/// Doctor-only projection. The raw status contains account and auth-generation
/// identifiers needed by the UI contract; diagnostics deliberately discard all
/// of them, as well as sidecar messages and local paths.
pub(crate) fn codex_auth_diagnostic_summary<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> String {
    match run_codex_auth_sidecar(app, CodexAuthAction::Status) {
        Ok(value) => summarize_auth_for_diagnostics(&value),
        Err(_) => "auth=unavailable".into(),
    }
}

#[tauri::command]
pub(crate) async fn codex_auth_status(app: tauri::AppHandle) -> Result<Value, String> {
    run_blocking(move || run_codex_auth_sidecar(&app, CodexAuthAction::Status)).await
}

#[tauri::command]
pub(crate) async fn codex_auth_start(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    supervisor: State<'_, SharedCodexAuthSupervisor>,
    method: String,
) -> Result<Value, String> {
    let action = match method.as_str() {
        "device" => CodexAuthAction::LoginDevice,
        "browser" => CodexAuthAction::LoginBrowser,
        _ => return Err("Codex 登录 method 必须是 device 或 browser。".into()),
    };
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    let worker_app = app.clone();
    let worker_supervisor = supervisor.clone();
    let method_for_start = method.clone();
    let (reservation, process) = run_blocking(move || {
        lifecycle.with_serialized(|| {
            let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
            config::require_template_enabled(&cfg, "codex")?;
            let route = csswitch_codex_network::resolve_from_process(&cfg.codex_network)
                .map_err(|_| "proxy_config_invalid：Codex 网络代理配置非法。".to_string())?;
            let reservation = supervisor.begin_login(&method_for_start)?;
            let operation_id = reservation.operation_id.clone();
            let process = (|| {
                prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())?;
                let process = spawn_codex_auth_sidecar(&app, action, &operation_id, &route)?;
                supervisor.set_pid(&operation_id, process.child.id())?;
                Ok(process)
            })();
            if process.is_err() {
                supervisor.abort_login_start(&operation_id);
            }
            process.map(|process| (reservation, process))
        })
    })
    .await?;
    let response = serde_json::to_value(&reservation.snapshot)
        .map_err(|_| "无法编码 Codex 登录 operation。".to_string())?;
    let operation_id = reservation.operation_id.clone();
    let cancel = reservation.cancel.clone();
    let _worker = tauri::async_runtime::spawn_blocking(move || {
        complete_login_operation(
            worker_app,
            worker_supervisor,
            operation_id,
            cancel,
            process,
            action,
        );
    });
    Ok(response)
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
            if let Ok(snapshot) = progress_supervisor.update_progress(
                &progress_operation_id,
                state,
                event.expires_at_ms,
                event.verification_url.clone(),
                event.user_code.clone(),
            ) {
                emit_operation_snapshot(&progress_app, &snapshot);
            }
        },
        move |disposition| {
            ack_supervisor.record_cancel_disposition(&ack_operation_id, disposition);
        },
    );
    let snapshot = match outcome {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            supervisor.finish(&operation_id, "succeeded", None)
        }
        Ok(value) => {
            let state = if value.get("state").and_then(Value::as_str) == Some("cancelled") {
                "cancelled"
            } else {
                "failed"
            };
            supervisor.finish(
                &operation_id,
                state,
                Some(operation_error_from_envelope(&value)),
            )
        }
        Err(_) => supervisor.finish(
            &operation_id,
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
    };
    if let Ok(snapshot) = snapshot {
        emit_operation_snapshot(&app, &snapshot);
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
pub(crate) async fn codex_auth_logout(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    supervisor: State<'_, SharedCodexAuthSupervisor>,
) -> Result<Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    let logout_app = app.clone();
    let mutation: CodexMutationLease = run_blocking(move || {
        lifecycle.with_serialized(|| {
            let mutation = CodexAuthSupervisor::begin_mutation(&supervisor)?;
            prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())?;
            Ok(mutation)
        })
    })
    .await?;
    run_blocking(move || {
        let _mutation = mutation;
        run_codex_logout_sidecar(&logout_app)
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
) -> Result<Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    run_blocking(move || {
        lifecycle.with_serialized(|| {
            let _mutation = if enabled {
                None
            } else {
                Some(CodexAuthSupervisor::begin_mutation(&supervisor)?)
            };
            set_experimental_codex_enabled_at(&config::default_dir(), enabled, || {
                prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref()).map(|_| ())
            })
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
) -> Result<Value, String> {
    let resolved = csswitch_codex_network::resolve_from_process(&settings)
        .map_err(|_| "proxy_config_invalid：Codex 网络代理配置非法。".to_string())?;
    let mode = settings.mode;
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let supervisor = supervisor.inner().clone();
    run_blocking(move || {
        lifecycle.with_serialized(|| {
            let _mutation = CodexAuthSupervisor::begin_mutation(&supervisor)?;
            prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())?;
            config::update(&config::default_dir(), move |cfg| {
                cfg.codex_network = settings;
            })
            .map_err(|error| error.to_string())?;
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
    Ok(codex_downgrade_preview_for(&cfg))
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
            "keychain_unchanged": true,
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
            let actions = downgrade_actions_for_expected(&cfg, &expected_profile_ids)?;
            stop_all_before_downgrade(&app, &state, lifecycle.as_ref())?;
            Ok(match config::downgrade_to_v2_and_latch(&dir, &actions, Some(&destination)) {
                Ok(_) => DowngradeCommandOutcome::Committed(json!({
                    "schema_version": 1,
                    "status": "DOWNGRADED_EXIT_REQUIRED",
                    "profile_count": actions.len(),
                    "exported": true,
                    "keychain_unchanged": true,
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
            "{{\"schema_version\":2,\"ok\":true,\"command\":\"{command}\",\"status\":{{\"authenticated\":true,\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":7}}}}",
            "ab".repeat(16),
            "cd".repeat(16)
        )
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
    fn sidecar_runner_uses_exact_args_clean_env_and_returns_safe_success() {
        let temp = TempDir::new("success");
        let output = success_json("status");
        let script = temp.script(&format!(
            "[ \"$#\" -eq 2 ]\n[ \"$1\" = \"codex-auth\" ]\n[ \"$2\" = \"status\" ]\n[ \"$HOME\" = \"{}\" ]\n[ \"$CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE\" = \"{}\" ]\n[ -z \"${{OPENAI_API_KEY:-}}\" ]\nprintf '%s\\n' '{}'",
            temp.0.display(),
            EXPECTED_CODEX_KEYCHAIN_SERVICE,
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
            "printf '%s\\n' 'secret-stderr' >&2\nprintf '%s\\n' '{\"schema_version\":2,\"ok\":false,\"command\":\"logout\",\"error\":{\"code\":\"oauth_denied\",\"message\":\"attacker supplied secret\",\"retryable\":false}}'\nexit 4",
        );
        let value = run_codex_auth_sidecar_at(&script, &temp.0, CodexAuthAction::Logout).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "oauth_denied");
        assert!(!value.to_string().contains("attacker supplied secret"));
        assert!(!value.to_string().contains("secret-stderr"));
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
        let extra = success.replacen(
            "\"schema_version\":2",
            "\"schema_version\":2,\"token\":\"must-reject\"",
            1,
        );
        assert!(parse_sidecar_output(extra.as_bytes(), CodexAuthAction::Status, Some(0)).is_err());

        let denied = br#"{"schema_version":2,"ok":false,"command":"logout","error":{"code":"oauth_denied","message":"denied","retryable":false}}"#;
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
    fn login_sidecar_ndjson_replays_progress_and_one_terminal() {
        let temp = TempDir::new("login-ndjson");
        let operation_id = "ab".repeat(16);
        let script = temp.script(&format!(
            "[ \"$CSSWITCH_CODEX_AUTH_OPERATION_ID\" = \"{operation_id}\" ]\nprintf '%s\\n' '{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"progress\",\"state\":\"verification_required\",\"verification_url\":\"https://auth.openai.com/codex/device\",\"user_code\":\"ABCD-1234\",\"expires_at_ms\":2000000000000}}'\nprintf '%s\\n' '{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"progress\",\"state\":\"waiting\"}}'\nprintf '%s\\n' '{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"terminal\",\"state\":\"succeeded\",\"status\":{{\"authenticated\":true,\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":1}}}}'",
            "ab".repeat(16),
            "cd".repeat(16),
        ));
        let process = spawn_codex_auth_sidecar_at(
            &script,
            &temp.0,
            CodexAuthAction::LoginDevice,
            None,
            Some(&operation_id),
            false,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut states = Vec::new();
        let value = wait_for_login_sidecar(
            process,
            CodexAuthAction::LoginDevice,
            &operation_id,
            &cancel,
            |event| states.push(event.state.clone().unwrap()),
            |_| {},
        )
        .unwrap();
        assert_eq!(states, vec!["verification_required", "waiting"]);
        assert_eq!(value["ok"], true);
        assert_eq!(value["state"], "succeeded");
    }

    #[test]
    fn accepted_cancel_watchdog_reaps_only_after_sidecar_ack() {
        let temp = TempDir::new("login-cancel-watchdog");
        let operation_id = "ef".repeat(16);
        let script = temp.script(&format!(
            "IFS= read -r cancel\nprintf '%s\\n' '{{\"schema_version\":2,\"operation_id\":\"{operation_id}\",\"kind\":\"cancel_ack\",\"disposition\":\"accepted\"}}'\nexec /bin/sleep 5"
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
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert!(started.elapsed() < Duration::from_secs(3));
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
        assert!(require_authenticated_status(&authenticated).is_ok());
        let unauthenticated = json!({
            "schema_version": 1,
            "ok": true,
            "command": "status",
            "status": {
                "authenticated": false,
                "account_hash": null,
                "expiry_state": "missing",
                "expires_at": null,
                "auth_epoch": null,
                "auth_generation": 0
            }
        });
        let error = require_authenticated_status(&unauthenticated).unwrap_err();
        assert!(error.starts_with("CODEX_LOGIN_REQUIRED"));
    }

    #[test]
    fn diagnostic_summary_exposes_only_auth_and_expiry_state() {
        let authenticated = json!({
            "ok": true,
            "status": {
                "authenticated": true,
                "expiry_state": "expiring",
                "account_hash": "sensitive-account-hash",
                "auth_epoch": "sensitive-auth-epoch",
                "auth_generation": 99,
                "access_token": "must-not-escape",
                "email": "person@example.test"
            }
        });
        let summary = summarize_auth_for_diagnostics(&authenticated);
        assert_eq!(summary, "auth=authenticated expiry=expiring");
        for secret in [
            "account",
            "epoch",
            "generation",
            "access_token",
            "person@example.test",
        ] {
            assert!(!summary.contains(secret));
        }

        let error = json!({
            "ok": false,
            "error": {
                "code": "keychain_unavailable",
                "message": "untrusted /Users/name path and secret",
                "retryable": true
            }
        });
        assert_eq!(
            summarize_auth_for_diagnostics(&error),
            "auth=error code=keychain_unavailable"
        );
        assert_eq!(
            summarize_auth_for_diagnostics(&json!({"ok": false, "error": {"code": "invented"}})),
            "auth=protocol_error"
        );
    }

    #[test]
    fn downgrade_preview_and_confirmation_are_complete_and_secret_free() {
        let mut codex = config::Profile {
            id: "codex-1".into(),
            name: "My Codex".into(),
            template_id: "codex".into(),
            api_key: "must-never-appear".into(),
            credential_source: crate::provider_contracts::CredentialSource::KeychainOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            ..Default::default()
        };
        // A valid OAuth profile never has an API key. Keep the fake secret only
        // long enough to prove the preview projection cannot serialize it.
        let preview_cfg = config::Config {
            profiles: vec![codex.clone()],
            active_id: codex.id.clone(),
            ..Default::default()
        };
        let preview = codex_downgrade_preview_for(&preview_cfg);
        assert_eq!(preview["profile_count"], 1);
        assert_eq!(preview["active_will_clear"], true);
        assert_eq!(preview["keychain_unchanged"], true);
        let encoded = preview.to_string();
        assert!(!encoded.contains("must-never-appear"));
        assert!(!encoded.contains("credential_ref"));

        codex.api_key.clear();
        let cfg = config::Config {
            profiles: vec![codex],
            ..Default::default()
        };
        let actions = downgrade_actions_for_expected(&cfg, &["codex-1".into()]).unwrap();
        assert_eq!(
            actions.get("codex-1"),
            Some(&config::CodexDowngradeAction::ExportThenRemove)
        );
        assert!(downgrade_actions_for_expected(&cfg, &[]).is_err());
        assert!(
            downgrade_actions_for_expected(&cfg, &["codex-1".into(), "codex-1".into()]).is_err()
        );
        assert!(downgrade_actions_for_expected(&cfg, &["other".into()]).is_err());
    }
}
