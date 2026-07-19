use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{SkillErrorCode, SkillManagerError, SkillResult};
use super::inspection::{
    InspectedFile, MAX_DIRECTORY_COUNT, MAX_FILE_COUNT, MAX_FILE_SIZE, MAX_PATH_BYTES,
    MAX_PATH_DEPTH, MAX_TOTAL_SIZE,
};
use super::model::{InstalledSkill, SkillId};
use super::store::{
    os_cstring, rename_noreplace, rename_swap, verify_root_marker_fd, CapturedTree, SafeDir,
    SafeFileIdentity,
};

const REGISTRY_FILE: &str = "deployments.v1.json";
const MAX_REGISTRY_SIZE: u64 = 4 * 1024 * 1024;
pub(super) const RUNTIME_MARKER_FILE: &str = ".csswitch-skill-deployment.v1.json";
const RUNTIME_ROOT_MARKER_FILE: &str = ".csswitch-skill-runtime-root.v1.json";
const RUNTIME_ROOT_OWNER: &str = "csswitch.skill-runtime-root";
const RUNTIME_OWNER: &str = "csswitch.skill-runtime";
const ACTIVE_ORG_FILE: &str = "active-org.json";
const MAX_ACTIVE_ORG_SIZE: u64 = 4_096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentRecord {
    pub(crate) skill_id: SkillId,
    pub(crate) runtime_name: String,
    pub(crate) content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentRegistry {
    pub(crate) schema_version: u32,
    pub(crate) deployments: Vec<DeploymentRecord>,
}

impl Default for DeploymentRegistry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            deployments: Vec::new(),
        }
    }
}

impl DeploymentRegistry {
    fn validate(&self) -> SkillResult<()> {
        if self.schema_version != 1 {
            return Err(deployment_conflict(None));
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for record in &self.deployments {
            if !ids.insert(record.skill_id.clone())
                || !names.insert(record.runtime_name.clone())
                || !valid_runtime_name(&record.runtime_name)
                || record.content_hash.len() != 64
                || !record
                    .content_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(deployment_conflict(Some(record.skill_id.clone())));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeRootMarker {
    schema_version: u32,
    owner: String,
    generation: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveOrgFile {
    org_uuid: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RuntimeMarker {
    schema_version: u32,
    owner: String,
    skill_id: SkillId,
    runtime_name: String,
    content_hash: String,
}

pub(crate) struct DeploymentService {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

enum RuntimeParent {
    #[cfg(test)]
    Legacy { data: SafeDir },
    Org {
        data: SafeDir,
        orgs: SafeDir,
        org: SafeDir,
        org_uuid: String,
        active_identity: SafeFileIdentity,
        active_contents: Vec<u8>,
    },
}

impl RuntimeParent {
    fn directory(&self) -> &SafeDir {
        match self {
            #[cfg(test)]
            Self::Legacy { data } => data,
            Self::Org { org, .. } => org,
        }
    }

    fn verify_current(&self, data_dir: &Path) -> SkillResult<()> {
        match self {
            #[cfg(test)]
            Self::Legacy { data } => {
                let current = SafeDir::open_absolute(data_dir)?;
                if !data.same_identity(&current)? {
                    return Err(deployment_conflict(None));
                }
            }
            Self::Org {
                data,
                orgs,
                org,
                org_uuid,
                active_identity,
                active_contents,
            } => {
                let current = SafeDir::open_absolute(data_dir)?;
                if !data.same_identity(&current)? {
                    return Err(deployment_conflict(None));
                }
                data.verify_file_identity(ACTIVE_ORG_FILE.as_ref(), *active_identity)?;
                let current_active =
                    data.read_file(ACTIVE_ORG_FILE.as_ref(), MAX_ACTIVE_ORG_SIZE)?;
                data.verify_file_identity(ACTIVE_ORG_FILE.as_ref(), *active_identity)?;
                if current_active.as_slice() != active_contents.as_slice() {
                    return Err(deployment_conflict(None));
                }
                data.verify_child_identity("orgs".as_ref(), orgs)?;
                orgs.verify_child_identity(org_uuid.as_ref(), org)?;
            }
        }
        Ok(())
    }
}

struct VerifiedRuntime {
    root: SafeDir,
    runtime: SafeDir,
    entry_name: String,
    tree: CapturedTree,
}

impl VerifiedRuntime {
    fn verify_current(&self) -> SkillResult<()> {
        self.verify_at(&self.entry_name)
    }

    fn verify_at(&self, entry_name: &str) -> SkillResult<()> {
        let opened = self.runtime.try_clone()?;
        let name = os_cstring(entry_name.as_ref())?;
        let current = self
            .root
            .child_stat(&name)?
            .ok_or_else(|| deployment_conflict(None))?;
        let anchored = opened
            .child_stat(&os_cstring(".".as_ref())?)?
            .ok_or_else(|| deployment_conflict(None))?;
        if current.st_dev != anchored.st_dev || current.st_ino != anchored.st_ino {
            return Err(deployment_conflict(None));
        }
        self.tree
            .verify_entries_unchanged()
            .map_err(|_| deployment_conflict(None))
    }

    fn remove(self) -> SkillResult<()> {
        self.verify_current()?;
        let runtime = self.runtime.try_clone()?;
        self.tree.remove_contents()?;
        self.root
            .remove_empty_verified_child(self.entry_name.as_ref(), &runtime)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReconcileAction {
    Deploy,
    Replace,
    Remove,
    Adopt,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileItem {
    pub(crate) skill_id: SkillId,
    pub(crate) runtime_name: String,
    pub(crate) action: ReconcileAction,
    pub(crate) applied: bool,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileError {
    pub(crate) skill_id: Option<SkillId>,
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) remediation: String,
}

impl From<SkillManagerError> for ReconcileError {
    fn from(error: SkillManagerError) -> Self {
        Self {
            skill_id: error.skill_id,
            code: error.code.as_str().to_string(),
            message: error.message,
            remediation: error.remediation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileReport {
    pub(crate) dry_run: bool,
    pub(crate) reason: String,
    pub(crate) planned: Vec<ReconcileItem>,
    pub(crate) applied: Vec<ReconcileItem>,
    pub(crate) skipped: Vec<ReconcileItem>,
    pub(crate) errors: Vec<ReconcileError>,
    pub(crate) restart_required: bool,
}

impl DeploymentService {
    pub(crate) fn new(config_dir: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config_dir,
            data_dir,
        }
    }

    pub(crate) fn runtime_fingerprint(&self) -> SkillResult<String> {
        let root = self
            .open_runtime_root_if_present()?
            .ok_or_else(|| deployment_conflict(None))?;
        let marker = read_runtime_root_marker(&root, None)?;
        let mut digest = Sha256::new();
        digest.update(b"csswitch.skill-runtime-target.v1\0");
        digest.update(marker.generation.as_bytes());
        let bytes = digest.finalize();
        let mut value = String::with_capacity(64);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        Ok(value)
    }

    pub(crate) fn load_registry(&self) -> SkillResult<DeploymentRegistry> {
        let root_path = self.config_dir.join("skills");
        if !root_path.exists() {
            return Ok(DeploymentRegistry::default());
        }
        let root = SafeDir::open_absolute(&root_path)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let name = os_cstring(REGISTRY_FILE.as_ref())?;
        if root.child_stat(&name)?.is_none() {
            return Ok(DeploymentRegistry::default());
        }
        let data = root.read_file(REGISTRY_FILE.as_ref(), MAX_REGISTRY_SIZE)?;
        let registry: DeploymentRegistry =
            serde_json::from_slice(&data).map_err(|_| deployment_conflict(None))?;
        registry.validate()?;
        Ok(registry)
    }

    pub(crate) fn save_registry(&self, registry: &DeploymentRegistry) -> SkillResult<()> {
        registry.validate()?;
        let root = SafeDir::open_absolute(&self.config_dir.join("skills"))?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let data = serde_json::to_vec_pretty(registry).map_err(|_| deployment_conflict(None))?;
        if data.len() as u64 > MAX_REGISTRY_SIZE {
            return Err(deployment_conflict(None));
        }
        let suffix = SkillId::new_random()
            .map_err(|_| deployment_conflict(None))?
            .short()
            .to_string();
        let temporary = format!(".{REGISTRY_FILE}.tmp-{}-{suffix}", std::process::id());
        if root.child_stat(&os_cstring(temporary.as_ref())?)?.is_some() {
            return Err(deployment_conflict(None));
        }
        let mut temporary_created = false;
        let result = (|| {
            root.create_file(temporary.as_ref(), &data)?;
            temporary_created = true;
            root.replace_child(temporary.as_ref(), REGISTRY_FILE.as_ref())?;
            root.sync()?;
            Ok(())
        })();
        if result.is_err() && temporary_created {
            let _ = root.remove_tree_child(temporary.as_ref());
        }
        result
    }

    #[cfg(test)]
    fn remove_owned_runtime_and_record(&self, skill: &InstalledSkill) -> SkillResult<bool> {
        let mut registry = self.load_registry()?;
        let Some(index) = registry
            .deployments
            .iter()
            .position(|record| record.skill_id == skill.skill_id)
        else {
            return Ok(false);
        };
        let record = registry.deployments[index].clone();
        if record.runtime_name != skill.runtime_name || record.content_hash != skill.content_hash {
            return Err(deployment_conflict(Some(skill.skill_id.clone())));
        }
        let runtime_removed = self.verify_and_remove_runtime(&record)?;
        registry.deployments.remove(index);
        self.save_registry(&registry)?;
        Ok(runtime_removed)
    }

    pub(crate) fn remove_owned_runtime_and_record_with_payload(
        &self,
        skill: &InstalledSkill,
        payload_loader: impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> SkillResult<bool> {
        let files = payload_loader(skill)?;
        let mut registry = self.load_registry()?;
        let stale = registry
            .deployments
            .iter()
            .find(|record| record.skill_id == skill.skill_id)
            .cloned();
        let current = if stale.as_ref().is_some_and(|record| {
            record.runtime_name == skill.runtime_name && record.content_hash == skill.content_hash
        }) {
            None
        } else {
            self.recover_current_record(skill, &files, stale.as_ref())?
        };
        let orphans = self.collect_orphan_staging(skill, &payload_loader)?;
        if stale.is_none() && current.is_none() && orphans.is_empty() {
            return Ok(false);
        }

        let current_verified = match &current {
            Some(record) => self.verify_record_runtime(record, &files)?,
            None => None,
        };
        let stale_verified = match &stale {
            Some(record)
                if current
                    .as_ref()
                    .is_none_or(|current| current.runtime_name != record.runtime_name) =>
            {
                let mut stored = skill.clone();
                stored.content_hash = record.content_hash.clone();
                stored.runtime_name = record.runtime_name.clone();
                let old_files = payload_loader(&stored)?;
                self.verify_record_runtime(record, &old_files)?
            }
            _ => None,
        };

        let mut removed = false;
        if let Some(current) = &current_verified {
            current.verify_current()?;
        }
        if let Some(stale) = &stale_verified {
            stale.verify_current()?;
        }
        for orphan in orphans {
            orphan.remove()?;
            removed = true;
        }
        if let Some(current) = &current_verified {
            current.verify_current()?;
        }
        if let Some(stale) = &stale_verified {
            stale.verify_current()?;
        }
        if let Some(current) = current_verified {
            current.remove()?;
            removed = true;
        }
        if let Some(stale) = stale_verified {
            stale.remove()?;
            removed = true;
        }
        if stale.is_some() {
            registry
                .deployments
                .retain(|record| record.skill_id != skill.skill_id);
            self.save_registry(&registry)?;
        }
        Ok(removed)
    }

    fn verify_and_remove_runtime(&self, record: &DeploymentRecord) -> SkillResult<bool> {
        self.verify_and_remove_runtime_with_hook(record, &|| {})
    }

    fn verify_and_remove_runtime_with_hook(
        &self,
        record: &DeploymentRecord,
        after_marker_verify: &dyn Fn(),
    ) -> SkillResult<bool> {
        let Some(skills) = self.open_runtime_root_container(Some(record.skill_id.clone()))? else {
            return Ok(false);
        };
        let runtime_name = os_cstring(record.runtime_name.as_ref())?;
        if skills.child_stat(&runtime_name)?.is_none() {
            return Ok(false);
        }
        verify_runtime_root(&skills, Some(record.skill_id.clone()))?;
        let runtime = skills.open_child(record.runtime_name.as_ref())?;
        runtime
            .validate_owned()
            .map_err(|_| deployment_conflict(Some(record.skill_id.clone())))?;
        let marker_data = runtime
            .read_file(RUNTIME_MARKER_FILE.as_ref(), 4_096)
            .map_err(|_| deployment_conflict(Some(record.skill_id.clone())))?;
        let marker: RuntimeMarker = serde_json::from_slice(&marker_data)
            .map_err(|_| deployment_conflict(Some(record.skill_id.clone())))?;
        if marker.schema_version != 1
            || marker.owner != RUNTIME_OWNER
            || marker.skill_id != record.skill_id
            || marker.runtime_name != record.runtime_name
            || marker.content_hash != record.content_hash
        {
            return Err(deployment_conflict(Some(record.skill_id.clone())));
        }
        let tree = CapturedTree::capture(&runtime)
            .map_err(|_| deployment_conflict(Some(record.skill_id.clone())))?;
        let verified = VerifiedRuntime {
            root: skills.try_clone()?,
            runtime,
            entry_name: record.runtime_name.clone(),
            tree,
        };
        after_marker_verify();
        verified.remove()?;
        Ok(true)
    }

    pub(crate) fn reconcile(
        &self,
        inventory: &[InstalledSkill],
        dry_run: bool,
        reason: &str,
        payload_loader: impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> ReconcileReport {
        let mut report = ReconcileReport {
            dry_run,
            reason: sanitize_reason(reason),
            planned: Vec::new(),
            applied: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
            restart_required: false,
        };
        let mut registry = match self.load_registry() {
            Ok(registry) => registry,
            Err(error) => {
                report.errors.push(error.into());
                return report;
            }
        };
        if !dry_run && inventory.iter().any(|skill| skill.enabled) {
            if let Err(error) = self.ensure_runtime_root() {
                report.errors.push(error.into());
                return report;
            }
        }

        for skill in inventory {
            let result = if skill.enabled {
                let files = match payload_loader(skill) {
                    Ok(files) => files,
                    Err(error) => {
                        report.errors.push(error.into());
                        break;
                    }
                };
                self.reconcile_enabled(
                    skill,
                    &files,
                    &mut registry,
                    dry_run,
                    &mut report,
                    &payload_loader,
                )
                .and_then(|()| {
                    self.reconcile_orphan_staging(skill, dry_run, &mut report, &payload_loader)
                })
            } else {
                self.reconcile_disabled(skill, &mut registry, dry_run, &mut report, &payload_loader)
            };
            if let Err(error) = result {
                report.errors.push(error.into());
                break;
            }
        }

        if report.errors.is_empty() {
            let installed_ids = inventory
                .iter()
                .map(|skill| skill.skill_id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let stale = registry
                .deployments
                .iter()
                .filter(|record| !installed_ids.contains(&record.skill_id))
                .cloned()
                .collect::<Vec<_>>();
            for record in stale {
                let item = reconcile_item(
                    &record,
                    ReconcileAction::Remove,
                    false,
                    "remove_stale_deployment",
                );
                report.planned.push(item.clone());
                if dry_run {
                    report.restart_required = true;
                    continue;
                }
                match self.remove_record_runtime(&record) {
                    Ok(removed) => {
                        registry
                            .deployments
                            .retain(|entry| entry.skill_id != record.skill_id);
                        if let Err(error) = self.save_registry(&registry) {
                            report.errors.push(error.into());
                            break;
                        }
                        let mut applied = item;
                        applied.applied = true;
                        report.restart_required |= removed;
                        report.applied.push(applied);
                    }
                    Err(error) => {
                        report.errors.push(error.into());
                        break;
                    }
                }
            }
        }
        report
    }

    fn reconcile_orphan_staging(
        &self,
        skill: &InstalledSkill,
        dry_run: bool,
        report: &mut ReconcileReport,
        payload_loader: &impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> SkillResult<()> {
        let orphans = self.collect_orphan_staging(skill, payload_loader)?;
        for orphan in orphans {
            let item = ReconcileItem {
                skill_id: skill.skill_id.clone(),
                runtime_name: orphan.entry_name.clone(),
                action: ReconcileAction::Remove,
                applied: false,
                detail: "recover_orphan_staging".to_string(),
            };
            report.planned.push(item.clone());
            report.restart_required = true;
            if dry_run {
                continue;
            }
            orphan.remove()?;
            let mut applied = item;
            applied.applied = true;
            report.applied.push(applied);
        }
        Ok(())
    }

    fn reconcile_enabled(
        &self,
        skill: &InstalledSkill,
        files: &[InspectedFile],
        registry: &mut DeploymentRegistry,
        dry_run: bool,
        report: &mut ReconcileReport,
        payload_loader: &impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> SkillResult<()> {
        let record = registry
            .deployments
            .iter()
            .find(|record| record.skill_id == skill.skill_id)
            .cloned();
        if let Some(record) = &record {
            if record.runtime_name != skill.runtime_name {
                return self.reconcile_renamed(
                    skill,
                    record,
                    registry,
                    dry_run,
                    report,
                    payload_loader,
                );
            }
        }
        let existing_root = self.open_runtime_root_if_present()?;
        let existing = match &existing_root {
            Some(root) => {
                let name = os_cstring(skill.runtime_name.as_ref())?;
                if root.child_stat(&name)?.is_some() {
                    Some(root.open_child(skill.runtime_name.as_ref())?)
                } else {
                    None
                }
            }
            None => None,
        };

        if let Some(runtime) = existing {
            let marker = read_runtime_marker(&runtime, &skill.skill_id)?;
            if marker.runtime_name != skill.runtime_name || marker.skill_id != skill.skill_id {
                return Err(deployment_conflict(Some(skill.skill_id.clone())));
            }
            if marker.content_hash == skill.content_hash {
                verify_runtime_files(&runtime, files, &skill.skill_id)?;
                if record.as_ref().is_some_and(|record| {
                    record.content_hash == skill.content_hash
                        && record.runtime_name == skill.runtime_name
                }) {
                    report.skipped.push(ReconcileItem {
                        skill_id: skill.skill_id.clone(),
                        runtime_name: skill.runtime_name.clone(),
                        action: ReconcileAction::Skip,
                        applied: false,
                        detail: "content_hash_unchanged".to_string(),
                    });
                    return Ok(());
                }
                let item = ReconcileItem {
                    skill_id: skill.skill_id.clone(),
                    runtime_name: skill.runtime_name.clone(),
                    action: ReconcileAction::Adopt,
                    applied: false,
                    detail: "recover_registry_after_committed_runtime".to_string(),
                };
                report.planned.push(item.clone());
                report.restart_required = true;
                if !dry_run {
                    upsert_record(registry, skill);
                    self.save_registry(registry)?;
                    let mut applied = item;
                    applied.applied = true;
                    report.applied.push(applied);
                }
                return Ok(());
            }

            let Some(old_record) = record else {
                return Err(deployment_conflict(Some(skill.skill_id.clone())));
            };
            if marker.content_hash != old_record.content_hash {
                return Err(deployment_conflict(Some(skill.skill_id.clone())));
            }
            let mut old_skill = skill.clone();
            old_skill.content_hash = old_record.content_hash.clone();
            let old_files = payload_loader(&old_skill)?;
            let root = existing_root
                .as_ref()
                .expect("existing runtime implies root");
            let verified_old =
                self.verify_named_runtime(root, &old_record.runtime_name, &old_record, &old_files)?;
            let item = ReconcileItem {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                action: ReconcileAction::Replace,
                applied: false,
                detail: "content_hash_changed".to_string(),
            };
            report.planned.push(item.clone());
            if dry_run {
                report.restart_required = true;
                return Ok(());
            }
            self.deploy_runtime(root, skill, files, Some(verified_old))?;
            upsert_record(registry, skill);
            self.save_registry(registry)?;
            let mut applied = item;
            applied.applied = true;
            report.applied.push(applied);
            report.restart_required = true;
            return Ok(());
        }

        if record.is_some() {
            // Stale registry after a sandbox rebuild is recoverable; deployment below recreates it.
        }
        let item = ReconcileItem {
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            action: ReconcileAction::Deploy,
            applied: false,
            detail: "runtime_missing".to_string(),
        };
        report.planned.push(item.clone());
        if dry_run {
            report.restart_required = true;
            return Ok(());
        }
        let root = self.ensure_runtime_root()?;
        self.deploy_runtime(&root, skill, files, None)?;
        upsert_record(registry, skill);
        self.save_registry(registry)?;
        let mut applied = item;
        applied.applied = true;
        report.applied.push(applied);
        report.restart_required = true;
        Ok(())
    }

    fn reconcile_disabled(
        &self,
        skill: &InstalledSkill,
        registry: &mut DeploymentRegistry,
        dry_run: bool,
        report: &mut ReconcileReport,
        payload_loader: &impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> SkillResult<()> {
        let current_files = payload_loader(skill)?;
        let record = registry
            .deployments
            .iter()
            .find(|record| record.skill_id == skill.skill_id)
            .cloned();
        let current = if record.as_ref().is_some_and(|record| {
            record.runtime_name == skill.runtime_name && record.content_hash == skill.content_hash
        }) {
            None
        } else {
            self.recover_current_record(skill, &current_files, record.as_ref())?
        };
        if record.is_none() && current.is_none() {
            report.skipped.push(ReconcileItem {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                action: ReconcileAction::Skip,
                applied: false,
                detail: "disabled_and_not_deployed".to_string(),
            });
            return Ok(());
        }
        let item_record = current
            .as_ref()
            .or(record.as_ref())
            .expect("candidate exists");
        let item = reconcile_item(item_record, ReconcileAction::Remove, false, "disabled");
        report.planned.push(item.clone());
        let current_verified = match &current {
            Some(record) => self.verify_record_runtime(record, &current_files)?,
            None => None,
        };
        let record_verified = match &record {
            Some(record)
                if current
                    .as_ref()
                    .is_none_or(|current| current.runtime_name != record.runtime_name) =>
            {
                let mut stored = skill.clone();
                stored.content_hash = record.content_hash.clone();
                stored.runtime_name = record.runtime_name.clone();
                let old_files = payload_loader(&stored)?;
                self.verify_record_runtime(record, &old_files)?
            }
            _ => None,
        };
        if let Some(current) = &current_verified {
            current.verify_current()?;
        }
        if let Some(record) = &record_verified {
            record.verify_current()?;
        }
        self.reconcile_orphan_staging(skill, dry_run, report, payload_loader)?;
        if dry_run {
            report.restart_required = true;
            return Ok(());
        }
        if let Some(current) = &current_verified {
            current.verify_current()?;
        }
        if let Some(record) = &record_verified {
            record.verify_current()?;
        }
        let mut removed = false;
        if let Some(current) = current_verified {
            current.remove()?;
            removed = true;
        }
        if let Some(record) = record_verified {
            record.remove()?;
            removed = true;
        }
        registry
            .deployments
            .retain(|entry| entry.skill_id != skill.skill_id);
        self.save_registry(registry)?;
        let mut applied = item;
        applied.applied = true;
        report.applied.push(applied);
        report.restart_required |= removed;
        Ok(())
    }

    fn reconcile_renamed(
        &self,
        skill: &InstalledSkill,
        old_record: &DeploymentRecord,
        registry: &mut DeploymentRegistry,
        dry_run: bool,
        report: &mut ReconcileReport,
        payload_loader: &impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> SkillResult<()> {
        if registry.deployments.iter().any(|record| {
            record.skill_id != skill.skill_id && record.runtime_name == skill.runtime_name
        }) {
            return Err(deployment_conflict(Some(skill.skill_id.clone())));
        }
        let item = ReconcileItem {
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            action: ReconcileAction::Replace,
            applied: false,
            detail: "runtime_name_changed".to_string(),
        };
        report.planned.push(item.clone());
        if dry_run {
            report.restart_required = true;
            return Ok(());
        }

        let root = self.ensure_runtime_root()?;
        let desired_files = payload_loader(skill)?;
        let desired_record = DeploymentRecord {
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            content_hash: skill.content_hash.clone(),
        };
        let desired_name = os_cstring(skill.runtime_name.as_ref())?;
        let desired = if root.child_stat(&desired_name)?.is_some() {
            self.verify_named_runtime(&root, &skill.runtime_name, &desired_record, &desired_files)?
        } else {
            self.deploy_runtime(&root, skill, &desired_files, None)?;
            self.verify_named_runtime(&root, &skill.runtime_name, &desired_record, &desired_files)?
        };

        let old_name = os_cstring(old_record.runtime_name.as_ref())?;
        let old_runtime = if root.child_stat(&old_name)?.is_some() {
            let mut old_skill = skill.clone();
            old_skill.content_hash = old_record.content_hash.clone();
            old_skill.runtime_name = old_record.runtime_name.clone();
            let old_files = payload_loader(&old_skill)?;
            Some(self.verify_named_runtime(
                &root,
                &old_record.runtime_name,
                old_record,
                &old_files,
            )?)
        } else {
            None
        };
        desired.verify_current()?;
        if let Some(old_runtime) = old_runtime {
            old_runtime.verify_current()?;
            old_runtime.remove()?;
        }
        desired.verify_current()?;
        upsert_record(registry, skill);
        self.save_registry(registry)?;
        let mut applied = item;
        applied.applied = true;
        report.applied.push(applied);
        report.restart_required = true;
        Ok(())
    }

    fn recover_current_record(
        &self,
        skill: &InstalledSkill,
        files: &[InspectedFile],
        stale: Option<&DeploymentRecord>,
    ) -> SkillResult<Option<DeploymentRecord>> {
        let Some(root) = self.open_runtime_root_for_recovery(&skill.skill_id)? else {
            return Ok(None);
        };
        let name = os_cstring(skill.runtime_name.as_ref())?;
        if root.child_stat(&name)?.is_none() {
            return Ok(None);
        }
        let runtime = root.open_child(skill.runtime_name.as_ref())?;
        let marker = read_runtime_marker(&runtime, &skill.skill_id)?;
        if marker.skill_id == skill.skill_id
            && marker.runtime_name == skill.runtime_name
            && marker.content_hash == skill.content_hash
        {
            verify_runtime_files(&runtime, files, &skill.skill_id)?;
            return Ok(Some(DeploymentRecord {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                content_hash: skill.content_hash.clone(),
            }));
        }
        if stale.is_some_and(|stale| {
            marker.skill_id == stale.skill_id
                && marker.runtime_name == stale.runtime_name
                && marker.content_hash == stale.content_hash
        }) {
            return Ok(None);
        }
        Err(deployment_conflict(Some(skill.skill_id.clone())))
    }

    fn verify_record_runtime(
        &self,
        record: &DeploymentRecord,
        files: &[InspectedFile],
    ) -> SkillResult<Option<VerifiedRuntime>> {
        let Some(root) = self.open_runtime_root_for_recovery(&record.skill_id)? else {
            return Ok(None);
        };
        let name = os_cstring(record.runtime_name.as_ref())?;
        if root.child_stat(&name)?.is_none() {
            return Ok(None);
        }
        self.verify_named_runtime(&root, &record.runtime_name, record, files)
            .map(Some)
    }

    fn verify_named_runtime(
        &self,
        root: &SafeDir,
        entry_name: &str,
        record: &DeploymentRecord,
        files: &[InspectedFile],
    ) -> SkillResult<VerifiedRuntime> {
        let runtime = root.open_child(entry_name.as_ref())?;
        let marker = read_runtime_marker(&runtime, &record.skill_id)?;
        if marker.skill_id != record.skill_id
            || marker.runtime_name != record.runtime_name
            || marker.content_hash != record.content_hash
        {
            return Err(deployment_conflict(Some(record.skill_id.clone())));
        }
        let tree = CapturedTree::capture(&runtime)
            .map_err(|_| deployment_conflict(Some(record.skill_id.clone())))?;
        verify_runtime_files(&runtime, files, &record.skill_id)?;
        tree.verify_unchanged()
            .map_err(|_| deployment_conflict(Some(record.skill_id.clone())))?;
        let verified = VerifiedRuntime {
            root: root.try_clone()?,
            runtime,
            entry_name: entry_name.to_string(),
            tree,
        };
        verified.verify_current()?;
        Ok(verified)
    }

    fn collect_orphan_staging(
        &self,
        skill: &InstalledSkill,
        payload_loader: &impl Fn(&InstalledSkill) -> SkillResult<Vec<InspectedFile>>,
    ) -> SkillResult<Vec<VerifiedRuntime>> {
        let Some(root) = self.open_runtime_root_for_recovery(&skill.skill_id)? else {
            return Ok(Vec::new());
        };
        let prefix = format!(".staging-{}-", skill.skill_id.short());
        let mut orphans = Vec::new();
        for name in root.names()? {
            let entry_name = name.to_string_lossy();
            if !entry_name.starts_with(&prefix) {
                continue;
            }
            let runtime = root.open_child(&name)?;
            let marker = read_runtime_marker(&runtime, &skill.skill_id)?;
            if marker.skill_id != skill.skill_id
                || !valid_runtime_name(&marker.runtime_name)
                || marker.content_hash.len() != 64
                || !marker
                    .content_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(deployment_conflict(Some(skill.skill_id.clone())));
            }
            let record = DeploymentRecord {
                skill_id: marker.skill_id,
                runtime_name: marker.runtime_name,
                content_hash: marker.content_hash,
            };
            let mut stored = skill.clone();
            stored.content_hash = record.content_hash.clone();
            stored.runtime_name = record.runtime_name.clone();
            let files = payload_loader(&stored)?;
            orphans.push(self.verify_named_runtime(&root, &entry_name, &record, &files)?);
        }
        Ok(orphans)
    }

    fn open_runtime_root_for_recovery(&self, skill_id: &SkillId) -> SkillResult<Option<SafeDir>> {
        self.open_runtime_root(Some(skill_id.clone()))
    }

    fn remove_record_runtime(&self, record: &DeploymentRecord) -> SkillResult<bool> {
        self.verify_and_remove_runtime(record)
    }

    fn open_runtime_root_if_present(&self) -> SkillResult<Option<SafeDir>> {
        self.open_runtime_root(None)
    }

    fn ensure_runtime_root(&self) -> SkillResult<SafeDir> {
        let parent = self
            .runtime_parent(true)?
            .ok_or_else(|| deployment_conflict(None))?;
        parent.verify_current(&self.data_dir)?;
        let data = parent.directory();
        let skills_name = os_cstring("skills".as_ref())?;
        if data.child_stat(&skills_name)?.is_some() {
            let root = data.open_child("skills".as_ref())?;
            root.validate_owned()?;
            parent.verify_current(&self.data_dir)?;
            data.verify_child_identity("skills".as_ref(), &root)?;
            let marker_name = os_cstring(RUNTIME_ROOT_MARKER_FILE.as_ref())?;
            if root.child_stat(&marker_name)?.is_none() {
                let marker = runtime_root_marker_bytes()?;
                root.create_file(RUNTIME_ROOT_MARKER_FILE.as_ref(), &marker)?;
                root.sync()?;
                parent.verify_current(&self.data_dir)?;
                data.verify_child_identity("skills".as_ref(), &root)?;
            }
            verify_runtime_root(&root, None)?;
            return Ok(root);
        }
        let staging_name = format!(".skills-init-{}", random_suffix()?);
        let staging = data.create_child(staging_name.as_ref())?;
        let marker = runtime_root_marker_bytes()?;
        let mut committed = false;
        let result = (|| {
            staging.create_file(RUNTIME_ROOT_MARKER_FILE.as_ref(), &marker)?;
            staging.sync()?;
            parent.verify_current(&self.data_dir)?;
            data.verify_child_identity(staging_name.as_ref(), &staging)?;
            rename_noreplace(data, staging_name.as_ref(), data, "skills".as_ref())?;
            committed = true;
            data.sync()?;
            parent.verify_current(&self.data_dir)?;
            Ok(())
        })();
        if let Err(error) = result {
            if !committed {
                let _ = data.remove_verified_child(staging_name.as_ref(), &staging);
            }
            return Err(error);
        }
        data.open_child("skills".as_ref())
    }

    fn open_runtime_root(&self, skill_id: Option<SkillId>) -> SkillResult<Option<SafeDir>> {
        let Some(root) = self.open_runtime_root_container(skill_id.clone())? else {
            return Ok(None);
        };
        verify_runtime_root(&root, skill_id)?;
        Ok(Some(root))
    }

    fn open_runtime_root_container(
        &self,
        skill_id: Option<SkillId>,
    ) -> SkillResult<Option<SafeDir>> {
        let parent = match self.runtime_parent(false) {
            Ok(parent) => parent,
            Err(error) => {
                if !self.data_dir.exists() {
                    return Ok(None);
                }
                return Err(match skill_id {
                    Some(skill_id) => error.with_skill_id(skill_id),
                    None => error,
                });
            }
        };
        let Some(parent) = parent else {
            return Ok(None);
        };
        parent
            .verify_current(&self.data_dir)
            .map_err(|_| deployment_conflict(skill_id.clone()))?;
        let directory = parent.directory();
        let skills_name = os_cstring("skills".as_ref())?;
        if directory.child_stat(&skills_name)?.is_none() {
            return Ok(None);
        }
        let root = directory
            .open_child("skills".as_ref())
            .map_err(|_| deployment_conflict(skill_id.clone()))?;
        root.validate_owned()
            .map_err(|_| deployment_conflict(skill_id.clone()))?;
        parent
            .verify_current(&self.data_dir)
            .map_err(|_| deployment_conflict(skill_id.clone()))?;
        directory
            .verify_child_identity("skills".as_ref(), &root)
            .map_err(|_| deployment_conflict(skill_id))?;
        Ok(Some(root))
    }

    fn runtime_parent(&self, create: bool) -> SkillResult<Option<RuntimeParent>> {
        self.runtime_parent_with_hook(create, &|| {})
    }

    fn runtime_parent_with_hook(
        &self,
        create: bool,
        after_active_read: &dyn Fn(),
    ) -> SkillResult<Option<RuntimeParent>> {
        self.runtime_parent_with_hooks(create, after_active_read, &|| {})
    }

    fn runtime_parent_with_hooks(
        &self,
        create: bool,
        after_active_read: &dyn Fn(),
        after_orgs_open: &dyn Fn(),
    ) -> SkillResult<Option<RuntimeParent>> {
        match std::fs::symlink_metadata(&self.data_dir) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            #[cfg(test)]
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                let data = SafeDir::ensure_absolute(&self.data_dir)?;
                data.validate_owned()?;
                return Ok(Some(RuntimeParent::Legacy { data }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None)
            }
            _ => return Err(deployment_conflict(None)),
        }
        let data = SafeDir::open_absolute(&self.data_dir)?;
        data.validate_owned()?;
        let active_name = os_cstring(ACTIVE_ORG_FILE.as_ref())?;
        if data.child_stat(&active_name)?.is_none() {
            #[cfg(test)]
            return Ok(Some(RuntimeParent::Legacy { data }));
            #[cfg(not(test))]
            return Err(deployment_conflict(None));
        }
        let (active_contents, active_identity) =
            data.read_bound_file(ACTIVE_ORG_FILE.as_ref(), MAX_ACTIVE_ORG_SIZE)?;
        let active: ActiveOrgFile =
            serde_json::from_slice(&active_contents).map_err(|_| deployment_conflict(None))?;
        if !valid_uuid(&active.org_uuid) {
            return Err(deployment_conflict(None));
        }
        after_active_read();
        let current = SafeDir::open_absolute(&self.data_dir)?;
        if !data.same_identity(&current)? {
            return Err(deployment_conflict(None));
        }
        let orgs_name = os_cstring("orgs".as_ref())?;
        let orgs = if data.child_stat(&orgs_name)?.is_some() {
            data.open_child("orgs".as_ref())?
        } else if create {
            data.create_child("orgs".as_ref())?
        } else {
            return Ok(None);
        };
        orgs.validate_owned()?;
        after_orgs_open();
        data.verify_child_identity("orgs".as_ref(), &orgs)?;
        let org_name = os_cstring(active.org_uuid.as_ref())?;
        let org = if orgs.child_stat(&org_name)?.is_some() {
            orgs.open_child(active.org_uuid.as_ref())?
        } else if create {
            orgs.create_child(active.org_uuid.as_ref())?
        } else {
            return Ok(None);
        };
        org.validate_owned()?;
        let parent = RuntimeParent::Org {
            data,
            orgs,
            org,
            org_uuid: active.org_uuid,
            active_identity,
            active_contents,
        };
        parent.verify_current(&self.data_dir)?;
        Ok(Some(parent))
    }

    fn deploy_runtime(
        &self,
        root: &SafeDir,
        skill: &InstalledSkill,
        files: &[InspectedFile],
        existing: Option<VerifiedRuntime>,
    ) -> SkillResult<()> {
        self.deploy_runtime_with_verified_hooks(
            root,
            skill,
            files,
            existing,
            &|| Ok(()),
            &|| Ok(()),
        )
    }

    #[cfg(test)]
    fn deploy_runtime_with_hook(
        &self,
        root: &SafeDir,
        skill: &InstalledSkill,
        files: &[InspectedFile],
        existing: Option<&SafeDir>,
        after_staging: &dyn Fn() -> SkillResult<()>,
    ) -> SkillResult<()> {
        self.deploy_runtime_with_hooks(root, skill, files, existing, after_staging, &|| Ok(()))
    }

    #[cfg(test)]
    fn deploy_runtime_with_hooks(
        &self,
        root: &SafeDir,
        skill: &InstalledSkill,
        files: &[InspectedFile],
        existing: Option<&SafeDir>,
        after_staging: &dyn Fn() -> SkillResult<()>,
        after_commit: &dyn Fn() -> SkillResult<()>,
    ) -> SkillResult<()> {
        let existing = existing
            .map(|runtime| {
                Ok(VerifiedRuntime {
                    root: root.try_clone()?,
                    runtime: runtime.try_clone()?,
                    entry_name: skill.runtime_name.clone(),
                    tree: CapturedTree::capture(runtime)?,
                })
            })
            .transpose()?;
        self.deploy_runtime_with_verified_hooks(
            root,
            skill,
            files,
            existing,
            after_staging,
            after_commit,
        )
    }

    fn deploy_runtime_with_verified_hooks(
        &self,
        root: &SafeDir,
        skill: &InstalledSkill,
        files: &[InspectedFile],
        existing: Option<VerifiedRuntime>,
        after_staging: &dyn Fn() -> SkillResult<()>,
        after_commit: &dyn Fn() -> SkillResult<()>,
    ) -> SkillResult<()> {
        let staging_name = format!(".staging-{}-{}", skill.skill_id.short(), random_suffix()?);
        let staging = root.create_child(staging_name.as_ref())?;
        let mut committed = false;
        let result = (|| {
            for file in files {
                let mut current = staging.try_clone()?;
                let mut components = file.relative_path.components().peekable();
                while let Some(component) = components.next() {
                    let std::path::Component::Normal(name) = component else {
                        return Err(deployment_conflict(Some(skill.skill_id.clone())));
                    };
                    if components.peek().is_some() {
                        current = current.open_or_create_child(name)?;
                    } else {
                        current.create_file_mode(name, &file.content, file.executable)?;
                    }
                }
            }
            let marker = serde_json::to_vec(&RuntimeMarker {
                schema_version: 1,
                owner: RUNTIME_OWNER.to_string(),
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                content_hash: skill.content_hash.clone(),
            })
            .map_err(|_| deployment_conflict(Some(skill.skill_id.clone())))?;
            staging.create_file(RUNTIME_MARKER_FILE.as_ref(), &marker)?;
            staging.sync()?;
            after_staging()?;
            match existing {
                Some(existing) => {
                    existing.verify_current()?;
                    rename_swap(
                        root,
                        staging_name.as_ref(),
                        root,
                        skill.runtime_name.as_ref(),
                    )?;
                    committed = true;
                    after_commit()?;
                    root.sync()?;
                    existing.verify_at(&staging_name)?;
                    existing
                        .tree
                        .remove_contents()
                        .map_err(|_| deployment_conflict(Some(skill.skill_id.clone())))?;
                    root.remove_empty_verified_child(staging_name.as_ref(), &existing.runtime)
                        .map_err(|_| deployment_conflict(Some(skill.skill_id.clone())))?;
                }
                None => {
                    rename_noreplace(
                        root,
                        staging_name.as_ref(),
                        root,
                        skill.runtime_name.as_ref(),
                    )?;
                    committed = true;
                    after_commit()?;
                    root.sync()?;
                }
            }
            Ok(())
        })();
        if result.is_err() && !committed {
            let _ = root.remove_verified_child(staging_name.as_ref(), &staging);
        }
        result
    }

    #[cfg(test)]
    fn seed_owned_runtime(&self, skill: &InstalledSkill) -> SkillResult<()> {
        let data = SafeDir::ensure_absolute(&self.data_dir)?;
        let skills = data.open_or_create_child("skills".as_ref())?;
        skills.validate_owned()?;
        let root_name = os_cstring(RUNTIME_ROOT_MARKER_FILE.as_ref())?;
        if skills.child_stat(&root_name)?.is_none() {
            let marker = runtime_root_marker_bytes()?;
            skills.create_file(RUNTIME_ROOT_MARKER_FILE.as_ref(), &marker)?;
        }
        let runtime = skills.open_or_create_child(skill.runtime_name.as_ref())?;
        let marker = serde_json::to_vec(&RuntimeMarker {
            schema_version: 1,
            owner: RUNTIME_OWNER.to_string(),
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            content_hash: skill.content_hash.clone(),
        })
        .map_err(|_| deployment_conflict(Some(skill.skill_id.clone())))?;
        runtime.create_file(RUNTIME_MARKER_FILE.as_ref(), &marker)?;
        let mut registry = self.load_registry()?;
        registry.deployments.push(DeploymentRecord {
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            content_hash: skill.content_hash.clone(),
        });
        self.save_registry(&registry)
    }
}

fn verify_runtime_root(skills: &SafeDir, skill_id: Option<SkillId>) -> SkillResult<()> {
    read_runtime_root_marker(skills, skill_id).map(|_| ())
}

fn read_runtime_root_marker(
    skills: &SafeDir,
    skill_id: Option<SkillId>,
) -> SkillResult<RuntimeRootMarker> {
    let data = skills
        .read_file(RUNTIME_ROOT_MARKER_FILE.as_ref(), 4_096)
        .map_err(|_| deployment_conflict(skill_id.clone()))?;
    let marker: RuntimeRootMarker =
        serde_json::from_slice(&data).map_err(|_| deployment_conflict(skill_id.clone()))?;
    if marker.schema_version != 1
        || marker.owner != RUNTIME_ROOT_OWNER
        || marker.generation.len() != 32
        || !marker
            .generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(deployment_conflict(skill_id));
    }
    Ok(marker)
}

fn runtime_root_marker_bytes() -> SkillResult<Vec<u8>> {
    serde_json::to_vec(&RuntimeRootMarker {
        schema_version: 1,
        owner: RUNTIME_ROOT_OWNER.to_string(),
        generation: random_generation()?,
    })
    .map_err(|_| deployment_conflict(None))
}

fn deployment_conflict(skill_id: Option<SkillId>) -> SkillManagerError {
    let error = SkillManagerError::new(
        SkillErrorCode::DeploymentConflict,
        "Skill 运行副本或部署登记的所有权不匹配",
        "请保留现场并运行 Skill Manager reconcile/诊断；不会删除未拥有内容",
    );
    match skill_id {
        Some(skill_id) => error.with_skill_id(skill_id),
        None => error,
    }
}

fn valid_runtime_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

fn random_suffix() -> SkillResult<String> {
    SkillId::new_random()
        .map(|id| id.short().to_string())
        .map_err(|_| deployment_conflict(None))
}

fn random_generation() -> SkillResult<String> {
    SkillId::new_random()
        .map(|id| id.as_str().trim_start_matches("sk_").to_string())
        .map_err(|_| deployment_conflict(None))
}

fn sanitize_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty()
        || trimmed.len() > 80
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        "unspecified".to_string()
    } else {
        trimmed.to_string()
    }
}

fn reconcile_item(
    record: &DeploymentRecord,
    action: ReconcileAction,
    applied: bool,
    detail: &str,
) -> ReconcileItem {
    ReconcileItem {
        skill_id: record.skill_id.clone(),
        runtime_name: record.runtime_name.clone(),
        action,
        applied,
        detail: detail.to_string(),
    }
}

fn upsert_record(registry: &mut DeploymentRegistry, skill: &InstalledSkill) {
    registry
        .deployments
        .retain(|record| record.skill_id != skill.skill_id);
    registry.deployments.push(DeploymentRecord {
        skill_id: skill.skill_id.clone(),
        runtime_name: skill.runtime_name.clone(),
        content_hash: skill.content_hash.clone(),
    });
}

fn read_runtime_marker(runtime: &SafeDir, skill_id: &SkillId) -> SkillResult<RuntimeMarker> {
    runtime.validate_owned()?;
    let data = runtime
        .read_file(RUNTIME_MARKER_FILE.as_ref(), 4_096)
        .map_err(|_| deployment_conflict(Some(skill_id.clone())))?;
    let marker: RuntimeMarker =
        serde_json::from_slice(&data).map_err(|_| deployment_conflict(Some(skill_id.clone())))?;
    if marker.schema_version != 1 || marker.owner != RUNTIME_OWNER {
        return Err(deployment_conflict(Some(skill_id.clone())));
    }
    Ok(marker)
}

fn verify_runtime_files(
    runtime: &SafeDir,
    expected: &[InspectedFile],
    skill_id: &SkillId,
) -> SkillResult<()> {
    use std::os::unix::ffi::OsStrExt;

    let expected = expected
        .iter()
        .map(|file| {
            (
                file.relative_path.as_os_str().as_bytes().to_vec(),
                (file.content.clone(), file.executable),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut actual = RuntimePayloadScan::default();
    walk_runtime_payload(runtime, PathBuf::new(), 0, &mut actual, skill_id)?;
    if actual.files != expected {
        return Err(deployment_conflict(Some(skill_id.clone())));
    }
    Ok(())
}

#[derive(Default)]
struct RuntimePayloadScan {
    files: std::collections::BTreeMap<Vec<u8>, (Vec<u8>, bool)>,
    total_size: u64,
    directory_count: usize,
}

fn walk_runtime_payload(
    directory: &SafeDir,
    relative: PathBuf,
    depth: usize,
    scan: &mut RuntimePayloadScan,
    skill_id: &SkillId,
) -> SkillResult<()> {
    use std::os::unix::ffi::OsStrExt;

    if depth > MAX_PATH_DEPTH {
        return Err(deployment_conflict(Some(skill_id.clone())));
    }
    for name in directory.names()? {
        if depth == 0 && name == RUNTIME_MARKER_FILE {
            continue;
        }
        let bytes = name.as_bytes();
        if bytes.is_empty() || !bytes.is_ascii() || bytes == b"." || bytes == b".." {
            return Err(deployment_conflict(Some(skill_id.clone())));
        }
        let path = relative.join(&name);
        if path.as_os_str().as_bytes().len() > MAX_PATH_BYTES {
            return Err(deployment_conflict(Some(skill_id.clone())));
        }
        let stat = directory
            .child_stat(&os_cstring(&name)?)?
            .ok_or_else(|| deployment_conflict(Some(skill_id.clone())))?;
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                if scan.directory_count >= MAX_DIRECTORY_COUNT {
                    return Err(deployment_conflict(Some(skill_id.clone())));
                }
                scan.directory_count += 1;
                let child = directory.open_child(&name)?;
                child.validate_owned()?;
                walk_runtime_payload(&child, path, depth + 1, scan, skill_id)?;
            }
            libc::S_IFREG => {
                if scan.files.len() >= MAX_FILE_COUNT || stat.st_size < 0 {
                    return Err(deployment_conflict(Some(skill_id.clone())));
                }
                let size = stat.st_size as u64;
                if size > MAX_FILE_SIZE {
                    return Err(deployment_conflict(Some(skill_id.clone())));
                }
                scan.total_size = scan
                    .total_size
                    .checked_add(size)
                    .ok_or_else(|| deployment_conflict(Some(skill_id.clone())))?;
                if scan.total_size > MAX_TOTAL_SIZE {
                    return Err(deployment_conflict(Some(skill_id.clone())));
                }
                let (content, executable) = directory.read_payload_file(&name, MAX_FILE_SIZE)?;
                scan.files
                    .insert(path.as_os_str().as_bytes().to_vec(), (content, executable));
            }
            _ => return Err(deployment_conflict(Some(skill_id.clone()))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_manager::store::{SkillManager, TEST_OPERATION_LOCK};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    const TEST_ORG: &str = "12345678-1234-4234-8234-123456789abc";
    const TEST_ORG_B: &str = "87654321-4321-4321-8321-cba987654321";

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = crate::test_temp_root().join(format!(
                "csswitch-deployment-{label}-{}-{}",
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
                "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nbody\n",
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

    fn fixture() -> (TestDir, TestDir, TestDir, InstalledSkill, DeploymentService) {
        let config = TestDir::new("config");
        let data = TestDir::new("data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let manager = SkillManager::new(config_dir.clone());
        let skill = manager.import_source(&source.0).unwrap().skill;
        let service = DeploymentService::new(config_dir, data.0.join("science"));
        (config, data, source, skill, service)
    }

    fn set_active_org(data_dir: &Path, org_uuid: &str) {
        let data = SafeDir::ensure_absolute(data_dir).unwrap();
        data.validate_owned().unwrap();
        data.create_file(
            ACTIVE_ORG_FILE.as_ref(),
            serde_json::to_string(&serde_json::json!({ "org_uuid": org_uuid }))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn active_org_selects_shared_science_skill_root_and_preserves_unmanaged_entries() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("org-root-config");
        let data = TestDir::new("org-root-data");
        let source = TestDir::skill();
        let data_dir = data.0.join("science");
        set_active_org(&data_dir, TEST_ORG);
        let org_root = SafeDir::ensure_absolute(&data_dir.join("orgs").join(TEST_ORG)).unwrap();
        let shared = org_root.create_child("skills".as_ref()).unwrap();
        let bundled = shared.create_child("bundled-control".as_ref()).unwrap();
        bundled
            .create_file("SKILL.md".as_ref(), b"bundled")
            .unwrap();

        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let applied = manager.reconcile(&data_dir, false, "org-root").unwrap();
        assert!(applied.errors.is_empty(), "{applied:?}");
        let runtime_root = data_dir.join("orgs").join(TEST_ORG).join("skills");
        assert!(runtime_root.join(&skill.runtime_name).is_dir());
        assert_eq!(
            fs::read(runtime_root.join("bundled-control/SKILL.md")).unwrap(),
            b"bundled"
        );

        manager.set_enabled(&skill.skill_id, false).unwrap();
        let disabled = manager.reconcile(&data_dir, false, "disable").unwrap();
        assert!(disabled.errors.is_empty());
        assert!(!runtime_root.join(&skill.runtime_name).exists());
        assert_eq!(
            fs::read(runtime_root.join("bundled-control/SKILL.md")).unwrap(),
            b"bundled"
        );
    }

    #[test]
    fn active_org_file_and_path_fail_closed_on_invalid_or_symlinked_state() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("org-invalid-config");
        let data = TestDir::new("org-invalid-data");
        let source = TestDir::skill();
        let data_dir = data.0.join("science");
        let data_fd = SafeDir::ensure_absolute(&data_dir).unwrap();
        data_fd
            .create_file(ACTIVE_ORG_FILE.as_ref(), br#"{"org_uuid":"../escape"}"#)
            .unwrap();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let invalid = manager.reconcile(&data_dir, false, "invalid-org").unwrap();
        assert_eq!(invalid.errors.len(), 1);
        assert!(!data_dir.join("orgs").exists());

        fs::remove_file(data_dir.join(ACTIVE_ORG_FILE)).unwrap();
        let external = data.0.join("external-active-org");
        fs::write(
            &external,
            serde_json::to_vec(&serde_json::json!({ "org_uuid": TEST_ORG })).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&external, data_dir.join(ACTIVE_ORG_FILE)).unwrap();
        let linked = manager.reconcile(&data_dir, false, "linked-org").unwrap();
        assert_eq!(linked.errors.len(), 1);
        assert!(!data_dir.join("orgs").exists());
        assert!(external.is_file());
    }

    #[test]
    fn active_org_and_intermediate_directory_rename_swaps_fail_closed() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("org-swap-config");
        let data = TestDir::new("org-swap-data");
        let data_dir = data.0.join("science");
        set_active_org(&data_dir, TEST_ORG);
        let service = DeploymentService::new(config.0.join(".csswitch"), data_dir.clone());

        let held_data = data.0.join("science-held");
        let data_swap = || {
            fs::rename(&data_dir, &held_data).unwrap();
            fs::create_dir(&data_dir).unwrap();
            fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(
                data_dir.join(ACTIVE_ORG_FILE),
                serde_json::to_vec(&serde_json::json!({ "org_uuid": TEST_ORG })).unwrap(),
            )
            .unwrap();
            fs::set_permissions(
                data_dir.join(ACTIVE_ORG_FILE),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        };
        assert!(service.runtime_parent_with_hook(true, &data_swap).is_err());
        assert!(!data_dir.join("orgs").exists());
        assert!(!held_data.join("orgs").exists());

        fs::remove_dir_all(&data_dir).unwrap();
        fs::rename(&held_data, &data_dir).unwrap();
        let data_fd = SafeDir::open_absolute(&data_dir).unwrap();
        let orgs = data_fd.create_child("orgs".as_ref()).unwrap();
        let held_orgs = data_dir.join("orgs-held");
        let orgs_path = data_dir.join("orgs");
        let orgs_swap = || {
            fs::rename(&orgs_path, &held_orgs).unwrap();
            fs::create_dir(&orgs_path).unwrap();
            fs::set_permissions(&orgs_path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        assert!(service
            .runtime_parent_with_hooks(true, &|| {}, &orgs_swap)
            .is_err());
        assert!(!orgs_path.join(TEST_ORG).exists());
        assert!(!held_orgs.join(TEST_ORG).exists());
        drop(orgs);
    }

    #[test]
    fn active_org_file_replacement_never_deploys_to_stale_or_new_org() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("active-file-swap-config");
        let data = TestDir::new("active-file-swap-data");
        let data_dir = data.0.join("science");
        set_active_org(&data_dir, TEST_ORG);
        let data_fd = SafeDir::open_absolute(&data_dir).unwrap();
        let orgs = data_fd.create_child("orgs".as_ref()).unwrap();
        let _org_a = orgs.create_child(TEST_ORG.as_ref()).unwrap();
        let _org_b = orgs.create_child(TEST_ORG_B.as_ref()).unwrap();
        let service = DeploymentService::new(config.0.join(".csswitch"), data_dir.clone());
        let held_active = data_dir.join("active-org-held.json");
        let replace_active = || {
            fs::rename(data_dir.join(ACTIVE_ORG_FILE), &held_active).unwrap();
            fs::write(
                data_dir.join(ACTIVE_ORG_FILE),
                serde_json::to_vec(&serde_json::json!({ "org_uuid": TEST_ORG_B })).unwrap(),
            )
            .unwrap();
            fs::set_permissions(
                data_dir.join(ACTIVE_ORG_FILE),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        };

        assert!(service
            .runtime_parent_with_hook(true, &replace_active)
            .is_err());
        assert!(!data_dir.join("orgs").join(TEST_ORG).join("skills").exists());
        assert!(!data_dir
            .join("orgs")
            .join(TEST_ORG_B)
            .join("skills")
            .exists());
        let active: serde_json::Value =
            serde_json::from_slice(&fs::read(data_dir.join(ACTIVE_ORG_FILE)).unwrap()).unwrap();
        assert_eq!(active["org_uuid"], TEST_ORG_B);
    }

    #[test]
    fn runtime_generation_changes_when_same_org_skill_root_is_rebuilt() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("generation-config");
        let data = TestDir::new("generation-data");
        let source = TestDir::skill();
        let data_dir = data.0.join("science");
        set_active_org(&data_dir, TEST_ORG);
        let config_dir = config.0.join(".csswitch");
        let manager = SkillManager::new(config_dir.clone());
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        assert!(manager
            .reconcile(&data_dir, false, "generation-one")
            .unwrap()
            .errors
            .is_empty());
        let service = DeploymentService::new(config_dir, data_dir.clone());
        let first = service.runtime_fingerprint().unwrap();

        manager.set_enabled(&skill.skill_id, false).unwrap();
        assert!(manager
            .reconcile(&data_dir, false, "generation-disable")
            .unwrap()
            .errors
            .is_empty());
        fs::remove_dir_all(data_dir.join("orgs").join(TEST_ORG).join("skills")).unwrap();
        manager.set_enabled(&skill.skill_id, true).unwrap();
        assert!(manager
            .reconcile(&data_dir, false, "generation-two")
            .unwrap()
            .errors
            .is_empty());
        let second = service.runtime_fingerprint().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn double_marker_owned_runtime_is_removed_and_registry_is_updated() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, data, _source, skill, service) = fixture();
        service.seed_owned_runtime(&skill).unwrap();
        assert!(service
            .load_registry()
            .unwrap()
            .deployments
            .iter()
            .any(|record| record.skill_id == skill.skill_id));
        assert!(service.remove_owned_runtime_and_record(&skill).unwrap());
        assert!(service.load_registry().unwrap().deployments.is_empty());
        assert!(!data
            .0
            .join("science/skills")
            .join(&skill.runtime_name)
            .exists());
        assert!(!service.remove_owned_runtime_and_record(&skill).unwrap());
    }

    #[test]
    fn missing_or_forged_runtime_marker_fails_closed_without_deletion() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, data, _source, skill, service) = fixture();
        service.seed_owned_runtime(&skill).unwrap();
        let runtime = data.0.join("science/skills").join(&skill.runtime_name);
        fs::remove_file(runtime.join(RUNTIME_MARKER_FILE)).unwrap();
        let error = service.remove_owned_runtime_and_record(&skill).unwrap_err();
        assert_eq!(error.code, SkillErrorCode::DeploymentConflict);
        assert_eq!(error.skill_id.as_ref(), Some(&skill.skill_id));
        assert!(runtime.is_dir());
        assert!(service
            .load_registry()
            .unwrap()
            .deployments
            .iter()
            .any(|record| record.skill_id == skill.skill_id));

        let marker = RuntimeMarker {
            schema_version: 1,
            owner: RUNTIME_OWNER.to_string(),
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            content_hash: "f".repeat(64),
        };
        fs::write(
            runtime.join(RUNTIME_MARKER_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            runtime.join(RUNTIME_MARKER_FILE),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_eq!(
            service
                .remove_owned_runtime_and_record(&skill)
                .unwrap_err()
                .code,
            SkillErrorCode::DeploymentConflict
        );
        assert!(runtime.is_dir());
    }

    #[test]
    fn no_registry_record_never_touches_unmanaged_runtime_name() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, data, _source, skill, service) = fixture();
        let manual = data.0.join("science/skills").join(&skill.runtime_name);
        fs::create_dir_all(&manual).unwrap();
        fs::write(manual.join("manual.txt"), b"keep").unwrap();
        assert!(!service.remove_owned_runtime_and_record(&skill).unwrap());
        assert_eq!(fs::read(manual.join("manual.txt")).unwrap(), b"keep");
    }

    #[test]
    fn registry_schema_round_trip_and_invalid_records_fail_closed() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, _data, _source, skill, service) = fixture();
        let registry = DeploymentRegistry {
            schema_version: 1,
            deployments: vec![DeploymentRecord {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                content_hash: skill.content_hash.clone(),
            }],
        };
        service.save_registry(&registry).unwrap();
        assert_eq!(service.load_registry().unwrap(), registry);

        let mut too_new = registry.clone();
        too_new.schema_version = 2;
        assert_eq!(
            service.save_registry(&too_new).unwrap_err().code,
            SkillErrorCode::DeploymentConflict
        );
        let mut invalid_name = registry;
        invalid_name.deployments[0].runtime_name = "../escape".to_string();
        assert_eq!(
            service.save_registry(&invalid_name).unwrap_err().code,
            SkillErrorCode::DeploymentConflict
        );
    }

    #[test]
    fn registry_record_must_match_inventory_name_and_hash() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, data, _source, skill, service) = fixture();
        service.seed_owned_runtime(&skill).unwrap();
        let mut registry = service.load_registry().unwrap();
        registry.deployments[0].content_hash = "f".repeat(64);
        service.save_registry(&registry).unwrap();
        assert_eq!(
            service
                .remove_owned_runtime_and_record(&skill)
                .unwrap_err()
                .code,
            SkillErrorCode::DeploymentConflict
        );
        assert!(data
            .0
            .join("science/skills")
            .join(&skill.runtime_name)
            .is_dir());
    }

    #[test]
    fn stale_registry_after_sandbox_rebuild_is_cleared_without_claiming_runtime_removal() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, data, _source, skill, service) = fixture();
        service.seed_owned_runtime(&skill).unwrap();
        let skills = data.0.join("science/skills");
        fs::remove_dir_all(&skills).unwrap();
        fs::create_dir(&skills).unwrap();
        fs::set_permissions(&skills, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!service.remove_owned_runtime_and_record(&skill).unwrap());
        assert!(service.load_registry().unwrap().deployments.is_empty());
    }

    #[test]
    fn runtime_name_swap_after_marker_verification_never_deletes_replacement() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_config, data, _source, skill, service) = fixture();
        service.seed_owned_runtime(&skill).unwrap();
        let record = service.load_registry().unwrap().deployments[0].clone();
        let runtime = data.0.join("science/skills").join(&skill.runtime_name);
        fs::write(runtime.join("owned.txt"), b"owned-before").unwrap();
        fs::set_permissions(runtime.join("owned.txt"), fs::Permissions::from_mode(0o600)).unwrap();
        let original_marker = fs::read(runtime.join(RUNTIME_MARKER_FILE)).unwrap();
        let original_payload = fs::read(runtime.join("owned.txt")).unwrap();
        let held = data.0.join("science/skills/held-runtime");
        let hook = || {
            fs::rename(&runtime, &held).unwrap();
            fs::create_dir(&runtime).unwrap();
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(runtime.join("manual.txt"), b"keep").unwrap();
        };
        assert_eq!(
            service
                .verify_and_remove_runtime_with_hook(&record, &hook)
                .unwrap_err()
                .code,
            SkillErrorCode::DeploymentConflict
        );
        assert_eq!(fs::read(runtime.join("manual.txt")).unwrap(), b"keep");
        assert!(held.is_dir());
        assert_eq!(
            fs::read(held.join(RUNTIME_MARKER_FILE)).unwrap(),
            original_marker
        );
        assert_eq!(fs::read(held.join("owned.txt")).unwrap(), original_payload);
    }

    #[test]
    fn reconcile_dry_run_deploy_idempotence_disable_and_sandbox_restore() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("reconcile-config");
        let data = TestDir::new("reconcile-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let data_dir = data.0.join("science");

        let dry = manager.reconcile(&data_dir, true, "manual").unwrap();
        assert_eq!(dry.planned.len(), 1);
        assert!(dry.applied.is_empty());
        assert!(!data_dir.exists());

        let applied = manager.reconcile(&data_dir, false, "before_start").unwrap();
        assert_eq!(applied.applied.len(), 1);
        assert!(applied.restart_required);
        let runtime = data_dir.join("skills").join(&skill.runtime_name);
        assert!(runtime.join("SKILL.md").is_file());
        assert_eq!(
            fs::symlink_metadata(runtime.join("SKILL.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let second = manager.reconcile(&data_dir, false, "before_start").unwrap();
        assert!(second.applied.is_empty());
        assert_eq!(second.skipped.len(), 1);
        assert!(!second.restart_required);

        fs::remove_dir_all(&data_dir).unwrap();
        let restored = manager
            .reconcile(&data_dir, false, "sandbox_rebuild")
            .unwrap();
        assert_eq!(restored.applied.len(), 1);
        assert!(runtime.join("SKILL.md").is_file());

        manager.set_enabled(&skill.skill_id, false).unwrap();
        let disabled = manager.reconcile(&data_dir, false, "disable").unwrap();
        assert_eq!(disabled.applied.len(), 1);
        assert!(disabled.restart_required);
        assert!(!runtime.exists());
        let repeat = manager.reconcile(&data_dir, false, "disable").unwrap();
        assert!(repeat.applied.is_empty());
    }

    #[test]
    fn reconcile_unmanaged_and_symlinked_runtime_roots_fail_closed() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("conflict-config");
        let data = TestDir::new("conflict-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let data_dir = data.0.join("science");
        fs::create_dir_all(data_dir.join("skills").join(&skill.runtime_name)).unwrap();
        fs::write(
            data_dir
                .join("skills")
                .join(&skill.runtime_name)
                .join("manual"),
            b"keep",
        )
        .unwrap();
        let report = manager.reconcile(&data_dir, false, "conflict").unwrap();
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            fs::read(
                data_dir
                    .join("skills")
                    .join(&skill.runtime_name)
                    .join("manual")
            )
            .unwrap(),
            b"keep"
        );

        fs::remove_dir_all(data_dir.join("skills")).unwrap();
        let external = data.0.join("external");
        fs::create_dir(&external).unwrap();
        std::os::unix::fs::symlink(&external, data_dir.join("skills")).unwrap();
        let report = manager.reconcile(&data_dir, false, "symlink").unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(fs::read_dir(external).unwrap().next().is_none());
    }

    #[test]
    fn failed_replace_keeps_old_runtime_and_cleans_staging() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("replace-config");
        let data = TestDir::new("replace-data");
        let source = TestDir::skill();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        let data_dir = data.0.join("science");
        manager.reconcile(&data_dir, false, "initial").unwrap();

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nnew\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let service = DeploymentService::new(config.0.join(".csswitch"), data_dir.clone());
        let root = service.open_runtime_root_if_present().unwrap().unwrap();
        let existing = root.open_child(updated.runtime_name.as_ref()).unwrap();
        let files = manager.payload_files(&updated).unwrap();
        let error = service
            .deploy_runtime_with_hook(&root, &updated, &files, Some(&existing), &|| {
                Err(deployment_conflict(Some(updated.skill_id.clone())))
            })
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::DeploymentConflict);
        let marker = read_runtime_marker(&existing, &original.skill_id).unwrap();
        assert_eq!(marker.content_hash, original.content_hash);
        assert!(!root
            .names()
            .unwrap()
            .iter()
            .any(|name| name.to_string_lossy().starts_with(".staging-")));
    }

    #[test]
    fn missing_registry_is_recovered_before_disable_and_uninstall() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("recover-config");
        let data = TestDir::new("recover-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();
        let service = DeploymentService::new(config_dir.clone(), data_dir.clone());
        service
            .save_registry(&DeploymentRegistry::default())
            .unwrap();

        manager.set_enabled(&skill.skill_id, false).unwrap();
        let disabled = manager.reconcile(&data_dir, false, "disable").unwrap();
        assert!(disabled.errors.is_empty());
        assert_eq!(disabled.applied.len(), 1);
        assert!(!data_dir.join("skills").join(&skill.runtime_name).exists());
        assert!(service.load_registry().unwrap().deployments.is_empty());

        manager.set_enabled(&skill.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "redeploy").unwrap();
        service
            .save_registry(&DeploymentRegistry::default())
            .unwrap();
        let outcome = manager.uninstall(&skill.skill_id, &data_dir).unwrap();
        assert!(outcome.runtime_removed);
        assert!(!data_dir.join("skills").join(&skill.runtime_name).exists());
        assert!(manager.load_inventory().unwrap().skills.is_empty());
    }

    #[test]
    fn runtime_name_change_migrates_safely_and_retries_after_commit() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("rename-config");
        let data = TestDir::new("rename-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();
        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Renamed Probe\ndescription: Deployment ownership probe\n---\nnew\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        assert_ne!(updated.runtime_name, original.runtime_name);

        let service = DeploymentService::new(config_dir, data_dir.clone());
        let root = service.open_runtime_root_if_present().unwrap().unwrap();
        let files = manager.payload_files(&updated).unwrap();
        service
            .deploy_runtime(&root, &updated, &files, None)
            .unwrap();
        let migrated = manager.reconcile(&data_dir, false, "rename_retry").unwrap();
        assert!(migrated.errors.is_empty(), "{:?}", migrated.errors);
        assert!(migrated.restart_required);
        assert!(!data_dir
            .join("skills")
            .join(&original.runtime_name)
            .exists());
        assert!(data_dir
            .join("skills")
            .join(&updated.runtime_name)
            .join("SKILL.md")
            .is_file());
        let registry = service.load_registry().unwrap();
        assert_eq!(registry.deployments.len(), 1);
        assert_eq!(registry.deployments[0].runtime_name, updated.runtime_name);
        assert_eq!(registry.deployments[0].content_hash, updated.content_hash);
    }

    #[test]
    fn runtime_name_conflict_preserves_old_and_unmanaged_new_directories() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("rename-conflict-config");
        let data = TestDir::new("rename-conflict-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir);
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();
        let original_runtime = data_dir.join("skills").join(&original.runtime_name);
        let original_payload = fs::read(original_runtime.join("SKILL.md")).unwrap();
        let original_marker = fs::read(original_runtime.join(RUNTIME_MARKER_FILE)).unwrap();
        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Conflicting Probe\ndescription: Deployment ownership probe\n---\nnew\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let manual = data_dir.join("skills").join(&updated.runtime_name);
        fs::create_dir(&manual).unwrap();
        fs::set_permissions(&manual, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(manual.join("manual.txt"), b"keep").unwrap();

        let report = manager
            .reconcile(&data_dir, false, "rename_conflict")
            .unwrap();
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            fs::read(original_runtime.join("SKILL.md")).unwrap(),
            original_payload
        );
        assert_eq!(
            fs::read(original_runtime.join(RUNTIME_MARKER_FILE)).unwrap(),
            original_marker
        );
        assert_eq!(fs::read(manual.join("manual.txt")).unwrap(), b"keep");
    }

    #[test]
    fn staging_replacement_and_post_commit_replacement_are_never_name_deleted() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (config, data, _source, skill, service) = fixture();
        let manager = SkillManager::new(config.0.join(".csswitch"));
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let files = manager.payload_files(&skill).unwrap();
        let root = service.ensure_runtime_root().unwrap();
        let skills_path = data.0.join("science/skills");
        let held_snapshot = std::cell::RefCell::new(None);

        let before_commit = || {
            let staging = root
                .names()
                .unwrap()
                .into_iter()
                .find(|name| name.to_string_lossy().starts_with(".staging-"))
                .unwrap();
            held_snapshot.replace(Some((
                fs::read(skills_path.join(&staging).join("SKILL.md")).unwrap(),
                fs::read(skills_path.join(&staging).join(RUNTIME_MARKER_FILE)).unwrap(),
            )));
            fs::rename(skills_path.join(&staging), skills_path.join("held-before")).unwrap();
            fs::create_dir(skills_path.join(&staging)).unwrap();
            fs::set_permissions(
                skills_path.join(&staging),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::write(skills_path.join(&staging).join("manual.txt"), b"keep").unwrap();
            Err(deployment_conflict(Some(skill.skill_id.clone())))
        };
        assert!(service
            .deploy_runtime_with_hooks(&root, &skill, &files, None, &before_commit, &|| Ok(()))
            .is_err());
        let replacement = root
            .names()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".staging-"))
            .unwrap();
        assert_eq!(
            fs::read(skills_path.join(replacement).join("manual.txt")).unwrap(),
            b"keep"
        );
        let (held_payload, held_marker) = held_snapshot.borrow().clone().unwrap();
        assert_eq!(
            fs::read(skills_path.join("held-before/SKILL.md")).unwrap(),
            held_payload
        );
        assert_eq!(
            fs::read(skills_path.join("held-before").join(RUNTIME_MARKER_FILE)).unwrap(),
            held_marker
        );

        let fresh_data = TestDir::new("post-commit-data");
        let post_service =
            DeploymentService::new(config.0.join(".csswitch"), fresh_data.0.join("science"));
        let post_root = post_service.ensure_runtime_root().unwrap();
        let post_error = post_service
            .deploy_runtime_with_hooks(&post_root, &skill, &files, None, &|| Ok(()), &|| {
                Err(deployment_conflict(Some(skill.skill_id.clone())))
            })
            .unwrap_err();
        assert_eq!(post_error.code, SkillErrorCode::DeploymentConflict);
        assert!(fresh_data
            .0
            .join("science/skills")
            .join(&skill.runtime_name)
            .join("SKILL.md")
            .is_file());
        assert!(post_service.load_registry().unwrap().deployments.is_empty());
        let adopted = manager
            .reconcile(&fresh_data.0.join("science"), false, "recover_commit")
            .unwrap();
        assert!(adopted.errors.is_empty());
        assert!(adopted.restart_required);
        assert!(matches!(
            adopted.applied.first().map(|item| &item.action),
            Some(ReconcileAction::Adopt)
        ));
    }

    #[test]
    fn stale_registry_after_same_name_swap_recovers_for_disable_and_uninstall() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("swap-recovery-config");
        let data = TestDir::new("swap-recovery-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nsecond\n",
        )
        .unwrap();
        let second = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let service = DeploymentService::new(config_dir.clone(), data_dir.clone());
        let root = service.open_runtime_root_if_present().unwrap().unwrap();
        let record = service.load_registry().unwrap().deployments[0].clone();
        let mut stored = second.clone();
        stored.content_hash = record.content_hash.clone();
        let existing = service
            .verify_named_runtime(
                &root,
                &record.runtime_name,
                &record,
                &manager.payload_files(&stored).unwrap(),
            )
            .unwrap();
        service
            .deploy_runtime(
                &root,
                &second,
                &manager.payload_files(&second).unwrap(),
                Some(existing),
            )
            .unwrap();
        assert_eq!(
            service.load_registry().unwrap().deployments[0].content_hash,
            original.content_hash
        );

        manager.set_enabled(&second.skill_id, false).unwrap();
        let disabled = manager
            .reconcile(&data_dir, false, "disable_after_swap")
            .unwrap();
        assert!(disabled.errors.is_empty(), "{:?}", disabled.errors);
        assert!(disabled.restart_required);
        assert!(!data_dir.join("skills").join(&second.runtime_name).exists());
        assert!(service.load_registry().unwrap().deployments.is_empty());

        manager.set_enabled(&second.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "redeploy").unwrap();
        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nthird\n",
        )
        .unwrap();
        let third = manager
            .update_source(&second.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let root = service.open_runtime_root_if_present().unwrap().unwrap();
        let record = service.load_registry().unwrap().deployments[0].clone();
        let mut stored = third.clone();
        stored.content_hash = record.content_hash.clone();
        let existing = service
            .verify_named_runtime(
                &root,
                &record.runtime_name,
                &record,
                &manager.payload_files(&stored).unwrap(),
            )
            .unwrap();
        service
            .deploy_runtime(
                &root,
                &third,
                &manager.payload_files(&third).unwrap(),
                Some(existing),
            )
            .unwrap();
        let removed = manager.uninstall(&third.skill_id, &data_dir).unwrap();
        assert!(removed.runtime_removed);
        assert!(!data_dir.join("skills").join(&third.runtime_name).exists());
        assert!(manager.load_inventory().unwrap().skills.is_empty());
    }

    #[test]
    fn dry_run_adopt_requires_restart_and_unsafe_existing_root_fails_closed() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("dry-adopt-config");
        let data = TestDir::new("dry-adopt-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let service = DeploymentService::new(config_dir, data_dir.clone());
        let root = service.ensure_runtime_root().unwrap();
        service
            .deploy_runtime(&root, &skill, &manager.payload_files(&skill).unwrap(), None)
            .unwrap();

        let dry = manager.reconcile(&data_dir, true, "dry_adopt").unwrap();
        assert!(dry.errors.is_empty());
        assert!(dry.restart_required);
        assert!(matches!(
            dry.planned.first().map(|item| &item.action),
            Some(ReconcileAction::Adopt)
        ));
        assert!(service.load_registry().unwrap().deployments.is_empty());

        fs::remove_dir_all(data_dir.join("skills")).unwrap();
        let external = data.0.join("external");
        fs::create_dir(&external).unwrap();
        std::os::unix::fs::symlink(&external, data_dir.join("skills")).unwrap();
        manager.set_enabled(&skill.skill_id, false).unwrap();
        let unsafe_report = manager.reconcile(&data_dir, false, "unsafe_root").unwrap();
        assert_eq!(unsafe_report.errors.len(), 1);
        assert!(manager.uninstall(&skill.skill_id, &data_dir).is_err());
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 1);
        assert!(fs::read_dir(&external).unwrap().next().is_none());
    }

    #[test]
    fn committed_swap_orphans_are_reconciled_for_retry_disable_and_uninstall() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("orphan-config");
        let data = TestDir::new("orphan-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();
        let service = DeploymentService::new(config_dir, data_dir.clone());

        let swap_with_post_commit_error = |skill: &InstalledSkill| {
            let root = service.open_runtime_root_if_present().unwrap().unwrap();
            let existing = root.open_child(skill.runtime_name.as_ref()).unwrap();
            let files = manager.payload_files(skill).unwrap();
            service
                .deploy_runtime_with_hooks(
                    &root,
                    skill,
                    &files,
                    Some(&existing),
                    &|| Ok(()),
                    &|| Err(deployment_conflict(Some(skill.skill_id.clone()))),
                )
                .unwrap_err();
            assert!(root
                .names()
                .unwrap()
                .iter()
                .any(|name| name.to_string_lossy().starts_with(".staging-")));
        };

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\ntwo\n",
        )
        .unwrap();
        let second = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        swap_with_post_commit_error(&second);
        let current_marker = data_dir
            .join("skills")
            .join(&second.runtime_name)
            .join(RUNTIME_MARKER_FILE);
        let marker_bytes = fs::read(&current_marker).unwrap();
        fs::write(&current_marker, b"{}\n").unwrap();
        let blocked = manager
            .reconcile(&data_dir, false, "corrupt_current")
            .unwrap();
        assert_eq!(blocked.errors.len(), 1);
        assert!(fs::read_dir(data_dir.join("skills"))
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-")));
        fs::write(&current_marker, marker_bytes).unwrap();
        let retried = manager.reconcile(&data_dir, false, "retry").unwrap();
        assert!(retried.errors.is_empty(), "{:?}", retried.errors);
        assert!(retried
            .applied
            .iter()
            .any(|item| item.detail == "recover_orphan_staging"));
        assert!(!fs::read_dir(data_dir.join("skills"))
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-")));

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nthree\n",
        )
        .unwrap();
        let third = manager
            .update_source(&second.skill_id, &source.0, false)
            .unwrap()
            .skill;
        swap_with_post_commit_error(&third);
        manager.set_enabled(&third.skill_id, false).unwrap();
        let disabled = manager.reconcile(&data_dir, false, "disable").unwrap();
        assert!(disabled.errors.is_empty(), "{:?}", disabled.errors);
        assert!(!data_dir.join("skills").join(&third.runtime_name).exists());
        assert!(!fs::read_dir(data_dir.join("skills"))
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-")));

        manager.set_enabled(&third.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "redeploy").unwrap();
        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nfour\n",
        )
        .unwrap();
        let fourth = manager
            .update_source(&third.skill_id, &source.0, false)
            .unwrap()
            .skill;
        swap_with_post_commit_error(&fourth);
        let removed = manager.uninstall(&fourth.skill_id, &data_dir).unwrap();
        assert!(removed.runtime_removed);
        assert!(manager.load_inventory().unwrap().skills.is_empty());
        assert!(!fs::read_dir(data_dir.join("skills"))
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-")));
    }

    #[test]
    fn rename_dual_runtime_validation_precedes_any_deletion() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("rename-preflight-config");
        let data = TestDir::new("rename-preflight-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();
        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Renamed Preflight\ndescription: Deployment ownership probe\n---\nnew\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let service = DeploymentService::new(config_dir, data_dir.clone());
        let root = service.open_runtime_root_if_present().unwrap().unwrap();
        service
            .deploy_runtime(
                &root,
                &updated,
                &manager.payload_files(&updated).unwrap(),
                None,
            )
            .unwrap();
        let old_runtime = data_dir.join("skills").join(&original.runtime_name);
        fs::write(old_runtime.join(RUNTIME_MARKER_FILE), b"{}\n").unwrap();
        manager.set_enabled(&updated.skill_id, false).unwrap();

        let report = manager
            .reconcile(&data_dir, false, "disable_conflict")
            .unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(data_dir
            .join("skills")
            .join(&updated.runtime_name)
            .join("SKILL.md")
            .is_file());
        assert!(old_runtime.is_dir());
        assert!(manager.uninstall(&updated.skill_id, &data_dir).is_err());
        assert!(data_dir
            .join("skills")
            .join(&updated.runtime_name)
            .join("SKILL.md")
            .is_file());
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 1);
    }

    #[test]
    fn replace_and_rename_cleanup_preserve_post_validation_insertions() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("runtime-inner-race-config");
        let data = TestDir::new("runtime-inner-race-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let original = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&original.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "initial").unwrap();
        let service = DeploymentService::new(config_dir, data_dir.clone());

        fs::write(
            source.0.join("SKILL.md"),
            "---\nname: Deploy Probe\ndescription: Deployment ownership probe\n---\nnew\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&original.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let root = service.open_runtime_root_if_present().unwrap().unwrap();
        let existing = root.open_child(updated.runtime_name.as_ref()).unwrap();
        let files = manager.payload_files(&updated).unwrap();
        let skills_path = data_dir.join("skills");
        let after_commit = || {
            let staging = root
                .names()
                .unwrap()
                .into_iter()
                .find(|name| name.to_string_lossy().starts_with(".staging-"))
                .unwrap();
            fs::write(skills_path.join(staging).join("late-unmanaged"), b"keep").unwrap();
            Ok(())
        };
        assert!(service
            .deploy_runtime_with_hooks(
                &root,
                &updated,
                &files,
                Some(&existing),
                &|| Ok(()),
                &after_commit,
            )
            .is_err());
        let staging = root
            .names()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".staging-"))
            .unwrap();
        assert_eq!(
            fs::read(skills_path.join(&staging).join("late-unmanaged")).unwrap(),
            b"keep"
        );
        assert!(skills_path.join(&staging).join("SKILL.md").is_file());
        assert!(skills_path
            .join(&staging)
            .join(RUNTIME_MARKER_FILE)
            .is_file());
        assert!(skills_path
            .join(&updated.runtime_name)
            .join("SKILL.md")
            .is_file());

        let rename_data = TestDir::new("rename-inner-race-data");
        let rename_service =
            DeploymentService::new(manager.paths.root.clone(), rename_data.0.join("science"));
        let rename_root = rename_service.ensure_runtime_root().unwrap();
        rename_service
            .deploy_runtime(&rename_root, &updated, &files, None)
            .unwrap();
        let record = DeploymentRecord {
            skill_id: updated.skill_id.clone(),
            runtime_name: updated.runtime_name.clone(),
            content_hash: updated.content_hash.clone(),
        };
        let verified = rename_service
            .verify_named_runtime(&rename_root, &updated.runtime_name, &record, &files)
            .unwrap();
        let runtime_path = rename_data
            .0
            .join("science/skills")
            .join(&updated.runtime_name);
        fs::write(runtime_path.join("late-unmanaged"), b"keep").unwrap();
        assert!(verified.remove().is_err());
        assert_eq!(
            fs::read(runtime_path.join("late-unmanaged")).unwrap(),
            b"keep"
        );
        assert!(runtime_path.join("SKILL.md").is_file());
        assert!(runtime_path.join(RUNTIME_MARKER_FILE).is_file());
    }

    #[test]
    fn science_start_clears_restart_only_for_consistent_deployments() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = TestDir::new("start-state-config");
        let data = TestDir::new("start-state-data");
        let source = TestDir::skill();
        let config_dir = config.0.join(".csswitch");
        let data_dir = data.0.join("science");
        let manager = SkillManager::new(config_dir.clone());
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        manager.reconcile(&data_dir, false, "before_start").unwrap();
        assert!(manager.has_pending_restart().unwrap());
        let runtime = data_dir.join("skills").join(&skill.runtime_name);

        fs::remove_dir_all(&runtime).unwrap();
        assert_eq!(
            manager.mark_science_started(&data_dir).unwrap_err().code,
            SkillErrorCode::DeploymentConflict
        );
        assert!(manager.has_pending_restart().unwrap());
        manager
            .reconcile(&data_dir, false, "restore_missing")
            .unwrap();

        fs::write(runtime.join("SKILL.md"), b"tampered\n").unwrap();
        assert_eq!(
            manager.mark_science_started(&data_dir).unwrap_err().code,
            SkillErrorCode::DeploymentConflict
        );
        assert!(manager.has_pending_restart().unwrap());
        fs::remove_dir_all(&runtime).unwrap();
        manager
            .reconcile(&data_dir, false, "restore_tampered")
            .unwrap();

        let cleared = manager.mark_science_started(&data_dir).unwrap();
        assert_eq!(cleared, vec![skill.skill_id.clone()]);
        assert!(!manager.has_pending_restart().unwrap());
        let installed = manager.load_inventory().unwrap().skills.remove(0);
        assert_eq!(
            installed.deployment_status,
            crate::skill_manager::model::DeploymentStatus::Deployed
        );
        assert!(!installed.restart_required);

        manager.set_enabled(&skill.skill_id, false).unwrap();
        manager.reconcile(&data_dir, false, "disable").unwrap();
        assert!(manager.has_pending_restart().unwrap());
        let service = DeploymentService::new(config_dir, data_dir.clone());
        let root = service.ensure_runtime_root().unwrap();
        service
            .deploy_runtime(&root, &skill, &manager.payload_files(&skill).unwrap(), None)
            .unwrap();
        let error = manager.mark_science_started(&data_dir).unwrap_err();
        assert_eq!(error.code, SkillErrorCode::DeploymentConflict);
        assert!(manager.has_pending_restart().unwrap());
    }
}
