//! CSSwitch 桌面 app 后端（进程管家）。
//!
//! 职责：管理「翻译代理」与「沙箱 Science」两个子进程的生命周期；读写
//! `~/.csswitch/config.json`（多 profile 形态）；把第三方 key 以【环境变量】注入代理子进程
//! （绝不进 argv）；探活；把沙箱 URL 交系统浏览器打开。推理与协议转换由随包交付的
//! Rust `csswitch-gateway` 完成；沙箱脚本仍作为受管子进程保留铁律护栏。
//!
//! 运行行为由 typed provider-contract catalog 与生效 profile 合并成受限 launch plan，
//! 再投影给 formal gateway、scratch 和前端；展示模板不再决定 adapter/鉴权/transport。
//!
//! 铁律相关：API key 只在内存与 0600 的 config.json，OAuth token 只在 CSSwitch 私有认证文件；回显前端只给掩码/脱敏状态；沙箱端口/目录护栏
//! 由被调脚本负责（对 8765 与真实目录失败关闭）；关窗只隐藏，显式退出停代理与沙箱。

mod codex_auth_supervisor;
mod commands;
mod config;
mod config_legacy;
mod lifecycle;
mod model_catalog;
mod oauth_forge;
mod proc;
mod provider_contracts;
mod runtime;
mod scratch;
mod templates;

use std::process::Child;
use std::sync::{Arc, Mutex};

use tauri::{Emitter, Manager};

use runtime::{science::stop_sandbox, system::kill_child};

#[cfg(test)]
pub(crate) fn test_temp_root() -> std::path::PathBuf {
    let private_tmp = std::path::PathBuf::from("/private/tmp");
    if private_tmp.is_dir() {
        private_tmp
    } else {
        std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
    }
}

use codex_auth_supervisor::{CodexAuthSupervisor, SharedCodexAuthSupervisor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchPath {
    ShowPanel,
    OpenOfficial,
    BootScience,
}

fn decide_launch_with_auto_boot(cfg: &config::Config, auto_boot: bool) -> LaunchPath {
    if !auto_boot {
        return LaunchPath::ShowPanel;
    }
    if cfg.mode == "official" {
        return LaunchPath::OpenOfficial;
    }
    match cfg.active_profile() {
        Some(p)
            if config::require_template_enabled(cfg, &p.template_id).is_ok()
                && p.template_id != "codex"
                && runtime::provider::resolve_launch_plan(p)
                    .map(|plan| plan.public().credential_configured)
                    .unwrap_or(false) =>
        {
            LaunchPath::BootScience
        }
        _ => LaunchPath::ShowPanel,
    }
}

fn decide_launch(cfg: &config::Config) -> LaunchPath {
    let auto_boot = std::env::var("CSSWITCH_AUTO_BOOT_ON_LAUNCH")
        .ok()
        .as_deref()
        == Some("1");
    decide_launch_with_auto_boot(cfg, auto_boot)
}

fn normalize_platform_mode_config(current: &mut config::Config, official_supported: bool) -> bool {
    if official_supported || current.mode != "official" {
        return false;
    }
    current.mode = "proxy".into();
    let note = "Linux beta 不支持托管官方 Claude Science，已切回第三方隔离模式。";
    current.pending_notice = Some(match current.pending_notice.take() {
        Some(existing) if !existing.trim().is_empty() => format!("{existing} {note}"),
        _ => note.into(),
    });
    true
}

fn normalize_unsupported_platform_mode() -> Result<(), String> {
    if runtime::platform::official_mode_supported() {
        return Ok(());
    }
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
    if cfg.mode != "official" {
        return Ok(());
    }
    config::update(&dir, |current| {
        let _ = normalize_platform_mode_config(current, false);
    })
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BootState {
    #[default]
    Idle,
    Starting,
    Ready,
    Failed,
}

fn should_begin_boot(state: BootState) -> bool {
    matches!(state, BootState::Idle | BootState::Failed)
}

#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) proxy: Option<Child>,
    pub(crate) proxy_port: u16,
    pub(crate) secret: String,
    /// 当前代理进程所用 adapter 名（deepseek | qwen | relay | openai-custom | openai-responses）；用于健康复用判定。
    pub(crate) provider: String,
    /// 当前代理进程的 gateway 实现身份（生产值固定为 rust）。
    pub(crate) gateway_kind: String,
    /// 当前代理进程的 DeepSeek tool-use shim 模式（off | detect | rewrite）。
    pub(crate) shim_mode: String,
    /// Tauri 每次启动 managed Rust gateway 时生成的唯一实例身份。
    pub(crate) launch_id: String,
    /// 当前代理进程所用 key 的非加密指纹（仅内存、绝不落盘/打印）。
    /// 换 key/换上游后指纹变化 → 触发重启，避免复用带旧配置的代理。
    pub(crate) key_fp: u64,
    pub(crate) sandbox: Option<Child>,
    pub(crate) sandbox_port: u16,
    pub(crate) sandbox_url: Option<String>,
    /// 当前 CSSwitch Science daemon 的实际 binary 身份，仅存内存；绝不形成版本偏好。
    pub(crate) science_runtime: Option<runtime::science::ScienceRuntimeIdentity>,
    /// CSSwitch 自己成功停止后的单次快速启动令牌；下一次启动消费，App 重启即丢弃。
    pub(crate) science_confirmed_stopped: Option<runtime::science::ScienceRuntimeIdentity>,
    /// Science 版本探测缓存与 daemon 身份分离；停止 daemon 后仍可复用未变化二进制的版本。
    pub(crate) science_version_cache: runtime::science::ScienceVersionCache,
    boot: BootState,
    pub(crate) boot_error: Option<String>,
}

impl AppState {
    pub(crate) fn clear_proxy_identity(&mut self) {
        self.secret.clear();
        self.provider.clear();
        self.gateway_kind.clear();
        self.shim_mode.clear();
        self.launch_id.clear();
        self.key_fp = 0;
    }

    pub(crate) fn stop_proxy(&mut self) {
        kill_child(&mut self.proxy);
        self.clear_proxy_identity();
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        // `std::process::Child` does not kill on drop. Keep a final owned-child
        // safety net in addition to the Tauri exit events so a graceful app
        // teardown cannot orphan the managed gateway.
        self.stop_proxy();
    }
}

pub(crate) type SharedAppState = Arc<Mutex<AppState>>;
pub(crate) type SharedLifecycle = Arc<lifecycle::Lifecycle>;

/// 取锁并从 poison 中恢复：某线程持锁时 panic 不应把整个 app 卡死。
pub(crate) fn lock(m: &Mutex<AppState>) -> std::sync::MutexGuard<'_, AppState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) async fn run_blocking<T>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("后台任务失败：{e}"))?
}

pub(crate) async fn run_blocking_typed<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E>
where
    T: Send + 'static,
    E: From<String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| E::from(format!("后台任务失败：{e}")))?
}

fn show_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn install_menu(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};

    let preferences = MenuItemBuilder::with_id("preferences", "偏好设置...")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "CSSwitch")
        .item(&preferences)
        .separator()
        .quit()
        .build()?;
    let menu_builder = MenuBuilder::new(app).item(&app_menu);
    #[cfg(target_os = "macos")]
    let menu_builder = {
        // Native predefined edit items are what wires the standard macOS
        // Command-X/C/V/A/Z shortcuts into the focused WebView field.
        let edit_menu = SubmenuBuilder::new(app, "编辑")
            .undo_with_text("撤销")
            .redo_with_text("重做")
            .separator()
            .cut_with_text("剪切")
            .copy_with_text("复制")
            .paste_with_text("粘贴")
            .select_all_with_text("全选")
            .build()?;
        menu_builder.item(&edit_menu)
    };
    let menu = menu_builder.build()?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| {
        if event.id().as_ref() == "preferences" {
            show_main_window(app);
        }
    });
    Ok(())
}

fn cleanup_for_exit<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let supervisor = app.state::<SharedCodexAuthSupervisor>().inner().clone();
    // First give login its protocol-level cancel path and read-only preflight
    // its cancellation token. Do not signal a possibly committing login child
    // until the bounded waiter has had a chance to reap it normally.
    let _ = supervisor.cancel_for_exit();
    let remaining = supervisor.wait_for_auth_children_exit(std::time::Duration::from_secs(2));
    for pid in remaining {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    let remaining = supervisor.wait_for_auth_children_exit(std::time::Duration::from_millis(500));
    for pid in remaining {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    let _ = supervisor.wait_for_auth_children_exit(std::time::Duration::from_millis(500));
    let state = app.state::<SharedAppState>().inner().clone();
    let lifecycle = app.state::<SharedLifecycle>().inner().clone();
    lifecycle.with_serialized(|| {
        let mut st = lock(&state);
        if let Some(runtime) = st.science_runtime.clone() {
            let stop_result = {
                let st = &mut *st;
                stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url, Some(&runtime))
            };
            if stop_result.is_ok() {
                st.science_runtime = None;
            }
        }
        st.stop_proxy();
    });
}

fn mark_boot_failed<R: tauri::Runtime>(app: &tauri::AppHandle<R>, error: String) {
    let state = app.state::<SharedAppState>();
    {
        let mut st = lock(state.inner());
        st.boot = BootState::Failed;
        st.boot_error = Some(error.clone());
    }
    show_main_window(app);
    let _ = app.emit("boot://failed", error);
}

fn boot_result_error(value: &serde_json::Value) -> Option<String> {
    (value.get("status").and_then(serde_json::Value::as_str) == Some("error")).then(|| {
        value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("自动启动未完成")
            .to_string()
    })
}

fn run_boot_coordinator(app: tauri::AppHandle) {
    {
        let state = app.state::<SharedAppState>();
        let mut st = lock(state.inner());
        if !should_begin_boot(st.boot) {
            show_main_window(&app);
            return;
        }
        st.boot = BootState::Starting;
    }

    tauri::async_runtime::spawn_blocking(move || {
        let cfg = match config::load_from(&config::default_dir()) {
            Ok(cfg) => cfg,
            Err(e) => {
                mark_boot_failed(&app, format!("读取配置失败：{e}"));
                return;
            }
        };
        let state = app.state::<SharedAppState>();
        match decide_launch(&cfg) {
            LaunchPath::ShowPanel => {
                let mut st = lock(state.inner());
                st.boot = BootState::Idle;
                st.boot_error = None;
                show_main_window(&app);
            }
            LaunchPath::OpenOfficial => match commands::runtime::open_official() {
                Ok(()) => {
                    let mut st = lock(state.inner());
                    st.boot = BootState::Idle;
                    st.boot_error = None;
                }
                Err(e) => mark_boot_failed(&app, e),
            },
            LaunchPath::BootScience => {
                let state_inner = state.inner().clone();
                let lifecycle = app.state::<SharedLifecycle>().inner().clone();
                match commands::runtime::one_click_login_cmd(
                    app.clone(),
                    state_inner,
                    lifecycle,
                    None,
                ) {
                    Ok(value) => match boot_result_error(&value) {
                        Some(message) => mark_boot_failed(&app, message),
                        None => {
                            let mut st = lock(state.inner());
                            st.boot = BootState::Ready;
                            st.boot_error = None;
                        }
                    },
                    Err(e) => mark_boot_failed(&app, e.to_string()),
                }
            }
        }
    });
}

// ---------- 入口 ----------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            run_boot_coordinator(app.clone());
        }))
        .manage(Arc::new(Mutex::new(AppState::default())))
        .manage(Arc::new(lifecycle::Lifecycle::new()))
        .manage(Arc::new(CodexAuthSupervisor::default()))
        .invoke_handler(tauri::generate_handler![
            commands::codex::set_experimental_codex_enabled,
            commands::codex::set_codex_network,
            commands::codex::codex_auth_status,
            commands::codex::codex_auth_start,
            commands::codex::codex_auth_cancel,
            commands::codex::codex_auth_operation_status,
            commands::codex::codex_ensure_profile,
            commands::codex::codex_auth_logout,
            commands::codex::codex_downgrade_preview,
            commands::codex::codex_downgrade_export_all,
            commands::profiles::get_config,
            commands::profiles::list_templates,
            commands::runtime::set_settings,
            commands::runtime::set_mode,
            commands::runtime::open_official,
            commands::profiles::create_profile,
            commands::profiles::update_profile_metadata,
            commands::profiles::update_profile_connection,
            commands::profiles::validate_profile_catalog_model,
            commands::profiles::preview_profile_preset_sync,
            commands::profiles::apply_profile_preset_sync,
            commands::profiles::clear_profile_key,
            commands::profiles::delete_profile,
            commands::profiles::set_active_profile,
            commands::runtime::start_proxy,
            commands::runtime::fetch_models,
            commands::runtime::stop_all,
            commands::runtime::one_click_login,
            commands::runtime::science_runtime_preflight,
            commands::runtime::open_science_download_page,
            commands::runtime::status,
            commands::runtime::boot_error,
            commands::runtime::open_url,
            commands::skill_install::install_local_skill_package,
            commands::skill_listing::list_installed_skills,
            commands::diagnostics::run_doctor,
            commands::diagnostics::app_version,
            commands::diagnostics::open_release_page,
            commands::diagnostics::report_bug,
            commands::diagnostics::open_logs,
            commands::runtime::quit_app
        ])
        .setup(|app| {
            install_menu(app)?;

            // 启动即触发一次 load：旧 v1/v2/v3 配置在这里经迁移链升级并只提交一次当前 v4；
            // v1/v2/v3 的版本备份由 config::load_from 按不可覆盖合同保存；
            // 悬空 active 归一化为空。迁移逻辑并入 config::load_from（不再单独跑 relay_presets）。
            let _ = config::load_from(&config::default_dir());
            let platform_mode_ready = match normalize_unsupported_platform_mode() {
                Ok(()) => true,
                Err(error) => {
                    let mut state = lock(app.state::<SharedAppState>().inner());
                    state.boot = BootState::Failed;
                    state.boot_error = Some(format!("平台模式归一化失败，已拒绝自动启动：{error}"));
                    false
                }
            };

            // 关窗隐藏配置面板，不销毁窗口、不停止后台链路。显式退出清理代理与沙箱。
            if let Some(win) = app.get_webview_window("main") {
                let w = win.clone();
                win.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }
            if platform_mode_ready {
                run_boot_coordinator(app.handle().clone());
            } else {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app, event| match event {
        tauri::RunEvent::Reopen { .. } => show_main_window(app),
        tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => cleanup_for_exit(app),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::config::{Config, Profile};
    use crate::runtime::system::redact;
    use crate::{
        boot_result_error, decide_launch_with_auto_boot, normalize_platform_mode_config,
        should_begin_boot, AppState, BootState, LaunchPath,
    };

    #[test]
    fn auto_boot_rejects_structured_runtime_failure() {
        let failed = serde_json::json!({
            "status": "error",
            "message": "gateway recovery degraded",
        });
        assert_eq!(
            boot_result_error(&failed).as_deref(),
            Some("gateway recovery degraded")
        );
        assert!(boot_result_error(&serde_json::json!({"status": "ok"})).is_none());
    }

    #[test]
    fn unsupported_official_mode_normalizes_once_without_schema_change() {
        let mut cfg = Config {
            mode: "official".into(),
            pending_notice: Some("existing".into()),
            ..Config::default()
        };
        assert!(normalize_platform_mode_config(&mut cfg, false));
        assert_eq!(cfg.mode, "proxy");
        assert_eq!(cfg.schema_version, 4);
        assert!(cfg.pending_notice.as_deref().unwrap().contains("existing"));
        assert!(cfg
            .pending_notice
            .as_deref()
            .unwrap()
            .contains("Linux beta"));
        assert!(!normalize_platform_mode_config(&mut cfg, false));
    }

    #[test]
    fn app_state_clear_proxy_identity_removes_runtime_credentials() {
        let mut st = AppState::default();
        st.secret = "secret".into();
        st.provider = "deepseek".into();
        st.gateway_kind = "rust".into();
        st.shim_mode = "off".into();
        st.launch_id = "launch-old".into();
        st.key_fp = 42;
        st.clear_proxy_identity();
        assert!(st.secret.is_empty());
        assert!(st.provider.is_empty());
        assert!(st.gateway_kind.is_empty());
        assert!(st.shim_mode.is_empty());
        assert!(st.launch_id.is_empty());
        assert_eq!(st.key_fp, 0);
    }

    #[test]
    fn app_state_drop_reaps_owned_proxy_child() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn owned test child");
        let pid = child.id();
        {
            let mut st = AppState::default();
            st.proxy = Some(child);
        }
        let status = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .output()
            .expect("inspect owned test child");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "AppState drop left owned proxy child {pid} alive"
        );
    }

    #[test]
    fn redact_scrubs_secret_and_is_noop_when_empty() {
        assert_eq!(
            redact("推理指向 http://127.0.0.1:18991/abcd1234 尾巴", "abcd1234"),
            "推理指向 http://127.0.0.1:18991/**** 尾巴"
        );
        assert_eq!(redact("原样返回", ""), "原样返回");
        assert!(!redact("leak abcd1234 leak abcd1234", "abcd1234").contains("abcd1234"));
    }

    fn keyed_profile(id: &str, key: &str) -> Profile {
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::new_profile_catalog("deepseek", "anthropic", None).unwrap();
        let model = model_catalog
            .iter()
            .find(|route| route.selector_id == default_model_route_id)
            .unwrap()
            .upstream_model
            .clone();
        Profile {
            id: id.into(),
            name: id.into(),
            template_id: "deepseek".into(),
            category: "cn_official".into(),
            api_format: "anthropic".into(),
            api_key: key.into(),
            model,
            model_catalog,
            default_model_route_id,
            role_bindings,
            model_policy: crate::provider_contracts::ModelPolicy::SavedCatalog,
            ..Default::default()
        }
    }

    fn codex_profile(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: id.into(),
            template_id: "codex".into(),
            category: "experimental".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            ..Default::default()
        }
    }

    #[test]
    fn decide_launch_defaults_to_showing_panel() {
        let active_with_key = Config {
            profiles: vec![keyed_profile("p1", "sk-present")],
            active_id: "p1".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&active_with_key, false),
            LaunchPath::ShowPanel
        );
    }

    #[test]
    fn decide_launch_auto_boot_uses_current_mode_and_active_profile_key() {
        let official = Config {
            mode: "official".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&official, true),
            LaunchPath::OpenOfficial
        );

        let no_active = Config {
            profiles: vec![keyed_profile("p1", "sk-present")],
            active_id: String::new(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&no_active, true),
            LaunchPath::ShowPanel
        );

        let active_without_key = Config {
            profiles: vec![keyed_profile("p1", "")],
            active_id: "p1".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&active_without_key, true),
            LaunchPath::ShowPanel
        );

        let active_with_key = Config {
            profiles: vec![keyed_profile("p1", "sk-present")],
            active_id: "p1".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&active_with_key, true),
            LaunchPath::BootScience
        );

        let dangling_active = Config {
            profiles: vec![keyed_profile("p1", "sk-present")],
            active_id: "missing".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&dangling_active, true),
            LaunchPath::ShowPanel
        );

        let codex_disabled = Config {
            profiles: vec![codex_profile("codex-1")],
            active_id: "codex-1".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_launch_with_auto_boot(&codex_disabled, true),
            LaunchPath::ShowPanel
        );
        let codex_enabled = Config {
            experimental_codex_enabled: true,
            ..codex_disabled
        };
        assert_eq!(
            decide_launch_with_auto_boot(&codex_enabled, true),
            LaunchPath::ShowPanel
        );
    }

    #[test]
    fn should_begin_boot_only_from_idle_or_failed() {
        assert!(should_begin_boot(BootState::Idle));
        assert!(should_begin_boot(BootState::Failed));
        assert!(!should_begin_boot(BootState::Starting));
        assert!(!should_begin_boot(BootState::Ready));
    }
}
