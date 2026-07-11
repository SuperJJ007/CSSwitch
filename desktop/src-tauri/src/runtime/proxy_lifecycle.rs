use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tauri::{Manager, Runtime};

use crate::runtime::legacy_proxy::{stop_legacy_csswitch_python_on_port, LegacyProxyCleanup};
use crate::runtime::operation::{self, OperationStage, OperationTrace, POLL_INTERVAL_MS};
use crate::runtime::provider::{
    assert_format_supported, current_shim_mode_for_adapter, is_native_adapter, is_openai_adapter,
    normalize_shim_mode, proxy_args_for, proxy_fingerprint_with_runtime, ProxyLaunch,
};
use crate::runtime::proxy::{health_timeout_reason, should_write_back, ProxyAction};
use crate::runtime::system::{asset_root, log_path, open_log, redact, repo_root, tail_file};
use crate::{config, lifecycle, lock, proc, SharedAppState};

fn formal_proxy_env(launch: &ProxyLaunch) -> Vec<(&'static str, String)> {
    let native = is_native_adapter(&launch.adapter);
    let mut env = vec![(launch.key_env, launch.key.clone())];
    if !native {
        if is_openai_adapter(&launch.adapter) {
            env.push(("CSSWITCH_OPENAI_BASE_URL", launch.base_url.clone()));
            if !launch.model.is_empty() {
                env.push(("CSSWITCH_OPENAI_MODEL", launch.model.clone()));
            }
        } else {
            env.push(("CSSWITCH_RELAY_BASE_URL", launch.base_url.clone()));
            if !launch.model.is_empty() {
                env.push(("CSSWITCH_RELAY_MODEL", launch.model.clone()));
            }
            if !launch.thinking_policy.is_empty() {
                env.push((
                    "CSSWITCH_RELAY_THINKING",
                    launch.thinking_policy.to_string(),
                ));
            }
        }
    }
    env
}

pub(crate) fn configure_managed_proxy_command(
    cmd: &mut Command,
    provider: &str,
    shim_mode: &str,
    port: u16,
    secret: &str,
    launch_id: &str,
) {
    let shim_mode = normalize_shim_mode(provider, Some(shim_mode));
    cmd.arg("--provider")
        .arg(provider)
        .arg("--port")
        .arg(port.to_string())
        .env("CSSWITCH_AUTH_TOKEN", secret)
        .env("CSSWITCH_LAUNCH_ID", launch_id)
        .env("CSSWITCH_TOOLUSE_SHIM", shim_mode);
    // CSSWITCH_UPSTREAM_URL is a native-provider test/diagnostic override. A stale
    // value inherited from the desktop process must never replace a candidate relay
    // or custom OpenAI base URL (or receive that candidate's key). Apply this at the
    // Shared command boundary keeps formal and scratch Rust launches aligned.
    if !is_native_adapter(provider) {
        cmd.env_remove("CSSWITCH_UPSTREAM_URL");
    }
}

fn find_gateway_in(dir: &Path) -> Option<PathBuf> {
    let exact = dir.join(if cfg!(windows) {
        "csswitch-gateway.exe"
    } else {
        "csswitch-gateway"
    });
    if exact.is_file() {
        return Some(exact);
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let matches = if cfg!(windows) {
            name.starts_with("csswitch-gateway-") && name.ends_with(".exe")
        } else {
            name.starts_with("csswitch-gateway-")
        };
        if matches && path.is_file() {
            return Some(path);
        }
    }
    None
}

pub(crate) fn gateway_bin_path<R: Runtime>(app: &tauri::AppHandle<R>) -> Option<PathBuf> {
    gateway_bin_path_from(
        std::env::var_os("CSSWITCH_GATEWAY_BIN").map(PathBuf::from),
        std::env::current_exe().ok(),
        app.path().resource_dir().ok(),
        repo_root(),
    )
}

fn gateway_bin_path_from(
    env_bin: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    resource_dir: Option<PathBuf>,
    repo_root: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = env_bin {
        return explicit_gateway_bin_is_safe(&path).then_some(path);
    }
    if let Some(exe) = current_exe {
        if let Some(dir) = exe.parent().and_then(find_gateway_in) {
            return Some(dir);
        }
    }
    if let Some(res) = resource_dir {
        if let Some(path) = find_gateway_in(&res) {
            return Some(path);
        }
        if let Some(path) = find_gateway_in(&res.join("binaries")) {
            return Some(path);
        }
    }
    if let Some(root) = repo_root {
        for dir in [
            root.join("desktop/gateway/target/release"),
            root.join("desktop/gateway/target/debug"),
            root.join("desktop/src-tauri/binaries"),
        ] {
            if let Some(path) = find_gateway_in(&dir) {
                return Some(path);
            }
        }
    }
    None
}

fn explicit_gateway_bin_is_safe(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

/// Ensure the active profile's proxy is running and healthy.
pub(crate) fn ensure_proxy<R: Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    trace: Option<&OperationTrace>,
) -> Result<(u16, String, ProxyAction), String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    let profile = cfg
        .active_profile()
        .cloned()
        .ok_or("未配置生效 profile，请先在面板选择或新建一条配置。")?;
    start_proxy_for(app, state, lifecycle, &profile, trace)
}

/// Start or reuse a proxy for a specific profile, without reading the active profile.
///
/// This function does not take the command serializer lock; callers own that boundary.
pub(crate) fn start_proxy_for<R: Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    profile: &config::Profile,
    trace: Option<&OperationTrace>,
) -> Result<(u16, String, ProxyAction), String> {
    assert_format_supported(profile)?;
    let launch = proxy_args_for(profile);
    if launch.key.is_empty() {
        return Err(format!(
            "「{}」还没填 API key，请先在面板填写并保存。",
            profile.name
        ));
    }
    let native = is_native_adapter(&launch.adapter);
    if !native && launch.base_url.is_empty() {
        return Err(
            "该配置需要填 base_url（如 https://your-relay/claude），请先在面板填写并保存。".into(),
        );
    }

    let shim_mode = current_shim_mode_for_adapter(&launch.adapter);
    let gateway_kind = "rust";
    let key_fp = proxy_fingerprint_with_runtime(profile, &launch, gateway_kind, shim_mode);
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let port = cfg.proxy_port;

    let secret = if !cfg.secret.is_empty() {
        cfg.secret.clone()
    } else {
        let s = proc::gen_secret().map_err(|e| format!("无法生成安全 secret：{e}"))?;
        let s2 = s.clone();
        config::update(&dir, move |c| c.secret = s2).map_err(|e| e.to_string())?;
        s
    };

    let gen = lifecycle.current_generation();

    let (mut child, launch_id) = {
        let mut st = lock(state);
        let tracked_child_running = proc::tracked_child_is_running(&mut st.proxy);
        if tracked_child_running
            && st.proxy_port == port
            && st.provider == launch.adapter
            && st.gateway_kind == gateway_kind
            && st.shim_mode == shim_mode
            && st.key_fp == key_fp
            && proc::http_health_gateway(
                port,
                Some(&st.secret),
                operation::PROXY_REUSE_HEALTH_TIMEOUT_MS,
                gateway_kind,
                Some(&launch.adapter),
                Some(st.shim_mode.as_str()),
                Some(st.launch_id.as_str()),
            )
        {
            if let Some(t) = trace {
                t.stage(
                    OperationStage::ProxyHealth,
                    format!(
                        "reused port={port} adapter={} gateway={gateway_kind}",
                        launch.adapter
                    ),
                );
            }
            return Ok((port, st.secret.clone(), ProxyAction::Reused));
        }

        st.stop_proxy();
        if proc::loopback_port_in_use(port, operation::LOCAL_HEALTH_TIMEOUT_MS) {
            let legacy_script = asset_root(app).map(|root| root.join("proxy/csswitch_proxy.py"));
            let cleanup = legacy_script
                .as_deref()
                .map(|script| stop_legacy_csswitch_python_on_port(port, script))
                .unwrap_or(LegacyProxyCleanup::NotLegacy);
            match cleanup {
                LegacyProxyCleanup::Stopped(pid) => {
                    if let Some(t) = trace {
                        t.stage(
                            OperationStage::ProxySpawn,
                            format!("stopped legacy CSSwitch Python proxy pid={pid} port={port}"),
                        );
                    }
                }
                LegacyProxyCleanup::StopFailed(pid) => {
                    return Err(format!(
                        "已确认端口 {port} 由旧版 CSSwitch Python proxy（PID {pid}）占用，但安全停止失败。请退出旧版或重启电脑后重试；未发送鉴权信息，也未强制结束进程。"
                    ));
                }
                LegacyProxyCleanup::NotLegacy => {
                    return Err(format!(
                        "端口 {port} 已被未知或旧 listener 占用；为避免发送鉴权信息或接管/结束非本轮进程，已拒绝启动。请手工确认后改用空闲端口。"
                    ));
                }
            }
            if proc::loopback_port_in_use(port, operation::LOCAL_HEALTH_TIMEOUT_MS) {
                return Err(format!(
                    "旧版 CSSwitch proxy 已停止，但端口 {port} 随即被其它 listener 占用；未发送鉴权信息，也未结束新占用者。请改用空闲端口。"
                ));
            }
        }
        st.secret = secret.clone();

        let logf = open_log("proxy.log").map_err(|e| format!("建日志失败：{e}"))?;
        let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
        if let Some(t) = trace {
            t.stage(
                OperationStage::ProxySpawn,
                format!(
                    "port={port} adapter={} gateway={gateway_kind}",
                    launch.adapter
                ),
            );
        }
        let launch_id =
            proc::gen_secret().map_err(|e| format!("无法生成 gateway launch_id：{e}"))?;
        let bin = gateway_bin_path(app)
            .ok_or("找不到 csswitch-gateway 二进制；请重新安装完整应用，开发态可设置绝对 CSSWITCH_GATEWAY_BIN。")?;
        let mut cmd = Command::new(bin);
        configure_managed_proxy_command(
            &mut cmd,
            &launch.adapter,
            shim_mode,
            port,
            &secret,
            &launch_id,
        );
        for (k, v) in formal_proxy_env(&launch) {
            cmd.env(k, v);
        }
        let child = cmd
            .stdout(Stdio::from(logf))
            .stderr(Stdio::from(logf2))
            .spawn()
            .map_err(|e| format!("启动代理失败：{e}"))?;
        (child, launch_id)
    };

    let mut ok = false;
    let mut early_exit = None;
    for _ in 0..(operation::PROXY_HEALTH_BUDGET_MS / POLL_INTERVAL_MS) {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        match proc::poll_child_liveness(&mut child) {
            proc::ChildLiveness::Exited(status) => {
                early_exit = Some(format!(
                    "新启动的 {gateway_kind} gateway 提前退出（{status}）"
                ));
                break;
            }
            proc::ChildLiveness::Running => {}
            proc::ChildLiveness::Unknown(error) => {
                early_exit = Some(format!(
                    "无法确认新启动的 {gateway_kind} gateway 是否存活：{error}"
                ));
                break;
            }
        }
        if proc::http_health_gateway(
            port,
            Some(&secret),
            operation::LOCAL_HEALTH_TIMEOUT_MS,
            gateway_kind,
            Some(&launch.adapter),
            Some(shim_mode),
            Some(&launch_id),
        ) {
            ok = true;
            break;
        }
    }
    if let Some(t) = trace {
        t.stage(
            OperationStage::ProxyHealth,
            if ok { "ready" } else { "not_ready" },
        );
    }
    if !ok {
        let _ = child.kill();
        let _ = child.wait();
        let tail = redact(&tail_file(&log_path("proxy.log"), 500), &secret);
        // Never authenticate to an unowned listener during failure diagnosis.
        // A bare TCP connect carries no path secret and is enough to report the
        // occupied-port class while leaving the unknown process untouched.
        let listener = if proc::loopback_port_in_use(port, operation::LOCAL_HEALTH_TIMEOUT_MS) {
            format!("端口 {port} 仍有未知或旧 listener；未发送鉴权信息、未接管且未结束该进程。")
        } else {
            String::new()
        };
        let primary = early_exit.unwrap_or_else(|| health_timeout_reason(port, &tail));
        let mut details = vec![primary];
        if !listener.is_empty() {
            details.push(listener);
        }
        if !tail.is_empty() {
            details.push(tail);
        }
        return Err(details.join("\n"));
    }

    {
        let mut st = lock(state);
        if !should_write_back(gen, lifecycle.current_generation(), &st.secret, &secret) {
            let mut c = child;
            let _ = c.kill();
            let _ = c.wait();
            return Err("代理启动期间配置已变更（被更晚的操作取代），本次启动未生效。".into());
        }
        if let Err(error) = proc::require_child_running(
            &mut child,
            &format!("新启动的 {gateway_kind} gateway 在发布 AppState 前"),
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        st.proxy = Some(child);
        st.proxy_port = port;
        st.secret = secret.clone();
        st.provider = launch.adapter.clone();
        st.gateway_kind = gateway_kind.to_string();
        st.shim_mode = shim_mode.to_string();
        st.launch_id = launch_id;
        st.key_fp = key_fp;
    }
    Ok((port, secret, ProxyAction::Restarted))
}

#[cfg(test)]
mod tests {
    use super::{
        configure_managed_proxy_command, find_gateway_in, formal_proxy_env, gateway_bin_path_from,
    };
    use crate::runtime::provider::ProxyLaunch;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn launch(adapter: &str, model: &str) -> ProxyLaunch {
        launch_with_thinking(adapter, model, "")
    }

    fn launch_with_thinking(
        adapter: &str,
        model: &str,
        thinking_policy: &'static str,
    ) -> ProxyLaunch {
        ProxyLaunch {
            adapter: adapter.to_string(),
            base_url: "https://upstream.example/api".to_string(),
            model: model.to_string(),
            key: "test-key".to_string(),
            key_env: if matches!(adapter, "openai-custom" | "openai-responses") {
                "CSSWITCH_OPENAI_KEY"
            } else {
                "CSSWITCH_RELAY_KEY"
            },
            thinking_policy,
        }
    }

    #[test]
    fn formal_proxy_env_pins_relay_model_only_on_formal_launch() {
        let env = formal_proxy_env(&launch("relay", "glm-5.2"));
        assert!(env.contains(&(
            "CSSWITCH_RELAY_BASE_URL",
            "https://upstream.example/api".to_string()
        )));
        assert!(env.contains(&("CSSWITCH_RELAY_MODEL", "glm-5.2".to_string())));
    }

    #[test]
    fn managed_proxy_command_keeps_secret_out_of_argv_and_injects_canonical_shim() {
        let fake_secret = "fake-managed-secret";
        for (provider, raw_shim, expected_shim, removes_upstream) in [
            ("deepseek", " Rewrite ", "rewrite", false),
            ("deepseek", "DETECT", "detect", false),
            ("qwen", " Rewrite ", "off", false),
            ("qwen", "off", "off", false),
            ("openai-custom", "DETECT", "off", true),
            ("openai-custom", "off", "off", true),
            ("openai-responses", "rewrite", "off", true),
            ("openai-responses", "off", "off", true),
            ("relay", "rewrite", "off", true),
            ("relay", "off", "off", true),
        ] {
            let mut cmd = Command::new("csswitch-gateway");
            configure_managed_proxy_command(
                &mut cmd,
                provider,
                raw_shim,
                18991,
                fake_secret,
                "fake-launch-id",
            );
            let args: Vec<String> = cmd
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(!args.iter().any(|arg| arg == "--auth-token"));
            assert!(!args.iter().any(|arg| arg == fake_secret));
            assert!(cmd.get_envs().any(|(key, value)| {
                key == "CSSWITCH_AUTH_TOKEN"
                    && value
                        .map(|value| value.to_string_lossy() == fake_secret)
                        .unwrap_or(false)
            }));
            assert!(cmd.get_envs().any(|(key, value)| {
                key == "CSSWITCH_TOOLUSE_SHIM"
                    && value
                        .map(|value| value.to_string_lossy() == expected_shim)
                        .unwrap_or(false)
            }));
            assert!(cmd.get_envs().any(|(key, value)| {
                key == "CSSWITCH_LAUNCH_ID"
                    && value
                        .map(|value| value.to_string_lossy() == "fake-launch-id")
                        .unwrap_or(false)
            }));
            let upstream_override = cmd
                .get_envs()
                .find(|(key, _)| *key == "CSSWITCH_UPSTREAM_URL")
                .map(|(_, value)| value);
            if removes_upstream {
                assert_eq!(
                    upstream_override,
                    Some(None),
                    "{provider} must remove inherited CSSWITCH_UPSTREAM_URL"
                );
            } else {
                assert_eq!(
                    upstream_override, None,
                    "native provider {provider} must preserve the explicit override contract"
                );
            }
        }
    }

    #[test]
    fn formal_proxy_env_pins_openai_model_only_on_formal_launch() {
        let env = formal_proxy_env(&launch("openai-custom", "gpt-5.2"));
        assert!(env.contains(&(
            "CSSWITCH_OPENAI_BASE_URL",
            "https://upstream.example/api".to_string()
        )));
        assert!(env.contains(&("CSSWITCH_OPENAI_MODEL", "gpt-5.2".to_string())));
    }

    #[test]
    fn formal_proxy_env_native_adapter_only_sets_native_key() {
        let mut native = launch("deepseek", "");
        native.key_env = "DEEPSEEK_API_KEY";
        let env = formal_proxy_env(&native);
        assert_eq!(env, vec![("DEEPSEEK_API_KEY", "test-key".to_string())]);
    }

    #[test]
    fn formal_proxy_env_empty_model_does_not_pin_model() {
        let env = formal_proxy_env(&launch("relay", ""));
        assert!(env.iter().any(|(k, _)| *k == "CSSWITCH_RELAY_BASE_URL"));
        assert!(!env.iter().any(|(k, _)| *k == "CSSWITCH_RELAY_MODEL"));
        assert!(!env.iter().any(|(k, _)| *k == "CSSWITCH_OPENAI_MODEL"));
    }

    #[test]
    fn formal_proxy_env_preserves_relay_thinking_policy() {
        let env = formal_proxy_env(&launch_with_thinking("relay", "glm-5.2", "enabled"));
        assert!(env.contains(&("CSSWITCH_RELAY_THINKING", "enabled".to_string())));
    }

    #[test]
    fn find_gateway_in_accepts_plain_or_tauri_suffixed_binary() {
        let dir = temp_dir("find-test");
        fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) {
            "csswitch-gateway-aarch64-pc-windows-msvc.exe"
        } else {
            "csswitch-gateway-aarch64-apple-darwin"
        };
        let path = dir.join(name);
        fs::write(&path, b"bin").unwrap();
        assert_eq!(find_gateway_in(&dir), Some(path.clone()));
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(dir);
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "csswitch-gateway-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn sidecar_name() -> &'static str {
        if cfg!(windows) {
            "csswitch-gateway-aarch64-pc-windows-msvc.exe"
        } else {
            "csswitch-gateway-aarch64-apple-darwin"
        }
    }

    fn write_marker(path: &std::path::Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"bin").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn gateway_lookup_prefers_explicit_env_binary() {
        let dir = temp_dir("env-override");
        let env_bin = dir.join("custom-gateway");
        write_marker(&env_bin);
        let canonical_env_bin = env_bin.canonicalize().unwrap();
        let found = gateway_bin_path_from(Some(canonical_env_bin.clone()), None, None, None);
        assert_eq!(found, Some(canonical_env_bin));
        let _ = fs::remove_file(env_bin);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_explicit_gateway_binary_fails_closed_without_fallback() {
        let dir = temp_dir("invalid-env-override");
        let fallback = dir.join(sidecar_name());
        write_marker(&fallback);
        assert_eq!(
            gateway_bin_path_from(
                Some(std::path::PathBuf::from("relative-gateway")),
                None,
                Some(dir.clone()),
                None,
            ),
            None
        );
        assert_eq!(
            gateway_bin_path_from(
                Some(dir.join("missing-gateway")),
                None,
                Some(dir.clone()),
                None,
            ),
            None
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_gateway_binary_rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink-env-override");
        let real_dir = dir.join("real");
        let real_bin = real_dir.join("gateway");
        write_marker(&real_bin);
        let linked_dir = dir.join("linked");
        symlink(&real_dir, &linked_dir).unwrap();
        assert_eq!(
            gateway_bin_path_from(Some(linked_dir.join("gateway")), None, None, None),
            None
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gateway_lookup_finds_packaged_resource_sidecar_layouts() {
        let dir = temp_dir("packaged-resource");
        let direct = dir.join(sidecar_name());
        write_marker(&direct);
        assert_eq!(
            gateway_bin_path_from(None, None, Some(dir.clone()), None),
            Some(direct.clone())
        );
        fs::remove_file(&direct).unwrap();

        let nested = dir.join("binaries").join(sidecar_name());
        write_marker(&nested);
        assert_eq!(
            gateway_bin_path_from(None, None, Some(dir.clone()), None),
            Some(nested.clone())
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gateway_lookup_finds_dev_repo_and_staged_sidecar_layouts() {
        let root = temp_dir("dev-repo");
        let debug = root
            .join("desktop/gateway/target/debug")
            .join(if cfg!(windows) {
                "csswitch-gateway.exe"
            } else {
                "csswitch-gateway"
            });
        write_marker(&debug);
        assert_eq!(
            gateway_bin_path_from(None, None, None, Some(root.clone())),
            Some(debug.clone())
        );
        fs::remove_file(&debug).unwrap();

        let staged = root.join("desktop/src-tauri/binaries").join(sidecar_name());
        write_marker(&staged);
        assert_eq!(
            gateway_bin_path_from(None, None, None, Some(root.clone())),
            Some(staged.clone())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_rs_stages_executable_sidecar_for_tauri_external_bin() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let staged_dir = manifest_dir.join("binaries");
        let staged = find_gateway_in(&staged_dir)
            .unwrap_or_else(|| panic!("missing staged sidecar in {}", staged_dir.display()));
        let name = staged.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(name.starts_with("csswitch-gateway-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&staged).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "{} is not executable", staged.display());
        }
    }
}
