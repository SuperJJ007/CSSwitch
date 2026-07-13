use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use tauri::Runtime;

use crate::{config, proc};

use super::system::{asset_root, kill_child};

pub(crate) const SCIENCE_BIN: &str =
    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science";

/// 沙箱可写工作目录（独立 HOME）：`~/.csswitch/sandbox/home`。
pub(crate) fn sandbox_home() -> PathBuf {
    config::default_dir().join("sandbox").join("home")
}

/// CSSwitch 隔离 Science 的持久化 data-dir；其中的 Skill 内容由 Science 自身管理。
pub(crate) fn sandbox_data_dir() -> PathBuf {
    sandbox_home().join(".claude-science")
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

fn is_executable_file(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match current.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    path.is_file()
        && path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn is_explicit_executable_file(path: &Path) -> bool {
    is_executable_file(path)
}

fn science_bins_for_paths(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    app_bin: &Path,
) -> Vec<PathBuf> {
    if let Some(bin) = explicit_bin {
        return if is_explicit_executable_file(bin) {
            vec![bin.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut bins = Vec::with_capacity(2);
    if is_executable_file(app_bin) {
        bins.push(app_bin.to_path_buf());
    }
    let sandbox_bin = data_dir.join("bin").join("claude-science");
    if sandbox_bin != app_bin && is_executable_file(&sandbox_bin) {
        bins.push(sandbox_bin);
    }
    bins
}

#[cfg(test)]
fn science_bin_for_paths(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    app_bin: &Path,
) -> Option<PathBuf> {
    science_bins_for_paths(data_dir, explicit_bin, app_bin)
        .into_iter()
        .next()
}

fn science_bins_for(data_dir: &Path) -> Vec<PathBuf> {
    let explicit_bin = std::env::var_os("SCIENCE_BIN").map(PathBuf::from);
    science_bins_for_paths(data_dir, explicit_bin.as_deref(), Path::new(SCIENCE_BIN))
}

#[cfg(test)]
fn science_status_running(out: &Output) -> bool {
    out.status.success() && science_status_value(out) == Some(true)
}

fn science_status_value(out: &Output) -> Option<bool> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (idx, ch) in stdout.char_indices() {
        if ch != '{' {
            continue;
        }
        let mut stream =
            serde_json::Deserializer::from_str(&stdout[idx..]).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            if let Some(running) = value.get("running").and_then(|running| running.as_bool()) {
                return Some(running);
            }
        }
    }
    None
}

fn trusted_science_status(out: &Output) -> Option<bool> {
    match science_status_value(out) {
        Some(false) => Some(false),
        Some(true) if out.status.success() => Some(true),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxScienceState {
    RunningHealthy,
    Stopped,
    Unknown,
}

#[cfg(test)]
fn classify_sandbox_state(
    status: Option<bool>,
    health_ready: bool,
    port_accepts_tcp: bool,
) -> SandboxScienceState {
    match status {
        Some(true) if health_ready => SandboxScienceState::RunningHealthy,
        Some(false) if !port_accepts_tcp => SandboxScienceState::Stopped,
        _ => SandboxScienceState::Unknown,
    }
}

fn classify_known_runtime_state(
    status: Option<bool>,
    health_ready: bool,
    port_accepts_tcp: bool,
    listener_matches_runtime: bool,
) -> SandboxScienceState {
    match status {
        Some(true) if health_ready && listener_matches_runtime => {
            SandboxScienceState::RunningHealthy
        }
        Some(false) if !port_accepts_tcp => SandboxScienceState::Stopped,
        _ => SandboxScienceState::Unknown,
    }
}

fn loopback_port_accepts_tcp(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

fn listener_uses_runtime(port: u16, runtime: &Path) -> bool {
    let listener = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output();
    let Ok(listener) = listener else {
        return false;
    };
    if !listener.status.success() {
        return false;
    }
    let Ok(stdout) = String::from_utf8(listener.stdout) else {
        return false;
    };
    let mut pids = stdout.lines().map(str::trim).filter(|pid| !pid.is_empty());
    let Some(pid) = pids.next() else {
        return false;
    };
    if pids.any(|other| other != pid) {
        return false;
    }
    let Ok(expected) = runtime.canonicalize() else {
        return false;
    };
    let text_files = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", pid, "-d", "txt", "-Fn"])
        .output();
    let Ok(text_files) = text_files else {
        return false;
    };
    if !text_files.status.success() {
        return false;
    }
    String::from_utf8_lossy(&text_files.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter_map(|path| Path::new(path).canonicalize().ok())
        .any(|path| path == expected)
}

/// Return the sandbox UI URL, falling back to the plain localhost port.
pub(crate) fn sandbox_url(port: u16) -> String {
    let home = sandbox_home();
    let data_dir = sandbox_data_dir();
    for bin in science_bins_for(&data_dir) {
        if let Ok(out) = Command::new(bin)
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
    sandbox_science_state(port) == SandboxScienceState::RunningHealthy
}

pub(crate) fn sandbox_science_state(port: u16) -> SandboxScienceState {
    let home = sandbox_home();
    let data_dir = sandbox_data_dir();
    let candidates = science_bins_for(&data_dir);
    let mut saw_running = false;
    let mut saw_stopped = false;
    for bin in &candidates {
        let status = Command::new(bin)
            .arg("status")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
            .ok()
            .and_then(|out| trusted_science_status(&out));
        saw_running |= status == Some(true);
        saw_stopped |= status == Some(false);
    }
    let status = if saw_running {
        Some(true)
    } else if saw_stopped {
        Some(false)
    } else {
        None
    };
    if status.is_none() {
        return SandboxScienceState::Unknown;
    }
    let health_ready = proc::http_health(port, None, 400);
    let port_accepts_tcp = health_ready || loopback_port_accepts_tcp(port);
    let listener_matches_runtime = status == Some(true)
        && health_ready
        && candidates
            .iter()
            .any(|runtime| listener_uses_runtime(port, runtime));
    classify_known_runtime_state(
        status,
        health_ready,
        port_accepts_tcp,
        listener_matches_runtime,
    )
}

/// Stop the sandbox Science process and clear the in-memory sandbox URL.
///
/// Returns `Err` when the stop script is missing or exits non-zero, so callers
/// can report that Science may not have stopped cleanly.
pub(crate) fn stop_sandbox<R: Runtime>(
    app: &tauri::AppHandle<R>,
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
                err = Some(
                    "找不到打包的停止脚本，无法确认沙箱已停止（沙箱可能仍在运行）。".to_string(),
                );
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
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    use super::{
        classify_known_runtime_state, classify_sandbox_state, first_http_url, sandbox_home,
        sandbox_running_ours, sandbox_url, science_bin_for_paths, science_status_running,
        settings_change_needs_teardown, trusted_science_status, SandboxScienceState,
    };

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
    fn science_status_running_accepts_compact_and_spaced_json() {
        assert!(science_status_running(&status_output(
            0,
            r#"{"running":true}"#
        )));
        assert!(science_status_running(&status_output(
            0,
            r#"{"running": true}"#
        )));
        assert!(!science_status_running(&status_output(
            0,
            r#"{"running":false}"#
        )));
        assert!(!science_status_running(&status_output(0, "running")));
        assert!(!science_status_running(&status_output(
            1,
            r#"{"running": true}"#
        )));
    }

    #[test]
    fn science_status_running_accepts_json_with_cli_text() {
        assert!(science_status_running(&status_output(
            0,
            "Claude Science status:\n{\"running\": true, \"port\": 8990}\nready"
        )));
        assert!(science_status_running(&status_output(
            0,
            "warning: {not-json}\n{\"state\":\"ok\"}\n{\"running\": true}"
        )));
        assert!(!science_status_running(&status_output(
            0,
            "warning\n{\"running\": false}\n{\"running\": true}"
        )));
    }

    #[test]
    fn sandbox_state_classification_fails_closed_on_probe_disagreement() {
        assert_eq!(
            classify_sandbox_state(Some(true), true, true),
            SandboxScienceState::RunningHealthy
        );
        assert_eq!(
            classify_sandbox_state(Some(false), false, false),
            SandboxScienceState::Stopped
        );
        for state in [
            classify_sandbox_state(None, false, false),
            classify_sandbox_state(Some(true), false, true),
            classify_sandbox_state(Some(true), false, false),
            classify_sandbox_state(Some(false), true, true),
            classify_sandbox_state(Some(false), false, true),
        ] {
            assert_eq!(state, SandboxScienceState::Unknown);
        }
        assert_eq!(
            trusted_science_status(&status_output(1, r#"{"running":false}"#)),
            Some(false),
            "a stopped daemon may be reported with a non-zero CLI exit"
        );
    }

    #[test]
    fn known_runtime_state_requires_listener_binary_match() {
        assert_eq!(
            classify_known_runtime_state(Some(true), true, true, true),
            SandboxScienceState::RunningHealthy
        );
        assert_eq!(
            classify_known_runtime_state(Some(true), true, true, false),
            SandboxScienceState::Unknown
        );
    }

    #[test]
    fn science_bin_selection_matches_launch_script_priority(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-bin-selection")?;
        let data_dir = root.join("home").join(".claude-science");
        let explicit_bin = root.join("explicit-claude-science");
        let sandbox_bin = data_dir.join("bin").join("claude-science");
        let app_bin = root.join("app-claude-science");

        write_fake_bin(&explicit_bin, 0o755)?;
        write_fake_bin(&sandbox_bin, 0o755)?;
        write_fake_bin(&app_bin, 0o755)?;
        assert_eq!(
            science_bin_for_paths(&data_dir, Some(&explicit_bin), &app_bin).as_deref(),
            Some(explicit_bin.as_path())
        );

        fs::set_permissions(&explicit_bin, fs::Permissions::from_mode(0o644))?;
        assert_eq!(
            science_bin_for_paths(&data_dir, Some(&explicit_bin), &app_bin).as_deref(),
            None,
            "an invalid explicit override must not fall through to sandbox or system Science"
        );

        assert_eq!(
            science_bin_for_paths(&data_dir, None, &app_bin).as_deref(),
            Some(app_bin.as_path()),
            "the locally installed official app must win over the retained sandbox fallback"
        );

        let explicit_link = root.join("explicit-link");
        symlink(&app_bin, &explicit_link)?;
        assert_eq!(
            science_bin_for_paths(&data_dir, Some(&explicit_link), &app_bin),
            None,
            "an explicit symlink must fail closed"
        );

        let real_parent = root.join("real-parent");
        let linked_parent = root.join("linked-parent");
        let parent_bin = real_parent.join("claude-science");
        write_fake_bin(&parent_bin, 0o755)?;
        symlink(&real_parent, &linked_parent)?;
        assert_eq!(
            science_bin_for_paths(
                &data_dir,
                Some(&linked_parent.join("claude-science")),
                &app_bin,
            ),
            None,
            "an explicit path with a symlinked parent must fail closed"
        );

        fs::set_permissions(&sandbox_bin, fs::Permissions::from_mode(0o644))?;
        assert_eq!(
            science_bin_for_paths(&data_dir, None, &app_bin).as_deref(),
            Some(app_bin.as_path())
        );

        fs::set_permissions(&app_bin, fs::Permissions::from_mode(0o644))?;
        assert_eq!(science_bin_for_paths(&data_dir, None, &app_bin), None);
        fs::remove_file(&sandbox_bin)?;
        fs::set_permissions(&explicit_bin, fs::Permissions::from_mode(0o755))?;
        symlink(&explicit_bin, &sandbox_bin)?;
        assert_eq!(
            science_bin_for_paths(&data_dir, None, &app_bin),
            None,
            "a retained sandbox symlink must never be executed"
        );
        fs::remove_file(&sandbox_bin)?;
        write_fake_bin(&sandbox_bin, 0o755)?;
        assert_eq!(
            science_bin_for_paths(&data_dir, None, &app_bin).as_deref(),
            Some(sandbox_bin.as_path()),
            "an unusable official app must fall back to the retained sandbox binary"
        );
        fs::remove_dir_all(root)?;
        Ok(())
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

    #[test]
    fn sandbox_identity_does_not_trust_health_when_cli_absent() {
        if !std::path::Path::new(super::SCIENCE_BIN).is_file() {
            assert!(!sandbox_running_ours(9));
        }
    }

    fn status_output(code: i32, stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn unique_temp_dir(name: &str) -> std::io::Result<std::path::PathBuf> {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "csswitch-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p)?;
        p.canonicalize()
    }

    fn write_fake_bin(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
}
