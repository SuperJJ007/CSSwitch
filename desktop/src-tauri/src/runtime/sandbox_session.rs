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
    probe_known_runtime, probe_sandbox_runtime_cached, sandbox_data_dir, sandbox_home,
    sandbox_listener_matches_runtime, sandbox_url, select_science_runtime_cached, stop_sandbox,
    SandboxScienceState, ScienceRuntimeIdentity, ScienceRuntimeSource,
};
use crate::runtime::skill_install_bridge::{
    configure_third_party_after_science_start, inspect_while_science_running,
    invalidate_route_configuration, mark_route_configuration_current,
    register_before_science_start, route_configuration_is_current, RegistrationStatus,
};
use crate::runtime::system::{asset_root, log_path, open_in_browser, open_log, redact, tail_file};
use crate::{config, lifecycle, lock, oauth_forge, proc, AppState, SharedAppState};

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
) -> Result<Value, String> {
    let trace = OperationTrace::start(OperationKind::OneClickLogin, "command=one_click_login");
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let active_profile = cfg
        .active_profile()
        .ok_or("未配置生效 profile，请先在面板选择或新建一条配置。")?;
    config::require_template_enabled(&cfg, &active_profile.template_id)?;
    let active_launch = crate::runtime::provider::resolve_launch_plan(active_profile)?;
    crate::commands::codex::ensure_provider_auth_ready(&app, &active_launch.adapter)?;
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
            if oauth_forge::login_intact(&auth_dir, "virtual@localhost.invalid", &sbx_home) {
                let (_pport, secret, proxy_action) = ensure_proxy(
                    &app,
                    &state,
                    lifecycle,
                    Some(&running_runtime),
                    Some(&trace),
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
                let base = match proxy_action {
                    ProxyAction::Reused => "已在运行",
                    ProxyAction::Restarted => "已用新配置重启代理，Science 沿用不变",
                };
                let msg = match open_science_surface(&app, &url) {
                    Ok("webview") => format!("{base}，已重新打开 Science 窗口。"),
                    Ok(_) => format!("{base}，已重新打开 Science。"),
                    Err(_) => format!("{base}，服务已就绪；自动打开失败，请点击“打开 Science”。"),
                };
                let msg = append_installer_note(msg, &installer);
                trace.finish(format!(
                    "ok action=reopened proxy_action={}",
                    proxy_action.as_str()
                ));
                return Ok(json!({
                    "msg": msg,
                    "action": "reopened",
                    "external_skill_installer": installer_status_json(&installer)
                }));
            }
            {
                let mut st = lock(&state);
                st.science_runtime = Some(running_runtime);
                if let Err(error) = stop_sandbox_state(&app, &mut st) {
                    trace.finish("error=sandbox_stop_for_login_refresh");
                    return Err(format!(
                        "隔离 Science 登录状态失效，但停止旧进程失败：{error}"
                    ));
                }
            }
            select_science_runtime_cached(runtime_choice, &version_cache)?
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
    let (forged, login_action) =
        oauth_forge::ensure_virtual_login(&auth_dir, "virtual@localhost.invalid", &sbx_home)
            .map_err(|e| format!("写虚拟登录失败：{e}"))?;
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

    let (pport, secret, proxy_action) =
        ensure_proxy(&app, &state, lifecycle, Some(&launch_runtime), Some(&trace))?;
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
    let msg = match open_science_surface(&app, &url) {
        Ok("webview") => format!("{started}，已打开 Science 窗口。"),
        Ok(_) => format!("{started}。"),
        Err(_) => format!("{started}，服务已就绪；自动打开失败，请点击“打开 Science”。"),
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
        "external_skill_installer": installer_status_json(&installer)
    }))
}
