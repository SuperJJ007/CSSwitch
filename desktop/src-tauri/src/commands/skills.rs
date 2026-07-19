use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::runtime::capability_catalog::{load_static_catalog, CapabilityCatalog};
use crate::runtime::science::{
    sandbox_data_dir, sandbox_science_state, sandbox_science_version, SandboxScienceState,
};
use crate::skill_manager::compatibility::{
    evaluate_compatibility_gate, BooleanCapability, CapabilityAvailability, CompatibilityGate,
    LocalCommandPolicy, NetworkMode, RuntimeContext, SandboxState, SshCapabilitySummary,
};
use crate::skill_manager::deployment::ReconcileReport;
use crate::skill_manager::discovery::{ScienceProbeState, SkillManagerStatus};
use crate::skill_manager::error::{SkillErrorCode, SkillManagerError, SkillResult};
#[cfg(test)]
use crate::skill_manager::external::scan_named_external_home_skill_for_test;
use crate::skill_manager::external::{scan_external_home_skills, ExternalSkillScanReport};
use crate::skill_manager::inspection::InspectionSummary;
use crate::skill_manager::model::{InstalledSkill, SkillId};
use crate::skill_manager::store::{EnableOutcome, InstallOutcome, SkillManager, UninstallOutcome};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillCommandError {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) remediation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) skill_id: Option<SkillId>,
}

impl From<SkillManagerError> for SkillCommandError {
    fn from(error: SkillManagerError) -> Self {
        Self {
            code: error.code.as_str().to_string(),
            message: error.message,
            remediation: error.remediation,
            skill_id: error.skill_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillCommandResult<T> {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<SkillCommandError>,
}

impl<T> SkillCommandResult<T> {
    fn success(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    fn failure(error: SkillManagerError) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }

    fn from_result(result: SkillResult<T>) -> Self {
        match result {
            Ok(data) => Self::success(data),
            Err(error) => Self::failure(error),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InspectSkillSourceInput {
    pub(crate) source_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImportSkillInput {
    pub(crate) source_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdateSkillInput {
    pub(crate) skill_id: String,
    pub(crate) source_path: String,
    #[serde(default)]
    pub(crate) allow_downgrade: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RevealSkillFolderInput {
    pub(crate) skill_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetSkillEnabledInput {
    pub(crate) skill_id: String,
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) acknowledged_rule_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EvaluateSkillCompatibilityInput {
    pub(crate) skill_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UninstallSkillInput {
    pub(crate) skill_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReconcileSkillsInput {
    #[serde(default)]
    pub(crate) dry_run: bool,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillMutationDto {
    pub(crate) skill: InstalledSkill,
    pub(crate) changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillCompatibilityDto {
    pub(crate) skill_id: SkillId,
    pub(crate) gate: CompatibilityGate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StaticCatalogDiagnosticsDto {
    pub(crate) available: bool,
    pub(crate) evaluations: Vec<SkillCompatibilityDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillManagerStatusDto {
    pub(crate) dynamic: SkillManagerStatus,
    pub(crate) catalog: StaticCatalogDiagnosticsDto,
}

impl From<InstallOutcome> for SkillMutationDto {
    fn from(outcome: InstallOutcome) -> Self {
        Self {
            skill: outcome.skill,
            changed: outcome.changed,
        }
    }
}

impl From<EnableOutcome> for SkillMutationDto {
    fn from(outcome: EnableOutcome) -> Self {
        Self {
            skill: outcome.skill,
            changed: outcome.changed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UninstallSkillDto {
    pub(crate) skill_id: SkillId,
    pub(crate) changed: bool,
    pub(crate) runtime_removed: bool,
    pub(crate) store_gc_pending: bool,
}

impl From<UninstallOutcome> for UninstallSkillDto {
    fn from(outcome: UninstallOutcome) -> Self {
        Self {
            skill_id: outcome.skill_id,
            changed: outcome.changed,
            runtime_removed: outcome.runtime_removed,
            store_gc_pending: outcome.store_gc_pending,
        }
    }
}

fn manager() -> SkillManager {
    SkillManager::new(config::default_dir())
}

async fn run_skill_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> SkillResult<T> + Send + 'static,
) -> SkillCommandResult<T> {
    match tauri::async_runtime::spawn_blocking(operation).await {
        Ok(result) => SkillCommandResult::from_result(result),
        Err(_) => SkillCommandResult::failure(SkillManagerError::new(
            SkillErrorCode::Internal,
            "Skill Manager 后台任务未完成",
            "请重试；若持续失败，请运行诊断并报告问题",
        )),
    }
}

#[tauri::command]
pub(crate) async fn list_skills() -> SkillCommandResult<Vec<InstalledSkill>> {
    run_skill_blocking(|| list_skills_inner(&manager())).await
}

#[tauri::command]
pub(crate) async fn inspect_skill_source(
    input: InspectSkillSourceInput,
) -> SkillCommandResult<InspectionSummary> {
    run_skill_blocking(move || inspect_skill_source_inner(&manager(), &input)).await
}

#[tauri::command]
pub(crate) async fn import_skill(input: ImportSkillInput) -> SkillCommandResult<SkillMutationDto> {
    run_skill_blocking(move || import_skill_inner(&manager(), &input)).await
}

#[tauri::command]
pub(crate) async fn update_skill(input: UpdateSkillInput) -> SkillCommandResult<SkillMutationDto> {
    run_skill_blocking(move || update_skill_inner(&manager(), &input)).await
}

#[tauri::command]
pub(crate) async fn reveal_skill_folder(input: RevealSkillFolderInput) -> SkillCommandResult<()> {
    run_skill_blocking(move || reveal_skill_folder_inner(&manager(), &input, reveal_in_finder))
        .await
}

#[tauri::command]
pub(crate) async fn set_skill_enabled(
    input: SetSkillEnabledInput,
) -> SkillCommandResult<SkillMutationDto> {
    if !input.enabled {
        return run_skill_blocking(move || disable_skill_inner(&manager(), &input)).await;
    }
    run_skill_blocking(move || {
        let config_dir = config::default_dir();
        let cfg = config::load_from(&config_dir).map_err(|_| runtime_context_error())?;
        set_skill_enabled_inner_with_state(
            &manager(),
            &input,
            sandbox_science_version().as_deref(),
            &cfg.mode,
            science_sandbox_state(sandbox_science_state(cfg.sandbox_port)),
            &load_catalog()?,
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn evaluate_skill_compatibility(
    input: EvaluateSkillCompatibilityInput,
) -> SkillCommandResult<SkillCompatibilityDto> {
    run_skill_blocking(move || {
        let config_dir = config::default_dir();
        let cfg = config::load_from(&config_dir).map_err(|_| runtime_context_error())?;
        evaluate_skill_compatibility_inner_with_state(
            &manager(),
            &input,
            sandbox_science_version().as_deref(),
            &cfg.mode,
            science_sandbox_state(sandbox_science_state(cfg.sandbox_port)),
            &load_catalog()?,
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn uninstall_skill(
    input: UninstallSkillInput,
    lifecycle: tauri::State<'_, crate::SharedLifecycle>,
) -> Result<SkillCommandResult<UninstallSkillDto>, ()> {
    let lifecycle = lifecycle.inner().clone();
    Ok(run_skill_blocking(move || {
        with_stopped_science_mutation(
            lifecycle.as_ref(),
            || {
                let config_dir = config::default_dir();
                let cfg = config::load_from(&config_dir).map_err(|_| runtime_context_error())?;
                Ok(sandbox_science_state(cfg.sandbox_port))
            },
            || uninstall_skill_inner(&manager(), &sandbox_data_dir(), &input),
        )
    })
    .await)
}

#[tauri::command]
pub(crate) async fn reconcile_skills(
    input: ReconcileSkillsInput,
    lifecycle: tauri::State<'_, crate::SharedLifecycle>,
) -> Result<SkillCommandResult<ReconcileReport>, ()> {
    let lifecycle = lifecycle.inner().clone();
    Ok(run_skill_blocking(move || {
        let operation = || {
            let config_dir = config::default_dir();
            let cfg = config::load_from(&config_dir).map_err(|_| runtime_context_error())?;
            let sandbox_state = sandbox_science_state(cfg.sandbox_port);
            if !input.dry_run {
                require_science_stopped(sandbox_state)?;
            }
            reconcile_skills_inner_with_state(
                &manager(),
                &sandbox_data_dir(),
                &input,
                sandbox_science_version().as_deref(),
                &cfg.mode,
                match sandbox_state {
                    SandboxScienceState::RunningHealthy => ScienceProbeState::Running,
                    SandboxScienceState::Stopped => ScienceProbeState::NotRunning,
                    SandboxScienceState::Unknown => ScienceProbeState::Unknown,
                },
                &load_catalog()?,
            )
        };
        if input.dry_run {
            operation()
        } else {
            lifecycle.with_serialized(operation)
        }
    })
    .await)
}

fn require_science_stopped(state: SandboxScienceState) -> SkillResult<()> {
    match state {
        SandboxScienceState::Stopped => Ok(()),
        SandboxScienceState::RunningHealthy => Err(SkillManagerError::new(
            SkillErrorCode::DeploymentConflict,
            "隔离 Science 运行时禁止修改 Skill 运行副本",
            "先停止隔离 Science，再重试 Skill 卸载或 reconcile",
        )),
        SandboxScienceState::Unknown => Err(SkillManagerError::new(
            SkillErrorCode::DeploymentConflict,
            "无法确认隔离 Science 已停止，已拒绝修改 Skill 运行副本",
            "确认隔离 Science 状态和端口所有权后再重试",
        )),
    }
}

fn with_stopped_science_mutation<T>(
    lifecycle: &crate::lifecycle::Lifecycle,
    probe: impl FnOnce() -> SkillResult<SandboxScienceState>,
    operation: impl FnOnce() -> SkillResult<T>,
) -> SkillResult<T> {
    lifecycle.with_serialized(|| {
        require_science_stopped(probe()?)?;
        operation()
    })
}

#[tauri::command]
pub(crate) async fn get_skill_manager_status() -> SkillCommandResult<SkillManagerStatusDto> {
    run_skill_blocking(|| {
        let config_dir = config::default_dir();
        let science_state = match sandbox_science_state(
            config::load_from(&config_dir)
                .map_err(|_| {
                    SkillManagerError::new(
                        SkillErrorCode::Internal,
                        "读取 CSSwitch 配置失败",
                        "请修复配置后重试",
                    )
                })?
                .sandbox_port,
        ) {
            SandboxScienceState::RunningHealthy => ScienceProbeState::Running,
            SandboxScienceState::Stopped => ScienceProbeState::NotRunning,
            SandboxScienceState::Unknown => ScienceProbeState::Unknown,
        };
        let version = sandbox_science_version();
        get_skill_manager_status_inner(
            &manager(),
            &sandbox_data_dir(),
            science_state,
            version.as_deref(),
            &config::load_from(&config_dir)
                .map_err(|_| runtime_context_error())?
                .mode,
        )
    })
    .await
}

fn get_skill_manager_status_inner(
    manager: &SkillManager,
    data_dir: &Path,
    science_state: ScienceProbeState,
    science_version: Option<&str>,
    runtime_mode: &str,
) -> SkillResult<SkillManagerStatusDto> {
    get_skill_manager_status_with_catalog(
        manager,
        data_dir,
        science_state,
        science_version,
        runtime_mode,
        load_static_catalog(),
    )
}

fn get_skill_manager_status_with_catalog(
    manager: &SkillManager,
    data_dir: &Path,
    science_state: ScienceProbeState,
    science_version: Option<&str>,
    runtime_mode: &str,
    catalog_result: Result<CapabilityCatalog, String>,
) -> SkillResult<SkillManagerStatusDto> {
    let dynamic = manager.status(data_dir, science_state, science_version)?;
    let inventory = manager.load_inventory()?;
    let catalog = match catalog_result {
        Ok(catalog) => catalog,
        Err(_) => {
            return Ok(SkillManagerStatusDto {
                dynamic,
                catalog: StaticCatalogDiagnosticsDto {
                    available: false,
                    evaluations: Vec::new(),
                    error_code: Some("COMPATIBILITY_CATALOG_INVALID".to_string()),
                },
            })
        }
    };
    let mut evaluations = Vec::with_capacity(inventory.skills.len());
    for skill in &inventory.skills {
        let dynamic_skill = dynamic
            .skills
            .iter()
            .find(|status| status.skill_id == skill.skill_id)
            .ok_or_else(runtime_context_error)?;
        let context = runtime_context(
            skill,
            science_version,
            runtime_mode,
            science_probe_sandbox_state(science_state),
            dynamic_deployment_status(dynamic_skill, &dynamic),
            dynamic_skill.discovery_status,
        );
        let gate = evaluate_compatibility_gate(skill, &context, &catalog)
            .map_err(|_| compatibility_catalog_error())?;
        evaluations.push(SkillCompatibilityDto {
            skill_id: skill.skill_id.clone(),
            gate,
        });
    }
    Ok(SkillManagerStatusDto {
        dynamic,
        catalog: StaticCatalogDiagnosticsDto {
            available: true,
            evaluations,
            error_code: None,
        },
    })
}

fn list_skills_inner(manager: &SkillManager) -> SkillResult<Vec<InstalledSkill>> {
    let inventory = manager.load_inventory()?;
    for skill in &inventory.skills {
        manager.verify_skill_store(skill)?;
    }
    Ok(inventory.skills)
}

fn inspect_skill_source_inner(
    manager: &SkillManager,
    input: &InspectSkillSourceInput,
) -> SkillResult<InspectionSummary> {
    manager
        .inspect(Path::new(&input.source_path))
        .map(|inspection| inspection.summary)
}

fn import_skill_inner(
    manager: &SkillManager,
    input: &ImportSkillInput,
) -> SkillResult<SkillMutationDto> {
    manager
        .import_source(Path::new(&input.source_path))
        .map(Into::into)
}

fn update_skill_inner(
    manager: &SkillManager,
    input: &UpdateSkillInput,
) -> SkillResult<SkillMutationDto> {
    let skill_id = SkillId::parse(&input.skill_id).map_err(|_| skill_not_found())?;
    manager
        .update_source(
            &skill_id,
            Path::new(&input.source_path),
            input.allow_downgrade,
        )
        .map(Into::into)
}

fn reveal_skill_folder_inner(
    manager: &SkillManager,
    input: &RevealSkillFolderInput,
    reveal: impl FnOnce(&Path) -> SkillResult<()>,
) -> SkillResult<()> {
    let skill_id = SkillId::parse(&input.skill_id).map_err(|_| skill_not_found())?;
    let inventory = manager.load_inventory()?;
    let skill = inventory
        .skills
        .iter()
        .find(|skill| skill.skill_id == skill_id)
        .ok_or_else(skill_not_found)?;
    manager.verify_skill_store(skill)?;
    let payload = manager.paths.payload(&skill.skill_id, &skill.content_hash);
    reveal(&payload)
}

fn set_skill_enabled_inner_with_state(
    manager: &SkillManager,
    input: &SetSkillEnabledInput,
    science_version: Option<&str>,
    runtime_mode: &str,
    sandbox_state: SandboxState,
    catalog: &CapabilityCatalog,
) -> SkillResult<SkillMutationDto> {
    let skill_id = SkillId::parse(&input.skill_id).map_err(|_| skill_not_found())?;
    let inventory = manager.load_inventory()?;
    let skill = inventory
        .skills
        .iter()
        .find(|skill| skill.skill_id == skill_id)
        .ok_or_else(skill_not_found)?;
    let context = runtime_context(
        skill,
        science_version,
        runtime_mode,
        sandbox_state,
        crate::skill_manager::model::DeploymentStatus::Pending,
        crate::skill_manager::model::DiscoveryStatus::Unknown,
    );
    manager
        .set_enabled_with_compatibility(
            &skill_id,
            input.enabled,
            &input.acknowledged_rule_ids,
            &context,
            catalog,
        )
        .map(Into::into)
}

fn disable_skill_inner(
    manager: &SkillManager,
    input: &SetSkillEnabledInput,
) -> SkillResult<SkillMutationDto> {
    let skill_id = SkillId::parse(&input.skill_id).map_err(|_| skill_not_found())?;
    manager.set_enabled(&skill_id, false).map(Into::into)
}

#[cfg(test)]
fn set_skill_enabled_inner(
    manager: &SkillManager,
    input: &SetSkillEnabledInput,
    science_version: Option<&str>,
    runtime_mode: &str,
    catalog: &CapabilityCatalog,
) -> SkillResult<SkillMutationDto> {
    set_skill_enabled_inner_with_state(
        manager,
        input,
        science_version,
        runtime_mode,
        SandboxState::Ready,
        catalog,
    )
}

fn evaluate_skill_compatibility_inner_with_state(
    manager: &SkillManager,
    input: &EvaluateSkillCompatibilityInput,
    science_version: Option<&str>,
    runtime_mode: &str,
    sandbox_state: SandboxState,
    catalog: &CapabilityCatalog,
) -> SkillResult<SkillCompatibilityDto> {
    let skill_id = SkillId::parse(&input.skill_id).map_err(|_| skill_not_found())?;
    let inventory = manager.load_inventory()?;
    let skill = inventory
        .skills
        .iter()
        .find(|skill| skill.skill_id == skill_id)
        .ok_or_else(skill_not_found)?;
    let context = runtime_context(
        skill,
        science_version,
        runtime_mode,
        sandbox_state,
        crate::skill_manager::model::DeploymentStatus::Pending,
        crate::skill_manager::model::DiscoveryStatus::Unknown,
    );
    let gate = evaluate_compatibility_gate(skill, &context, catalog)
        .map_err(|_| compatibility_catalog_error())?;
    Ok(SkillCompatibilityDto { skill_id, gate })
}

#[cfg(test)]
fn evaluate_skill_compatibility_inner(
    manager: &SkillManager,
    input: &EvaluateSkillCompatibilityInput,
    science_version: Option<&str>,
    runtime_mode: &str,
    catalog: &CapabilityCatalog,
) -> SkillResult<SkillCompatibilityDto> {
    evaluate_skill_compatibility_inner_with_state(
        manager,
        input,
        science_version,
        runtime_mode,
        SandboxState::Ready,
        catalog,
    )
}

fn uninstall_skill_inner(
    manager: &SkillManager,
    data_dir: &Path,
    input: &UninstallSkillInput,
) -> SkillResult<UninstallSkillDto> {
    let skill_id = SkillId::parse(&input.skill_id).map_err(|_| skill_not_found())?;
    manager.uninstall(&skill_id, data_dir).map(Into::into)
}

fn reconcile_skills_inner_with_state(
    manager: &SkillManager,
    data_dir: &Path,
    input: &ReconcileSkillsInput,
    science_version: Option<&str>,
    runtime_mode: &str,
    science_state: ScienceProbeState,
    catalog: &CapabilityCatalog,
) -> SkillResult<ReconcileReport> {
    let dynamic = manager.status(data_dir, science_state, science_version)?;
    manager.reconcile_with_compatibility(data_dir, input.dry_run, &input.reason, catalog, |skill| {
        let status = dynamic
            .skills
            .iter()
            .find(|status| status.skill_id == skill.skill_id)
            .ok_or_else(runtime_context_error)?;
        Ok(runtime_context(
            skill,
            science_version,
            runtime_mode,
            science_probe_sandbox_state(science_state),
            dynamic_deployment_status(status, &dynamic),
            status.discovery_status,
        ))
    })
}

#[cfg(test)]
fn reconcile_skills_inner(
    manager: &SkillManager,
    data_dir: &Path,
    input: &ReconcileSkillsInput,
    science_version: Option<&str>,
    runtime_mode: &str,
    catalog: &CapabilityCatalog,
) -> SkillResult<ReconcileReport> {
    reconcile_skills_inner_with_state(
        manager,
        data_dir,
        input,
        science_version,
        runtime_mode,
        ScienceProbeState::NotRunning,
        catalog,
    )
}

pub(crate) fn reconcile_skills_for_runtime(
    manager: &SkillManager,
    data_dir: &Path,
    dry_run: bool,
    reason: &str,
    science_version: Option<&str>,
    runtime_mode: &str,
    science_state: ScienceProbeState,
) -> SkillResult<ReconcileReport> {
    reconcile_skills_inner_with_state(
        manager,
        data_dir,
        &ReconcileSkillsInput {
            dry_run,
            reason: reason.to_string(),
        },
        science_version,
        runtime_mode,
        science_state,
        &load_catalog()?,
    )
}

pub(crate) struct RuntimeSkillReconcileContext<'a> {
    pub(crate) external_root: &'a Path,
    pub(crate) data_dir: &'a Path,
    pub(crate) dry_run: bool,
    pub(crate) reason: &'a str,
    pub(crate) science_version: Option<&'a str>,
    pub(crate) runtime_mode: &'a str,
    pub(crate) science_state: ScienceProbeState,
}

pub(crate) fn scan_and_reconcile_skills_for_runtime(
    manager: &SkillManager,
    context: RuntimeSkillReconcileContext<'_>,
) -> SkillResult<(ExternalSkillScanReport, ReconcileReport)> {
    manager.recover_store_orphans(context.data_dir)?;
    let scan = scan_external_home_skills(manager, context.external_root)?;
    let reconcile = reconcile_skills_for_runtime(
        manager,
        context.data_dir,
        context.dry_run,
        context.reason,
        context.science_version,
        context.runtime_mode,
        context.science_state,
    )?;
    Ok((scan, reconcile))
}

#[cfg(test)]
pub(crate) fn scan_named_and_reconcile_skills_for_test(
    manager: &SkillManager,
    external_root: &Path,
    directory_name: &str,
    data_dir: &Path,
    reason: &str,
    science_version: Option<&str>,
) -> SkillResult<(ExternalSkillScanReport, ReconcileReport)> {
    manager.recover_store_orphans(data_dir)?;
    let scan = scan_named_external_home_skill_for_test(manager, external_root, directory_name)?;
    let reconcile = reconcile_skills_for_runtime(
        manager,
        data_dir,
        false,
        reason,
        science_version,
        "proxy",
        ScienceProbeState::NotRunning,
    )?;
    Ok((scan, reconcile))
}

fn runtime_context(
    _skill: &InstalledSkill,
    science_version: Option<&str>,
    runtime_mode: &str,
    sandbox_state: SandboxState,
    deployment_status: crate::skill_manager::model::DeploymentStatus,
    discovery_status: crate::skill_manager::model::DiscoveryStatus,
) -> RuntimeContext {
    let (network_mode, network) = match runtime_mode {
        "proxy" => (NetworkMode::Gateway, CapabilityAvailability::Unknown),
        "official" => (NetworkMode::Inherit, CapabilityAvailability::Unknown),
        _ => (NetworkMode::Unknown, CapabilityAvailability::Unknown),
    };
    RuntimeContext {
        science_version: science_version.map(str::to_string),
        platform: if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "windows"
        }
        .to_string(),
        sandbox_state,
        deployment_status,
        discovery_status,
        network_mode,
        network,
        mcp: CapabilityAvailability::Unknown,
        local_command_policy: LocalCommandPolicy::Unknown,
        ssh: SshCapabilitySummary {
            transport: CapabilityAvailability::Unknown,
            agent_visible: BooleanCapability::Unknown,
            config_available: BooleanCapability::Unknown,
        },
        available_binaries: BTreeSet::new(),
        binary_inventory: CapabilityAvailability::Unknown,
        available_environment: BTreeSet::new(),
        environment_inventory: CapabilityAvailability::Unknown,
        available_runtime_assets: BTreeSet::new(),
        runtime_asset_inventory: CapabilityAvailability::Unknown,
    }
}

fn science_sandbox_state(state: SandboxScienceState) -> SandboxState {
    match state {
        SandboxScienceState::RunningHealthy => SandboxState::Ready,
        SandboxScienceState::Stopped => SandboxState::Unknown,
        SandboxScienceState::Unknown => SandboxState::Unavailable,
    }
}

fn science_probe_sandbox_state(state: ScienceProbeState) -> SandboxState {
    match state {
        ScienceProbeState::Running => SandboxState::Ready,
        ScienceProbeState::NotRunning => SandboxState::Unknown,
        ScienceProbeState::Unknown => SandboxState::Unavailable,
    }
}

fn dynamic_deployment_status(
    status: &crate::skill_manager::discovery::SkillRuntimeStatus,
    manager_status: &SkillManagerStatus,
) -> crate::skill_manager::model::DeploymentStatus {
    if status.deployed {
        crate::skill_manager::model::DeploymentStatus::Deployed
    } else if status.enabled && !manager_status.diagnostic_codes.is_empty() {
        crate::skill_manager::model::DeploymentStatus::Failed
    } else if status.enabled {
        crate::skill_manager::model::DeploymentStatus::Pending
    } else {
        crate::skill_manager::model::DeploymentStatus::NotDeployed
    }
}

fn load_catalog() -> SkillResult<CapabilityCatalog> {
    load_static_catalog().map_err(|_| compatibility_catalog_error())
}

fn reveal_in_finder(path: &Path) -> SkillResult<()> {
    let status = reveal_command(path)
        .status()
        .map_err(|_| reveal_rejected())?;
    if !status.success() {
        return Err(reveal_rejected());
    }
    Ok(())
}

fn reveal_command(path: &Path) -> Command {
    let mut command = Command::new(crate::runtime::platform::browser_open_bin());
    #[cfg(target_os = "macos")]
    command.arg("-R");
    command.arg(path);
    command
}

fn skill_not_found() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::SkillNotFound,
        "找不到指定的 CSSwitch Skill",
        "请刷新 Skill 列表后重试",
    )
}

fn compatibility_catalog_error() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::CompatibilityCatalogInvalid,
        "Skill 兼容性 catalog 未通过完整性校验",
        "恢复随应用发布的 capability catalog 后重试；现有库存未被修改",
    )
}

fn runtime_context_error() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::CompatibilityAcknowledgmentRequired,
        "无法建立安全的 Skill 运行能力摘要",
        "刷新 CSSwitch 配置与运行状态后重新评估兼容性",
    )
}

fn reveal_rejected() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::RevealRejected,
        "只能显示经过所有权与内容校验的 CSSwitch Skill 目录",
        "请运行 Skill Manager 诊断并修复存储冲突",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    static TEST_SERIAL: &std::sync::Mutex<()> = &crate::skill_manager::store::TEST_OPERATION_LOCK;
    const TEST_ORG: &str = "12345678-1234-4234-8234-123456789abc";

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = crate::test_temp_root().join(format!(
                "csswitch-skill-command-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn skill() -> Self {
            let dir = Self::new("source");
            fs::write(
                dir.0.join("SKILL.md"),
                "---\nname: Command Probe\ndescription: Command contract probe\n---\nold\n",
            )
            .unwrap();
            dir
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn prepare_active_org(data_dir: &Path) -> PathBuf {
        let orgs_dir = data_dir.join("orgs");
        let org_dir = orgs_dir.join(TEST_ORG);
        fs::create_dir_all(&org_dir).unwrap();
        for directory in [data_dir, orgs_dir.as_path(), org_dir.as_path()] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let active_org = data_dir.join("active-org.json");
        fs::write(
            &active_org,
            serde_json::to_vec(&serde_json::json!({ "org_uuid": TEST_ORG })).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&active_org, fs::Permissions::from_mode(0o600)).unwrap();
        org_dir.join("skills")
    }

    fn current_acknowledgments(
        manager: &SkillManager,
        skill_id: &SkillId,
        catalog: &CapabilityCatalog,
    ) -> Vec<String> {
        evaluate_skill_compatibility_inner(
            manager,
            &EvaluateSkillCompatibilityInput {
                skill_id: skill_id.as_str().to_string(),
            },
            Some("0.1.18"),
            "proxy",
            catalog,
        )
        .unwrap()
        .gate
        .required_rule_ids
    }

    #[test]
    fn runtime_mutations_fail_closed_without_calling_store_operation() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = TestDir::new("guarded-mutation-config");
        let data = TestDir::new("guarded-mutation-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        let data_dir = data.0.join("science");
        let shared_root = prepare_active_org(&data_dir);
        fs::create_dir(&shared_root).unwrap();
        fs::set_permissions(&shared_root, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = shared_root.join(&installed.runtime_name);
        fs::create_dir(&runtime).unwrap();
        fs::write(runtime.join("must-remain.txt"), b"untouched").unwrap();
        let inventory_before = manager.load_inventory().unwrap();

        for state in [
            SandboxScienceState::RunningHealthy,
            SandboxScienceState::Unknown,
        ] {
            let lifecycle = crate::lifecycle::Lifecycle::new();
            let operation_called = std::sync::atomic::AtomicBool::new(false);
            let result = with_stopped_science_mutation(
                &lifecycle,
                || Ok(state),
                || {
                    operation_called.store(true, Ordering::SeqCst);
                    uninstall_skill_inner(
                        &manager,
                        &data_dir,
                        &UninstallSkillInput {
                            skill_id: installed.skill_id.as_str().to_string(),
                        },
                    )
                },
            );
            assert_eq!(result.unwrap_err().code, SkillErrorCode::DeploymentConflict);
            assert!(!operation_called.load(Ordering::SeqCst));
            assert_eq!(manager.load_inventory().unwrap(), inventory_before);
            assert_eq!(
                fs::read(runtime.join("must-remain.txt")).unwrap(),
                b"untouched"
            );
            assert!(manager
                .paths
                .store
                .join(installed.skill_id.as_str())
                .exists());
        }
    }

    #[test]
    fn runtime_mutation_probe_is_serialized_after_concurrent_start() {
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let runtime_state = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let operation_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (start_entered_tx, start_entered_rx) = mpsc::channel();
        let (release_start_tx, release_start_rx) = mpsc::channel();

        let start_lifecycle = lifecycle.clone();
        let start_state = runtime_state.clone();
        let start = std::thread::spawn(move || {
            start_lifecycle.with_serialized(|| {
                start_state.store(1, Ordering::SeqCst);
                start_entered_tx.send(()).unwrap();
                release_start_rx.recv().unwrap();
            });
        });
        start_entered_rx.recv().unwrap();

        let mutation_lifecycle = lifecycle.clone();
        let mutation_state = runtime_state.clone();
        let mutation_called = operation_called.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let mutation = std::thread::spawn(move || {
            let result = with_stopped_science_mutation(
                mutation_lifecycle.as_ref(),
                || {
                    Ok(if mutation_state.load(Ordering::SeqCst) == 1 {
                        SandboxScienceState::RunningHealthy
                    } else {
                        SandboxScienceState::Stopped
                    })
                },
                || {
                    mutation_called.store(true, Ordering::SeqCst);
                    Ok(())
                },
            );
            result_tx.send(result).unwrap();
        });

        assert!(result_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_start_tx.send(()).unwrap();
        let result = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result.unwrap_err().code, SkillErrorCode::DeploymentConflict);
        assert!(!operation_called.load(Ordering::SeqCst));
        start.join().unwrap();
        mutation.join().unwrap();
    }

    #[test]
    fn command_result_contract_has_exact_success_and_error_shapes() {
        let success = serde_json::to_value(SkillCommandResult::success(vec!["ok"])).unwrap();
        assert_eq!(success["ok"], true);
        assert_eq!(success["data"][0], "ok");
        assert!(success.get("error").is_none());

        let failure: SkillCommandResult<()> = SkillCommandResult::failure(skill_not_found());
        let failure = serde_json::to_value(failure).unwrap();
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["error"]["code"], "SKILL_NOT_FOUND");
        assert!(failure.get("data").is_none());
    }

    #[test]
    fn inspect_does_not_write_and_import_revalidates_changed_source() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("config");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let inspected = inspect_skill_source_inner(
            &manager,
            &InspectSkillSourceInput {
                source_path: source.0.to_string_lossy().to_string(),
            },
        )
        .unwrap();
        assert!(!manager.paths.root.exists());

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Command Probe\ndescription: Command contract probe\n---\nnew\n",
        )
        .unwrap();
        let imported = import_skill_inner(
            &manager,
            &ImportSkillInput {
                source_path: source.0.to_string_lossy().to_string(),
            },
        )
        .unwrap();
        assert_ne!(inspected.content_hash, imported.skill.content_hash);
    }

    #[test]
    fn duplicate_import_update_failure_and_list_are_stable() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("config");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let input = ImportSkillInput {
            source_path: source.0.to_string_lossy().to_string(),
        };
        let first = import_skill_inner(&manager, &input).unwrap();
        let second = import_skill_inner(&manager, &input).unwrap();
        assert!(first.changed);
        assert!(!second.changed);
        assert_eq!(list_skills_inner(&manager).unwrap().len(), 1);

        let error = update_skill_inner(
            &manager,
            &UpdateSkillInput {
                skill_id: "invalid".to_string(),
                source_path: source.0.to_string_lossy().to_string(),
                allow_downgrade: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::SkillNotFound);

        let missing_id = "sk_ffffffffffffffffffffffffffffffff";
        let error = update_skill_inner(
            &manager,
            &UpdateSkillInput {
                skill_id: missing_id.to_string(),
                source_path: source.0.to_string_lossy().to_string(),
                allow_downgrade: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::SkillNotFound);
        assert_eq!(error.skill_id.unwrap().as_str(), missing_id);
        assert!(error.remediation.contains("刷新"));
    }

    #[test]
    fn reveal_resolves_only_verified_owned_payload_without_opening_finder() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("config");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        let mut revealed = PathBuf::new();
        reveal_skill_folder_inner(
            &manager,
            &RevealSkillFolderInput {
                skill_id: installed.skill_id.as_str().to_string(),
            },
            |path| {
                revealed = path.to_path_buf();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            revealed,
            manager
                .paths
                .payload(&installed.skill_id, &installed.content_hash)
        );

        fs::write(revealed.join("SKILL.md"), b"tampered").unwrap();
        let error = reveal_skill_folder_inner(
            &manager,
            &RevealSkillFolderInput {
                skill_id: installed.skill_id.as_str().to_string(),
            },
            |_| panic!("unverified path must not be revealed"),
        )
        .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::StoreConflict);
    }

    #[test]
    fn errors_never_include_source_absolute_path_or_skill_body() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("config");
        let source = TestDir::new("secret-source");
        let secret = "SECRET_SKILL_BODY";
        fs::write(source.0.join("SKILL.md"), secret).unwrap();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let result: SkillCommandResult<SkillMutationDto> =
            SkillCommandResult::from_result(import_skill_inner(
                &manager,
                &ImportSkillInput {
                    source_path: source.0.to_string_lossy().to_string(),
                },
            ));
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains(source.0.to_string_lossy().as_ref()));
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn reveal_command_uses_fixed_system_binary_and_exact_arguments() {
        let path = Path::new("/owned/payload");
        let command = reveal_command(path);
        assert_eq!(
            command.get_program(),
            crate::runtime::platform::browser_open_bin()
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("-R"), path.as_os_str()]
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![path.as_os_str()]
        );
    }

    #[test]
    fn enable_disable_and_uninstall_fail_closed_on_unmanaged_runtime_root() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("config");
        let data = TestDir::new("data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        let catalog = load_static_catalog().unwrap();
        let acknowledged_rule_ids =
            current_acknowledgments(&manager, &installed.skill_id, &catalog);

        let enabled = set_skill_enabled_inner(
            &manager,
            &SetSkillEnabledInput {
                skill_id: installed.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids: acknowledged_rule_ids.clone(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert!(enabled.changed);
        assert!(enabled.skill.enabled);
        assert!(
            !set_skill_enabled_inner(
                &manager,
                &SetSkillEnabledInput {
                    skill_id: installed.skill_id.as_str().to_string(),
                    enabled: true,
                    acknowledged_rule_ids: acknowledged_rule_ids.clone(),
                },
                Some("0.1.18"),
                "proxy",
                &catalog,
            )
            .unwrap()
            .changed
        );
        let disabled = set_skill_enabled_inner(
            &manager,
            &SetSkillEnabledInput {
                skill_id: installed.skill_id.as_str().to_string(),
                enabled: false,
                acknowledged_rule_ids: Vec::new(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert!(disabled.changed);
        assert!(!disabled.skill.enabled);
        assert!(disabled.skill.compatibility_acknowledgment.is_none());
        assert!(
            !set_skill_enabled_inner(
                &manager,
                &SetSkillEnabledInput {
                    skill_id: installed.skill_id.as_str().to_string(),
                    enabled: false,
                    acknowledged_rule_ids: Vec::new(),
                },
                Some("0.1.18"),
                "proxy",
                &catalog,
            )
            .unwrap()
            .changed
        );

        let data_dir = data.0.join("science");
        let shared_root = prepare_active_org(&data_dir);
        fs::create_dir(&shared_root).unwrap();
        fs::set_permissions(&shared_root, fs::Permissions::from_mode(0o700)).unwrap();
        let manual = shared_root.join(&installed.runtime_name);
        fs::create_dir_all(&manual).unwrap();
        fs::set_permissions(&manual, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(manual.join("manual.txt"), b"keep").unwrap();
        let error = uninstall_skill_inner(
            &manager,
            &data_dir,
            &UninstallSkillInput {
                skill_id: installed.skill_id.as_str().to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::DeploymentConflict);
        assert_eq!(fs::read(manual.join("manual.txt")).unwrap(), b"keep");
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 1);
        assert!(manager
            .paths
            .store
            .join(installed.skill_id.as_str())
            .exists());
    }

    #[test]
    fn reconcile_command_dry_run_reports_plan_without_runtime_writes() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("reconcile-config");
        let data = TestDir::new("reconcile-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let catalog = load_static_catalog().unwrap();
        let acknowledged_rule_ids = current_acknowledgments(&manager, &skill.skill_id, &catalog);
        set_skill_enabled_inner(
            &manager,
            &SetSkillEnabledInput {
                skill_id: skill.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids,
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        let data_dir = data.0.join("science");
        let shared_root = prepare_active_org(&data_dir);
        let report = reconcile_skills_inner(
            &manager,
            &data_dir,
            &ReconcileSkillsInput {
                dry_run: true,
                reason: "manual".to_string(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.planned.len(), 1);
        assert!(report.applied.is_empty());
        assert!(report.restart_required);
        assert!(!shared_root.exists());
        let encoded = serde_json::to_value(report).unwrap();
        assert_eq!(encoded["reason"], "manual");
        assert_eq!(encoded["planned"][0]["action"], "deploy");
    }

    #[test]
    fn startup_scan_imports_unknown_external_skill_before_reconcile_without_ack() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("startup-external-config");
        let home = TestDir::new("startup-external-home");
        let external_root = home.0.join(".claude/skills");
        let source = external_root.join("nature-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: nature-skill\ndescription: external startup test\n---\nbody\n",
        )
        .unwrap();
        let data = TestDir::new("startup-external-data");
        let data_dir = data.0.join("science");
        let runtime_root = prepare_active_org(&data_dir);
        let manager = SkillManager::new(config.0.join(".csswitch"));

        let (scan, reconcile) = scan_and_reconcile_skills_for_runtime(
            &manager,
            RuntimeSkillReconcileContext {
                external_root: &external_root,
                data_dir: &data_dir,
                dry_run: false,
                reason: "before_start",
                science_version: Some("0.1.18"),
                runtime_mode: "proxy",
                science_state: ScienceProbeState::NotRunning,
            },
        )
        .unwrap();
        assert_eq!(scan.imported, 1);
        assert!(reconcile.errors.is_empty());
        assert_eq!(reconcile.applied.len(), 1);
        let skill = manager.load_inventory().unwrap().skills.remove(0);
        assert!(skill.enabled);
        assert!(skill.compatibility_acknowledgment.is_none());
        assert!(runtime_root
            .join(&skill.runtime_name)
            .join("SKILL.md")
            .is_file());
    }

    #[test]
    fn status_keeps_deployment_discovery_and_probe_failure_separate() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("status-config");
        let data = TestDir::new("status-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let data_dir = data.0.join("science");
        let shared_root = prepare_active_org(&data_dir);
        manager.reconcile(&data_dir, false, "before_start").unwrap();

        let stopped = get_skill_manager_status_inner(
            &manager,
            &data_dir,
            ScienceProbeState::NotRunning,
            None,
            "proxy",
        )
        .unwrap();
        assert!(stopped.dynamic.skills[0].installed);
        assert!(stopped.dynamic.skills[0].deployed);
        assert_eq!(
            stopped.dynamic.skills[0].discovery_status,
            crate::skill_manager::model::DiscoveryStatus::NotRunning
        );

        let running_unknown = get_skill_manager_status_inner(
            &manager,
            &data_dir,
            ScienceProbeState::Running,
            Some("science-test"),
            "proxy",
        )
        .unwrap();
        assert!(running_unknown.dynamic.skills[0].deployed);
        assert_eq!(
            running_unknown.dynamic.skills[0].discovery_status,
            crate::skill_manager::model::DiscoveryStatus::Unknown
        );

        fs::write(
            shared_root.join(&skill.runtime_name).join("SKILL.md"),
            b"tampered\n",
        )
        .unwrap();
        let failed = get_skill_manager_status_inner(
            &manager,
            &data_dir,
            ScienceProbeState::Unknown,
            None,
            "proxy",
        )
        .unwrap();
        assert!(!failed.dynamic.skills[0].deployed);
        assert_eq!(
            failed.dynamic.skills[0].discovery_status,
            crate::skill_manager::model::DiscoveryStatus::ProbeFailed
        );
        assert!(!failed.dynamic.diagnostic_codes.is_empty());
        assert!(failed.catalog.available);
        assert_eq!(failed.catalog.evaluations.len(), 1);
    }

    #[test]
    fn compatibility_unknown_requires_enable_ack_but_does_not_block_existing_deployment() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("compat-unknown-config");
        let data = TestDir::new("compat-unknown-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let catalog = load_static_catalog().unwrap();

        let no_ack = set_skill_enabled_inner_with_state(
            &manager,
            &SetSkillEnabledInput {
                skill_id: skill.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids: Vec::new(),
            },
            Some("0.1.18"),
            "proxy",
            SandboxState::Unknown,
            &catalog,
        )
        .unwrap_err();
        assert_eq!(
            no_ack.code,
            SkillErrorCode::CompatibilityAcknowledgmentRequired
        );
        assert!(!manager.load_inventory().unwrap().skills[0].enabled);

        let rule_ids = evaluate_skill_compatibility_inner_with_state(
            &manager,
            &EvaluateSkillCompatibilityInput {
                skill_id: skill.skill_id.as_str().to_string(),
            },
            Some("0.1.18"),
            "proxy",
            SandboxState::Unknown,
            &catalog,
        )
        .unwrap()
        .gate
        .required_rule_ids;
        assert!(!rule_ids.is_empty());
        let enabled = set_skill_enabled_inner_with_state(
            &manager,
            &SetSkillEnabledInput {
                skill_id: skill.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids: rule_ids,
            },
            Some("0.1.18"),
            "proxy",
            SandboxState::Unknown,
            &catalog,
        )
        .unwrap();
        assert!(enabled.skill.compatibility_acknowledgment.is_some());

        let data_dir = data.0.join("science");
        prepare_active_org(&data_dir);
        let deployed = reconcile_skills_inner(
            &manager,
            &data_dir,
            &ReconcileSkillsInput {
                dry_run: false,
                reason: "compatibility_test".to_string(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert!(deployed.errors.is_empty());
        assert_eq!(deployed.applied.len(), 1);

        for (version, mode) in [(Some("0.1.19"), "proxy"), (Some("0.1.18"), "official")] {
            let diagnostic_only = reconcile_skills_inner(
                &manager,
                &data_dir,
                &ReconcileSkillsInput {
                    dry_run: false,
                    reason: "stale_context".to_string(),
                },
                version,
                mode,
                &catalog,
            )
            .unwrap();
            assert!(diagnostic_only.errors.is_empty());
            assert!(data_dir
                .join("orgs")
                .join(TEST_ORG)
                .join("skills")
                .join(&skill.runtime_name)
                .join("SKILL.md")
                .is_file());
        }

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Command Probe\ndescription: Command contract probe\n---\nupdated\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&skill.skill_id, &source.0, false)
            .unwrap()
            .skill;
        assert!(updated.enabled);
        assert!(updated.compatibility_acknowledgment.is_none());
        let update_is_deployed_with_diagnostics = reconcile_skills_inner(
            &manager,
            &data_dir,
            &ReconcileSkillsInput {
                dry_run: false,
                reason: "updated_requirements".to_string(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert!(update_is_deployed_with_diagnostics.errors.is_empty());
        assert_eq!(update_is_deployed_with_diagnostics.applied.len(), 1);
    }

    #[test]
    fn compatibility_supported_needs_no_ack_and_unsupported_never_accepts_ack() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let catalog = load_static_catalog().unwrap();

        let supported_config = TestDir::new("compat-supported-config");
        let supported_source = TestDir::skill();
        fs::write(
            supported_source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_network":false,"needs_ssh":false,"needs_mcp":false,"needs_local_command":false,"required_binaries":[],"required_environment":[],"required_runtime_assets":[],"supported_platforms":["macos"],"minimum_runtime_version":"0.1.0"}}"#,
        )
        .unwrap();
        let supported_manager = SkillManager::new(supported_config.0.join(".csswitch"));
        let supported = supported_manager
            .import_source(&supported_source.0)
            .unwrap()
            .skill;
        let before_invalid_catalog = fs::read(&supported_manager.paths.inventory).unwrap();
        let mut invalid_catalog = load_static_catalog().unwrap();
        invalid_catalog.skills.clear();
        let invalid_catalog_error = set_skill_enabled_inner(
            &supported_manager,
            &SetSkillEnabledInput {
                skill_id: supported.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids: Vec::new(),
            },
            Some("0.1.18"),
            "proxy",
            &invalid_catalog,
        )
        .unwrap_err();
        assert_eq!(
            invalid_catalog_error.code,
            SkillErrorCode::CompatibilityCatalogInvalid
        );
        assert_eq!(
            before_invalid_catalog,
            fs::read(&supported_manager.paths.inventory).unwrap()
        );
        let evaluation = evaluate_skill_compatibility_inner(
            &supported_manager,
            &EvaluateSkillCompatibilityInput {
                skill_id: supported.skill_id.as_str().to_string(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert_eq!(
            evaluation.gate.capability_verdict.status,
            crate::skill_manager::compatibility::CompatibilityStatus::Supported
        );
        assert!(!evaluation.gate.required_rule_ids.is_empty());
        assert!(set_skill_enabled_inner(
            &supported_manager,
            &SetSkillEnabledInput {
                skill_id: supported.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids: evaluation.gate.required_rule_ids,
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .is_ok());
        let disabled_without_catalog = disable_skill_inner(
            &supported_manager,
            &SetSkillEnabledInput {
                skill_id: supported.skill_id.as_str().to_string(),
                enabled: false,
                acknowledged_rule_ids: Vec::new(),
            },
        )
        .unwrap();
        assert!(!disabled_without_catalog.skill.enabled);
        assert!(disabled_without_catalog
            .skill
            .compatibility_acknowledgment
            .is_none());

        let unsupported_config = TestDir::new("compat-unsupported-config");
        let unsupported_source = TestDir::skill();
        fs::write(
            unsupported_source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"required_binaries":["missing-runtime-binary"]}}"#,
        )
        .unwrap();
        let unsupported_manager = SkillManager::new(unsupported_config.0.join(".csswitch"));
        let unsupported = unsupported_manager
            .import_source(&unsupported_source.0)
            .unwrap()
            .skill;
        let evaluation = evaluate_skill_compatibility_inner(
            &unsupported_manager,
            &EvaluateSkillCompatibilityInput {
                skill_id: unsupported.skill_id.as_str().to_string(),
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .unwrap();
        assert_eq!(
            evaluation.gate.capability_verdict.status,
            crate::skill_manager::compatibility::CompatibilityStatus::Unknown
        );
        assert!(set_skill_enabled_inner(
            &unsupported_manager,
            &SetSkillEnabledInput {
                skill_id: unsupported.skill_id.as_str().to_string(),
                enabled: true,
                acknowledged_rule_ids: evaluation.gate.required_rule_ids,
            },
            Some("0.1.18"),
            "proxy",
            &catalog,
        )
        .is_ok());
    }

    #[test]
    fn catalog_failure_status_is_separate_and_inventory_is_unchanged() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("catalog-failure-config");
        let data = TestDir::new("catalog-failure-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        manager.import_source(&source.0).unwrap();
        let data_dir = data.0.join("science");
        prepare_active_org(&data_dir);
        let before = fs::read(&manager.paths.inventory).unwrap();
        let status = get_skill_manager_status_with_catalog(
            &manager,
            &data_dir,
            ScienceProbeState::NotRunning,
            Some("0.1.18"),
            "proxy",
            Err("invalid bundled catalog".to_string()),
        )
        .unwrap();
        assert_eq!(status.dynamic.skills.len(), 1);
        assert!(!status.catalog.available);
        assert_eq!(
            status.catalog.error_code.as_deref(),
            Some("COMPATIBILITY_CATALOG_INVALID")
        );
        assert!(status.catalog.evaluations.is_empty());
        assert_eq!(before, fs::read(&manager.paths.inventory).unwrap());
        let encoded = serde_json::to_value(status).unwrap();
        assert!(encoded.get("dynamic").is_some());
        assert!(encoded.get("catalog").is_some());
        assert!(encoded["dynamic"].get("catalog").is_none());
        assert!(encoded["catalog"].get("skills").is_none());
    }

    #[test]
    fn enabled_skill_is_blocked_when_capability_degrades_to_unsupported() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("compat-degrade-config");
        let data = TestDir::new("compat-degrade-data");
        let source = TestDir::skill();
        fs::write(
            source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_network":true,"needs_ssh":false,"needs_mcp":false,"needs_local_command":false,"required_binaries":[],"required_environment":[],"required_runtime_assets":[],"supported_platforms":["macos"],"minimum_runtime_version":"0.1.0"}}"#,
        )
        .unwrap();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let catalog = load_static_catalog().unwrap();
        let mut initial = runtime_context(
            &skill,
            Some("0.1.18"),
            "proxy",
            SandboxState::Ready,
            skill.deployment_status,
            skill.discovery_status,
        );
        initial.network = CapabilityAvailability::Unknown;
        let gate = evaluate_compatibility_gate(&skill, &initial, &catalog).unwrap();
        manager
            .set_enabled_with_compatibility(
                &skill.skill_id,
                true,
                &gate.required_rule_ids,
                &initial,
                &catalog,
            )
            .unwrap();

        let data_dir = data.0.join("science");
        let shared_root = prepare_active_org(&data_dir);
        let degraded = manager
            .reconcile_with_compatibility(
                &data_dir,
                false,
                "capability_degraded",
                &catalog,
                |installed| {
                    let mut context = runtime_context(
                        installed,
                        Some("0.1.18"),
                        "proxy",
                        SandboxState::Ready,
                        installed.deployment_status,
                        installed.discovery_status,
                    );
                    context.network = CapabilityAvailability::Unavailable;
                    Ok(context)
                },
            )
            .unwrap_err();
        assert_eq!(degraded.code, SkillErrorCode::CompatibilityUnsupported);
        assert!(!shared_root.exists());
        let inventory = manager.load_inventory().unwrap();
        assert!(inventory.skills[0].enabled);
        assert_eq!(
            inventory.skills[0].deployment_status,
            crate::skill_manager::model::DeploymentStatus::Pending
        );
    }

    #[test]
    fn full_runtime_unsupported_states_block_enable_and_reconcile() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("full-verdict-config");
        let data = TestDir::new("full-verdict-data");
        let source = TestDir::skill();
        fs::write(
            source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_network":false,"needs_ssh":false,"needs_mcp":false,"needs_local_command":false,"required_binaries":[],"required_environment":[],"required_runtime_assets":[],"supported_platforms":["macos"],"minimum_runtime_version":"0.1.0"}}"#,
        )
        .unwrap();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let catalog = load_static_catalog().unwrap();
        let base = runtime_context(
            &skill,
            Some("0.1.18"),
            "proxy",
            SandboxState::Ready,
            crate::skill_manager::model::DeploymentStatus::NotDeployed,
            crate::skill_manager::model::DiscoveryStatus::Unknown,
        );
        let mut not_discovered = base.clone();
        not_discovered.discovery_status =
            crate::skill_manager::model::DiscoveryStatus::NotDiscovered;
        assert_eq!(
            manager
                .set_enabled_with_compatibility(
                    &skill.skill_id,
                    true,
                    &[],
                    &not_discovered,
                    &catalog,
                )
                .unwrap_err()
                .code,
            SkillErrorCode::CompatibilityUnsupported
        );
        let mut failed = base.clone();
        failed.deployment_status = crate::skill_manager::model::DeploymentStatus::Failed;
        assert_eq!(
            manager
                .set_enabled_with_compatibility(&skill.skill_id, true, &[], &failed, &catalog)
                .unwrap_err()
                .code,
            SkillErrorCode::CompatibilityUnsupported
        );
        let mut unavailable = base.clone();
        unavailable.sandbox_state = SandboxState::Unavailable;
        assert_eq!(
            manager
                .set_enabled_with_compatibility(&skill.skill_id, true, &[], &unavailable, &catalog)
                .unwrap_err()
                .code,
            SkillErrorCode::CompatibilityUnsupported
        );

        manager.set_enabled(&skill.skill_id, true).unwrap();
        let data_dir = data.0.join("science");
        prepare_active_org(&data_dir);
        let dynamic_running = reconcile_skills_inner_with_state(
            &manager,
            &data_dir,
            &ReconcileSkillsInput {
                dry_run: true,
                reason: "dynamic_running".to_string(),
            },
            Some("0.1.18"),
            "proxy",
            ScienceProbeState::Running,
            &catalog,
        )
        .unwrap();
        assert!(dynamic_running.errors.is_empty());
        assert!(dynamic_running.restart_required);
        let error = manager
            .reconcile_with_compatibility(&data_dir, false, "full_unsupported", &catalog, |_| {
                Ok(failed.clone())
            })
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::CompatibilityUnsupported);
    }

    #[test]
    fn enable_action_replay_is_idempotent_but_never_bypasses_capability_changes() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("enable-replay-config");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let catalog = load_static_catalog().unwrap();
        let initial = runtime_context(
            &skill,
            Some("0.1.18"),
            "proxy",
            SandboxState::Ready,
            crate::skill_manager::model::DeploymentStatus::NotDeployed,
            crate::skill_manager::model::DiscoveryStatus::Unknown,
        );
        let gate = evaluate_compatibility_gate(&skill, &initial, &catalog).unwrap();
        let original_action_ids = gate.required_rule_ids.clone();
        assert!(
            manager
                .set_enabled_with_compatibility(
                    &skill.skill_id,
                    true,
                    &original_action_ids,
                    &initial,
                    &catalog,
                )
                .unwrap()
                .changed
        );

        let installed = manager.load_inventory().unwrap().skills.remove(0);
        let pending = runtime_context(
            &installed,
            Some("0.1.18"),
            "proxy",
            SandboxState::Ready,
            crate::skill_manager::model::DeploymentStatus::Pending,
            crate::skill_manager::model::DiscoveryStatus::Unknown,
        );
        assert!(
            !manager
                .set_enabled_with_compatibility(
                    &skill.skill_id,
                    true,
                    &original_action_ids,
                    &pending,
                    &catalog,
                )
                .unwrap()
                .changed
        );
        assert_eq!(
            manager
                .set_enabled_with_compatibility(
                    &skill.skill_id,
                    true,
                    &["skill.invalid.warning".to_string()],
                    &pending,
                    &catalog,
                )
                .unwrap_err()
                .code,
            SkillErrorCode::CompatibilityAcknowledgmentRequired
        );
        let mut stale = pending.clone();
        stale.science_version = Some("0.1.19".to_string());
        assert_eq!(
            manager
                .set_enabled_with_compatibility(
                    &skill.skill_id,
                    true,
                    &original_action_ids,
                    &stale,
                    &catalog,
                )
                .unwrap_err()
                .code,
            SkillErrorCode::CompatibilityAcknowledgmentRequired
        );
        let mut unsupported = pending;
        unsupported.discovery_status = crate::skill_manager::model::DiscoveryStatus::NotDiscovered;
        assert_eq!(
            manager
                .set_enabled_with_compatibility(
                    &skill.skill_id,
                    true,
                    &original_action_ids,
                    &unsupported,
                    &catalog,
                )
                .unwrap_err()
                .code,
            SkillErrorCode::CompatibilityUnsupported
        );
    }
}
