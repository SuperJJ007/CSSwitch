use std::process::{Command, Stdio};
use std::time::Duration;

use tauri::Runtime;

use crate::runtime::operation::{self, OperationStage, OperationTrace, POLL_INTERVAL_MS};
use crate::runtime::provider::{
    assert_format_supported, is_native_adapter, is_openai_adapter, proxy_args_for,
    proxy_fingerprint, ProxyLaunch,
};
use crate::runtime::proxy::{ere_escape, health_timeout_reason, should_write_back, ProxyAction};
use crate::runtime::system::{asset_root, log_path, open_log, redact, tail_file};
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

    let key_fp = proxy_fingerprint(profile, &launch);
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let port = cfg.proxy_port;
    let root = asset_root(app)
        .ok_or("找不到代理脚本 proxy/csswitch_proxy.py（打包资源或仓库根均未命中）。开发态可设 CSSWITCH_REPO。")?;
    let py = proc::find_exe("python3")
        .ok_or("缺少依赖 python3（起翻译代理需要）。已查 PATH、常见目录与登录 shell 仍未找到；macOS 一般自带 /usr/bin/python3（装 Xcode 命令行工具：xcode-select --install）。")?;

    let secret = if !cfg.secret.is_empty() {
        cfg.secret.clone()
    } else {
        let s = proc::gen_secret().map_err(|e| format!("无法生成安全 secret：{e}"))?;
        let s2 = s.clone();
        config::update(&dir, move |c| c.secret = s2).map_err(|e| e.to_string())?;
        s
    };

    let gen = lifecycle.current_generation();

    let child = {
        let mut st = lock(state);
        if st.proxy.is_some()
            && st.proxy_port == port
            && st.provider == launch.adapter
            && st.key_fp == key_fp
            && proc::http_health(
                port,
                Some(&st.secret),
                operation::PROXY_REUSE_HEALTH_TIMEOUT_MS,
            )
        {
            if let Some(t) = trace {
                t.stage(
                    OperationStage::ProxyHealth,
                    format!("reused port={port} adapter={}", launch.adapter),
                );
            }
            return Ok((port, st.secret.clone(), ProxyAction::Reused));
        }

        st.stop_proxy();
        st.secret = secret.clone();
        let script = root.join("proxy/csswitch_proxy.py");
        let pat = format!("{}.*--port {port}", ere_escape(&script.to_string_lossy()));
        let _ = Command::new("pkill").arg("-f").arg(&pat).status();

        let logf = open_log("proxy.log").map_err(|e| format!("建日志失败：{e}"))?;
        let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&py);
        if let Some(t) = trace {
            t.stage(
                OperationStage::ProxySpawn,
                format!("port={port} adapter={}", launch.adapter),
            );
        }
        cmd.arg(&script)
            .arg("--provider")
            .arg(&launch.adapter)
            .arg("--port")
            .arg(port.to_string())
            .arg("--auth-token")
            .arg(&secret);
        for (k, v) in formal_proxy_env(&launch) {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::from(logf))
            .stderr(Stdio::from(logf2))
            .spawn()
            .map_err(|e| format!("启动代理失败：{e}"))?
    };

    let mut ok = false;
    for _ in 0..(operation::PROXY_HEALTH_BUDGET_MS / POLL_INTERVAL_MS) {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        if proc::http_health(port, Some(&secret), operation::LOCAL_HEALTH_TIMEOUT_MS) {
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
        let mut c = child;
        let _ = c.kill();
        let _ = c.wait();
        let tail = redact(&tail_file(&log_path("proxy.log"), 500), &secret);
        return Err(format!("{}\n{tail}", health_timeout_reason(port, &tail)));
    }

    {
        let mut st = lock(state);
        if !should_write_back(gen, lifecycle.current_generation(), &st.secret, &secret) {
            let mut c = child;
            let _ = c.kill();
            let _ = c.wait();
            return Err("代理启动期间配置已变更（被更晚的操作取代），本次启动未生效。".into());
        }
        st.proxy = Some(child);
        st.proxy_port = port;
        st.secret = secret.clone();
        st.provider = launch.adapter.clone();
        st.key_fp = key_fp;
    }
    Ok((port, secret, ProxyAction::Restarted))
}

#[cfg(test)]
mod tests {
    use super::formal_proxy_env;
    use crate::runtime::provider::ProxyLaunch;

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
}
