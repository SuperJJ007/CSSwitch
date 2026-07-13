use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Manager, Runtime};

use crate::runtime::operation::{
    self, OperationKind, OperationStage, OperationTrace, POLL_INTERVAL_MS,
};
use crate::runtime::proxy::ProxyAction;
use crate::runtime::proxy_lifecycle::ensure_proxy;
use crate::runtime::science::{
    sandbox_home, sandbox_running_ours, sandbox_science_state, sandbox_url, stop_sandbox,
    SandboxScienceState,
};
use crate::runtime::system::{asset_root, log_path, open_in_browser, open_log, redact, tail_file};
use crate::{config, lifecycle, lock, oauth_forge, proc, AppState, SharedAppState};

fn stop_sandbox_state<R: Runtime>(
    app: &tauri::AppHandle<R>,
    st: &mut AppState,
) -> Result<(), String> {
    stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url)
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

/// One-click session startup: active proxy, virtual login, sandbox, browser.
///
/// Callers must hold the command serializer lock.
pub(crate) fn one_click_login<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<Value, String> {
    let trace = OperationTrace::start(OperationKind::OneClickLogin, "command=one_click_login");
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    crate::runtime::settings::validate_runtime_ports(cfg.proxy_port, cfg.sandbox_port)?;
    let sport = cfg.sandbox_port;

    let sbx_home = sandbox_home();
    let auth_dir = sbx_home.join(".claude-science");

    match sandbox_science_state(sport) {
        SandboxScienceState::RunningHealthy => {
            if oauth_forge::login_intact(&auth_dir, "virtual@localhost.invalid", &sbx_home) {
                let (_, _, proxy_action) = ensure_proxy(&app, &state, lifecycle, Some(&trace))?;
                let url = sandbox_url(sport);
                {
                    let mut st = lock(&state);
                    st.sandbox_port = sport;
                    st.sandbox_url = Some(url.clone());
                }
                let base = match proxy_action {
                    ProxyAction::Reused => "已在运行",
                    ProxyAction::Restarted => "已用新配置重启代理，Science 沿用不变",
                };
                let msg = match open_science_surface(&app, &url) {
                    Ok("webview") => format!("{base}，已重新打开 Science 窗口。"),
                    Ok(_) => format!("{base}，已重新打开 Science。"),
                    Err(_) => format!("{base}，服务已就绪，请手动打开：{url}"),
                };
                trace.finish(format!(
                    "ok action=reopened proxy_action={}",
                    proxy_action.as_str()
                ));
                return Ok(json!({ "url": url, "msg": msg, "action": "reopened" }));
            }
            let mut st = lock(&state);
            if let Err(error) = stop_sandbox_state(&app, &mut st) {
                trace.finish("error=sandbox_stop_for_login_refresh");
                return Err(format!(
                    "隔离 Science 登录状态失效，但停止旧进程失败：{error}"
                ));
            }
        }
        SandboxScienceState::Stopped => {}
        SandboxScienceState::Unknown => {
            trace.finish("error=sandbox_state_unknown_before_start");
            return Err(format!(
                "无法确认隔离 Science 状态（端口 {sport} 或 data-dir 状态不一致）。请先停止占用该端口的进程后重试。"
            ));
        }
    }

    let preview_port = sport
        .checked_add(1)
        .ok_or("沙箱端口必须小于 65535，才能分配隔离预览端口。")?;
    if proc::loopback_port_in_use(preview_port, operation::LOCAL_HEALTH_TIMEOUT_MS) {
        trace.finish("error=science_preview_port_in_use");
        return Err(format!(
            "隔离 Science 预览端口 {preview_port} 已被占用；未启动或结束任何占用者。请修改沙箱端口后重试。"
        ));
    }

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

    let (pport, secret, proxy_action) = ensure_proxy(&app, &state, lifecycle, Some(&trace))?;
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
        .arg("--proxy-url")
        .arg(&proxy_url)
        .arg("--skip-oauth-forge")
        .env("SANDBOX_HOME", sandbox_home())
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2))
        .status()
        .map_err(|e| format!("起沙箱失败：{e}"))?;
    if !status.success() {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        trace.finish("error=sandbox_launch_failed");
        return Err(format!("起沙箱脚本失败。\n{tail}"));
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

    if !sandbox_running_ours(sport) {
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_state(&app, &mut st);
        }
        trace.finish("error=sandbox_identity_mismatch");
        return Err(format!(
            "端口 {sport} 有服务响应，但按 data-dir 确认不是本沙箱 Science（疑似被其它服务占用）。已尝试停掉刚起的沙箱。"
        ));
    }

    let url = sandbox_url(sport);
    {
        let mut st = lock(&state);
        st.sandbox_port = sport;
        st.sandbox_url = Some(url.clone());
    }
    let started = match login_action {
        oauth_forge::LoginAction::Created => "已启动",
        _ => "沙箱已重新启动，沿用原有对话",
    };
    let msg = match open_science_surface(&app, &url) {
        Ok("webview") => format!("{started}，已打开 Science 窗口。"),
        Ok(_) => format!("{started}。"),
        Err(_) => format!("{started}，服务已就绪，请手动打开：{url}"),
    };
    trace.stage(OperationStage::OpenBrowser, "done");
    trace.finish(format!(
        "ok action=started proxy_action={}",
        proxy_action.as_str()
    ));
    Ok(json!({ "url": url, "msg": msg, "action": "started" }))
}
