use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::json;
use tauri::State;

use crate::runtime::diagnostics::{build_status_response, status_lights, StatusProbeInput};
use crate::runtime::operation::{self, OperationKind, OperationStage, OperationTrace};
use crate::runtime::profile::profile_capabilities;
use crate::runtime::provider::{
    adapter_for_profile, current_shim_mode_for_adapter, gateway_kind_for_adapter, upstream_host,
};
use crate::runtime::proxy_lifecycle::ensure_proxy;
use crate::runtime::science::{settings_change_needs_teardown, stop_sandbox};
use crate::runtime::settings::validate_runtime_ports;
use crate::runtime::system::open_in_browser;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

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

fn start_proxy_inner_cmd(
    app: tauri::AppHandle,
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

/// 「存 key 即验证」：确保代理在跑，再经代理向上游发一个最小请求，据状态码判断 key 是否可用。
#[tauri::command]
pub(crate) async fn verify_key(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || verify_key_inner_cmd(app, state, lifecycle)).await
}

fn verify_key_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    // 经串行器（修 P1-a）：ensure_proxy 与其它生命周期操作不并发交叠。
    lifecycle.with_serialized(|| {
        let trace = OperationTrace::start(OperationKind::VerifyKey, "command=verify_key");
        let (port, secret, _action) =
            ensure_proxy(&app, &state, lifecycle.as_ref(), Some(&trace))?;
        let body = br#"{"model":"claude-opus-4-8","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
        trace.stage(OperationStage::UpstreamProbe, "POST /v1/messages via active proxy");
        match proc::http_post_status(
            port,
            Some(&secret),
            "/v1/messages",
            body,
            operation::VERIFY_KEY_TIMEOUT_MS,
        ) {
            Some(200) => {
                trace.finish("ok status=200");
                Ok(json!({ "ok": true, "hint": "key 有效，上游已接受。" }))
            }
            Some(code @ (401 | 403)) => {
                trace.finish(format!("rejected status={code}"));
                Ok(
                    json!({ "ok": false, "hint": format!("上游拒绝（{code}），key 可能无效或无权限。") }),
                )
            }
            Some(code) => {
                trace.finish(format!("upstream_status={code}"));
                Ok(json!({
                    "ok": false,
                    "hint": format!("上游返回 {code}，可能是 key 无效、额度不足或上游异常。")
                }))
            }
            None => {
                trace.finish("error=no_response");
                Err("验证请求无响应（多为网络或上游不通）。".to_string())
            }
        }
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

fn one_click_login_cmd(
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
    let (pport, secret, sport, adapter, base_url, active_profile) = {
        let st = lock(state.inner());
        let cfg = config::load_from(&config::default_dir()).unwrap_or_default();
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
        // 上游灯读生效 profile 的 adapter/base_url；无生效配置 → 空（灯显黄，不误探）。
        let (adapter, base_url, active_profile) = match cfg.active_profile() {
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
                )
            }
            None => (String::new(), String::new(), serde_json::Value::Null),
        };
        (
            pport,
            st.secret.clone(),
            sport,
            adapter,
            base_url,
            active_profile,
        )
    };
    let uhost = upstream_host(&adapter, &base_url);
    let lights = status_lights(StatusProbeInput {
        proxy_ok: !secret.is_empty()
            && proc::http_health(pport, Some(&secret), operation::STATUS_HEALTH_TIMEOUT_MS),
        sandbox_ok: proc::http_health(sport, None, operation::STATUS_HEALTH_TIMEOUT_MS),
        upstream_ok: !uhost.is_empty()
            && proc::tcp_reachable(&uhost, 443, operation::STATUS_UPSTREAM_TIMEOUT_MS),
    });
    build_status_response(
        lights,
        active_profile,
        gateway_kind_for_adapter(&adapter),
        current_shim_mode_for_adapter(&adapter),
    )
}

#[tauri::command]
pub(crate) fn open_url(state: State<'_, SharedAppState>) -> Result<(), String> {
    let url = { lock(state.inner()).sandbox_url.clone() };
    let url = url.ok_or("还没有沙箱 URL，请先「一键开始」。")?;
    open_in_browser(&url)
}

#[tauri::command]
pub(crate) fn quit_app(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
) -> Result<(), String> {
    // 默认：退 app 停代理、保留沙箱运行（spec §5.1）。
    {
        let mut st = lock(state.inner());
        st.stop_proxy();
    }
    app.exit(0);
    Ok(())
}
