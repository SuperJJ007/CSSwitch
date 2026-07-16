use std::process::Command;

use crate::provider_contracts::AuthMode;
use crate::runtime::provider::adapter_for_profile;
use crate::runtime::system::{asset_root, open_in_browser};
use crate::{config, run_blocking, SharedAppState, SharedLifecycle};

#[tauri::command]
pub(crate) async fn run_doctor(
    app: tauri::AppHandle,
    state: tauri::State<'_, SharedAppState>,
    lifecycle: tauri::State<'_, SharedLifecycle>,
) -> Result<String, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        let mut output = run_doctor_inner_cmd(&app)?;
        let route = lifecycle.with_serialized(|| {
            crate::runtime::sandbox_session::force_third_party_reconcile(&app, &state)
        });
        output.push_str("\n[Skill 路由] ");
        match route {
            Ok(message) => output.push_str(&message),
            Err(error) => output.push_str(&format!("核验失败：{error}")),
        }
        Ok(output)
    })
    .await
}

fn run_doctor_inner_cmd(app: &tauri::AppHandle) -> Result<String, String> {
    let root = asset_root(app).ok_or("找不到 scripts/doctor.sh（打包资源或仓库根均未命中）。")?;
    let cfg = doctor_config_from(&config::default_dir())?;
    let doctor = root.join("scripts/doctor.sh");
    // 生效 profile 的展示名（template_id）+ adapter + 脱敏认证类型；无生效配置则留空。
    let (provider_label, adapter, auth_mode, has_key) = match cfg.active_profile() {
        Some(profile) => {
            let public = crate::runtime::provider::resolve_launch_plan(profile)
                .ok()
                .map(|plan| plan.public());
            let auth_mode = match public.as_ref().map(|view| view.auth_mode) {
                Some(AuthMode::ApiKey) => "api_key",
                Some(AuthMode::KeychainOauth) => "keychain_oauth",
                Some(AuthMode::None) => "none",
                None => "",
            };
            let has_key = public.as_ref().is_some_and(|view| {
                view.auth_mode == AuthMode::ApiKey && view.credential_configured
            });
            (
                profile.template_id.clone(),
                adapter_for_profile(profile),
                auth_mode,
                has_key,
            )
        }
        None => (String::new(), String::new(), "", false),
    };
    let mut cmd = Command::new("bash");
    // 多 profile：传 template_id + adapter + key 有无（布尔）。doctor 不再按 provider 名写死、
    // 不再去 shell 环境找 key（key 存 config.json）。绝不把真实 key 值传进其环境。
    cmd.arg(&doctor)
        .env("CSSWITCH_PROVIDER", &provider_label)
        .env("CSSWITCH_ADAPTER", adapter)
        .env("CSSWITCH_AUTH_MODE", auth_mode)
        .env("CSSWITCH_KEY_PRESENT", if has_key { "1" } else { "0" })
        .env("CSSWITCH_PROXY_PORT", cfg.proxy_port.to_string())
        .env("CSSWITCH_SANDBOX_PORT", cfg.sandbox_port.to_string());
    if let Some(gateway) = crate::runtime::proxy_lifecycle::gateway_bin_path(app) {
        cmd.env("CSSWITCH_GATEWAY_BIN", gateway);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str("\n[stderr] ");
        text.push_str(err.trim());
    }
    let codex_profile_count = cfg
        .profiles
        .iter()
        .filter(|profile| profile.template_id == "codex")
        .count();
    let auth_summary = if cfg.experimental_codex_enabled || codex_profile_count > 0 {
        super::codex::codex_auth_diagnostic_summary(app)
    } else {
        "auth=not_checked".into()
    };
    text.push_str("\n[Codex 实验]\n  开关=");
    text.push_str(if cfg.experimental_codex_enabled {
        "启用"
    } else {
        "关闭"
    });
    text.push_str(&format!("  配置数={codex_profile_count}  {auth_summary}\n"));
    Ok(text)
}

fn doctor_config_from(dir: &std::path::Path) -> Result<config::Config, String> {
    config::load_from(dir).map_err(|e| format!("读取配置失败，无法运行自检：{e}"))
}

/// 当前 app 版本（供前端「检查更新」与页脚版本号用）。
#[tauri::command]
pub(crate) fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 打开 GitHub Releases 页（检查更新时用系统浏览器打开，浏览器走用户自己的代理）。
#[tauri::command]
pub(crate) fn open_release_page() -> Result<(), String> {
    open_in_browser("https://github.com/SuperJJ007/CSSwitch/releases/latest")
}

/// 打开「报 bug」页（预填 bug 模板）；用系统浏览器，走用户自己的代理。
#[tauri::command]
pub(crate) fn report_bug() -> Result<(), String> {
    open_in_browser("https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml")
}

/// 在访达里打开日志目录 `~/.csswitch/logs`，方便用户附到 bug 反馈里（先自查有无密钥）。
#[tauri::command]
pub(crate) fn open_logs() -> Result<(), String> {
    let dir = config::default_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    Command::new("open")
        .arg(&dir)
        .status()
        .map_err(|e| format!("打开日志目录失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::doctor_config_from;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csswitch-doctor-{name}-{nanos}"))
    }

    #[test]
    fn doctor_config_rejects_reserved_port_instead_of_defaulting() {
        let dir = tmpdir("reserved-port");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.json"),
            br#"{"schema_version":2,"profiles":[],"active_id":"","proxy_port":8765,"sandbox_port":8990}"#,
        )
        .unwrap();

        let err = doctor_config_from(&dir).unwrap_err();
        assert!(err.contains("读取配置失败"));
        assert!(err.contains("8765"));
    }
}
