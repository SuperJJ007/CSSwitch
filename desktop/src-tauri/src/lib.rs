//! CSSwitch 桌面 app 后端（进程管家）。
//!
//! 职责：管理「翻译代理」与「沙箱 Science」两个子进程的生命周期；读写
//! `~/.csswitch/config.json`（多 profile 形态）；把第三方 key 以【环境变量】注入代理子进程
//! （绝不进 argv）；探活；把沙箱 URL 交系统浏览器打开。已验证的越权/翻译逻辑仍留在
//! Python/Node/shell 里被当作子进程调用，以保住铁律护栏与已验证行为。
//!
//! 运行行为由生效 profile 的 `template_id` 经 [`templates`] 注册表派生出 adapter
//! （deepseek | qwen | relay | openai-custom | openai-responses），再传给 python 代理 `--provider`。
//!
//! 铁律相关：key 只在内存与 0600 的 config.json；回显前端只给掩码；沙箱端口/目录护栏
//! 由被调脚本负责（对 8765 与真实目录失败关闭）；退 app 默认停代理、保留沙箱。

mod commands;
mod config;
mod config_legacy;
mod lifecycle;
mod oauth_forge;
mod proc;
mod runtime;
mod scratch;
mod templates;

use std::process::Child;
use std::sync::{Arc, Mutex};

use tauri::Manager;

use runtime::system::kill_child;

#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) proxy: Option<Child>,
    pub(crate) proxy_port: u16,
    pub(crate) secret: String,
    /// 当前代理进程所用 adapter 名（deepseek | qwen | relay | openai-custom | openai-responses）；用于健康复用判定。
    pub(crate) provider: String,
    /// 当前代理进程所用 key 的非加密指纹（仅内存、绝不落盘/打印）。
    /// 换 key/换上游后指纹变化 → 触发重启，避免复用带旧配置的代理。
    pub(crate) key_fp: u64,
    pub(crate) sandbox: Option<Child>,
    pub(crate) sandbox_port: u16,
    pub(crate) sandbox_url: Option<String>,
}

impl AppState {
    pub(crate) fn clear_proxy_identity(&mut self) {
        self.secret.clear();
        self.provider.clear();
        self.key_fp = 0;
    }

    pub(crate) fn stop_proxy(&mut self) {
        kill_child(&mut self.proxy);
        self.clear_proxy_identity();
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

// ---------- 入口 ----------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Arc::new(Mutex::new(AppState::default())))
        .manage(Arc::new(lifecycle::Lifecycle::new()))
        .invoke_handler(tauri::generate_handler![
            commands::profiles::get_config,
            commands::profiles::list_templates,
            commands::runtime::set_settings,
            commands::runtime::set_mode,
            commands::runtime::open_official,
            commands::profiles::create_profile,
            commands::profiles::update_profile_metadata,
            commands::profiles::update_profile_connection,
            commands::profiles::clear_profile_key,
            commands::profiles::delete_profile,
            commands::profiles::set_active_profile,
            commands::runtime::start_proxy,
            commands::runtime::verify_key,
            commands::runtime::fetch_models,
            commands::runtime::stop_all,
            commands::runtime::one_click_login,
            commands::runtime::status,
            commands::runtime::open_url,
            commands::diagnostics::run_doctor,
            commands::diagnostics::app_version,
            commands::diagnostics::open_release_page,
            commands::diagnostics::report_bug,
            commands::diagnostics::open_logs,
            commands::runtime::quit_app
        ])
        .setup(|app| {
            // 正常桌面应用：进 Dock、走常规应用生命周期。窗口在 tauri.conf.json 里配了
            // decorations + visible + center，启动即居中弹出、可拖动。托盘图标已移除。

            // 启动即触发一次 load：若是旧 v1 固定槽文件，这里完成 v1→v2 迁移 + 落盘 + 留 .v1.bak；
            // 悬空 active 归一化为空。迁移逻辑并入 config::load_from（不再单独跑 relay_presets）。
            let _ = config::load_from(&config::default_dir());

            // 关窗即退出：与「退出」按钮一致 —— 停代理、清 secret，保留沙箱运行（spec §5.1）。
            if let Some(win) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                win.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { .. } = ev {
                        let state = handle.state::<SharedAppState>();
                        let mut st = lock(state.inner());
                        st.stop_proxy();
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use crate::runtime::system::redact;
    use crate::AppState;

    #[test]
    fn app_state_clear_proxy_identity_removes_runtime_credentials() {
        let mut st = AppState {
            secret: "secret".into(),
            provider: "deepseek".into(),
            key_fp: 42,
            ..AppState::default()
        };
        st.clear_proxy_identity();
        assert!(st.secret.is_empty());
        assert!(st.provider.is_empty());
        assert_eq!(st.key_fp, 0);
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
}
