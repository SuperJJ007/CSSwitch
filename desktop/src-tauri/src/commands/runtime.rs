use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::State;

use crate::runtime::capability_catalog::diagnostics_for_profile;
use crate::runtime::diagnostics::{
    build_status_response, proxy_status_last_error, science_diagnostics, status_lights,
    ScienceDiagnosticsInput, StatusProbeInput,
};
use crate::runtime::operation::{self, OperationKind, OperationTrace};
use crate::runtime::profile::profile_capabilities;
use crate::runtime::provider::{
    current_shim_mode_for_adapter, gateway_kind_for_adapter, resolve_launch_plan,
    status_upstream_endpoint,
};
use crate::runtime::proxy_lifecycle::ensure_proxy;
use crate::runtime::science::{
    sandbox_listener_matches_runtime, sandbox_url, science_runtime_preflight as runtime_preflight,
    settings_change_needs_teardown, stop_sandbox, SCIENCE_DOWNLOAD_URL,
};
use crate::runtime::settings::{
    remove_managed_sandbox_ssh_stub, system_ssh_config_path, validate_runtime_ports,
};
use crate::runtime::ssh_bridge::{revoke_science_ssh_bridge, system_ssh_hosts};
use crate::runtime::system::open_in_browser;
use crate::{
    config, lock, proc, run_blocking, run_blocking_typed, AppState, SharedAppState, SharedLifecycle,
};

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
            upstream_applicable: true,
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

fn status_upstream_applicable(adapter: &str) -> bool {
    !adapter.is_empty() && adapter != "codex"
}

pub(crate) fn stop_sandbox_state<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    st: &mut AppState,
) -> Result<(), String> {
    let runtime = st.science_runtime.clone();
    let result = stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url, runtime.as_ref());
    if result.is_ok() {
        st.science_confirmed_stopped = runtime;
        st.science_runtime = None;
    }
    result
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
        {
            let mut app_state = lock(&state);
            app_state.history_recovery = None;
            app_state.boot_attention = None;
        }
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
    #[serde(default)]
    reuse_system_ssh: bool,
}

/// 运行设置（端口 + 系统 SSH 配置授权；provider/连接改走 profile CRUD + set_active_profile）。
/// 经串行器（修 P1-c）：端口或 SSH 授权一旦变化，正在跑的沙箱都必须拆掉，
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
    if cfg.reuse_system_ssh {
        system_ssh_config_path()?;
        system_ssh_hosts()?;
    }
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let old = config::load_from(&dir).map_err(|e| e.to_string())?;
        let teardown = settings_change_needs_teardown(
            old.proxy_port,
            cfg.proxy_port,
            old.sandbox_port,
            cfg.sandbox_port,
        ) || old.reuse_system_ssh != cfg.reuse_system_ssh;
        // 拆链路【先】于落盘，且停沙箱结果必须据实处理（修增量 P1）：停不掉就【不改端口】——
        // 否则会留下「config 已是新端口、旧沙箱仍在旧端口指向旧代理」的不一致态，下次一键还会复用这条死链路。
        // 保持端口不变则一切仍自洽（旧沙箱指旧代理端口、下次一键在旧端口重建代理，链路照通）。
        if teardown {
            let mut st = lock(&state);
            stop_sandbox_state(&app, &mut st).map_err(|e| {
                format!(
                    "设置未更改：无法停止仍使用旧端口或旧 SSH 授权的沙箱（{e}）。请手动停止沙箱或重启 app 后重试。（真实实例 8765 未受影响）"
                )
            })?;
            lifecycle.bump_generation(); // 停成功后作废在途启动
            st.stop_proxy();
        }
        if !cfg.reuse_system_ssh {
            revoke_science_ssh_bridge(&crate::runtime::science::sandbox_home())?;
            remove_managed_sandbox_ssh_stub(&crate::runtime::science::sandbox_home())?;
        }
        // 拆链路成功（或无需拆）→ 才落盘新端口，保证 config 与运行态一致。
        config::update(&dir, move |c| {
            c.proxy_port = cfg.proxy_port;
            c.sandbox_port = cfg.sandbox_port;
            c.reuse_system_ssh = cfg.reuse_system_ssh;
        })
        .map_err(|e| e.to_string())?;
        {
            let mut app_state = lock(&state);
            app_state.history_recovery = None;
            app_state.boot_attention = None;
        }
        Ok(())
    })
}

#[tauri::command]
pub(crate) async fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || start_proxy_inner_cmd(app, state, lifecycle)).await
}

fn start_proxy_inner_cmd<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    let active = cfg
        .active_profile()
        .ok_or("未配置生效 profile，请先在面板选择或新建一条配置。")?;
    let adapter = resolve_launch_plan(active)?.adapter;
    let prepared = crate::commands::codex::prepare_provider_auth(
        &app,
        &adapter,
        crate::commands::codex::CodexPreflightTarget::ActiveProfile,
    )?;
    // 经串行器：与切换/连接编辑/清 key/删/停等 ensure_proxy 竞争串行化，防陈旧读起旧配置代理
    // 又写回运行态（修 P1-a，比照 spec §8.1「ensure_proxy 都经一把 app 级 mutex」）。
    lifecycle.with_serialized(|| {
        if let Some(prepared) = prepared.as_ref() {
            prepared.verify_unchanged()?;
        }
        let trace = OperationTrace::start(OperationKind::StartProxy, "command=start_proxy");
        let (port, _secret, _action) = ensure_proxy(
            &app,
            &state,
            lifecycle.as_ref(),
            None,
            Some(&trace),
            prepared.as_ref().map(|prepared| prepared.proof()),
        )?;
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
    lifecycle: State<'_, SharedLifecycle>,
    req: FetchModelsReq,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(
        move || -> Result<_, crate::commands::codex::RuntimeCommandError> {
            let request = crate::runtime::model_discovery::ModelDiscoveryRequest {
                template_id: req.template_id,
                api_format: req.api_format,
                base_url: req.base_url,
                key: req.key,
                profile_id: req.profile_id,
            };
            let adapter = crate::runtime::model_discovery::request_adapter(&request)?;
            let target = request.profile_id.as_ref().map_or(
                crate::commands::codex::CodexPreflightTarget::NoProfile,
                |id| crate::commands::codex::CodexPreflightTarget::Profile(id.clone()),
            );
            let prepared = crate::commands::codex::prepare_provider_auth(&app, &adapter, target)?;
            lifecycle
                .with_serialized(|| -> Result<(), String> {
                    if let Some(prepared) = prepared.as_ref() {
                        prepared.verify_unchanged()?;
                    }
                    Ok(())
                })
                .map_err(crate::commands::codex::RuntimeCommandError::from)?;
            Ok(crate::runtime::model_discovery::fetch_models(
                app,
                request,
                prepared.as_ref().map(|prepared| prepared.proof()),
            )?)
        },
    )
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
    runtime_choice: Option<String>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || one_click_login_cmd(app, state, lifecycle, runtime_choice)).await
}

pub(crate) fn one_click_login_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    runtime_choice: Option<String>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let cfg = match config::load_from(&config::default_dir()) {
        Ok(cfg) => cfg,
        Err(error) => return Ok(one_click_failure_value(error.to_string())),
    };
    let active = match cfg.active_profile() {
        Some(active) => active,
        None => {
            return Ok(one_click_failure_value(
                "未配置生效 profile，请先在面板选择或新建一条配置。".into(),
            ))
        }
    };
    let adapter = match resolve_launch_plan(active) {
        Ok(plan) => plan.adapter,
        Err(message) => return Ok(one_click_failure_value(message)),
    };
    let prepared = match crate::commands::codex::prepare_provider_auth(
        &app,
        &adapter,
        crate::commands::codex::CodexPreflightTarget::ActiveProfile,
    ) {
        Ok(prepared) => prepared,
        Err(crate::commands::codex::RuntimeCommandError::Message(message)) => {
            return Ok(one_click_failure_value(message))
        }
        Err(auth @ crate::commands::codex::RuntimeCommandError::Auth(_)) => return Err(auth),
    };
    match lifecycle.with_serialized(|| -> Result<_, String> {
        if let Some(prepared) = prepared.as_ref() {
            prepared.verify_unchanged()?;
        }
        crate::runtime::proxy_lifecycle::recover_interrupted_gateway(&app, &state)?;
        crate::runtime::sandbox_session::one_click_login(
            app,
            state,
            lifecycle.as_ref(),
            runtime_choice.as_deref(),
            prepared.as_ref().map(|prepared| prepared.proof()),
        )
    }) {
        Ok(value) => Ok(value),
        Err(message) => Ok(one_click_failure_value(message)),
    }
}

#[tauri::command]
pub(crate) async fn restore_history_choice(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    reference: String,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        lifecycle.with_serialized(|| {
            let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
            if cfg.mode != "proxy" {
                return Err("当前已不是第三方模型模式，本次历史恢复选择已作废".into());
            }
            if cfg.runtime_transaction.is_some() {
                return Err("当前有新的运行事务尚未完成，已拒绝覆盖其历史身份".into());
            }
            let active_profile_id = cfg
                .active_profile()
                .map(|profile| profile.id.clone())
                .ok_or("当前生效配置已变化，本次历史恢复选择已作废")?;
            let (auth_dir, sandbox_root, candidate, expected_port) = {
                let app_state = lock(&state);
                let session = app_state
                    .history_recovery
                    .as_ref()
                    .ok_or("历史恢复选择已过期，请重新点击一键开始")?;
                if session.active_profile_id != active_profile_id
                    || session.sandbox_port != cfg.sandbox_port
                {
                    return Err("当前配置或端口已变化，本次历史恢复选择已作废".into());
                }
                let choice = session
                    .choices
                    .iter()
                    .find(|choice| choice.reference == reference)
                    .ok_or("历史恢复引用无效或已过期")?;
                (
                    session.auth_dir.clone(),
                    session.sandbox_root.clone(),
                    choice.candidate.clone(),
                    session.sandbox_port,
                )
            };

            // A user may discover after opening Science that A/B was the wrong
            // history. Keep the one-shot mapping in memory for this app session,
            // but stop only the exact managed runtime before changing credentials.
            {
                let mut app_state = lock(&state);
                if app_state.science_runtime.is_some() {
                    stop_sandbox_state(&app, &mut app_state)?;
                } else if proc::loopback_port_in_use(
                    expected_port,
                    operation::LOCAL_HEALTH_TIMEOUT_MS,
                ) {
                    return Err("Science 端口被未知进程占用，已拒绝改写历史身份".into());
                }
            }
            let current_cfg =
                config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
            if current_cfg.mode != "proxy"
                || current_cfg.sandbox_port != expected_port
                || current_cfg.runtime_transaction.is_some()
                || current_cfg
                    .active_profile()
                    .map(|profile| profile.id.as_str())
                    != Some(active_profile_id.as_str())
            {
                return Err("运行配置或事务在恢复前已变化，本次选择已作废".into());
            }
            let _ = crate::oauth_forge::restore_history_choice(
                &auth_dir,
                "virtual@localhost.invalid",
                &sandbox_root,
                &candidate,
            )?;
            // Consume every old reference after a successful selection. Fresh
            // references preserve the in-session "choose again" escape hatch
            // without making an invoke token replayable.
            let refreshed_choices = {
                let mut app_state = lock(&state);
                app_state.boot_attention = None;
                let session = app_state
                    .history_recovery
                    .as_mut()
                    .ok_or("历史恢复会话已过期")?;
                session
                    .choices
                    .iter_mut()
                    .enumerate()
                    .map(|(index, choice)| {
                        choice.reference = config::new_id();
                        let label = if index < 26 {
                            format!("历史记录 {}", (b'A' + index as u8) as char)
                        } else {
                            format!("历史记录 {}", index + 1)
                        };
                        json!({"reference": choice.reference, "label": label})
                    })
                    .collect::<Vec<_>>()
            };
            Ok(json!({
                "status": "ok",
                "action": "history_choice_restored",
                "message": "已恢复所选历史记录；其他历史记录未被删除。",
                "choices": refreshed_choices
            }))
        })
    })
    .await
}

fn one_click_failure_value(message: String) -> serde_json::Value {
    let recovery_status = config::load_from(&config::default_dir())
        .ok()
        .and_then(|cfg| cfg.runtime_transaction)
        .map(|_| "degraded")
        .unwrap_or("not_needed");
    let stage = science_failure_stage(&message);
    json!({
        "action": "failed",
        "stage": stage,
        "status": "error",
        "recovery_status": recovery_status,
        "message": message,
        "fallback_url": null,
    })
}

fn science_failure_stage(message: &str) -> &'static str {
    if message.contains("停止旧进程") || message.contains("停止沙箱") {
        "science_stop"
    } else if message.contains("模型目录")
        || message.contains("selector")
        || message.contains("Codex published model snapshot")
    {
        "catalog_verify"
    } else if message.contains("代理") || message.contains("gateway") {
        "gateway_start"
    } else if message.contains("沙箱") || message.contains("Science") {
        "science_start"
    } else {
        "prepare"
    }
}

#[tauri::command]
pub(crate) async fn science_runtime_preflight(
    state: State<'_, SharedAppState>,
) -> Result<Value, String> {
    let (version_cache, confirmed_stopped) = {
        let st = lock(state.inner());
        (
            st.science_version_cache.clone(),
            st.science_confirmed_stopped.clone(),
        )
    };
    run_blocking(move || runtime_preflight(&version_cache, confirmed_stopped.as_ref())).await
}

#[tauri::command]
pub(crate) fn open_science_download_page() -> Result<(), String> {
    open_in_browser(SCIENCE_DOWNLOAD_URL)
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
        active_contract_id,
        active_contract_digest,
        science_runtime,
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
        let (
            adapter,
            base_url,
            active_contract_id,
            active_contract_digest,
            active_profile,
            catalog_profile,
        ) = match cfg.active_profile() {
            Some(p) => {
                let (adapter, endpoint, contract_id, contract_digest) = resolve_launch_plan(p)
                    .map(|plan| {
                        (
                            plan.adapter,
                            plan.endpoint,
                            plan.contract_id,
                            plan.contract_digest,
                        )
                    })
                    .unwrap_or_else(|_| {
                        (
                            "unsupported".to_string(),
                            String::new(),
                            String::new(),
                            String::new(),
                        )
                    });
                (
                    adapter,
                    endpoint,
                    contract_id,
                    contract_digest,
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
            None => (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                serde_json::Value::Null,
                None,
            ),
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
            active_contract_id,
            active_contract_digest,
            st.science_runtime.clone(),
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
            proc::GatewayHealthExpectation {
                gateway: &launched_gateway_kind,
                provider: Some(&launched_provider),
                shim: Some(launched_shim_mode.as_str()),
                launch_id: Some(launched_launch_id.as_str()),
                provider_contract_id: Some(active_contract_id.as_str()),
                provider_contract_digest: Some(active_contract_digest.as_str()),
            },
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
        upstream_applicable: status_upstream_applicable(&adapter),
    });
    let (gateway_kind, shim_mode, catalog_shim_mode) =
        status_runtime_identity(&adapter, &secret, launched_gateway_kind, launched_shim_mode);
    let mut science = science_diagnostics(ScienceDiagnosticsInput {
        sandbox_port: sport,
        sandbox_ok,
    });
    if let (Some(object), Some(runtime)) = (science.as_object_mut(), science_runtime) {
        object.insert(
            "runtime".into(),
            json!({
                "source": runtime.source.code(),
                "version": runtime.version,
            }),
        );
    }
    build_status_response(
        lights,
        active_profile,
        &gateway_kind,
        &shim_mode,
        diagnostics_for_profile(catalog_profile.as_ref(), catalog_shim_mode),
        science,
        last_error,
    )
}

#[tauri::command]
pub(crate) fn boot_error(state: State<'_, SharedAppState>) -> Option<String> {
    lock(state.inner()).boot_error.clone()
}

#[tauri::command]
pub(crate) fn boot_attention(state: State<'_, SharedAppState>) -> Option<serde_json::Value> {
    lock(state.inner()).boot_attention.take()
}

fn manual_open_result(url: String, result: Result<(), String>) -> serde_json::Value {
    match result {
        Ok(()) => json!({
            "status": "ok",
            "message": "已向默认浏览器发出打开 Science 的请求。",
            "fallback_url": null,
        }),
        Err(error) => json!({
            "status": "error",
            "message": format!("打开浏览器失败：{error}"),
            "fallback_url": url,
        }),
    }
}

fn open_url_inner(state: &SharedAppState) -> Result<serde_json::Value, String> {
    let (sandbox_port, runtime) = {
        let st = lock(state);
        let runtime = st
            .science_runtime
            .clone()
            .ok_or("隔离 Science 尚未运行，请先「一键开始」。")?;
        (st.sandbox_port, runtime)
    };
    if sandbox_port == 0 || !sandbox_listener_matches_runtime(sandbox_port, &runtime) {
        return Err("隔离 Science 尚未就绪，请重新点击「一键开始」。".into());
    }
    // Science 的控制地址可能是短期、一次性的。每次手动打开都重新获取，
    // 不复用 one-click 已消费的内存 URL。成功时不返回 URL；只有系统
    // opener 失败时才把同一次新 URL 交给 UI，供用户复制或再次打开。
    let url = sandbox_url(sandbox_port, &runtime);
    Ok(manual_open_result(url.clone(), open_in_browser(&url)))
}

#[tauri::command]
pub(crate) async fn open_url(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || lifecycle.with_serialized(|| open_url_inner(&state))).await
}

#[tauri::command]
pub(crate) async fn quit_app(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    let exit_app = app.clone();
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || stop_all_inner_cmd(app, state, lifecycle)).await?;
    exit_app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        config_last_error_json, manual_open_result, science_failure_stage,
        status_response_for_config_error, status_runtime_identity, status_upstream_applicable,
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
        time::{Instant, SystemTime, UNIX_EPOCH},
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
    fn upstream_applicability_is_provider_semantics_not_endpoint_parse_success() {
        assert!(!status_upstream_applicable(""));
        assert!(!status_upstream_applicable("codex"));
        assert!(status_upstream_applicable("relay"));
        assert!(status_upstream_applicable("deepseek"));
    }

    #[test]
    fn status_runtime_identity_prefers_launched_identity_and_fail_closes_partial_launch() {
        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "", String::new(), String::new());
        assert_eq!(gateway, "rust");
        assert_eq!(shim, "rewrite");
        assert_eq!(catalog_shim, "rewrite");

        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "secret-present", "rust".into(), "off".into());
        assert_eq!(gateway, "rust");
        assert_eq!(shim, "off");
        assert_eq!(catalog_shim, "rewrite");

        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "secret-present", String::new(), String::new());
        assert_eq!(gateway, "");
        assert_eq!(shim, "");
        assert_eq!(catalog_shim, "rewrite");
    }

    #[test]
    fn science_operation_failures_have_stable_structured_stages() {
        assert_eq!(science_failure_stage("停止旧进程失败"), "science_stop");
        assert_eq!(science_failure_stage("代理探活失败"), "gateway_start");
        assert_eq!(science_failure_stage("模型目录不一致"), "catalog_verify");
        assert_eq!(
            science_failure_stage("gateway 模型目录探活无响应"),
            "catalog_verify"
        );
        assert_eq!(
            science_failure_stage("Codex published model snapshot 为空或包含非法 alias"),
            "catalog_verify"
        );
        assert_eq!(science_failure_stage("沙箱起后超时"), "science_start");
        assert_eq!(science_failure_stage("配置不可用"), "prepare");
    }

    #[test]
    fn manual_browser_failure_returns_the_same_fresh_url_for_copy_and_retry() {
        let url = "http://127.0.0.1:8990/?nonce=fresh".to_string();
        let failed = manual_open_result(url.clone(), Err("opener rejected".into()));
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["fallback_url"], url);
        assert!(failed["message"]
            .as_str()
            .unwrap()
            .contains("打开浏览器失败"));

        let opened = manual_open_result(url, Ok(()));
        assert_eq!(opened["status"], "ok");
        assert!(opened["fallback_url"].is_null());
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
        let path = env::temp_dir().join(format!("csswitch-{label}-{}-{now}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
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
if [ -n "${CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE:-}" ] && [ ! -e "$CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE" ]; then
  : > "$CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE"
  exit 1
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
if [ -n "${CSSWITCH_FAKE_SCIENCE_CALL_LOG:-}" ]; then
  printf '%s\n' "$cmd" >> "$CSSWITCH_FAKE_SCIENCE_CALL_LOG"
fi
if [ "$cmd" = "--version" ]; then
  echo "claude-science 0.0.0-csswitch-test"
  exit 0
fi
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
socketserver.TCPServer.allow_reuse_address = True
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
    count="$(cat "$state/url-count" 2>/dev/null || echo 0)"
    count=$((count + 1))
    printf '%s' "$count" > "$state/url-count"
    echo "http://127.0.0.1:$p/?nonce=$count"
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

    fn call_count(path: &Path, command: &str) -> usize {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == command)
            .count()
    }

    fn stop_test_sandbox<R: tauri::Runtime>(
        handle: &tauri::AppHandle<R>,
        state: &SharedAppState,
        sandbox_port: u16,
    ) {
        {
            let mut st = lock(state);
            let AppState {
                sandbox,
                sandbox_url,
                science_runtime,
                science_confirmed_stopped,
                ..
            } = &mut *st;
            let runtime = science_runtime.clone();
            assert!(science::stop_sandbox(handle, sandbox, sandbox_url, runtime.as_ref()).is_ok());
            *science_confirmed_stopped = runtime;
            *science_runtime = None;
        }
        wait_http_unreachable(sandbox_port);
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
        let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
        let open_log = tmp.join("open.log");
        let science_call_log = tmp.join("science-call.log");
        let route_config_log = tmp.join("route-config.log");
        let mock_upstream_port = start_mock_upstream();
        let proxy_port = free_port();
        let sandbox_port = free_port();
        assert_ne!(proxy_port, sandbox_port);

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &root);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
        env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
        env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
        env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
        env_guard.set("CSSWITCH_TEST_THIRD_PARTY_CONFIG_LOG", &route_config_log);
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
            model_catalog: vec![crate::model_catalog::ModelRoute {
                selector_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                display_name: "Mock model".into(),
                upstream_model: "mock-model".into(),
                supports_tools: Some(true),
                ..Default::default()
            }],
            default_model_route_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            role_bindings: crate::model_catalog::RoleBindings {
                sonnet: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                opus: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                haiku: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                fable: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                ..Default::default()
            },
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

        let first = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("first one-click should start proxy and sandbox");
        assert_eq!(first["action"], "started");
        assert!(
            first.get("url").is_none(),
            "one-time URL must stay backend-only"
        );
        wait_http_health(sandbox_port);
        let fake_state_dir = home
            .join(config::CONFIG_DIR_NAME)
            .join("sandbox")
            .join("home")
            .join(".claude-science")
            .join("fake-science");
        let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );
        assert_eq!(call_count(&science_call_log, "--version"), 1);
        assert_eq!(call_count(&science_call_log, "status"), 1);
        assert_eq!(call_count(&science_call_log, "url"), 2);
        assert_eq!(call_count(&route_config_log, "configure-third-party"), 1);

        let second = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("second one-click should reuse running sandbox");
        assert_eq!(second["action"], "reopened");
        assert!(
            second.get("url").is_none(),
            "one-time URL must stay backend-only"
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            first_pid
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );
        assert_eq!(call_count(&science_call_log, "--version"), 1);
        assert_eq!(call_count(&science_call_log, "status"), 2);
        assert_eq!(call_count(&science_call_log, "url"), 3);
        assert_eq!(call_count(&route_config_log, "configure-third-party"), 1);

        super::open_url_inner(&state)
            .expect("first manual open should refresh the one-time Science URL");
        super::open_url_inner(&state)
            .expect("second manual open should refresh the one-time Science URL again");
        assert_eq!(call_count(&science_call_log, "url"), 5);
        let opened_urls: Vec<_> = fs::read_to_string(&open_log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert!(opened_urls.len() >= 4);
        assert!(opened_urls[opened_urls.len() - 2].ends_with("/?nonce=4"));
        assert!(opened_urls[opened_urls.len() - 1].ends_with("/?nonce=5"));
        assert_ne!(
            opened_urls[opened_urls.len() - 2],
            opened_urls[opened_urls.len() - 1]
        );

        let fail_once = tmp.join("open-failed-once");
        env_guard.set("CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE", &fail_once);
        let failed_open = super::open_url_inner(&state)
            .expect("manual opener failure should be a structured UI result");
        assert_eq!(failed_open["status"], "error");
        assert!(failed_open["fallback_url"]
            .as_str()
            .unwrap()
            .ends_with("/?nonce=6"));
        let retried_open = super::open_url_inner(&state)
            .expect("retry should fetch and submit another fresh Science URL");
        assert_eq!(retried_open["status"], "ok");
        assert!(retried_open["fallback_url"].is_null());
        assert_eq!(call_count(&science_call_log, "url"), 7);

        let route_check = lifecycle
            .with_serialized(|| sandbox_session::force_third_party_reconcile(&handle, &state));
        assert_eq!(route_check.as_deref(), Ok("Skill 路由已强制核验并同步。"));
        assert_eq!(call_count(&science_call_log, "--version"), 2);
        assert_eq!(call_count(&science_call_log, "status"), 3);
        assert_eq!(call_count(&science_call_log, "url"), 8);
        assert_eq!(call_count(&route_config_log, "configure-third-party"), 2);

        stop_test_sandbox(&handle, &state, sandbox_port);
        let mut cold_start_ms = Vec::new();
        for cycle in 0..5 {
            let (version_cache, confirmed_stopped) = {
                let st = lock(&state);
                (
                    st.science_version_cache.clone(),
                    st.science_confirmed_stopped.clone(),
                )
            };
            let preflight =
                science::science_runtime_preflight(&version_cache, confirmed_stopped.as_ref())
                    .expect("confirmed stop should make preflight ready without status CLI");
            assert_eq!(preflight["status"], "installed_ready");
            let started_at = Instant::now();
            let restarted = sandbox_session::one_click_login(
                handle.clone(),
                state.clone(),
                lifecycle.as_ref(),
                None,
                None,
            )
            .expect("normal cold start should not re-probe or reconfigure");
            cold_start_ms.push(started_at.elapsed().as_millis());
            assert_eq!(restarted["action"], "started");
            if cycle < 4 {
                stop_test_sandbox(&handle, &state, sandbox_port);
            }
        }
        let mut sorted_cold_start_ms = cold_start_ms.clone();
        sorted_cold_start_ms.sort_unstable();
        eprintln!(
            "focused cold starts ms={cold_start_ms:?} median_ms={}",
            sorted_cold_start_ms[2]
        );
        assert_eq!(call_count(&science_call_log, "--version"), 2);
        assert_eq!(call_count(&science_call_log, "status"), 3);
        assert_eq!(call_count(&science_call_log, "url"), 13);
        assert_eq!(call_count(&route_config_log, "configure-third-party"), 2);
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "6"
        );

        stop_test_sandbox(&handle, &state, sandbox_port);
        let upgraded_script = fs::read_to_string(&fake_science)
            .unwrap()
            .replace("0.0.0-csswitch-test", "0.0.1-csswitch-test");
        let upgraded_candidate = bin_dir.join("claude-science-upgraded");
        write_executable(&upgraded_candidate, &upgraded_script);
        fs::rename(&upgraded_candidate, &fake_science).unwrap();
        let (version_cache, confirmed_stopped) = {
            let st = lock(&state);
            (
                st.science_version_cache.clone(),
                st.science_confirmed_stopped.clone(),
            )
        };
        assert_eq!(
            science::science_runtime_preflight(&version_cache, confirmed_stopped.as_ref()).unwrap()
                ["status"],
            "installed_ready"
        );
        let upgraded = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("binary replacement should re-probe and reconcile once");
        assert_eq!(upgraded["action"], "started");
        assert_eq!(call_count(&science_call_log, "--version"), 3);
        assert_eq!(call_count(&science_call_log, "status"), 3);
        assert_eq!(call_count(&science_call_log, "url"), 15);
        assert_eq!(call_count(&route_config_log, "configure-third-party"), 3);
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "7"
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
                science_runtime,
                ..
            } = &mut *st;
            let runtime = science_runtime.clone();
            let _ = science::stop_sandbox(&handle, sandbox, sandbox_url, runtime.as_ref());
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
        let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
        let open_log = tmp.join("open.log");
        let mock_upstream_port = start_mock_upstream();
        let proxy_port = free_port();
        let sandbox_port = free_port();
        assert_ne!(proxy_port, sandbox_port);

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &root);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
        env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
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
            model_catalog: vec![crate::model_catalog::ModelRoute {
                selector_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                display_name: "Mock model".into(),
                upstream_model: "mock-model".into(),
                supports_tools: Some(true),
                ..Default::default()
            }],
            default_model_route_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            role_bindings: crate::model_catalog::RoleBindings {
                sonnet: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                opus: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                haiku: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                fable: "claude-csswitch-relay-mock-model-0123456789ab".into(),
                ..Default::default()
            },
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

        let first = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("first one-click should start proxy and sandbox");
        assert_eq!(first["action"], "started");
        assert!(
            first.get("url").is_none(),
            "one-time URL must stay backend-only"
        );
        wait_http_health(proxy_port);
        wait_http_health(sandbox_port);
        let fake_state_dir = home
            .join(config::CONFIG_DIR_NAME)
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
            "代理进程不可达或已退出，请点击「一键开始」恢复。"
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

        let recovered = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("one-click should manually recover a dead proxy");
        assert_eq!(recovered["action"], "reopened");
        assert!(recovered["msg"]
            .as_str()
            .unwrap()
            .starts_with("已用新配置重启代理，Science 沿用不变，已重新打开 Science。"));
        assert_eq!(recovered["external_skill_installer"]["status"], "WARNING");
        assert!(
            recovered.get("url").is_none(),
            "one-time URL must stay backend-only"
        );
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
                science_runtime,
                ..
            } = &mut *st;
            let runtime = science_runtime.clone();
            let _ = science::stop_sandbox(&handle, sandbox, sandbox_url, runtime.as_ref());
            st.stop_proxy();
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
