use std::path::Path;

use serde_json::json;
use tauri::State;

use crate::runtime::profile::{
    build_get_config, build_list_templates, clear_profile_key_inner, create_profile_inner,
    delete_profile_inner, update_profile_connection_inner, update_profile_metadata_inner,
    ConnectionEdit,
};
use crate::runtime::profile_switch::{scratch_validate_candidate, set_active_profile_txn};
use crate::runtime::provider::{
    adapter_for_profile, reject_openai_custom_anthropic_base, relay_missing_base_url,
    relay_missing_model,
};
use crate::{config, lifecycle, lock, run_blocking, SharedAppState, SharedLifecycle};

#[tauri::command]
pub(crate) fn get_config() -> Result<serde_json::Value, String> {
    build_get_config(&config::default_dir())
}

/// 模板注册表交前端铺 UI（新建向导用）。
#[tauri::command]
pub(crate) fn list_templates() -> Vec<serde_json::Value> {
    build_list_templates()
}

// ---------- profile CRUD 命令（薄包装 *_inner，统一经串行器） ----------
#[tauri::command]
pub(crate) fn create_profile(
    lifecycle: State<'_, SharedLifecycle>,
    template_id: String,
    name: String,
    key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> Result<String, String> {
    lifecycle.with_serialized(|| {
        create_profile_inner(
            &config::default_dir(),
            &template_id,
            &name,
            key.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
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
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        update_profile_connection_inner_cmd(
            app, state, lifecycle, id, base_url, api_format, model, key,
        )
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
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
        // 未命中 id → Err（不静默 Ok）。
        let mut candidate = cfg
            .profile_by_id(&id)
            .cloned()
            .ok_or_else(|| format!("找不到 profile：{id}"))?;
        // 生效【后】的候选连接（None=不改则沿用旧值），active/非 active 共用一份。
        let edit = ConnectionEdit::new(
            base_url.clone(),
            api_format.clone(),
            model.clone(),
            key.clone(),
        );
        edit.apply(&mut candidate);
        let adapter = adapter_for_profile(&candidate);
        reject_openai_custom_anthropic_base(adapter, &candidate.base_url)?;
        // 保存前守卫（修 P2）：relay/自定义端点清空 base_url → 不可用连接（激活必失败）。
        // 校验生效后的 base_url，空则拒绝落盘、绝不谎报「已保存」；native 走硬编码端点，空无妨。
        if relay_missing_base_url(adapter, &candidate.base_url) {
            return Err("中转 / 自定义端点必须填写连接地址（base_url），连接未保存。".to_string());
        }
        // 保存前守卫（修 #9 P1-a）：relay/自定义端点空 model → 无 force → 退回 passthrough（显示 claude）。
        if relay_missing_model(adapter, &candidate.model) {
            return Err("中转 / 自定义端点必须选择或填写一个模型，连接未保存。".to_string());
        }
        if cfg.active_id == id {
            // active（有正在服务的代理）：validate-before-persist —— 新连接作【内存候选】喂进
            // 切换事务（校验→起正式→健康），探活健康【才】连同落盘；失败则磁盘连接零改动、
            // 仍跑旧连接（杜绝「盘新运行旧」，修 P1-4）。
            let v =
                set_active_profile_txn(&app, &state, lifecycle.as_ref(), &id, false, Some(&edit))?;
            // 连接编辑：committed:false（scratch 分类失败）也如实作为错误上抛（磁盘未改、代理仍跑旧的）。
            if v.get("committed").and_then(|b| b.as_bool()) == Some(false) {
                let hint = v
                    .get("hint")
                    .and_then(|h| h.as_str())
                    .unwrap_or("连接校验未通过，连接未保存。")
                    .to_string();
                return Err(hint);
            }
            // active：已连同起正式代理探活并落盘，视为已校验。
            Ok(json!({ "validated": true }))
        } else {
            // 非 active：无正在服务的代理。先对候选做上游 scratch 校验（仅明确拒绝才拦，其余
            // best-effort 落盘并如实标记「未校验」，修 P2-d：贴合设计「校验候选后提交」+ 如实报告），
            // 再落盘（inner 内含格式门 + 覆盖前留底）。
            let validated = scratch_validate_candidate(&app, &candidate)?;
            update_profile_connection_inner(
                &dir,
                &id,
                base_url.as_deref(),
                api_format.as_deref(),
                model.as_deref(),
                key.as_deref(),
            )?;
            Ok(json!({ "validated": validated }))
        }
    })
}

/// 一键切生效 profile：经串行器走 [`set_active_profile_txn`] 切换事务。
#[tauri::command]
pub(crate) async fn set_active_profile(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_active_profile_inner_cmd(app, state, lifecycle, id, skip_verify)).await
}

fn set_active_profile_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        set_active_profile_txn(&app, &state, lifecycle.as_ref(), &id, skip_verify, None)
    })
}

#[cfg(test)]
mod tests {
    use super::{clear_profile_key_cmd, delete_profile_cmd};
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
        Profile {
            id: id.into(),
            name: id.into(),
            template_id: "deepseek".into(),
            category: "cn_official".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: key.into(),
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
}
