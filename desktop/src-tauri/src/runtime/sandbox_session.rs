use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Manager, Runtime};

use crate::runtime::operation::{
    self, OperationKind, OperationStage, OperationTrace, POLL_INTERVAL_MS,
};
use crate::runtime::proxy::ProxyAction;
use crate::runtime::proxy_lifecycle::{
    current_skill_install_bridge_key, ensure_proxy, skill_install_bridge_dir,
};
use crate::runtime::science::{
    probe_known_runtime, probe_sandbox_runtime_cached, runtime_identity_is_current,
    sandbox_data_dir, sandbox_home, sandbox_listener_matches_runtime, sandbox_url,
    select_science_runtime_cached, stop_sandbox, SandboxScienceState, ScienceRuntimeIdentity,
    ScienceRuntimeSource,
};
use crate::runtime::skill_install_bridge::{
    configure_third_party_after_science_start, inspect_while_science_running,
    invalidate_route_configuration, mark_route_configuration_current,
    register_before_science_start, route_configuration_is_current, RegistrationStatus,
};
use crate::runtime::system::{asset_root, log_path, open_in_browser, open_log, redact, tail_file};
use crate::{
    config, lifecycle, lock, oauth_forge, proc, AppState, HistoryRecoveryChoice,
    HistoryRecoverySession, SharedAppState,
};

fn stop_sandbox_state<R: Runtime>(
    app: &tauri::AppHandle<R>,
    st: &mut AppState,
) -> Result<(), String> {
    let runtime = st.science_runtime.clone();
    let result = stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url, runtime.as_ref());
    if result.is_ok() {
        st.science_confirmed_stopped = runtime;
        st.science_runtime = None;
    }
    result
}

fn open_science_surface<R: Runtime>(
    app: &tauri::AppHandle<R>,
    url: &str,
) -> Result<&'static str, String> {
    if std::env::var("CSSWITCH_SCIENCE_WEBVIEW_SPIKE")
        .ok()
        .as_deref()
        == Some("1")
    {
        if let Some(win) = app.get_webview_window("science") {
            let _ = win.close();
        }
        let parsed = url
            .parse()
            .map_err(|e| format!("Science URL 解析失败：{e}"))?;
        match tauri::WebviewWindowBuilder::new(app, "science", tauri::WebviewUrl::External(parsed))
            .title("Claude Science")
            .inner_size(1100.0, 800.0)
            .build()
        {
            Ok(win) => {
                let _ = win.set_focus();
                return Ok("webview");
            }
            Err(_) => {
                // Spike-only path: construction failure falls through to the existing browser surface.
            }
        }
    }
    open_in_browser(url)?;
    Ok("browser")
}

fn installer_status_json(status: &RegistrationStatus) -> Value {
    match status {
        RegistrationStatus::Warning(message) => {
            json!({"status": status.code(), "message": message})
        }
        _ => json!({"status": status.code()}),
    }
}

fn append_installer_note(mut message: String, status: &RegistrationStatus) -> String {
    if let Some(note) = status.user_note() {
        message.push_str(&format!(" {note}"));
    }
    message
}

fn validate_running_system_ssh_bridge<R: Runtime>(
    app: &tauri::AppHandle<R>,
    sandbox_home: &Path,
) -> Result<(), String> {
    let expected_hosts = crate::runtime::ssh_bridge::validate_science_ssh_bridge(sandbox_home)?;
    crate::runtime::settings::validate_managed_sandbox_ssh_stub(sandbox_home, &expected_hosts)?;
    let root = asset_root(app).ok_or("找不到打包的 CSSwitch SSH bridge")?;
    let wrapper_dir = root.join("scripts/ssh-bridge");
    let wrapper = wrapper_dir.join("ssh");
    for path in [&root, &root.join("scripts"), &wrapper_dir, &wrapper] {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| "打包的 CSSwitch SSH bridge 缺失或无法检查")?;
        if metadata.file_type().is_symlink() {
            return Err("打包的 CSSwitch SSH bridge 路径包含符号链接".into());
        }
    }
    let metadata =
        std::fs::symlink_metadata(&wrapper).map_err(|_| "打包的 CSSwitch SSH bridge 缺失")?;
    if !metadata.file_type().is_file()
        || metadata.len() > 128 * 1024
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err("打包的 CSSwitch SSH bridge 不是安全的可执行文件".into());
    }
    Ok(())
}

fn verify_gateway_model_catalog(
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
    let ids: Vec<&str> = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .collect();
    if profile.model_policy == crate::provider_contracts::ModelPolicy::DynamicCatalog {
        if ids.is_empty()
            || ids
                .iter()
                .any(|id| !id.starts_with("claude-csswitch-codex-"))
        {
            return Err("Codex published model snapshot 为空或包含非法 alias".into());
        }
        return Ok(());
    }
    let expected: std::collections::BTreeSet<&str> = profile
        .model_catalog
        .iter()
        .map(|route| route.selector_id.as_str())
        .collect();
    let actual: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
    if actual != expected || ids.first().copied() != Some(profile.default_model_route_id.as_str()) {
        return Err("gateway 模型目录与已提交白名单/default selector 不一致".into());
    }
    Ok(())
}

fn gateway_model_catalog_timeout_ms(profile: &config::Profile) -> u64 {
    if profile.model_policy == crate::provider_contracts::ModelPolicy::DynamicCatalog {
        operation::CODEX_MODELS_PROBE_TIMEOUT_MS
    } else {
        operation::LOCAL_HEALTH_TIMEOUT_MS
    }
}

fn verify_gateway_model_catalog_traced(
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

fn configure_third_party_best_effort<R: Runtime>(
    app: &tauri::AppHandle<R>,
    status: RegistrationStatus,
    data_dir: &std::path::Path,
    port: u16,
    runtime: &ScienceRuntimeIdentity,
    force: bool,
) -> RegistrationStatus {
    if !matches!(
        status,
        RegistrationStatus::Registered | RegistrationStatus::AlreadyRegistered
    ) {
        let _ = invalidate_route_configuration(data_dir);
        return status;
    }
    let Some(science_version) = runtime.version.as_deref() else {
        let _ = invalidate_route_configuration(data_dir);
        return RegistrationStatus::Warning(
            "Science 版本无法确认，未记录第三方能力配置状态".into(),
        );
    };
    let needs_configuration = force
        || matches!(status, RegistrationStatus::Registered)
        || match route_configuration_is_current(data_dir, science_version) {
            Ok(current) => !current,
            Err(error) => return RegistrationStatus::Warning(error),
        };
    if !needs_configuration {
        return status;
    }
    if let Err(error) = invalidate_route_configuration(data_dir) {
        return RegistrationStatus::Warning(error);
    }
    let control_url = sandbox_url(port, runtime);
    if let Err(error) = configure_third_party_after_science_start(app, &control_url) {
        return RegistrationStatus::Warning(error);
    }
    match mark_route_configuration_current(data_dir, science_version) {
        Ok(()) => status,
        Err(error) => RegistrationStatus::Warning(error),
    }
}

/// Explicit doctor action: bypass the version cache and route marker without
/// starting Science or the proxy solely for diagnostics.
pub(crate) fn force_third_party_reconcile<R: Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
) -> Result<String, String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    let data_dir = sandbox_data_dir();
    let (remembered_runtime, version_cache) = {
        let st = lock(state);
        (st.science_runtime.clone(), st.science_version_cache.clone())
    };

    let (science_state, running_runtime) = match remembered_runtime {
        Some(mut runtime) => {
            if !runtime_identity_is_current(&runtime) {
                invalidate_route_configuration(&data_dir)?;
                return Ok(
                    "Science 二进制文件已变化；已安排下次停止并启动后重新选择 runtime。".into(),
                );
            }
            let previous_version = runtime.version.clone();
            let refreshed = version_cache
                .force_refresh(&runtime.path)
                .ok_or("Science 版本强制复检失败")?;
            if previous_version
                .as_deref()
                .is_some_and(|version| version != refreshed)
            {
                invalidate_route_configuration(&data_dir)?;
                return Ok(
                    "Science 二进制版本已变化；已安排下次停止并启动后重新配置 Skill 路由。".into(),
                );
            }
            runtime.version = Some(refreshed);
            let science_state = probe_known_runtime(cfg.sandbox_port, &runtime);
            let running = (science_state == SandboxScienceState::RunningHealthy).then_some(runtime);
            (science_state, running)
        }
        None => {
            version_cache.clear();
            probe_sandbox_runtime_cached(cfg.sandbox_port, &version_cache)?
        }
    };

    if cfg.mode == "official" {
        return Ok("官方模式无需核验 CSSwitch 第三方 Skill 路由。".into());
    }
    match science_state {
        SandboxScienceState::Stopped => {
            invalidate_route_configuration(&data_dir)?;
            Ok("Science 未运行；已安排下次一键开始重新核验 Skill 路由。".into())
        }
        SandboxScienceState::Unknown => {
            invalidate_route_configuration(&data_dir)?;
            Err("无法确认 Science 实例身份；已使路由标记失效，未执行修复".into())
        }
        SandboxScienceState::RunningHealthy => {
            let runtime = running_runtime.ok_or("Science 运行身份缺失")?;
            let secret = { lock(state).secret.clone() };
            if secret.is_empty() {
                invalidate_route_configuration(&data_dir)?;
                return Ok("当前代理身份不可用；已安排下次一键开始重新核验 Skill 路由。".into());
            }
            let bridge_dir = skill_install_bridge_dir(&secret)?;
            let bridge_key = match current_skill_install_bridge_key() {
                Ok(path) => path,
                Err(error) => {
                    invalidate_route_configuration(&data_dir)?;
                    return Ok(format!(
                        "Skill bridge 尚未就绪；已安排下次一键开始重新核验：{error}"
                    ));
                }
            };
            let status = inspect_while_science_running(app, &data_dir, &bridge_dir, &bridge_key);
            let status = configure_third_party_best_effort(
                app,
                status,
                &data_dir,
                cfg.sandbox_port,
                &runtime,
                true,
            );
            {
                let mut st = lock(state);
                st.science_runtime = Some(runtime);
                st.science_confirmed_stopped = None;
            }
            match status {
                RegistrationStatus::AlreadyRegistered | RegistrationStatus::Registered => {
                    Ok("Skill 路由已强制核验并同步。".into())
                }
                RegistrationStatus::RestartRequired => {
                    Ok("Skill 路由文件需要重启 Science 后加载；状态标记已失效。".into())
                }
                RegistrationStatus::Warning(message) => {
                    Ok(format!("Skill 路由核验未完成：{message}"))
                }
            }
        }
    }
}

/// One-click session startup: active proxy, virtual login, sandbox, browser.
///
/// Callers must hold the command serializer lock.
pub(crate) fn one_click_login<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    runtime_choice: Option<&str>,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> Result<Value, String> {
    one_click_login_with_options(app, state, lifecycle, runtime_choice, auth_proof, true)
}

pub(crate) fn reconcile_science_for_active<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> Result<Value, String> {
    one_click_login_with_options(app, state, lifecycle, None, auth_proof, false)
}

/// Rollback-only recovery path. The persisted config is already the old,
/// authoritative profile. Do not trust its previous runtime binding to decide
/// reuse: a healthy process may actually have loaded the failed candidate
/// catalog. Stop only the exact in-memory Science identity and start the
/// committed chain again from a clean process.
pub(crate) fn force_restart_science_for_active<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> Result<Value, String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    let remembered = { lock(&state).science_runtime.clone() };
    match remembered {
        Some(runtime) => match probe_known_runtime(cfg.sandbox_port, &runtime) {
            SandboxScienceState::RunningHealthy => {
                let mut st = lock(&state);
                st.science_runtime = Some(runtime);
                stop_sandbox_state(&app, &mut st).map_err(|error| {
                    format!("回滚时停止候选 Science 失败，未猜测 PID 或按端口结束进程：{error}")
                })?;
            }
            SandboxScienceState::Stopped => {
                let mut st = lock(&state);
                st.science_confirmed_stopped = Some(runtime);
                st.science_runtime = None;
            }
            SandboxScienceState::Unknown => {
                return Err(
                    "回滚时 Science 可能正在运行，但身份无法确认；已拒绝猜测 PID 或按端口结束进程。"
                        .into(),
                );
            }
        },
        None if proc::loopback_port_in_use(
            cfg.sandbox_port,
            operation::LOCAL_HEALTH_TIMEOUT_MS,
        ) =>
        {
            return Err(
                "回滚时 Science 端口仍被占用，但没有可确认的 runtime 身份；已拒绝强制结束。".into(),
            );
        }
        None => {}
    }
    one_click_login_with_options(app, state, lifecycle, None, auth_proof, false)
}

fn advance_runtime_transaction(
    dir: &Path,
    active_profile_id: &str,
    previous_binding: Option<config::RuntimeBindingCommit>,
    stage: &str,
) -> Result<(), String> {
    config::update(dir, |current| match current.runtime_transaction.as_mut() {
        Some(journal) if journal.target_profile_id == active_profile_id => {
            journal.stage = stage.to_string();
        }
        _ => {
            current.runtime_transaction = Some(config::RuntimeTransactionJournal {
                transaction_id: config::new_id(),
                target_profile_id: active_profile_id.to_string(),
                stage: stage.to_string(),
                previous_binding: previous_binding.clone(),
                previous_gateway: None,
            });
        }
    })
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn one_click_login_with_options<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    runtime_choice: Option<&str>,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
    open_surface: bool,
) -> Result<Value, String> {
    let trace = OperationTrace::start(OperationKind::OneClickLogin, "command=one_click_login");
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let active_profile = cfg
        .active_profile()
        .ok_or("未配置生效 profile，请先在面板选择或新建一条配置。")?;
    config::require_template_enabled(&cfg, &active_profile.template_id)?;
    let active_launch = crate::runtime::provider::resolve_launch_plan(active_profile)?;
    crate::commands::codex::require_provider_auth_proof(&active_launch.adapter, auth_proof)?;
    crate::runtime::settings::validate_runtime_ports(cfg.proxy_port, cfg.sandbox_port)?;
    let sport = cfg.sandbox_port;

    let sbx_home = sandbox_home();
    let auth_dir = sbx_home.join(".claude-science");
    let version_cache = { lock(&state).science_version_cache.clone() };

    let (remembered_runtime, confirmed_stopped) = {
        let mut st = lock(&state);
        (
            st.science_runtime.clone(),
            st.science_confirmed_stopped.take(),
        )
    };
    let (science_state, running_runtime) = match remembered_runtime {
        Some(runtime) => {
            let science_state = probe_known_runtime(sport, &runtime);
            let running_runtime =
                (science_state == SandboxScienceState::RunningHealthy).then_some(runtime);
            (science_state, running_runtime)
        }
        None if confirmed_stopped
            .as_ref()
            .is_some_and(|runtime| runtime.source != ScienceRuntimeSource::CachedOnce)
            && !proc::loopback_port_in_use(sport, 100) =>
        {
            (SandboxScienceState::Stopped, None)
        }
        None => probe_sandbox_runtime_cached(sport, &version_cache)?,
    };
    let launch_runtime: ScienceRuntimeIdentity = match science_state {
        SandboxScienceState::RunningHealthy => {
            let running_runtime =
                running_runtime.ok_or("Science 状态为运行中，但无法确认其 binary 身份")?;
            let desired_binding = crate::runtime::provider::desired_runtime_binding(
                &cfg,
                active_profile,
                &running_runtime,
            )?;
            let science_binding_matches = !crate::runtime::provider::science_restart_required(
                cfg.runtime_binding.as_ref(),
                &desired_binding,
            );
            let login_intact =
                oauth_forge::login_intact(&auth_dir, "virtual@localhost.invalid", &sbx_home);
            if login_intact && science_binding_matches {
                if cfg.reuse_system_ssh {
                    validate_running_system_ssh_bridge(&app, &sbx_home)?;
                }
                oauth_forge::bootstrap_marker_for_intact_login(
                    &auth_dir,
                    "virtual@localhost.invalid",
                    &sbx_home,
                )
                .map_err(|error| format!("补齐历史恢复标记失败：{error}"))?;
                let (_pport, secret, proxy_action) = ensure_proxy(
                    &app,
                    &state,
                    lifecycle,
                    Some(&running_runtime),
                    Some(&trace),
                    auth_proof,
                )?;
                verify_gateway_model_catalog_traced(
                    &trace,
                    cfg.proxy_port,
                    &secret,
                    active_profile,
                )?;
                let installer_bridge = skill_install_bridge_dir(&secret)?;
                // Science 已在运行时只读检查，不并发改写它的 MCP 配置。
                let installer = match current_skill_install_bridge_key() {
                    Ok(installer_key) => inspect_while_science_running(
                        &app,
                        &auth_dir,
                        &installer_bridge,
                        &installer_key,
                    ),
                    Err(error) => RegistrationStatus::Warning(error),
                };
                let installer = configure_third_party_best_effort(
                    &app,
                    installer,
                    &auth_dir,
                    sport,
                    &running_runtime,
                    false,
                );
                let url = sandbox_url(sport, &running_runtime);
                {
                    let mut st = lock(&state);
                    st.sandbox_port = sport;
                    st.sandbox_url = Some(url.clone());
                    st.science_runtime = Some(running_runtime.clone());
                    st.science_confirmed_stopped = None;
                }
                let refreshed_cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
                let committed = crate::runtime::provider::desired_runtime_binding(
                    &refreshed_cfg,
                    refreshed_cfg
                        .active_profile()
                        .ok_or("生效 profile 在启动期间消失")?,
                    &running_runtime,
                )?;
                config::update(&dir, |cfg| {
                    cfg.runtime_binding = Some(committed.clone());
                    cfg.runtime_transaction = None;
                })
                .map_err(|error| error.to_string())?;
                let base = match proxy_action {
                    ProxyAction::Reused => "已在运行",
                    ProxyAction::Restarted => "已用新配置重启代理，Science 沿用不变",
                };
                let (msg, fallback_url) = if open_surface {
                    match open_science_surface(&app, &url) {
                        Ok("webview") => (format!("{base}，已重新打开 Science 窗口。"), None),
                        Ok(_) => (format!("{base}，已向系统浏览器发送打开请求。"), None),
                        Err(_) => (
                            format!("{base}，服务已就绪；自动打开失败。"),
                            Some(url.clone()),
                        ),
                    }
                } else {
                    (format!("{base}，Science 绑定保持不变。"), None)
                };
                let msg = append_installer_note(msg, &installer);
                trace.finish(format!(
                    "ok action=reopened proxy_action={}",
                    proxy_action.as_str()
                ));
                return Ok(json!({
                    "msg": msg,
                    "action": "reopened",
                    "stage": "complete",
                    "status": "ok",
                    "recovery_status": "not_needed",
                    "fallback_url": fallback_url,
                    "external_skill_installer": installer_status_json(&installer)
                }));
            }
            config::update(&dir, |current| {
                current.runtime_transaction = Some(config::RuntimeTransactionJournal {
                    transaction_id: config::new_id(),
                    target_profile_id: active_profile.id.clone(),
                    stage: "stop_old_science".into(),
                    previous_binding: cfg.runtime_binding.clone(),
                    previous_gateway: None,
                });
            })
            .map_err(|error| error.to_string())?;
            {
                let mut st = lock(&state);
                st.science_runtime = Some(running_runtime.clone());
                if let Err(error) = stop_sandbox_state(&app, &mut st) {
                    trace.finish("error=sandbox_stop_for_login_refresh");
                    return Err(format!(
                        "隔离 Science 需要刷新登录或模型目录，但停止旧进程失败：{error}"
                    ));
                }
            }
            if login_intact {
                running_runtime
            } else {
                select_science_runtime_cached(runtime_choice, &version_cache)?
            }
        }
        SandboxScienceState::Stopped => {
            select_science_runtime_cached(runtime_choice, &version_cache)?
        }
        SandboxScienceState::Unknown => {
            trace.finish("error=sandbox_state_unknown_before_start");
            return Err(format!(
                "无法确认隔离 Science 状态（端口 {sport} 或 data-dir 状态不一致）。请先停止占用该端口的进程后重试。"
            ));
        }
    };

    let transaction_cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
    advance_runtime_transaction(
        &dir,
        &active_profile.id,
        transaction_cfg.runtime_binding.clone(),
        "start_gateway",
    )?;

    let preview_port = sport
        .checked_add(1)
        .ok_or("沙箱端口必须小于 65535，才能分配隔离预览端口。")?;
    if proc::loopback_port_in_use(preview_port, operation::LOCAL_HEALTH_TIMEOUT_MS) {
        trace.finish("error=science_preview_port_in_use");
        return Err(format!(
            "隔离 Science 预览端口 {preview_port} 已被占用；未启动或结束任何占用者。请修改沙箱端口后重试。"
        ));
    }
    lock(&state).science_confirmed_stopped = None;

    trace.stage(OperationStage::SandboxLogin, "ensure_virtual_login");
    let (forged, login_action) = match oauth_forge::ensure_virtual_login(
        &auth_dir,
        "virtual@localhost.invalid",
        &sbx_home,
    ) {
        Ok(result) => result,
        Err(oauth_forge::EnsureVirtualLoginError::HistoryChoiceRequired(candidates)) => {
            if candidates.len() > 64 {
                return Err("历史记录候选超过安全上限（64），已拒绝生成恢复会话".into());
            }
            let choices: Vec<HistoryRecoveryChoice> = candidates
                .into_iter()
                .map(|candidate| HistoryRecoveryChoice {
                    reference: config::new_id(),
                    candidate,
                })
                .collect();
            let visible_choices: Vec<Value> = choices
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    let label = if index < 26 {
                        format!("历史记录 {}", (b'A' + index as u8) as char)
                    } else {
                        format!("历史记录 {}", index + 1)
                    };
                    json!({
                        "reference": choice.reference,
                        "label": label
                    })
                })
                .collect();
            {
                let mut app_state = lock(&state);
                app_state.science_confirmed_stopped = Some(launch_runtime.clone());
                app_state.history_recovery = Some(HistoryRecoverySession {
                    active_profile_id: active_profile.id.clone(),
                    sandbox_port: sport,
                    auth_dir: auth_dir.clone(),
                    sandbox_root: sbx_home.clone(),
                    choices,
                });
            }
            config::update(&dir, |current| current.runtime_transaction = None)
                .map_err(|error| error.to_string())?;
            trace.finish("attention=history_choice_required");
            return Ok(json!({
                "msg": "检测到多份旧历史记录。请选择要恢复的一份；CSSwitch 不会删除其他记录。",
                "action": "history_choice_required",
                "stage": "history_recovery",
                "status": "attention",
                "recovery_status": "choice_required",
                "choices": visible_choices,
                "fallback_url": null
            }));
        }
        Err(oauth_forge::EnsureVirtualLoginError::Message(message)) => {
            return Err(format!("写虚拟登录失败：{message}"));
        }
    };
    // Keep the full identity available for internal validation without writing
    // UUIDs or filesystem paths to the sandbox log or frontend error state.
    let _validated_login_identity = (
        &forged.auth_dir,
        &forged.account_uuid,
        &forged.org_uuid,
        &forged.enc_file,
    );

    let root = asset_root(&app)
        .ok_or("找不到 scripts/launch-virtual-sandbox.sh（打包资源或仓库根均未命中）。")?;

    let launch = root.join("scripts/launch-virtual-sandbox.sh");
    if !launch.is_file() {
        return Err("找不到 scripts/launch-virtual-sandbox.sh。".into());
    }

    let ssh_hosts = if cfg.reuse_system_ssh {
        crate::runtime::ssh_bridge::prepare_science_ssh_bridge(&sbx_home)?
    } else {
        crate::runtime::ssh_bridge::revoke_science_ssh_bridge(&sbx_home)?;
        Vec::new()
    };

    let (pport, secret, proxy_action) = ensure_proxy(
        &app,
        &state,
        lifecycle,
        Some(&launch_runtime),
        Some(&trace),
        auth_proof,
    )?;
    verify_gateway_model_catalog_traced(&trace, pport, &secret, active_profile)?;
    advance_runtime_transaction(
        &dir,
        &active_profile.id,
        transaction_cfg.runtime_binding.clone(),
        "start_science",
    )?;
    let installer_bridge = skill_install_bridge_dir(&secret)?;
    // 本地 MCP 注册是 best-effort：失败只降级该工具，绝不阻断 Science 启动。
    let installer = match current_skill_install_bridge_key() {
        Ok(installer_key) => {
            register_before_science_start(&app, &auth_dir, &installer_bridge, &installer_key)
        }
        Err(error) => RegistrationStatus::Warning(error),
    };

    let proxy_url = format!("http://127.0.0.1:{pport}/{secret}");
    let logf = open_log("sandbox.log").map_err(|e| format!("建日志失败：{e}"))?;
    {
        use std::io::Write;
        let mut lw = &logf;
        let _ = writeln!(
            lw,
            "[oauth] 虚拟登录已就绪（Rust，零 node；action={:?}；isolated=true）",
            login_action
        );
    }
    let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
    trace.stage(OperationStage::SandboxLaunch, format!("port={sport}"));
    if !runtime_identity_is_current(&launch_runtime) {
        trace.finish("error=science_runtime_changed_before_launch");
        return Err("Science runtime 在预检后发生变化；已拒绝启动，请重试".into());
    }
    let status = Command::new("zsh")
        .arg(&launch)
        .arg("--port")
        .arg(sport.to_string())
        .arg("--skip-oauth-forge")
        .env("SANDBOX_HOME", sandbox_home())
        .env("SCIENCE_BIN", &launch_runtime.path)
        .env("CSSWITCH_RUNTIME_VERSION_PRECHECKED", "1")
        .env("CSSWITCH_PROXY_URL", &proxy_url)
        .env(
            "CSSWITCH_REUSE_SYSTEM_SSH",
            if cfg.reuse_system_ssh { "1" } else { "0" },
        )
        .env("CSSWITCH_SYSTEM_SSH_HOSTS", ssh_hosts.join(" "))
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2))
        .status()
        .map_err(|e| format!("起沙箱失败：{e}"))?;
    if !status.success() {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        let mut no_child = None;
        let mut no_url = None;
        let cleanup = stop_sandbox(&app, &mut no_child, &mut no_url, Some(&launch_runtime));
        trace.finish("error=sandbox_launch_failed");
        let cleanup_note = if cleanup.is_ok() {
            "已按本次 runtime/data-dir 尝试清理部分启动。"
        } else {
            "部分启动清理未能确认完成；请使用“全部停止”重试。"
        };
        return Err(format!("起沙箱脚本失败。{cleanup_note}\n{tail}"));
    }

    // From this point onward stop/status/url must use the exact binary selected
    // for this launch. Keep this identity in memory before health polling so a
    // failed launch can still be stopped without guessing from the port.
    {
        let mut st = lock(&state);
        st.sandbox_port = sport;
        st.science_runtime = Some(launch_runtime.clone());
        st.science_confirmed_stopped = None;
    }

    let mut ok = false;
    for _ in 0..(operation::SANDBOX_HEALTH_BUDGET_MS / POLL_INTERVAL_MS) {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        if proc::http_health(sport, None, operation::LOCAL_HEALTH_TIMEOUT_MS) {
            ok = true;
            break;
        }
    }
    trace.stage(
        OperationStage::SandboxHealth,
        if ok { "ready" } else { "not_ready" },
    );
    if !ok {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_state(&app, &mut st);
        }
        trace.finish("error=sandbox_health_timeout");
        return Err(format!(
            "沙箱起后探活超时（端口 {sport}）。已尝试停掉刚起的沙箱。\n{tail}"
        ));
    }

    if !sandbox_listener_matches_runtime(sport, &launch_runtime) {
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_state(&app, &mut st);
        }
        trace.finish("error=sandbox_identity_mismatch");
        return Err(format!(
            "端口 {sport} 有服务响应，但按 data-dir 确认不是本沙箱 Science（疑似被其它服务占用）。已尝试停掉刚起的沙箱。"
        ));
    }
    advance_runtime_transaction(
        &dir,
        &active_profile.id,
        transaction_cfg.runtime_binding.clone(),
        "verify_science_catalog",
    )?;

    // Third-party policy setup is best-effort. A dedicated control URL is only
    // consumed when the persisted route state says reconciliation is required.
    let installer = configure_third_party_best_effort(
        &app,
        installer,
        &auth_dir,
        sport,
        &launch_runtime,
        false,
    );
    let url = sandbox_url(sport, &launch_runtime);
    {
        let mut st = lock(&state);
        st.sandbox_port = sport;
        st.sandbox_url = Some(url.clone());
        st.science_runtime = Some(launch_runtime.clone());
        st.science_confirmed_stopped = None;
    }
    let started = match login_action {
        oauth_forge::LoginAction::Created => "已启动",
        _ => "沙箱已重新启动，沿用原有对话",
    };
    let refreshed_cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
    let committed = crate::runtime::provider::desired_runtime_binding(
        &refreshed_cfg,
        refreshed_cfg
            .active_profile()
            .ok_or("生效 profile 在启动期间消失")?,
        &launch_runtime,
    )?;
    config::update(&dir, |cfg| {
        cfg.runtime_binding = Some(committed.clone());
        cfg.runtime_transaction = None;
    })
    .map_err(|error| error.to_string())?;
    let (msg, fallback_url) = if open_surface {
        match open_science_surface(&app, &url) {
            Ok("webview") => (format!("{started}，已打开 Science 窗口。"), None),
            Ok(_) => (format!("{started}，已向系统浏览器发送打开请求。"), None),
            Err(_) => (
                format!("{started}，服务已就绪；自动打开失败。"),
                Some(url.clone()),
            ),
        }
    } else {
        (format!("{started}，Science 已按新模型目录刷新。"), None)
    };
    let msg = append_installer_note(msg, &installer);
    trace.stage(OperationStage::OpenBrowser, "done");
    trace.finish(format!(
        "ok action=started proxy_action={}",
        proxy_action.as_str()
    ));
    Ok(json!({
        "msg": msg,
        "action": "started",
        "stage": "complete",
        "status": "ok",
        "recovery_status": "not_needed",
        "fallback_url": fallback_url,
        "external_skill_installer": installer_status_json(&installer)
    }))
}

#[cfg(test)]
mod transaction_tests {
    use super::{
        advance_runtime_transaction, gateway_model_catalog_timeout_ms, verify_gateway_model_catalog,
    };
    use crate::config::{self, Config, RuntimeBindingCommit};
    use crate::provider_contracts::ModelPolicy;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn profile_with_policy(policy: ModelPolicy) -> config::Profile {
        config::Profile {
            model_policy: policy,
            ..Default::default()
        }
    }

    fn serve_models_after(
        delay: Duration,
        body: &'static str,
    ) -> (u16, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8765);
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /test-secret/v1/models HTTP/1.0\r\n"));
            server_requests.fetch_add(1, Ordering::SeqCst);
            thread::sleep(delay);
            write!(
                stream,
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        (port, requests, server)
    }

    #[test]
    fn gateway_catalog_timeout_matches_model_policy_contract() {
        assert_eq!(
            gateway_model_catalog_timeout_ms(&profile_with_policy(ModelPolicy::DynamicCatalog)),
            crate::runtime::operation::CODEX_MODELS_PROBE_TIMEOUT_MS
        );
        assert_eq!(
            gateway_model_catalog_timeout_ms(&profile_with_policy(ModelPolicy::SavedCatalog)),
            crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS
        );
    }

    #[test]
    fn dynamic_catalog_cold_response_uses_one_long_local_request() {
        let body = r#"{"data":[{"id":"claude-csswitch-codex-gpt-5"}]}"#;
        let (port, requests, server) = serve_models_after(Duration::from_millis(600), body);
        let profile = profile_with_policy(ModelPolicy::DynamicCatalog);

        verify_gateway_model_catalog(port, "test-secret", &profile).unwrap();
        server.join().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dynamic_catalog_still_rejects_empty_or_non_codex_aliases() {
        for body in [r#"{"data":[]}"#, r#"{"data":[{"id":"gpt-5"}]}"#] {
            let (port, _requests, server) = serve_models_after(Duration::ZERO, body);
            let profile = profile_with_policy(ModelPolicy::DynamicCatalog);
            let error = verify_gateway_model_catalog(port, "test-secret", &profile).unwrap_err();
            assert!(error.contains("Codex published model snapshot"));
            server.join().unwrap();
        }
    }

    #[test]
    fn runtime_journal_advances_in_place_and_retargets_without_secrets() {
        let dir = std::env::temp_dir().join(format!(
            "csswitch-runtime-journal-{}-{}",
            std::process::id(),
            config::new_id()
        ));
        let previous = RuntimeBindingCommit {
            profile_id: "old".into(),
            route_fp: "route-fp".into(),
            catalog_fp: "catalog-fp".into(),
            binding_fp: "binding-fp".into(),
        };
        config::save_to(
            &dir,
            &Config {
                runtime_binding: Some(previous.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        advance_runtime_transaction(&dir, "new", Some(previous.clone()), "start_gateway").unwrap();
        let first = config::load_from(&dir)
            .unwrap()
            .runtime_transaction
            .unwrap();
        assert_eq!(first.target_profile_id, "new");
        assert_eq!(first.stage, "start_gateway");
        assert_eq!(first.previous_binding, Some(previous.clone()));

        advance_runtime_transaction(&dir, "new", Some(previous.clone()), "start_science").unwrap();
        let second = config::load_from(&dir)
            .unwrap()
            .runtime_transaction
            .unwrap();
        assert_eq!(second.transaction_id, first.transaction_id);
        assert_eq!(second.stage, "start_science");

        advance_runtime_transaction(&dir, "newer", Some(previous), "start_gateway").unwrap();
        let retargeted = config::load_from(&dir)
            .unwrap()
            .runtime_transaction
            .unwrap();
        assert_ne!(retargeted.transaction_id, second.transaction_id);
        assert_eq!(retargeted.target_profile_id, "newer");
        let encoded = serde_json::to_string(&retargeted).unwrap();
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("base_url"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
