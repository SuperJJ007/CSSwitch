use std::path::Path;

use serde_json::json;
use tauri::State;

use crate::runtime::profile::{
    build_get_config, build_list_templates, build_preset_sync_preview, clear_profile_key_inner,
    create_profile_with_catalog_inner, delete_profile_inner, persist_profile_candidate_inner,
    update_profile_metadata_inner, CatalogEdit, ConnectionEdit,
};
use crate::runtime::profile_switch::{scratch_validate_candidate, set_active_profile_txn};
use crate::runtime::provider::{reject_openai_custom_anthropic_base, resolve_launch_plan};
use crate::{config, lifecycle, lock, run_blocking_typed, SharedAppState, SharedLifecycle};

fn catalog_edit_from_parts(
    legacy_model_present: bool,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<Option<CatalogEdit>, String> {
    let catalog_edit = match (model_catalog, default_model_route_id, role_bindings) {
        (None, None, None) => None,
        (Some(routes), Some(default_model_route_id), Some(role_bindings)) => Some(CatalogEdit {
            routes,
            default_model_route_id,
            role_bindings,
        }),
        _ => {
            return Err("model_catalog 必须与 default_model_route_id/role_bindings 一起提交".into())
        }
    };
    if legacy_model_present && catalog_edit.is_some() {
        return Err("legacy model 与完整 model_catalog 不能同时提交".into());
    }
    Ok(catalog_edit)
}

fn require_preview_fingerprint(preview: &serde_json::Value, expected: &str) -> Result<(), String> {
    if preview
        .get("preview_fingerprint")
        .and_then(serde_json::Value::as_str)
        == Some(expected)
    {
        Ok(())
    } else {
        Err("推荐目录预览已过期；配置或内置推荐已变化，请重新预览后确认。".into())
    }
}

#[tauri::command]
pub(crate) fn get_config() -> Result<serde_json::Value, String> {
    build_get_config(&config::default_dir())
}

/// 模板注册表交前端铺 UI（新建向导用）。
#[tauri::command]
pub(crate) fn list_templates() -> Result<Vec<serde_json::Value>, String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    Ok(build_list_templates(cfg.experimental_codex_enabled))
}

#[tauri::command]
pub(crate) fn preview_profile_preset_sync(id: String) -> Result<serde_json::Value, String> {
    build_preset_sync_preview(&config::default_dir(), &id)
}

#[tauri::command]
pub(crate) async fn apply_profile_preset_sync(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    expected_preview_fingerprint: String,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || {
        lifecycle.with_serialized(|| {
            let dir = config::default_dir();
            let preview = build_preset_sync_preview(&dir, &id)?;
            require_preview_fingerprint(&preview, &expected_preview_fingerprint)
                .map_err(crate::commands::codex::RuntimeCommandError::from)?;
            let edit = CatalogEdit {
                routes: serde_json::from_value(preview["model_catalog"].clone())
                    .map_err(|error| error.to_string())?,
                default_model_route_id: preview["default_model_route_id"]
                    .as_str()
                    .ok_or("推荐目录缺少默认 selector")?
                    .to_string(),
                role_bindings: serde_json::from_value(preview["role_bindings"].clone())
                    .map_err(|error| error.to_string())?,
            };
            let cfg = config::load_from(&dir).map_err(|error| error.to_string())?;
            if cfg.active_id == id {
                set_active_profile_txn(
                    &app,
                    &state,
                    lifecycle.as_ref(),
                    &id,
                    false,
                    Some(&ConnectionEdit::default().with_catalog(Some(edit))),
                    None,
                )
                .map_err(crate::commands::codex::RuntimeCommandError::from)
            } else {
                let mut candidate = cfg
                    .profile_by_id(&id)
                    .cloned()
                    .ok_or_else(|| format!("找不到 profile：{id}"))?;
                ConnectionEdit::default()
                    .with_catalog(Some(edit))
                    .apply(&mut candidate)?;
                persist_profile_candidate_inner(&dir, &id, &candidate)?;
                Ok(json!({
                    "committed": true,
                    "status": "ok",
                    "stage": "complete",
                    "recovery_status": "not_needed",
                    "message": "已同步最新推荐；下次激活时会验证默认模型。",
                }))
            }
        })
    })
    .await
}

// ---------- profile CRUD 命令（薄包装 *_inner，统一经串行器） ----------
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_profile(
    lifecycle: State<'_, SharedLifecycle>,
    template_id: String,
    name: String,
    key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<String, String> {
    let catalog_edit = catalog_edit_from_parts(
        model.is_some(),
        model_catalog,
        default_model_route_id,
        role_bindings,
    )?;
    lifecycle.with_serialized(|| {
        create_profile_with_catalog_inner(
            &config::default_dir(),
            &template_id,
            &name,
            key.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
            catalog_edit,
        )
    })
}

#[tauri::command]
pub(crate) fn update_profile_metadata(
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    name: String,
    notes: Option<String>,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        update_profile_metadata_inner(&config::default_dir(), &id, &name, notes.as_deref())
    })
}

/// 清 key：经串行器；若清的是【生效】profile → bump_generation 作废在途启动 + 停运行中代理
/// （不再拿旧 key 服务，比照 spec §8.2 运行态撤销）。
#[tauri::command]
pub(crate) fn clear_profile_key(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<(), String> {
    clear_profile_key_cmd(
        &config::default_dir(),
        state.inner(),
        lifecycle.as_ref(),
        &id,
    )
}

/// 删 profile：经串行器；删的是【生效】profile → active 置空（inner 内）+ bump + 停代理。
#[tauri::command]
pub(crate) fn delete_profile(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<(), String> {
    delete_profile_cmd(
        &config::default_dir(),
        state.inner(),
        lifecycle.as_ref(),
        &id,
    )
}

fn clear_profile_key_cmd(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        let was_active = config::load_from(dir)
            .map(|c| c.active_id == id)
            .unwrap_or(false);
        clear_profile_key_inner(dir, id)?;
        if was_active {
            lifecycle.bump_generation();
            let mut st = lock(state);
            st.stop_proxy();
        }
        Ok(())
    })
}

fn delete_profile_cmd(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        let was_active = config::load_from(dir)
            .map(|c| c.active_id == id)
            .unwrap_or(false);
        delete_profile_inner(dir, id)?;
        if was_active {
            lifecycle.bump_generation();
            let mut st = lock(state);
            st.stop_proxy();
        }
        Ok(())
    })
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn update_profile_connection(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || {
        update_profile_connection_inner_cmd(
            app,
            state,
            lifecycle,
            id,
            base_url,
            api_format,
            model,
            key,
            model_catalog,
            default_model_route_id,
            role_bindings,
        )
    })
    .await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_profile_catalog_model(
    app: tauri::AppHandle,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    model_reference: String,
    base_url: Option<String>,
    key: Option<String>,
    model_catalog: Vec<crate::model_catalog::ModelRoute>,
    role_bindings: crate::model_catalog::RoleBindings,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || {
        lifecycle.with_serialized(|| {
            let cfg =
                config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
            let mut candidate = cfg
                .profile_by_id(&id)
                .cloned()
                .ok_or_else(|| format!("找不到 profile：{id}"))?;
            if candidate.model_policy != crate::provider_contracts::ModelPolicy::SavedCatalog {
                return Err(crate::commands::codex::RuntimeCommandError::from(
                    "动态 Codex 目录不支持静态逐模型验证".to_string(),
                ));
            }
            ConnectionEdit::new(base_url, None, None, key)
                .with_catalog(Some(CatalogEdit {
                    routes: model_catalog,
                    default_model_route_id: model_reference,
                    role_bindings,
                }))
                .apply(&mut candidate)?;
            let validated = scratch_validate_candidate(&app, &candidate, None)?;
            Ok(json!({
                "validated": validated,
                "status": if validated { "ok" } else { "unknown" },
                "message": if validated {
                    "该模型已通过隔离 scratch 请求验证。"
                } else {
                    "未能确认该模型；未修改配置。"
                },
            }))
        })
    })
    .await
}

#[allow(clippy::too_many_arguments)]
fn update_profile_connection_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let catalog_edit = catalog_edit_from_parts(
        model.is_some(),
        model_catalog,
        default_model_route_id,
        role_bindings,
    )
    .map_err(crate::commands::codex::RuntimeCommandError::from)?;
    let preflight_cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    let mut preflight_candidate = preflight_cfg
        .profile_by_id(&id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    let preflight_edit = ConnectionEdit::new(
        base_url.clone(),
        api_format.clone(),
        model.clone(),
        key.clone(),
    )
    .with_catalog(catalog_edit.clone());
    preflight_edit.apply(&mut preflight_candidate)?;
    let target_adapter = resolve_launch_plan(&preflight_candidate)?.adapter;
    let active_adapter = preflight_cfg
        .active_profile()
        .map(resolve_launch_plan)
        .transpose()?
        .map(|launch| launch.adapter);
    let (preflight_adapter, preflight_target) = if target_adapter == "codex" {
        (
            "codex",
            crate::commands::codex::CodexPreflightTarget::Profile(id.clone()),
        )
    } else if active_adapter.as_deref() == Some("codex") && preflight_cfg.active_id == id {
        (
            "codex",
            crate::commands::codex::CodexPreflightTarget::ActiveProfile,
        )
    } else {
        (
            target_adapter.as_str(),
            crate::commands::codex::CodexPreflightTarget::NoProfile,
        )
    };
    let prepared =
        crate::commands::codex::prepare_provider_auth(&app, preflight_adapter, preflight_target)?;
    lifecycle
        .with_serialized(|| -> Result<_, String> {
            if let Some(prepared) = prepared.as_ref() {
                prepared.verify_unchanged()?;
            }
            let dir = config::default_dir();
            let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
            // 未命中 id → Err（不静默 Ok）。
            let mut candidate = cfg
                .profile_by_id(&id)
                .cloned()
                .ok_or_else(|| format!("找不到 profile：{id}"))?;
            config::require_template_enabled(&cfg, &candidate.template_id)?;
            // 生效【后】的候选连接（None=不改则沿用旧值），active/非 active 共用一份。
            let edit = ConnectionEdit::new(
                base_url.clone(),
                api_format.clone(),
                model.clone(),
                key.clone(),
            )
            .with_catalog(catalog_edit.clone());
            edit.apply(&mut candidate)?;
            let resolved = resolve_launch_plan(&candidate)?;
            reject_openai_custom_anthropic_base(&resolved.adapter, &candidate.base_url)?;
            // 保存前守卫（修 P2）：relay/自定义端点清空 base_url → 不可用连接（激活必失败）。
            // 校验生效后的 base_url，空则拒绝落盘、绝不谎报「已保存」；native 走硬编码端点，空无妨。
            if resolved.endpoint_policy
                == crate::provider_contracts::EndpointPolicy::ProfileRequired
                && candidate.base_url.trim().is_empty()
            {
                return Err(
                    "中转 / 自定义端点必须填写连接地址（base_url），连接未保存。".to_string(),
                );
            }
            // 保存前守卫（修 #9 P1-a）：relay/自定义端点空 model → 无 force → 退回 passthrough（显示 claude）。
            if resolved.model_policy == crate::provider_contracts::ModelPolicy::SavedCatalog
                && candidate.model.trim().is_empty()
            {
                return Err("中转 / 自定义端点必须选择或填写一个模型，连接未保存。".to_string());
            }
            if cfg.active_id == id {
                // active（有正在服务的代理）：validate-before-persist —— 新连接作【内存候选】喂进
                // 切换事务（校验→起正式→健康），探活健康【才】连同落盘；失败则磁盘连接零改动、
                // 仍跑旧连接（杜绝「盘新运行旧」，修 P1-4）。
                let v = set_active_profile_txn(
                    &app,
                    &state,
                    lifecycle.as_ref(),
                    &id,
                    false,
                    Some(&edit),
                    prepared.as_ref().map(|prepared| prepared.proof()),
                )?;
                // Preserve the transaction's structured stage and recovery
                // result. The UI must be able to distinguish restored from
                // degraded instead of receiving a downgraded string error.
                Ok(v)
            } else {
                // 非 active：无正在服务的代理。先对候选做上游 scratch 校验（仅明确拒绝才拦，其余
                // best-effort 落盘并如实标记「未校验」，修 P2-d：贴合设计「校验候选后提交」+ 如实报告），
                // 再落盘（inner 内含格式门 + 覆盖前留底）。
                let validated = scratch_validate_candidate(
                    &app,
                    &candidate,
                    prepared.as_ref().map(|prepared| prepared.proof()),
                )?;
                persist_profile_candidate_inner(&dir, &id, &candidate)?;
                Ok(json!({ "validated": validated }))
            }
        })
        .map_err(crate::commands::codex::RuntimeCommandError::from)
}

/// 一键切生效 profile：经串行器走 [`set_active_profile_txn`] 切换事务。
#[tauri::command]
pub(crate) async fn set_active_profile(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || set_active_profile_inner_cmd(app, state, lifecycle, id, skip_verify))
        .await
}

fn set_active_profile_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let preflight_cfg =
        config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    let target = preflight_cfg
        .profile_by_id(&id)
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    let target_adapter = resolve_launch_plan(target)?.adapter;
    // The target decides whether this user action needs Codex preflight.
    // Switching away from an active Codex profile must remain possible even
    // while Codex login/status is busy or its local auth record is incomplete.
    let (preflight_adapter, preflight_target) = activation_preflight(&target_adapter, &id);
    let prepared =
        crate::commands::codex::prepare_provider_auth(&app, &preflight_adapter, preflight_target)?;
    lifecycle
        .with_serialized(|| -> Result<_, String> {
            if let Some(prepared) = prepared.as_ref() {
                prepared.verify_unchanged()?;
            }
            let cfg =
                config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
            let profile = cfg
                .profile_by_id(&id)
                .ok_or_else(|| format!("找不到 profile：{id}"))?;
            config::require_template_enabled(&cfg, &profile.template_id)?;
            set_active_profile_txn(
                &app,
                &state,
                lifecycle.as_ref(),
                &id,
                skip_verify,
                None,
                prepared.as_ref().map(|prepared| prepared.proof()),
            )
        })
        .map_err(crate::commands::codex::RuntimeCommandError::from)
}

fn activation_preflight(
    target_adapter: &str,
    id: &str,
) -> (String, crate::commands::codex::CodexPreflightTarget) {
    if target_adapter == "codex" {
        (
            "codex".into(),
            crate::commands::codex::CodexPreflightTarget::Profile(id.to_string()),
        )
    } else {
        (
            target_adapter.to_string(),
            crate::commands::codex::CodexPreflightTarget::NoProfile,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        activation_preflight, catalog_edit_from_parts, clear_profile_key_cmd, delete_profile_cmd,
        require_preview_fingerprint,
    };
    use crate::{
        config::{self, Config, Profile},
        lifecycle, lock, AppState, SharedAppState,
    };
    use std::{
        fs,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csswitch-{name}-{n}"))
    }

    fn profile(id: &str, key: &str) -> Profile {
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::new_profile_catalog("deepseek", "anthropic", None).unwrap();
        let model = model_catalog
            .iter()
            .find(|route| route.selector_id == default_model_route_id)
            .unwrap()
            .upstream_model
            .clone();
        Profile {
            id: id.into(),
            name: id.into(),
            template_id: "deepseek".into(),
            category: "cn_official".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: key.into(),
            model,
            model_catalog,
            default_model_route_id,
            role_bindings,
            model_policy: crate::provider_contracts::ModelPolicy::SavedCatalog,
            ..Default::default()
        }
    }

    fn state_with_proxy_identity() -> SharedAppState {
        let mut state = AppState::default();
        state.secret = "runtime-secret".into();
        state.provider = "deepseek".into();
        state.gateway_kind = "rust".into();
        state.shim_mode = "off".into();
        state.launch_id = "launch-current".into();
        state.key_fp = 42;
        Arc::new(Mutex::new(state))
    }

    #[test]
    fn clear_active_profile_key_stops_runtime_proxy_identity() {
        let dir = tmpdir("clear-active-key");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active")],
            active_id: "active".into(),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        clear_profile_key_cmd(&dir, &state, &lifecycle, "active").unwrap();

        let after = config::load_from(&dir).unwrap();
        assert_eq!(after.profile_by_id("active").unwrap().api_key, "");
        assert!(lifecycle.current_generation() > before);
        let st = lock(&state);
        assert!(st.secret.is_empty());
        assert!(st.provider.is_empty());
        assert!(st.gateway_kind.is_empty());
        assert!(st.shim_mode.is_empty());
        assert!(st.launch_id.is_empty());
        assert_eq!(st.key_fp, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_non_active_profile_key_leaves_runtime_proxy_identity() {
        let dir = tmpdir("clear-non-active-key");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active"), profile("other", "sk-other")],
            active_id: "active".into(),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        clear_profile_key_cmd(&dir, &state, &lifecycle, "other").unwrap();

        let after = config::load_from(&dir).unwrap();
        assert_eq!(after.profile_by_id("other").unwrap().api_key, "");
        assert_eq!(lifecycle.current_generation(), before);
        let st = lock(&state);
        assert_eq!(st.secret, "runtime-secret");
        assert_eq!(st.provider, "deepseek");
        assert_eq!(st.gateway_kind, "rust");
        assert_eq!(st.shim_mode, "off");
        assert_eq!(st.launch_id, "launch-current");
        assert_eq!(st.key_fp, 42);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_active_profile_stops_runtime_proxy_identity() {
        let dir = tmpdir("delete-active-profile");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active")],
            active_id: "active".into(),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        delete_profile_cmd(&dir, &state, &lifecycle, "active").unwrap();

        let after = config::load_from(&dir).unwrap();
        assert!(after.active_id.is_empty());
        assert!(after.profile_by_id("active").is_none());
        assert!(lifecycle.current_generation() > before);
        let st = lock(&state);
        assert!(st.secret.is_empty());
        assert!(st.provider.is_empty());
        assert!(st.gateway_kind.is_empty());
        assert!(st.shim_mode.is_empty());
        assert!(st.launch_id.is_empty());
        assert_eq!(st.key_fp, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_codex_activation_never_reserves_codex_preflight() {
        let (adapter, target) = activation_preflight("relay", "glm-profile");
        assert_eq!(adapter, "relay");
        assert!(matches!(
            target,
            crate::commands::codex::CodexPreflightTarget::NoProfile
        ));
        let (adapter, target) = activation_preflight("codex", "codex-profile");
        assert_eq!(adapter, "codex");
        assert!(matches!(
            target,
            crate::commands::codex::CodexPreflightTarget::Profile(id) if id == "codex-profile"
        ));
    }

    #[test]
    fn catalog_edit_requires_all_three_fields_and_rejects_legacy_mix() {
        let route = crate::model_catalog::ModelRoute {
            selector_id: "claude-csswitch-test-model-123456789abc".into(),
            display_name: "Model".into(),
            upstream_model: "model-upstream".into(),
            supports_tools: Some(true),
            ..Default::default()
        };
        let roles = crate::model_catalog::RoleBindings {
            sonnet: route.selector_id.clone(),
            opus: route.selector_id.clone(),
            haiku: route.selector_id.clone(),
            fable: route.selector_id.clone(),
            ..Default::default()
        };
        assert!(catalog_edit_from_parts(false, None, None, None)
            .unwrap()
            .is_none());
        assert!(catalog_edit_from_parts(
            false,
            Some(vec![route.clone()]),
            Some(route.selector_id.clone()),
            Some(roles.clone()),
        )
        .unwrap()
        .is_some());
        assert!(catalog_edit_from_parts(false, Some(vec![route.clone()]), None, None).is_err());
        assert!(catalog_edit_from_parts(
            true,
            Some(vec![route]),
            Some("claude-csswitch-test-model-123456789abc".into()),
            Some(roles),
        )
        .is_err());
    }

    #[test]
    fn stale_preset_preview_fingerprint_is_rejected() {
        let preview = serde_json::json!({ "preview_fingerprint": "fingerprint-a" });
        assert!(require_preview_fingerprint(&preview, "fingerprint-a").is_ok());
        assert!(require_preview_fingerprint(&preview, "fingerprint-b").is_err());
        assert!(require_preview_fingerprint(&serde_json::json!({}), "fingerprint-a").is_err());
    }
}
