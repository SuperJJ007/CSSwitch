use serde_json::{json, Value};

use crate::runtime::operation::{OperationKind, OperationStage, OperationTrace};
use crate::runtime::profile::{nonactive_probe_verdict, probe_kind_for, ConnectionEdit};
use crate::runtime::provider::{
    assert_format_supported, is_native_adapter, proxy_args_for, reject_openai_custom_anthropic_base,
};
use crate::runtime::proxy_lifecycle::start_proxy_for;
use crate::runtime::transaction::{rollback_status_clause, skip_scratch_verify};
use crate::{config, lifecycle, lock, scratch, SharedAppState};

fn uncommitted_scratch_result(message: String, can_skip: bool) -> Value {
    json!({
        "committed": false,
        "can_skip": can_skip,
        "hint": message,
        "message": message,
        "stage": "scratch_upstream",
        "status": "error",
        "recovery_status": "not_needed",
        "fallback_url": null,
    })
}

/// Validate a non-active candidate without touching config, AppState, or the active proxy.
pub(crate) fn scratch_validate_candidate(
    app: &tauri::AppHandle,
    candidate: &config::Profile,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> Result<bool, String> {
    let launch = proxy_args_for(candidate)?;
    let scratch_plan = launch.scratch();
    crate::commands::codex::require_provider_auth_proof(&scratch_plan.provider, auth_proof)?;
    let trace = OperationTrace::start(
        OperationKind::ValidateConnection,
        format!(
            "profile_id={} template_id={} adapter={}",
            candidate.id, candidate.template_id, launch.adapter
        ),
    );
    if !scratch_plan.should_probe() {
        trace.finish("skipped reason=missing_key_or_base");
        return Ok(false);
    }
    let (key_env, key) = scratch_plan.credential_parts();
    let backend = scratch::backend_for_app(app, &scratch_plan.provider)?;
    let res = scratch::scratch_probe(
        &backend,
        &scratch::ScratchTarget {
            provider: &scratch_plan.provider,
            key_env,
            base_url: &scratch_plan.endpoint,
            key,
            model: Some(&scratch_plan.model),
            static_model_catalog: scratch_plan.static_model_catalog.as_deref(),
            relay_thinking: &scratch_plan.thinking_policy,
        },
        probe_kind_for(&scratch_plan.provider, &scratch_plan.model),
        Some(&trace),
        auth_proof.map(|proof| proof.exit_cancel_flag()),
    );
    let outcome = scratch::classify(res.status);
    trace.finish(format!("outcome={outcome:?}"));
    nonactive_probe_verdict(&outcome)
}

fn apply_candidate_transaction(
    current: &mut config::Config,
    candidate: &config::Profile,
    id: &str,
    is_edit: bool,
    previous_binding: Option<config::RuntimeBindingCommit>,
    previous_gateway: Option<config::GatewayRuntimeJournalIdentity>,
) {
    current.active_id = id.to_string();
    if is_edit {
        if let Some(profile) = current.profile_by_id_mut(id) {
            *profile = candidate.clone();
        }
    }
    current.runtime_transaction = Some(config::RuntimeTransactionJournal {
        transaction_id: config::new_id(),
        target_profile_id: id.to_string(),
        stage: "start_formal_gateway".into(),
        previous_binding,
        previous_gateway,
    });
}

fn current_gateway_journal_identity(
    state: &SharedAppState,
    cfg: &config::Config,
) -> Option<config::GatewayRuntimeJournalIdentity> {
    let mut st = lock(state);
    if !crate::proc::tracked_child_is_running(&mut st.proxy)
        || st.proxy_port != cfg.proxy_port
        || st.secret.is_empty()
        || st.launch_id.is_empty()
    {
        return None;
    }
    let health = crate::proc::http_gateway_health(
        cfg.proxy_port,
        Some(&st.secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    )?;
    (health.gateway == "rust"
        && health.intent == "formal"
        && health.provider == st.provider
        && health.shim == st.shim_mode
        && health.launch_id == st.launch_id)
        .then_some(config::GatewayRuntimeJournalIdentity {
            provider: health.provider,
            shim: health.shim,
            launch_id: health.launch_id,
            catalog_fp: health.catalog_fp,
        })
}

/// Switch the active profile transactionally: scratch validate, atomically
/// publish the candidate config + journal, then install the formal gateway and
/// reconcile Science. A crash after publication therefore resumes from the
/// committed profile instead of leaving old config paired with a new gateway.
///
/// Callers must hold the command serializer lock.
pub(crate) fn set_active_profile_txn(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
    skip_verify: bool,
    conn_edit: Option<&ConnectionEdit>,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> Result<Value, String> {
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let mut candidate = cfg
        .profile_by_id(id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    config::require_template_enabled(&cfg, &candidate.template_id)?;
    if let Some(edit) = conn_edit {
        edit.apply(&mut candidate)?;
    }
    let is_edit = conn_edit.is_some();
    let (verb, tail): (&str, &str) = if is_edit {
        ("未保存", "仍在用原配置运行")
    } else {
        ("未切换", "当前配置不变")
    };
    assert_format_supported(&candidate)?;
    let launch = proxy_args_for(&candidate)?;
    let formal_plan = launch.formal();
    crate::commands::codex::require_provider_auth_proof(&formal_plan.adapter, auth_proof)?;
    reject_openai_custom_anthropic_base(&formal_plan.adapter, &candidate.base_url)?;
    if !formal_plan.credential_configured() {
        return Err(format!(
            "「{}」还没配置凭据，请先填写或登录。",
            candidate.name
        ));
    }
    let native = is_native_adapter(&formal_plan.adapter);
    if formal_plan.endpoint_policy == crate::provider_contracts::EndpointPolicy::ProfileRequired
        && formal_plan.endpoint.is_empty()
    {
        return Err("该配置需要填 base_url（http:// 或 https:// 开头）。".into());
    }
    if formal_plan.model_policy == crate::provider_contracts::ModelPolicy::SavedCatalog
        && formal_plan.model.trim().is_empty()
    {
        return Err(
            "该配置需要选择或填写一个模型（中转/自定义端点必填），请在连接编辑里补上。".into(),
        );
    }

    let old_active = cfg.active_id.clone();
    let previous_gateway = current_gateway_journal_identity(state, &cfg);
    let science_runtime_before = { lock(state).science_runtime.clone() };
    let science_was_running = match science_runtime_before.as_ref() {
        Some(runtime) => {
            match crate::runtime::science::probe_known_runtime(cfg.sandbox_port, runtime) {
                crate::runtime::science::SandboxScienceState::RunningHealthy => true,
                crate::runtime::science::SandboxScienceState::Stopped => false,
                crate::runtime::science::SandboxScienceState::Unknown => {
                    return Err("Science 可能正在运行，但身份无法确认；已拒绝切换，未猜测 PID 或结束端口进程。".into());
                }
            }
        }
        None if crate::proc::loopback_port_in_use(
            cfg.sandbox_port,
            crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
        ) =>
        {
            return Err(
                "Science 端口正在使用，但当前进程没有可确认的 runtime 身份；已拒绝切换。".into(),
            );
        }
        None => false,
    };
    let trace = OperationTrace::start(
        if is_edit {
            OperationKind::UpdateActiveConnection
        } else {
            OperationKind::ActivateProfile
        },
        format!(
            "profile_id={} template_id={} adapter={} skip_verify={}",
            candidate.id, candidate.template_id, formal_plan.adapter, skip_verify
        ),
    );

    if skip_scratch_verify(native, skip_verify) {
        trace.stage(OperationStage::ScratchUpstreamProbe, "skipped_by_user");
    } else {
        let scratch_plan = launch.scratch();
        let (key_env, key) = scratch_plan.credential_parts();
        let backend = scratch::backend_for_app(app, &scratch_plan.provider)?;
        let res = scratch::scratch_probe(
            &backend,
            &scratch::ScratchTarget {
                provider: &scratch_plan.provider,
                key_env,
                base_url: &scratch_plan.endpoint,
                key,
                model: Some(&scratch_plan.model),
                static_model_catalog: scratch_plan.static_model_catalog.as_deref(),
                relay_thinking: &scratch_plan.thinking_policy,
            },
            probe_kind_for(&scratch_plan.provider, &scratch_plan.model),
            Some(&trace),
            auth_proof.map(|proof| proof.exit_cancel_flag()),
        );
        let outcome = scratch::classify(res.status);
        trace.stage(
            OperationStage::ScratchUpstreamProbe,
            format!("outcome={outcome:?}"),
        );
        let codex_protocol_failure = scratch_plan.provider == "codex"
            && scratch::gateway_models_error_kind(&res.body) == Some("protocol");
        match outcome {
            scratch::ProbeOutcome::Ok => {}
            scratch::ProbeOutcome::Auth(code) => {
                trace.finish(format!("rejected status={code}"));
                return Ok(uncommitted_scratch_result(
                    format!("上游拒绝（{code}），key/权限有误，{verb}（{tail}）。"),
                    false,
                ));
            }
            scratch::ProbeOutcome::ModelError(code) => {
                trace.finish(format!("model_error status={code}"));
                return Ok(uncommitted_scratch_result(
                    format!("上游拒绝该模型（{code}），{verb}。请换一个模型或核对 base_url。"),
                    false,
                ));
            }
            scratch::ProbeOutcome::Ambiguous(_) if codex_protocol_failure => {
                trace.finish("codex_protocol can_skip=true");
                return Ok(uncommitted_scratch_result(
                    format!("Codex 模型目录响应不兼容，{verb}。这不是网络繁忙；可重试，或用「跳过验证」仅启动链路。"),
                    true,
                ));
            }
            scratch::ProbeOutcome::Ambiguous(_)
            | scratch::ProbeOutcome::NoResponse
            | scratch::ProbeOutcome::Unsupported(_) => {
                trace.finish("ambiguous can_skip=true");
                return Ok(uncommitted_scratch_result(
                    format!("无法确认（网络/上游繁忙），{verb}。可重试，或用「跳过验证」。"),
                    true,
                ));
            }
        }
    }

    if is_edit {
        config::write_rolling_backup(&dir).ok();
    }
    if let Err(error) = config::update(&dir, |current| {
        apply_candidate_transaction(
            current,
            &candidate,
            id,
            is_edit,
            cfg.runtime_binding.clone(),
            previous_gateway.clone(),
        );
    }) {
        trace.finish("error=candidate_config_publish_failed");
        return Ok(json!({
            "committed": false,
            "stage": "config_commit",
            "status": "error",
            "recovery_status": "not_needed",
            "message": format!("候选连接已校验，但配置与事务日志原子提交失败：{error}"),
            "fallback_url": null,
        }));
    }

    lifecycle.bump_generation();
    if let Err(error) = start_proxy_for(
        app,
        state,
        lifecycle,
        &candidate,
        None,
        Some(&trace),
        auth_proof,
    ) {
        trace.stage(OperationStage::Rollback, "reason=proxy_unhealthy");
        let config_restored = config::save_to(&dir, &cfg).is_ok();
        let proxy_restored = config_restored
            && restore_proxy_for_active(
                app,
                state,
                lifecycle,
                &cfg,
                &old_active,
                Some(&trace),
                auth_proof,
            );
        let restored = config_restored && proxy_restored;
        let clause = rollback_status_clause(restored);
        trace.finish(format!("error=proxy_unhealthy restored={restored}"));
        return Ok(json!({
            "committed": false,
            "stage": "gateway_health",
            "status": "error",
            "recovery_status": if restored { "restored" } else { "degraded" },
            "message": format!("候选配置已校验，但正式代理启动/探活失败（{error}），{clause}。"),
            "fallback_url": null,
        }));
    }

    if science_was_running {
        trace.stage(
            OperationStage::SandboxLaunch,
            "reason=model_binding_reconcile",
        );
        if let Err(error) = crate::runtime::sandbox_session::reconcile_science_for_active(
            app.clone(),
            state.clone(),
            lifecycle,
            auth_proof,
        ) {
            trace.stage(OperationStage::Rollback, "reason=science_reconcile_failed");
            let config_restored = config::save_to(&dir, &cfg).is_ok();
            let proxy_restored = config_restored
                && restore_proxy_for_active(
                    app,
                    state,
                    lifecycle,
                    &cfg,
                    &old_active,
                    Some(&trace),
                    auth_proof,
                );
            // Ordinary reconcile must not be used here: the currently running
            // process may have loaded the candidate catalog while disk now
            // contains the old binding. Stop only the exact remembered
            // Science identity, then launch the old committed chain afresh.
            let science_restored = config_restored
                && crate::runtime::sandbox_session::force_restart_science_for_active(
                    app.clone(),
                    state.clone(),
                    lifecycle,
                    auth_proof,
                )
                .is_ok();
            // A successful forced Science restart necessarily passed through
            // ensure_proxy for the restored config, so it is the final proof
            // that both old gateway and old Science are healthy. Keep the
            // earlier direct proxy result only as recovery trace evidence.
            trace.stage(
                OperationStage::Rollback,
                format!("old_proxy_direct_restore={proxy_restored}"),
            );
            let restored = config_restored && science_restored;
            let recovery_status = if restored { "restored" } else { "degraded" };
            trace.finish(format!(
                "error=science_reconcile_failed recovery_status={recovery_status}"
            ));
            return Ok(json!({
                "committed": false,
                "stage": "science_start",
                "status": "error",
                "recovery_status": recovery_status,
                "message": format!("新模型目录未能加载到 Science：{error}"),
                "fallback_url": null,
            }));
        }
    } else if let Err(error) = config::update(&dir, |current| {
        current.runtime_binding = None;
        current.runtime_transaction = None;
    }) {
        trace.stage(OperationStage::Rollback, "reason=commit_marker_failed");
        let config_restored = config::save_to(&dir, &cfg).is_ok();
        let proxy_restored = config_restored
            && restore_proxy_for_active(
                app,
                state,
                lifecycle,
                &cfg,
                &old_active,
                Some(&trace),
                auth_proof,
            );
        let restored = config_restored && proxy_restored;
        trace.finish(format!("error=commit_marker_failed restored={restored}"));
        return Ok(json!({
            "committed": false,
            "stage": "config_commit",
            "status": "error",
            "recovery_status": if restored { "restored" } else { "degraded" },
            "message": format!("正式代理已就绪，但提交标记写入失败（{error}），{}。", rollback_status_clause(restored)),
            "fallback_url": null,
        }));
    }

    let hint = if is_edit {
        format!("已保存并应用「{}」的新连接。", candidate.name)
    } else {
        format!("已切到「{}」。", candidate.name)
    };
    trace.stage(OperationStage::Commit, "ok");
    trace.finish("committed=true");
    Ok(json!({
        "committed": true,
        "active_id": id,
        "hint": hint,
        "stage": "complete",
        "status": "ok",
        "recovery_status": "not_needed",
        "message": hint,
        "fallback_url": null,
    }))
}

fn restore_proxy_for_active<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    cfg: &config::Config,
    old_active: &str,
    trace: Option<&OperationTrace>,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
) -> bool {
    if old_active.is_empty() {
        lock(state).stop_proxy();
        return !crate::proc::loopback_port_in_use(
            cfg.proxy_port,
            crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
        );
    }
    match cfg.profile_by_id(old_active) {
        Some(old) => {
            lifecycle.bump_generation();
            let old_adapter = proxy_args_for(old)
                .ok()
                .map(|launch| launch.formal().adapter);
            if old_adapter.as_deref() == Some("codex") && auth_proof.is_none() {
                // A switch to a non-Codex target intentionally performs no
                // interactive Codex preflight. If rollback would require
                // restarting the old Codex Gateway, fail closed without a
                // auth-file read and stop the candidate runtime so
                // config/runtime cannot silently disagree.
                lock(state).stop_proxy();
                return false;
            }
            start_proxy_for(app, state, lifecycle, old, None, trace, auth_proof).is_ok()
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn candidate_profile_and_recovery_journal_are_published_as_one_config_state() {
        let old_binding = config::RuntimeBindingCommit {
            profile_id: "old".into(),
            route_fp: "route-old".into(),
            catalog_fp: "catalog-old".into(),
            binding_fp: "binding-old".into(),
        };
        let mut cfg = config::Config {
            active_id: "old".into(),
            profiles: vec![
                config::Profile {
                    id: "old".into(),
                    ..Default::default()
                },
                config::Profile {
                    id: "new".into(),
                    name: "before".into(),
                    ..Default::default()
                },
            ],
            runtime_binding: Some(old_binding.clone()),
            ..Default::default()
        };
        let candidate = config::Profile {
            id: "new".into(),
            name: "candidate".into(),
            ..Default::default()
        };

        let previous_gateway = config::GatewayRuntimeJournalIdentity {
            provider: "qwen".into(),
            shim: "off".into(),
            launch_id: "launch-old".into(),
            catalog_fp: "gateway-catalog-old".into(),
        };
        apply_candidate_transaction(
            &mut cfg,
            &candidate,
            "new",
            true,
            Some(old_binding.clone()),
            Some(previous_gateway.clone()),
        );

        assert_eq!(cfg.active_id, "new");
        assert_eq!(cfg.profile_by_id("new").unwrap().name, "candidate");
        let journal = cfg.runtime_transaction.unwrap();
        assert_eq!(journal.target_profile_id, "new");
        assert_eq!(journal.stage, "start_formal_gateway");
        assert_eq!(journal.previous_binding, Some(old_binding));
        assert_eq!(journal.previous_gateway, Some(previous_gateway));
    }

    #[test]
    fn uncommitted_scratch_result_is_always_structured_as_an_error() {
        let result = uncommitted_scratch_result("not saved".into(), true);
        assert_eq!(result["committed"], false);
        assert_eq!(result["can_skip"], true);
        assert_eq!(result["status"], "error");
        assert_eq!(result["stage"], "scratch_upstream");
        assert_eq!(result["recovery_status"], "not_needed");
        assert_eq!(result["message"], "not saved");
    }

    #[test]
    fn rollback_to_codex_without_a_current_proof_stops_candidate_fail_closed() {
        let old = config::Profile {
            id: "old-codex".into(),
            name: "Old Codex".into(),
            template_id: "codex".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            ..Default::default()
        };
        let cfg = config::Config {
            profiles: vec![old],
            active_id: "old-codex".into(),
            experimental_codex_enabled: true,
            ..Default::default()
        };
        let mut running = crate::AppState::default();
        running.provider = "relay".into();
        running.secret = "candidate-secret".into();
        running.gateway_kind = "rust".into();
        let state = Arc::new(Mutex::new(running));
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let lifecycle = crate::lifecycle::Lifecycle::new();

        assert!(!restore_proxy_for_active(
            app.handle(),
            &state,
            &lifecycle,
            &cfg,
            "old-codex",
            None,
            None,
        ));
        let stopped = lock(&state);
        assert!(stopped.provider.is_empty());
        assert!(stopped.secret.is_empty());
        assert!(stopped.gateway_kind.is_empty());
    }
}
