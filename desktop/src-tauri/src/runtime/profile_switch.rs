use serde_json::{json, Value};

use crate::runtime::operation::{OperationKind, OperationStage, OperationTrace};
use crate::runtime::profile::{nonactive_probe_verdict, probe_kind_for, ConnectionEdit};
use crate::runtime::provider::{
    assert_format_supported, is_native_adapter, proxy_args_for,
    reject_openai_custom_anthropic_base, relay_missing_model, should_scratch_candidate,
};
use crate::runtime::proxy_lifecycle::start_proxy_for;
use crate::runtime::system::asset_root;
use crate::runtime::transaction::{
    decide_switch, rollback_status_clause, skip_scratch_verify, SwitchOutcome,
};
use crate::{config, lifecycle, proc, scratch, SharedAppState};

/// Validate a non-active candidate without touching config, AppState, or the active proxy.
pub(crate) fn scratch_validate_candidate(
    app: &tauri::AppHandle,
    candidate: &config::Profile,
) -> Result<bool, String> {
    let launch = proxy_args_for(candidate);
    let trace = OperationTrace::start(
        OperationKind::ValidateConnection,
        format!(
            "profile_id={} template_id={} adapter={}",
            candidate.id, candidate.template_id, launch.adapter
        ),
    );
    if !should_scratch_candidate(&launch.adapter, &launch.key, &launch.base_url) {
        trace.finish("skipped reason=missing_key_or_base");
        return Ok(false);
    }
    let root = asset_root(app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
    let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
    let script = root.join("proxy/csswitch_proxy.py");
    let res = scratch::scratch_probe(
        &py,
        &script,
        &scratch::ScratchTarget {
            provider: &launch.adapter,
            key_env: launch.key_env,
            base_url: &launch.base_url,
            key: &launch.key,
            model: Some(&launch.model),
            relay_thinking: launch.thinking_policy,
        },
        probe_kind_for(&launch.adapter, &launch.model),
        Some(&trace),
    );
    let outcome = scratch::classify(res.status);
    trace.finish(format!("outcome={outcome:?}"));
    nonactive_probe_verdict(&outcome)
}

/// Switch the active profile transactionally: scratch validate, start real proxy, then commit.
///
/// Callers must hold the command serializer lock.
pub(crate) fn set_active_profile_txn(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
    skip_verify: bool,
    conn_edit: Option<&ConnectionEdit>,
) -> Result<Value, String> {
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let mut candidate = cfg
        .profile_by_id(id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    if let Some(edit) = conn_edit {
        edit.apply(&mut candidate);
    }
    let is_edit = conn_edit.is_some();
    let (verb, tail): (&str, &str) = if is_edit {
        ("未保存", "仍在用原配置运行")
    } else {
        ("未切换", "当前配置不变")
    };
    assert_format_supported(&candidate)?;
    let launch = proxy_args_for(&candidate);
    reject_openai_custom_anthropic_base(&launch.adapter, &candidate.base_url)?;
    if launch.key.is_empty() {
        return Err(format!("「{}」还没填 API key，请先填写。", candidate.name));
    }
    let native = is_native_adapter(&launch.adapter);
    if !native && launch.base_url.is_empty() {
        return Err("该配置需要填 base_url（http:// 或 https:// 开头）。".into());
    }
    if relay_missing_model(&launch.adapter, &candidate.model) {
        return Err(
            "该配置需要选择或填写一个模型（中转/自定义端点必填），请在连接编辑里补上。".into(),
        );
    }

    let old_active = cfg.active_id.clone();
    let trace = OperationTrace::start(
        if is_edit {
            OperationKind::UpdateActiveConnection
        } else {
            OperationKind::ActivateProfile
        },
        format!(
            "profile_id={} template_id={} adapter={} skip_verify={}",
            candidate.id, candidate.template_id, launch.adapter, skip_verify
        ),
    );

    let scratch_ok = if skip_scratch_verify(native, skip_verify) {
        trace.stage(OperationStage::ScratchUpstreamProbe, "skipped_by_user");
        true
    } else {
        let root = asset_root(app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
        let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
        let script = root.join("proxy/csswitch_proxy.py");
        let res = scratch::scratch_probe(
            &py,
            &script,
            &scratch::ScratchTarget {
                provider: &launch.adapter,
                key_env: launch.key_env,
                base_url: &launch.base_url,
                key: &launch.key,
                model: Some(&launch.model),
                relay_thinking: launch.thinking_policy,
            },
            probe_kind_for(&launch.adapter, &launch.model),
            Some(&trace),
        );
        let outcome = scratch::classify(res.status);
        trace.stage(
            OperationStage::ScratchUpstreamProbe,
            format!("outcome={outcome:?}"),
        );
        match outcome {
            scratch::ProbeOutcome::Ok => true,
            scratch::ProbeOutcome::Auth(code) => {
                trace.finish(format!("rejected status={code}"));
                return Ok(json!({ "committed": false,
                    "hint": format!("上游拒绝（{code}），key/权限有误，{verb}（{tail}）。") }));
            }
            scratch::ProbeOutcome::ModelError(code) => {
                trace.finish(format!("model_error status={code}"));
                return Ok(json!({ "committed": false,
                    "hint": format!("上游拒绝该模型（{code}），{verb}。请换一个模型或核对 base_url。") }));
            }
            scratch::ProbeOutcome::Ambiguous(_)
            | scratch::ProbeOutcome::NoResponse
            | scratch::ProbeOutcome::Unsupported(_) => {
                trace.finish("ambiguous can_skip=true");
                return Ok(json!({ "committed": false, "can_skip": true,
                    "hint": format!("无法确认（网络/上游繁忙），{verb}。可重试，或用「跳过验证」。") }));
            }
        }
    };

    lifecycle.bump_generation();
    let real_healthy =
        scratch_ok && start_proxy_for(app, state, lifecycle, &candidate, Some(&trace)).is_ok();

    match decide_switch(scratch_ok, real_healthy) {
        SwitchOutcome::Commit => {
            if is_edit {
                config::write_rolling_backup(&dir).ok();
            }
            if let Err(e) = config::update(&dir, |c| {
                c.active_id = id.to_string();
                if let Some(edit) = conn_edit {
                    if let Some(p) = c.profile_by_id_mut(id) {
                        edit.apply(p);
                    }
                }
            }) {
                trace.stage(OperationStage::Rollback, "reason=config_write_failed");
                let restored = restore_proxy_for_active(
                    app,
                    state,
                    lifecycle,
                    &cfg,
                    &old_active,
                    Some(&trace),
                );
                trace.finish(format!("error=config_write_failed restored={restored}"));
                return Err(format!(
                    "校验通过、代理已起，但写盘失败（{e}），{}。请检查磁盘空间/权限后重试。",
                    rollback_status_clause(restored)
                ));
            }
            let hint = if is_edit {
                format!("已保存并应用「{}」的新连接。", candidate.name)
            } else {
                format!("已切到「{}」。", candidate.name)
            };
            trace.stage(OperationStage::Commit, "ok");
            trace.finish("committed=true");
            Ok(json!({ "committed": true, "active_id": id, "hint": hint }))
        }
        SwitchOutcome::RollbackToOld => {
            trace.stage(OperationStage::Rollback, "reason=proxy_unhealthy");
            let restored =
                restore_proxy_for_active(app, state, lifecycle, &cfg, &old_active, Some(&trace));
            let clause = rollback_status_clause(restored);
            trace.finish(format!("rollback restored={restored}"));
            if is_edit {
                Err(format!(
                    "连接已校验通过，但正式代理启动/探活失败，连接未保存，{clause}。"
                ))
            } else {
                Err(format!(
                    "候选配置校验通过，但正式代理启动/探活失败，{clause}。"
                ))
            }
        }
        SwitchOutcome::AbortBeforeStart => {
            trace.finish("aborted_before_start");
            if is_edit {
                Err("连接上游校验失败（key/base_url/网络？），连接未保存。".into())
            } else {
                Err("候选上游校验失败（key/base_url/网络？），未切换。".into())
            }
        }
    }
}

fn restore_proxy_for_active(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    cfg: &config::Config,
    old_active: &str,
    trace: Option<&OperationTrace>,
) -> bool {
    if old_active.is_empty() {
        return true;
    }
    match cfg.profile_by_id(old_active) {
        Some(old) => {
            lifecycle.bump_generation();
            start_proxy_for(app, state, lifecycle, old, trace).is_ok()
        }
        None => false,
    }
}
