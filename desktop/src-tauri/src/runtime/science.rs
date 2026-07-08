use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::{config, proc};

use super::system::{asset_root, kill_child};

pub(crate) const SCIENCE_BIN: &str =
    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science";

/// 沙箱可写工作目录（独立 HOME）：`~/.csswitch/sandbox/home`。
pub(crate) fn sandbox_home() -> PathBuf {
    config::default_dir().join("sandbox").join("home")
}

/// 端口变更是否需要拆掉现有链路（纯函数，P1-c）。代理/沙箱任一端口变了，正在跑的代理就绑在
/// 旧端口、正在跑的沙箱又把旧代理 URL 烘死了，二者与新配置不一致 → 拆掉逼下次「一键开始」按新端口重建。
pub(crate) fn settings_change_needs_teardown(
    old_proxy: u16,
    new_proxy: u16,
    old_sandbox: u16,
    new_sandbox: u16,
) -> bool {
    old_proxy != new_proxy || old_sandbox != new_sandbox
}

/// 从 `claude-science url` 的 stdout 里取**第一条**合法 http(s) URL。
pub(crate) fn first_http_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let t = line.trim();
        if t.starts_with("http://") || t.starts_with("https://") {
            let url = t.split_whitespace().next().unwrap_or(t);
            return Some(url.to_string());
        }
    }
    None
}

/// Return the sandbox UI URL, falling back to the plain localhost port.
pub(crate) fn sandbox_url(port: u16) -> String {
    let home = sandbox_home();
    let data_dir = home.join(".claude-science");
    if Path::new(SCIENCE_BIN).is_file() {
        if let Ok(out) = Command::new(SCIENCE_BIN)
            .arg("url")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(url) = first_http_url(&s) {
                return url;
            }
        }
    }
    format!("http://127.0.0.1:{port}")
}

/// Check that the sandbox Science associated with our data-dir is running.
/// A naked `/health` response is not sufficient identity proof.
pub(crate) fn sandbox_running_ours(port: u16) -> bool {
    let home = sandbox_home();
    let data_dir = home.join(".claude-science");
    if Path::new(SCIENCE_BIN).is_file() {
        match Command::new(SCIENCE_BIN)
            .arg("status")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout);
                let running = s.contains("\"running\":true") || s.contains("\"running\": true");
                return running && proc::http_health(port, None, 400);
            }
            Err(_) => return proc::http_health(port, None, 400),
        }
    }
    proc::http_health(port, None, 400)
}

/// Stop the sandbox Science process and clear the in-memory sandbox URL.
///
/// Returns `Err` when the stop script is missing or exits non-zero, so callers
/// can report that Science may not have stopped cleanly.
pub(crate) fn stop_sandbox(
    app: &tauri::AppHandle,
    sandbox: &mut Option<Child>,
    sandbox_url: &mut Option<String>,
) -> Result<(), String> {
    let mut err = None;
    match asset_root(app) {
        Some(root) => {
            let stop = root.join("scripts/stop-science-sandbox.sh");
            if stop.is_file() {
                match Command::new("zsh")
                    .arg(&stop)
                    .env("SANDBOX_HOME", sandbox_home())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => {}
                    Ok(s) => err = Some(format!("停止沙箱脚本非零退出（{:?}）。", s.code())),
                    Err(e) => err = Some(format!("调用停止沙箱脚本失败：{e}")),
                }
            } else {
                err = Some(format!(
                    "找不到停止脚本 {}，无法确认沙箱已停止（沙箱可能仍在运行）。",
                    stop.display()
                ));
            }
        }
        None => {
            err = Some(
                "定位不到资源根，取不到停止脚本，无法确认沙箱已停止（沙箱可能仍在运行）。"
                    .to_string(),
            );
        }
    }
    kill_child(sandbox);
    *sandbox_url = None;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{first_http_url, sandbox_home, sandbox_url, settings_change_needs_teardown};

    // ---------- P1-c: 端口变更是否需拆链路（纯函数，4 组合） ----------
    #[test]
    fn settings_teardown_when_any_port_changes() {
        assert!(
            !settings_change_needs_teardown(18991, 18991, 8990, 8990),
            "端口未变 → 不拆链路"
        );
        assert!(
            settings_change_needs_teardown(18991, 19000, 8990, 8990),
            "代理端口变 → 拆（旧代理绑旧端口、沙箱烘旧 URL）"
        );
        assert!(
            settings_change_needs_teardown(18991, 18991, 8990, 9000),
            "沙箱端口变 → 拆（旧沙箱在旧端口成孤儿）"
        );
        assert!(
            settings_change_needs_teardown(18991, 19000, 8990, 9000),
            "都变 → 拆"
        );
    }

    #[test]
    fn first_http_url_takes_only_first_valid_url() {
        let multi = "http://127.0.0.1:8990/setup?nonce=abc123\n\
                     This is a single-use link, expires in 60 seconds.";
        assert_eq!(
            first_http_url(multi).as_deref(),
            Some("http://127.0.0.1:8990/setup?nonce=abc123"),
        );
        let inline = "https://x.example/y?z=1  (single-use)";
        assert_eq!(
            first_http_url(inline).as_deref(),
            Some("https://x.example/y?z=1")
        );
        let lead = "Open this link in your browser:\nhttp://127.0.0.1:8990/a";
        assert_eq!(
            first_http_url(lead).as_deref(),
            Some("http://127.0.0.1:8990/a")
        );
        assert_eq!(first_http_url("no url here\nnor here"), None);
        assert_eq!(
            first_http_url("http://127.0.0.1:8990").as_deref(),
            Some("http://127.0.0.1:8990")
        );
    }

    #[test]
    fn sandbox_home_is_writable_under_config_dir() {
        let h = sandbox_home();
        assert!(h.ends_with("sandbox/home"), "应以 sandbox/home 结尾：{h:?}");
        assert!(
            h.to_string_lossy().contains(".csswitch"),
            "应在 .csswitch 下：{h:?}"
        );
    }

    #[test]
    fn sandbox_url_falls_back_to_localhost_when_cli_absent() {
        // In CI/dev environments without Claude Science installed, this keeps
        // the command behavior deterministic and matches the previous fallback.
        if !std::path::Path::new(super::SCIENCE_BIN).is_file() {
            assert_eq!(sandbox_url(8990), "http://127.0.0.1:8990");
        }
    }
}
