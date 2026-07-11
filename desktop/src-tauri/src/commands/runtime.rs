use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::json;
use tauri::State;

use crate::runtime::capability_catalog::diagnostics_for_profile;
use crate::runtime::diagnostics::{
    build_status_response, proxy_status_last_error, science_diagnostics, status_lights,
    ScienceDiagnosticsInput, StatusProbeInput,
};
use crate::runtime::operation::{self, OperationKind, OperationTrace};
use crate::runtime::profile::profile_capabilities;
use crate::runtime::provider::{
    adapter_for_profile, current_shim_mode_for_adapter, gateway_kind_for_adapter,
    status_upstream_endpoint,
};
use crate::runtime::proxy_lifecycle::ensure_proxy;
use crate::runtime::science::{settings_change_needs_teardown, stop_sandbox};
use crate::runtime::settings::validate_runtime_ports;
use crate::runtime::system::open_in_browser;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

fn config_last_error_json(error: &dyn std::fmt::Display) -> serde_json::Value {
    json!({
        "type": "config_error",
        "message": error.to_string(),
    })
}

fn status_response_for_config_error(error: &dyn std::fmt::Display) -> serde_json::Value {
    build_status_response(
        status_lights(StatusProbeInput {
            proxy_ok: false,
            sandbox_ok: false,
            upstream_ok: false,
        }),
        serde_json::Value::Null,
        "",
        "off",
        diagnostics_for_profile(None, "off"),
        science_diagnostics(ScienceDiagnosticsInput {
            sandbox_port: 0,
            sandbox_ok: false,
        }),
        Some(config_last_error_json(error)),
    )
}

fn status_runtime_identity(
    adapter: &str,
    secret: &str,
    launched_gateway_kind: String,
    launched_shim_mode: String,
) -> (String, String, &'static str) {
    let current_shim_mode = current_shim_mode_for_adapter(adapter);
    let gateway_kind = if !launched_gateway_kind.is_empty() {
        launched_gateway_kind
    } else if !secret.is_empty() {
        String::new()
    } else {
        gateway_kind_for_adapter(adapter).to_string()
    };
    let runtime_shim_mode = if !launched_shim_mode.is_empty() {
        launched_shim_mode
    } else if !secret.is_empty() {
        String::new()
    } else {
        current_shim_mode.to_string()
    };
    (gateway_kind, runtime_shim_mode, current_shim_mode)
}

fn stop_sandbox_state(app: &tauri::AppHandle, st: &mut AppState) -> Result<(), String> {
    stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url)
}

/// 切换运行模式（"proxy" 第三方 / "official" 官方）。切官方要先拆第三方链路成功再落盘。
#[tauri::command]
pub(crate) async fn set_mode(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    mode: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_mode_inner(app, state, lifecycle, mode)).await
}

fn set_mode_inner(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    mode: String,
) -> Result<(), String> {
    if mode != "proxy" && mode != "official" {
        return Err(format!("未知模式：{mode}（只支持 proxy / official）。"));
    }
    // 经串行器（修 P1-b）：切官方的「拆链路 + 落盘」必须与「一键开始」等互斥，否则一键起到一半时
    // 切官方会先停链路、一键随后又把沙箱/OAuth 起起来 → 显示官方却有第三方沙箱在跑。bump_generation
    // 作废任何在途启动，防被停后又拿旧配置写回运行态。
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        if mode == "official" {
            lifecycle.bump_generation();
            let mut st = lock(&state);
            stop_sandbox_state(&app, &mut st).map_err(|e| {
                format!("停止沙箱失败，未切换到官方模式：{e}（真实实例 8765 未受影响）")
            })?;
            st.stop_proxy();
        }
        config::update(&dir, {
            let mode = mode.clone();
            move |c| c.mode = mode
        })
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// 官方模式：干净地打开用户【真实】的 Claude Science（不碰/复制真实凭证，抹掉 ANTHROPIC_*）。
#[tauri::command]
pub(crate) fn open_official() -> Result<(), String> {
    let app_path = "/Applications/Claude Science.app";
    let mut cmd = Command::new("open");
    if Path::new(app_path).is_dir() {
        cmd.arg(app_path);
    } else {
        cmd.arg("-a").arg("Claude Science");
    }
    cmd.env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("未能打开 Claude Science。请确认已安装官方 Claude Science。".into()),
        Err(e) => Err(format!("打开官方 Claude Science 失败：{e}")),
    }
}

#[derive(Deserialize)]
pub(crate) struct UiSettings {
    proxy_port: u16,
    sandbox_port: u16,
}

/// 端口设置（provider/连接改走 profile CRUD + set_active_profile）。
/// 经串行器（修 P1-c）：端口一旦变化，正在跑的代理绑在旧端口、正在跑的沙箱又烘死了旧代理 URL，
/// 与新端口不一致；此处把这条陈旧链路拆掉（只停我们的沙箱、绝不碰 8765），逼下次「一键开始」按新端口重建，
/// 杜绝「复用旧沙箱指向死端口、UI 却报沿用不变」。
#[tauri::command]
pub(crate) async fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    cfg: UiSettings,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_settings_inner(app, state, lifecycle, cfg)).await
}

fn set_settings_inner(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    cfg: UiSettings,
) -> Result<(), String> {
    validate_runtime_ports(cfg.proxy_port, cfg.sandbox_port)?;
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let old = config::load_from(&dir).map_err(|e| e.to_string())?;
        let teardown = settings_change_needs_teardown(
            old.proxy_port,
            cfg.proxy_port,
            old.sandbox_port,
            cfg.sandbox_port,
        );
        // 拆链路【先】于落盘，且停沙箱结果必须据实处理（修增量 P1）：停不掉就【不改端口】——
        // 否则会留下「config 已是新端口、旧沙箱仍在旧端口指向旧代理」的不一致态，下次一键还会复用这条死链路。
        // 保持端口不变则一切仍自洽（旧沙箱指旧代理端口、下次一键在旧端口重建代理，链路照通）。
        if teardown {
            let mut st = lock(&state);
            stop_sandbox_state(&app, &mut st).map_err(|e| {
                format!(
                    "端口未更改：无法停止指向旧端口的沙箱（{e}），为避免留下失效链路，端口保持不变。请手动停止沙箱或重启 app 后重试。（真实实例 8765 未受影响）"
                )
            })?;
            lifecycle.bump_generation(); // 停成功后作废在途启动
            st.stop_proxy();
        }
        // 拆链路成功（或无需拆）→ 才落盘新端口，保证 config 与运行态一致。
        config::update(&dir, move |c| {
            c.proxy_port = cfg.proxy_port;
            c.sandbox_port = cfg.sandbox_port;
        })
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub(crate) async fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || start_proxy_inner_cmd(app, state, lifecycle)).await
}

fn start_proxy_inner_cmd<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    // 经串行器：与切换/连接编辑/清 key/删/停等 ensure_proxy 竞争串行化，防陈旧读起旧配置代理
    // 又写回运行态（修 P1-a，比照 spec §8.1「ensure_proxy 都经一把 app 级 mutex」）。
    lifecycle.with_serialized(|| {
        let trace = OperationTrace::start(OperationKind::StartProxy, "command=start_proxy");
        let (port, _secret, _action) =
            ensure_proxy(&app, &state, lifecycle.as_ref(), Some(&trace))?;
        trace.finish(format!("ok port={port}"));
        Ok(json!({ "port": port }))
    })
}

#[derive(Deserialize)]
pub(crate) struct FetchModelsReq {
    /// 模板 id（决定 builtin / base_url 可编辑性 / 默认 base_url）。
    template_id: String,
    /// 编辑已存 profile 时的实际 api_format；为空则按模板默认值。
    #[serde(default)]
    api_format: Option<String>,
    /// 自定义模板时用户填的 base_url（不可编辑模板忽略）。
    #[serde(default)]
    base_url: String,
    /// 用户新填的 key；为空表示沿用 profile_id 已存的 key（后端不回传完整 key）。
    #[serde(default)]
    key: String,
    /// 编辑已存 profile 时传其 id（用于沿用已存 key）。
    #[serde(default)]
    profile_id: Option<String>,
}

/// 「获取可用模型」——纯 scratch 探测：只用临时代理探候选 base_url/key 的 /v1/models，
/// 绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。
#[tauri::command]
pub(crate) async fn fetch_models(
    app: tauri::AppHandle,
    req: FetchModelsReq,
) -> Result<serde_json::Value, String> {
    run_blocking(move || {
        crate::runtime::model_discovery::fetch_models(
            app,
            crate::runtime::model_discovery::ModelDiscoveryRequest {
                template_id: req.template_id,
                api_format: req.api_format,
                base_url: req.base_url,
                key: req.key,
                profile_id: req.profile_id,
            },
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn stop_all(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || stop_all_inner_cmd(app, state, lifecycle)).await
}

fn stop_all_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        lifecycle.bump_generation(); // 作废任何在途启动（防被停后又拿旧 key 复活）
        let mut st = lock(&state);
        let sandbox_res = stop_sandbox_state(&app, &mut st);
        st.stop_proxy();
        sandbox_res.map_err(|e| format!("代理已停；但{e}真实实例 8765 未受影响。"))
    })
}

#[tauri::command]
pub(crate) async fn one_click_login(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || one_click_login_cmd(app, state, lifecycle)).await
}

pub(crate) fn one_click_login_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        crate::runtime::sandbox_session::one_click_login(app, state, lifecycle.as_ref())
    })
}

#[tauri::command]
pub(crate) fn status(state: State<'_, SharedAppState>) -> serde_json::Value {
    // 只在锁内取值，锁外做短超时探活。这里是高频 UI 状态灯，
    // 不能反复调用外部 `claude-science status`，否则前端轮询会卡住主线程。
    // 沙箱强身份确认保留在 one_click_login 的启动/复用边界。
    let (
        pport,
        secret,
        sport,
        adapter,
        base_url,
        active_profile,
        catalog_profile,
        tracked_proxy_child_alive,
        launched_provider,
        launched_gateway_kind,
        launched_shim_mode,
        launched_launch_id,
    ) = {
        let mut st = lock(state.inner());
        let cfg = match config::load_from(&config::default_dir()) {
            Ok(cfg) => cfg,
            Err(e) => return status_response_for_config_error(&e),
        };
        let pport = if st.proxy_port != 0 {
            st.proxy_port
        } else {
            cfg.proxy_port
        };
        let sport = if st.sandbox_port != 0 {
            st.sandbox_port
        } else {
            cfg.sandbox_port
        };
        let tracked_proxy_child_alive = proc::tracked_child_is_running(&mut st.proxy);
        // 上游灯读生效 profile 的 adapter/base_url；无生效配置 → 空（灯显黄，不误探）。
        let (adapter, base_url, active_profile, catalog_profile) = match cfg.active_profile() {
            Some(p) => {
                let adapter = adapter_for_profile(p).to_string();
                (
                    adapter,
                    p.base_url.clone(),
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "template_id": p.template_id,
                        "api_format": p.api_format,
                        "model": p.model,
                        "capabilities": profile_capabilities(p),
                    }),
                    Some(p.clone()),
                )
            }
            None => (String::new(), String::new(), serde_json::Value::Null, None),
        };
        (
            pport,
            st.secret.clone(),
            sport,
            adapter,
            base_url,
            active_profile,
            catalog_profile,
            tracked_proxy_child_alive,
            st.provider.clone(),
            st.gateway_kind.clone(),
            st.shim_mode.clone(),
            st.launch_id.clone(),
        )
    };
    let diagnostic_override = std::env::var_os("CSSWITCH_UPSTREAM_URL");
    let upstream = status_upstream_endpoint(&adapter, &base_url, diagnostic_override.as_deref());
    let proxy_ok = tracked_proxy_child_alive
        && !secret.is_empty()
        && !launched_gateway_kind.is_empty()
        && !launched_provider.is_empty()
        && proc::http_health_gateway(
            pport,
            Some(&secret),
            operation::STATUS_HEALTH_TIMEOUT_MS,
            &launched_gateway_kind,
            Some(&launched_provider),
            Some(launched_shim_mode.as_str()),
            Some(launched_launch_id.as_str()),
        );
    let last_error = proxy_status_last_error(!secret.is_empty(), proxy_ok, pport);
    let sandbox_ok = proc::http_health(sport, None, operation::STATUS_HEALTH_TIMEOUT_MS);
    let upstream_ok = upstream
        .as_ref()
        .map(|e| proc::tcp_reachable(&e.host, e.port, operation::STATUS_UPSTREAM_TIMEOUT_MS))
        .unwrap_or(false);
    let lights = status_lights(StatusProbeInput {
        proxy_ok,
        sandbox_ok,
        upstream_ok,
    });
    let (gateway_kind, shim_mode, catalog_shim_mode) =
        status_runtime_identity(&adapter, &secret, launched_gateway_kind, launched_shim_mode);
    build_status_response(
        lights,
        active_profile,
        &gateway_kind,
        &shim_mode,
        diagnostics_for_profile(catalog_profile.as_ref(), catalog_shim_mode),
        science_diagnostics(ScienceDiagnosticsInput {
            sandbox_port: sport,
            sandbox_ok,
        }),
        last_error,
    )
}

#[tauri::command]
pub(crate) fn boot_error(state: State<'_, SharedAppState>) -> Option<String> {
    lock(state.inner()).boot_error.clone()
}

#[tauri::command]
pub(crate) fn open_url(state: State<'_, SharedAppState>) -> Result<(), String> {
    let url = { lock(state.inner()).sandbox_url.clone() };
    let url = url.ok_or("还没有沙箱 URL，请先「一键开始」。")?;
    open_in_browser(&url)
}

#[tauri::command]
pub(crate) fn quit_app(app: tauri::AppHandle) -> Result<(), String> {
    // 显式退出统一交给 RunEvent::ExitRequested 清理，避免多个退出入口各自停代理。
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        config_last_error_json, status_response_for_config_error, status_runtime_identity,
    };
    use crate::{
        config::{self, Config, Profile},
        lifecycle, lock,
        runtime::{sandbox_session, science},
        AppState, SharedAppState,
    };
    use std::{
        env, fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tauri::Manager;

    #[test]
    fn config_last_error_json_preserves_typed_config_error() {
        let err = config_last_error_json(&"bad config");
        assert_eq!(
            err.get("type").and_then(|v| v.as_str()),
            Some("config_error")
        );
        assert_eq!(
            err.get("message").and_then(|v| v.as_str()),
            Some("bad config")
        );
    }

    #[test]
    fn status_response_for_config_error_is_fail_closed() {
        let v = status_response_for_config_error(&"bad config");
        assert_eq!(v["proxy"], "amber");
        assert_eq!(v["sandbox"], "amber");
        assert_eq!(v["upstream"], "amber");
        assert_eq!(v["active_profile"], serde_json::Value::Null);
        assert_eq!(v["science"]["sandbox"]["port"], 0);
        assert_eq!(v["last_error"]["type"], "config_error");
        assert_eq!(v["last_error"]["message"], "bad config");
    }

    #[test]
    fn status_runtime_identity_prefers_launched_identity_and_fail_closes_partial_launch() {
        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "", String::new(), String::new());
        assert_eq!(gateway, "rust");
        assert_eq!(shim, "off");
        assert_eq!(catalog_shim, "off");

        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "secret-present", "rust".into(), "off".into());
        assert_eq!(gateway, "rust");
        assert_eq!(shim, "off");
        assert_eq!(catalog_shim, "off");

        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "secret-present", String::new(), String::new());
        assert_eq!(gateway, "");
        assert_eq!(shim, "");
        assert_eq!(catalog_shim, "off");
    }

    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self { saved: Vec::new() }
        }

        fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
            self.saved.push((key.to_string(), env::var_os(key)));
            env::set_var(key, value);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.iter().rev() {
                match value {
                    Some(v) => env::set_var(key, v),
                    None => env::remove_var(key),
                }
            }
        }
    }

    fn tmpdir(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("csswitch-{label}-{}-{now}", std::process::id()))
    }

    fn free_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8765);
        port
    }

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write_test_bins(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        write_executable(
            &dir.join("open"),
            r#"#!/bin/sh
if [ -n "${CSSWITCH_FAKE_OPEN_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$CSSWITCH_FAKE_OPEN_LOG"
fi
exit 0
"#,
        );
        write_executable(
            &dir.join("security"),
            r#"#!/bin/sh
exit 0
"#,
        );
        let science_bin = dir.join("claude-science");
        write_executable(
            &science_bin,
            r#"#!/bin/sh
set -eu
cmd="${1:-}"
if [ "$#" -gt 0 ]; then shift; fi
data_dir=""
port=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --data-dir) data_dir="$2"; shift 2 ;;
    --port) port="$2"; shift 2 ;;
    *) shift ;;
  esac
done
state="$data_dir/fake-science"
mkdir -p "$state"
case "$cmd" in
  serve)
    count="$(cat "$state/serve-count" 2>/dev/null || echo 0)"
    count=$((count + 1))
    printf '%s' "$count" > "$state/serve-count"
    printf '%s' "$port" > "$state/port"
    python3 - "$port" "$state/pid" >/dev/null 2>&1 <<'PY' &
import http.server
import os
import socketserver
import sys
port = int(sys.argv[1])
pidfile = sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        if self.path.startswith("/health"):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"status":"ok"}')
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"fake science")
with open(pidfile, "w", encoding="utf-8") as f:
    f.write(str(os.getpid()))
with socketserver.TCPServer(("127.0.0.1", port), Handler) as httpd:
    httpd.serve_forever()
PY
    exit 0
    ;;
  status)
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      echo '{"running":true}'
    else
      echo '{"running":false}'
      exit 1
    fi
    ;;
  url)
    p="$(cat "$state/port")"
    echo "http://127.0.0.1:$p"
    ;;
  stop)
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
    rm -f "$state/pid"
    echo "stopped"
    ;;
  *)
    echo "unsupported fake science command: $cmd" >&2
    exit 2
    ;;
esac
"#,
        );
        science_bin
    }

    fn start_mock_upstream() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8765);
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
            }
        });
        port
    }

    fn wait_http_health(port: u16) {
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("mock service on port {port} did not become reachable");
    }

    fn wait_http_unreachable(port: u16) {
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("mock service on port {port} remained reachable");
    }

    fn kill_tracked_proxy(state: &SharedAppState, proxy_port: u16) {
        let mut proxy_child = {
            let mut st = lock(state);
            assert_eq!(st.proxy_port, proxy_port);
            assert!(!st.secret.is_empty());
            st.proxy.take().expect("proxy child should be tracked")
        };
        let _ = proxy_child.kill();
        let _ = proxy_child.wait();
        wait_http_unreachable(proxy_port);
    }

    #[test]
    #[ignore = "explicit isolated runtime smoke; uses fake Science and local loopback ports"]
    fn isolated_one_click_reuse_status_smoke_with_fake_science() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let tmp = tmpdir("isolated-runtime-smoke");
        let home = tmp.join("home");
        let bin_dir = tmp.join("bin");
        fs::create_dir_all(&home).unwrap();
        let fake_science = write_test_bins(&bin_dir);
        let open_log = tmp.join("open.log");
        let mock_upstream_port = start_mock_upstream();
        let proxy_port = free_port();
        let sandbox_port = free_port();
        assert_ne!(proxy_port, sandbox_port);

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &root);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
        env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
        env_guard.set(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                bin_dir.to_string_lossy()
            ),
        );

        let fake_key = "csswitch-isolated-fake-key-never-log";
        let profile = Profile {
            id: "mock-relay".into(),
            name: "Mock Relay".into(),
            template_id: "custom".into(),
            category: "custom".into(),
            api_format: "anthropic".into(),
            base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
            api_key: fake_key.into(),
            model: "mock-model".into(),
            ..Default::default()
        };
        let cfg = Config {
            profiles: vec![profile],
            active_id: "mock-relay".into(),
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        let config_dir = config::default_dir();
        config::save_to(&config_dir, &cfg).unwrap();

        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let first =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("first one-click should start proxy and sandbox");
        assert_eq!(first["action"], "started");
        assert_eq!(first["url"], format!("http://127.0.0.1:{sandbox_port}"));
        wait_http_health(sandbox_port);
        let fake_state_dir = home
            .join(".csswitch")
            .join("sandbox")
            .join("home")
            .join(".claude-science")
            .join("fake-science");
        let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );

        let second =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("second one-click should reuse running sandbox");
        assert_eq!(second["action"], "reopened");
        assert_eq!(second["url"], format!("http://127.0.0.1:{sandbox_port}"));
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            first_pid
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );

        let status = super::status(app.state::<SharedAppState>());
        assert_eq!(status["proxy"], "green");
        assert_eq!(status["sandbox"], "green");
        assert_eq!(status["upstream"], "green");
        assert_eq!(status["active_profile"]["id"], "mock-relay");
        assert_eq!(status["science"]["sandbox"]["port"], sandbox_port);
        assert_eq!(status["science"]["schema_version"], 1);
        assert!(status["last_error"].is_null());

        let doctor = std::process::Command::new(root.join("scripts/doctor.sh"))
            .env("HOME", &home)
            .env("SCIENCE_BIN", &fake_science)
            .env("CSSWITCH_CONFIG", config_dir.join("config.json"))
            .env("CSSWITCH_PROXY_PORT", proxy_port.to_string())
            .env("CSSWITCH_SANDBOX_PORT", sandbox_port.to_string())
            .output()
            .expect("doctor should run");
        assert!(doctor.status.success());
        let doctor_out = String::from_utf8_lossy(&doctor.stdout);
        assert!(doctor_out.contains("真实 HOME 检查默认跳过"));
        assert!(!doctor_out.contains(&format!("{}/.claude-science", home.display())));

        let cfg_after = config::load_from(&config_dir).unwrap();
        let secret = cfg_after.secret;
        assert!(!secret.is_empty());
        let doctor_err = String::from_utf8_lossy(&doctor.stderr);
        assert!(!doctor_out.contains(fake_key));
        assert!(!doctor_out.contains(&secret));
        assert!(!doctor_err.contains(fake_key));
        assert!(!doctor_err.contains(&secret));
        assert!(!first.to_string().contains(fake_key));
        assert!(!first.to_string().contains(&secret));
        assert!(!second.to_string().contains(fake_key));
        assert!(!second.to_string().contains(&secret));
        let opened = fs::read_to_string(&open_log).unwrap_or_default();
        assert!(!opened.contains(fake_key));
        assert!(!opened.contains(&secret));
        for name in ["proxy.log", "sandbox.log", "operation.log"] {
            let body = fs::read_to_string(config_dir.join("logs").join(name))
                .unwrap_or_else(|e| panic!("expected {name} to exist: {e}"));
            assert!(!body.contains(fake_key), "{name} leaked fake key");
            assert!(!body.contains(&secret), "{name} leaked path secret");
        }

        {
            let mut st = lock(&state);
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *st;
            let _ = science::stop_sandbox(&handle, sandbox, sandbox_url);
            st.stop_proxy();
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[ignore = "explicit isolated recovery proof; uses fake Science and local loopback ports"]
    fn isolated_manual_actions_recover_dead_proxy_with_fake_science() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let tmp = tmpdir("isolated-recovery-proof");
        let home = tmp.join("home");
        let bin_dir = tmp.join("bin");
        fs::create_dir_all(&home).unwrap();
        let fake_science = write_test_bins(&bin_dir);
        let open_log = tmp.join("open.log");
        let mock_upstream_port = start_mock_upstream();
        let proxy_port = free_port();
        let sandbox_port = free_port();
        assert_ne!(proxy_port, sandbox_port);

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &root);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
        env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
        env_guard.set(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                bin_dir.to_string_lossy()
            ),
        );

        let fake_key = "csswitch-isolated-fake-key-never-log";
        let profile = Profile {
            id: "mock-relay".into(),
            name: "Mock Relay".into(),
            template_id: "custom".into(),
            category: "custom".into(),
            api_format: "anthropic".into(),
            base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
            api_key: fake_key.into(),
            model: "mock-model".into(),
            ..Default::default()
        };
        let cfg = Config {
            profiles: vec![profile],
            active_id: "mock-relay".into(),
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        let config_dir = config::default_dir();
        config::save_to(&config_dir, &cfg).unwrap();

        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let first =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("first one-click should start proxy and sandbox");
        assert_eq!(first["action"], "started");
        assert_eq!(first["url"], format!("http://127.0.0.1:{sandbox_port}"));
        wait_http_health(proxy_port);
        wait_http_health(sandbox_port);
        let fake_state_dir = home
            .join(".csswitch")
            .join("sandbox")
            .join("home")
            .join(".claude-science")
            .join("fake-science");
        let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();

        kill_tracked_proxy(&state, proxy_port);

        let down_status = super::status(app.state::<SharedAppState>());
        assert_eq!(down_status["proxy"], "amber");
        assert_eq!(down_status["sandbox"], "green");
        assert_eq!(down_status["last_error"]["type"], "proxy_unhealthy");
        assert_eq!(
            down_status["last_error"]["message"],
            "代理进程不可达或已退出，请点击「一键开始」或「启动代理」恢复。"
        );
        assert_eq!(down_status["last_error"]["port"], proxy_port);

        let start_proxy_recovered =
            super::start_proxy_inner_cmd(handle.clone(), state.clone(), lifecycle.clone())
                .expect("start_proxy should manually recover a dead proxy");
        assert_eq!(start_proxy_recovered["port"], proxy_port);
        wait_http_health(proxy_port);

        let start_proxy_status = super::status(app.state::<SharedAppState>());
        assert_eq!(start_proxy_status["proxy"], "green");
        assert_eq!(start_proxy_status["sandbox"], "green");
        assert_eq!(start_proxy_status["upstream"], "green");
        assert!(start_proxy_status["last_error"].is_null());

        kill_tracked_proxy(&state, proxy_port);
        let down_again_status = super::status(app.state::<SharedAppState>());
        assert_eq!(down_again_status["proxy"], "amber");
        assert_eq!(down_again_status["sandbox"], "green");
        assert_eq!(down_again_status["last_error"]["type"], "proxy_unhealthy");

        let recovered =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("one-click should manually recover a dead proxy");
        assert_eq!(recovered["action"], "reopened");
        assert_eq!(
            recovered["msg"],
            "已用新配置重启代理，Science 沿用不变，已重新打开 Science。"
        );
        assert_eq!(recovered["url"], format!("http://127.0.0.1:{sandbox_port}"));
        wait_http_health(proxy_port);
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            first_pid
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );

        let recovered_status = super::status(app.state::<SharedAppState>());
        assert_eq!(recovered_status["proxy"], "green");
        assert_eq!(recovered_status["sandbox"], "green");
        assert_eq!(recovered_status["upstream"], "green");
        assert!(recovered_status["last_error"].is_null());

        let cfg_after = config::load_from(&config_dir).unwrap();
        let secret = cfg_after.secret;
        assert!(!secret.is_empty());
        assert!(!down_status.to_string().contains(fake_key));
        assert!(!down_status.to_string().contains(&secret));
        assert!(!recovered.to_string().contains(fake_key));
        assert!(!recovered.to_string().contains(&secret));
        assert!(!recovered_status.to_string().contains(fake_key));
        assert!(!recovered_status.to_string().contains(&secret));
        for name in ["proxy.log", "sandbox.log", "operation.log"] {
            let body = fs::read_to_string(config_dir.join("logs").join(name))
                .unwrap_or_else(|e| panic!("expected {name} to exist: {e}"));
            assert!(!body.contains(fake_key), "{name} leaked fake key");
            assert!(!body.contains(&secret), "{name} leaked path secret");
        }

        {
            let mut st = lock(&state);
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *st;
            let _ = science::stop_sandbox(&handle, sandbox, sandbox_url);
            st.stop_proxy();
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
