use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::proc::ChildLiveness;
use crate::runtime::proxy_lifecycle::gateway_bin_path;
use crate::runtime::science::{
    probe_known_runtime, probe_sandbox_runtime_cached, SandboxScienceState,
};
use crate::runtime::system::kill_child;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

const AUTH_SCHEMA_VERSION: u32 = 1;
const MAX_AUTH_OUTPUT_BYTES: u64 = 64 * 1024;
const AUTH_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(feature = "acceptance-keychain"))]
const EXPECTED_CODEX_KEYCHAIN_SERVICE: &str = "com.csswitch.codex.oauth.v1";
#[cfg(feature = "acceptance-keychain")]
const EXPECTED_CODEX_KEYCHAIN_SERVICE: &str = "com.csswitch.acceptance.codex.oauth.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexAuthAction {
    Login,
    Status,
    Logout,
}

impl CodexAuthAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Status => "status",
            Self::Logout => "logout",
        }
    }

    fn timeout(self) -> Duration {
        match self {
            // Gateway's browser callback budget is five minutes. The outer
            // supervisor allows a small cleanup margin but never waits forever.
            Self::Login => Duration::from_secs(5 * 60 + 15),
            Self::Status => Duration::from_secs(15),
            Self::Logout => Duration::from_secs(60),
        }
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarErrorView {
    code: String,
    message: String,
    retryable: bool,
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
        "oauth_network_error" | "oauth_protocol_error" => Some(7),
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
        "auth_storage_error" => "CSSwitch 无法安全保存 Codex 认证状态。",
        "unsupported_platform" => "当前平台不支持 CSSwitch Codex 钥匙串认证。",
        _ => "Codex 认证 sidecar 发生内部错误。",
    }
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
            serde_json::to_value(success.status)
                .map(|status| {
                    json!({
                        "schema_version": AUTH_SCHEMA_VERSION,
                        "ok": true,
                        "command": action.as_str(),
                        "status": status,
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

fn run_codex_auth_sidecar_at(
    binary: &Path,
    home: &Path,
    action: CodexAuthAction,
) -> Result<Value, String> {
    run_codex_auth_sidecar_at_with_timeout(binary, home, action, action.timeout())
}

fn run_codex_auth_sidecar_at_with_timeout(
    binary: &Path,
    home: &Path,
    action: CodexAuthAction,
    timeout: Duration,
) -> Result<Value, String> {
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
    let mut child = Command::new(binary)
        .arg("codex-auth")
        .arg(action.as_str())
        .env_clear()
        .env("HOME", home)
        .env(
            "CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE",
            EXPECTED_CODEX_KEYCHAIN_SERVICE,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法启动 Codex 认证 sidecar。".to_string())?;
    let Some(mut stdout) = child.stdout.take() else {
        stop_auth_child(&mut child);
        return Err("无法读取 Codex 认证 sidecar 输出。".into());
    };
    if let Err(error) = set_nonblocking_stdout(&stdout) {
        stop_auth_child(&mut child);
        return Err(error);
    }

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
                        stop_auth_child(&mut child);
                        return Err("Codex 认证 sidecar 输出读取失败或超过 64 KiB。".into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    stop_auth_child(&mut child);
                    return Err("Codex 认证 sidecar 输出读取失败或超过 64 KiB。".into());
                }
            }
        }
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(_) => {
                    stop_auth_child(&mut child);
                    return Err("无法确认 Codex 认证 sidecar 退出状态。".into());
                }
            }
        }
        if exit_status.is_some() && output_eof {
            break;
        }
        if Instant::now() >= deadline {
            stop_auth_child(&mut child);
            return Err("Codex 认证 sidecar 超时，受管进程已结束。".into());
        }
        std::thread::sleep(AUTH_POLL_INTERVAL);
    }
    parse_sidecar_output(&bytes, action, exit_status.and_then(|status| status.code()))
}

fn run_codex_auth_sidecar<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    action: CodexAuthAction,
) -> Result<Value, String> {
    let binary = gateway_bin_path(app).ok_or("找不到受管 csswitch-gateway sidecar。")?;
    run_codex_auth_sidecar_at(&binary, &production_home()?, action)
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
) -> Result<(), String> {
    if adapter != "codex" {
        return Ok(());
    }
    let value = run_codex_auth_sidecar(app, CodexAuthAction::Status)
        .map_err(|error| format!("CODEX_AUTH_UNAVAILABLE：{error}"))?;
    require_authenticated_status(&value)
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
pub(crate) async fn codex_auth_login(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        lifecycle.with_serialized(|| {
            let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
            config::require_template_enabled(&cfg, "codex")?;
            prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())?;
            run_codex_auth_sidecar(&app, CodexAuthAction::Login)
        })
    })
    .await
}

#[tauri::command]
pub(crate) async fn codex_auth_logout(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        lifecycle.with_serialized(|| {
            prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref())?;
            run_codex_auth_sidecar(&app, CodexAuthAction::Logout)
        })
    })
    .await
}

#[tauri::command]
pub(crate) async fn set_experimental_codex_enabled(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    enabled: bool,
) -> Result<Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        lifecycle.with_serialized(|| {
            set_experimental_codex_enabled_at(&config::default_dir(), enabled, || {
                prepare_codex_auth_mutation(&app, &state, lifecycle.as_ref()).map(|_| ())
            })
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
            "{{\"schema_version\":1,\"ok\":true,\"command\":\"{command}\",\"status\":{{\"authenticated\":true,\"account_hash\":\"{}\",\"expiry_state\":\"valid\",\"expires_at\":2000000000,\"auth_epoch\":\"{}\",\"auth_generation\":7}}}}",
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
            "printf '%s\\n' 'secret-stderr' >&2\nprintf '%s\\n' '{\"schema_version\":1,\"ok\":false,\"command\":\"login\",\"error\":{\"code\":\"oauth_denied\",\"message\":\"attacker supplied secret\",\"retryable\":false}}'\nexit 4",
        );
        let value = run_codex_auth_sidecar_at(&script, &temp.0, CodexAuthAction::Login).unwrap();
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
            success_json("login").as_bytes(),
            CodexAuthAction::Status,
            Some(0)
        )
        .is_err());
        let extra = success.replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"token\":\"must-reject\"",
            1,
        );
        assert!(parse_sidecar_output(extra.as_bytes(), CodexAuthAction::Status, Some(0)).is_err());

        let denied = br#"{"schema_version":1,"ok":false,"command":"login","error":{"code":"oauth_denied","message":"denied","retryable":false}}"#;
        assert!(parse_sidecar_output(denied, CodexAuthAction::Login, Some(7)).is_err());
        assert!(parse_sidecar_output(denied, CodexAuthAction::Login, None).is_err());
        assert!(parse_sidecar_output(denied, CodexAuthAction::Login, Some(4)).is_ok());

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
