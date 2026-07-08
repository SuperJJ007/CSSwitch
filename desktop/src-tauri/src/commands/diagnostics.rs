use std::process::Command;

use crate::runtime::provider::adapter_for_profile;
use crate::runtime::system::{asset_root, open_in_browser};
use crate::{config, run_blocking};

#[tauri::command]
pub(crate) async fn run_doctor(app: tauri::AppHandle) -> Result<String, String> {
    run_blocking(move || run_doctor_inner_cmd(app)).await
}

fn run_doctor_inner_cmd(app: tauri::AppHandle) -> Result<String, String> {
    let root = asset_root(&app).ok_or("找不到 scripts/doctor.sh（打包资源或仓库根均未命中）。")?;
    let cfg = config::load_from(&config::default_dir()).unwrap_or_default();
    let doctor = root.join("scripts/doctor.sh");
    // 生效 profile 的展示名（template_id）+ adapter + 有无 key；无生效配置则留空。
    let (provider_label, adapter, has_key) = match cfg.active_profile() {
        Some(p) => (
            p.template_id.clone(),
            adapter_for_profile(p),
            !p.api_key.is_empty(),
        ),
        None => (String::new(), "", false),
    };
    let mut cmd = Command::new("bash");
    // 多 profile：传 template_id + adapter + key 有无（布尔）。doctor 不再按 provider 名写死、
    // 不再去 shell 环境找 key（key 存 config.json）。绝不把真实 key 值传进其环境。
    cmd.arg(&doctor)
        .env("CSSWITCH_PROVIDER", &provider_label)
        .env("CSSWITCH_ADAPTER", adapter)
        .env("CSSWITCH_KEY_PRESENT", if has_key { "1" } else { "0" })
        .env("CSSWITCH_PROXY_PORT", cfg.proxy_port.to_string())
        .env("CSSWITCH_SANDBOX_PORT", cfg.sandbox_port.to_string());
    let out = cmd.output().map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str("\n[stderr] ");
        text.push_str(err.trim());
    }
    Ok(text)
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
