use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, OsStr};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime::capability_catalog::CapabilityCatalog;

use super::compatibility::{
    acknowledgment_for, evaluate_compatibility_gate, CompatibilityErrorCode, CompatibilityGate,
    CompatibilityStatus, RuntimeContext,
};
use super::deployment::{DeploymentRegistry, DeploymentService, ReconcileReport};
use super::discovery::{
    evaluate_status, validate_science_version, DiscoveryEvidence, DiscoveryEvidenceRegistry,
    ScienceProbeState, SkillManagerStatus,
};
use super::error::{SkillErrorCode, SkillManagerError, SkillResult};
use super::inspection::{
    inspect_skill_source, InspectedFile, InspectionResult, MAX_DIRECTORY_COUNT, MAX_FILE_COUNT,
    MAX_FILE_SIZE, MAX_PATH_BYTES, MAX_PATH_DEPTH, MAX_SKILL_MD_SIZE, MAX_TOTAL_SIZE,
};
use super::model::{
    runtime_name, DeploymentStatus, DiscoveryStatus, InstalledSkill, Inventory, SkillId,
    SkillSource, ValidationStatus,
};
use super::requirements::SkillRequirements;

const INVENTORY_FILE: &str = "inventory.v1.json";
const STORE_MARKER_FILE: &str = ".csswitch-owner.v1.json";
const STORE_MARKER_OWNER: &str = "csswitch.skill-store";
const ROOT_MARKER_FILE: &str = ".csswitch-skill-root.v1.json";
const ROOT_MARKER_OWNER: &str = "csswitch.skill-root";
const MAX_INVENTORY_SIZE: u64 = 8 * 1024 * 1024;
const DISCOVERY_FILE: &str = "discovery.v1.json";
const MAX_DISCOVERY_SIZE: u64 = 4 * 1024 * 1024;
const MAX_EXTERNAL_VERSIONS_PER_SKILL: usize = 8;
const MAX_EXTERNAL_STORED_BYTES_PER_SKILL: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_EXTERNAL_SKILL_COUNT: usize = 128;
const MAX_OWNED_STORE_VERSIONS: usize = 1_024;
const MAX_OWNED_STORE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

static WRITE_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
pub(crate) static TEST_OPERATION_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
pub(crate) static OWNED_STORE_AUDIT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkillPaths {
    pub(crate) root: PathBuf,
    pub(crate) store: PathBuf,
    pub(crate) inventory: PathBuf,
}

impl SkillPaths {
    pub(crate) fn from_config_dir(config_dir: &Path) -> Self {
        let root = config_dir.join("skills");
        Self {
            store: root.join("store"),
            inventory: root.join(INVENTORY_FILE),
            root,
        }
    }

    pub(crate) fn payload(&self, skill_id: &SkillId, hash: &str) -> PathBuf {
        self.store
            .join(skill_id.as_str())
            .join(hash)
            .join("payload")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultPoint {
    BeforeRootMarker,
    AfterRootMarker,
    AfterStaging,
    BeforeStoreRename,
    AfterStoreRename,
    BeforeInventoryRename,
    BeforeInventoryRenameCleanupReadFails,
    AfterInventoryRename,
    BeforeVersionGc,
}

#[derive(Clone, Debug)]
pub(crate) struct SkillManager {
    config_dir: PathBuf,
    pub(crate) paths: SkillPaths,
    fault: Option<FaultPoint>,
}

pub(crate) struct ExternalScanBatch<'a> {
    manager: &'a SkillManager,
    _guard: MutexGuard<'static, ()>,
    quota: ExternalBatchQuota,
}

struct ExternalBatchQuota {
    external_skills: usize,
    usage: OwnedStoreUsage,
    valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstallOutcome {
    pub(crate) skill: InstalledSkill,
    pub(crate) changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnableOutcome {
    pub(crate) skill: InstalledSkill,
    pub(crate) changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UninstallOutcome {
    pub(crate) skill_id: SkillId,
    pub(crate) changed: bool,
    pub(crate) runtime_removed: bool,
    pub(crate) store_gc_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreRecoveryReport {
    pub(crate) quarantine_path: PathBuf,
    pub(crate) recovered: usize,
    pub(crate) skipped: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OwnedStoreUsage {
    versions: usize,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreVersionPreflight {
    Absent,
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreVersionCommit {
    Created,
    AlreadyPresent,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreMarker {
    schema_version: u32,
    owner: String,
    skill_id: SkillId,
    content_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootMarker {
    schema_version: u32,
    owner: String,
}

impl SkillManager {
    pub(crate) fn new(config_dir: PathBuf) -> Self {
        Self {
            paths: SkillPaths::from_config_dir(&config_dir),
            config_dir,
            fault: None,
        }
    }

    #[cfg(test)]
    fn with_fault(mut self, fault: FaultPoint) -> Self {
        self.fault = Some(fault);
        self
    }

    pub(crate) fn inspect(&self, source: &Path) -> SkillResult<InspectionResult> {
        inspect_skill_source(source)
    }

    /// Moves a conflicting owned Skill root aside and salvages every payload still accepted by
    /// the normal inspector. Nothing in the quarantine is deleted.
    pub(crate) fn quarantine_and_restore_store(&self) -> SkillResult<StoreRecoveryReport> {
        let (old_inventory, quarantine_path) = {
            let _guard = acquire_write_lock()?;
            assert_path_chain_no_symlink(&self.config_dir)?;
            let metadata = fs::symlink_metadata(&self.paths.root).map_err(|_| store_conflict())?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err(store_conflict());
            }
            let old_inventory = fs::read(&self.paths.inventory)
                .ok()
                .filter(|bytes| bytes.len() as u64 <= MAX_INVENTORY_SIZE)
                .and_then(|bytes| serde_json::from_slice::<Inventory>(&bytes).ok())
                .unwrap_or_default();
            let quarantine_path = self.config_dir.join(format!(
                "skills.quarantine.{}.{}",
                now_ms(),
                std::process::id()
            ));
            if quarantine_path.exists() {
                return Err(store_conflict());
            }
            fs::rename(&self.paths.root, &quarantine_path).map_err(|_| atomic_error())?;
            (old_inventory, quarantine_path)
        };

        let mut recovered = 0_usize;
        let mut skipped = 0_usize;
        for old in old_inventory.skills {
            let payload = quarantine_path
                .join("store")
                .join(old.skill_id.as_str())
                .join(&old.content_hash)
                .join("payload");
            let restored = (|| {
                let outcome = self.import_source(&payload)?;
                if old.enabled {
                    self.set_enabled(&outcome.skill.skill_id, true)?;
                }
                Ok::<(), SkillManagerError>(())
            })();
            if restored.is_ok() {
                recovered += 1;
            } else {
                skipped += 1;
            }
        }
        Ok(StoreRecoveryReport {
            quarantine_path,
            recovered,
            skipped,
        })
    }

    pub(crate) fn verify_skill_store(&self, skill: &InstalledSkill) -> SkillResult<()> {
        self.verify_store_version(skill)
            .map_err(|error| error.with_skill_id(skill.skill_id.clone()))
    }

    pub(crate) fn payload_files(&self, skill: &InstalledSkill) -> SkillResult<Vec<InspectedFile>> {
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        let id = store.open_child(skill.skill_id.as_str().as_ref())?;
        let version = id.open_child(skill.content_hash.as_ref())?;
        verify_store_version_fd(skill, &version)?;
        let payload = version.open_child("payload".as_ref())?;
        let mut scan = StoredPayloadScan::default();
        walk_stored_payload(&payload, Path::new(""), 0, &mut scan)?;
        use std::os::unix::ffi::OsStringExt;
        Ok(scan
            .files
            .into_iter()
            .map(|(path, file)| InspectedFile {
                relative_path: PathBuf::from(std::ffi::OsString::from_vec(path)),
                size: file.content.len() as u64,
                executable: file.executable,
                content: file.content,
            })
            .collect())
    }

    pub(crate) fn load_inventory(&self) -> SkillResult<Inventory> {
        match fs::symlink_metadata(&self.config_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Inventory::default())
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(inventory_invalid())
            }
            Ok(_) => {}
            Err(_) => return Err(inventory_invalid()),
        }
        assert_path_chain_no_symlink(&self.config_dir)?;
        match fs::symlink_metadata(&self.paths.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Inventory::default())
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(inventory_invalid())
            }
            Ok(_) => {}
            Err(_) => return Err(inventory_invalid()),
        }
        let root = SafeDir::open_absolute(&self.paths.root).map_err(|_| inventory_invalid())?;
        root.validate_owned().map_err(|_| inventory_invalid())?;
        verify_root_marker_fd(&root).map_err(|_| inventory_invalid())?;
        let inventory_name =
            os_cstring(INVENTORY_FILE.as_ref()).map_err(|_| inventory_invalid())?;
        if root
            .child_stat(&inventory_name)
            .map_err(|_| inventory_invalid())?
            .is_none()
        {
            return Ok(Inventory::default());
        }
        let data = root
            .read_file(INVENTORY_FILE.as_ref(), MAX_INVENTORY_SIZE)
            .map_err(|_| inventory_invalid())?;
        Inventory::from_slice(&data).map_err(|_| inventory_invalid())
    }

    pub(crate) fn load_discovery_evidence(&self) -> SkillResult<DiscoveryEvidenceRegistry> {
        match fs::symlink_metadata(&self.paths.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DiscoveryEvidenceRegistry::default())
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(inventory_invalid())
            }
            Ok(_) => {}
            Err(_) => return Err(inventory_invalid()),
        }
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let name = os_cstring(DISCOVERY_FILE.as_ref())?;
        if root.child_stat(&name)?.is_none() {
            return Ok(DiscoveryEvidenceRegistry::default());
        }
        let data = root.read_file(DISCOVERY_FILE.as_ref(), MAX_DISCOVERY_SIZE)?;
        let evidence: DiscoveryEvidenceRegistry =
            serde_json::from_slice(&data).map_err(|_| inventory_invalid())?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub(crate) fn recover_store_orphans(&self, data_dir: &Path) -> SkillResult<usize> {
        let _guard = acquire_write_lock()?;
        if !self.paths.root.exists() {
            return Ok(0);
        }
        let inventory = self.load_inventory()?;
        let registry = DeploymentService::new(self.config_dir.clone(), data_dir.to_path_buf())
            .load_registry()?;
        let mut referenced = BTreeSet::new();
        for skill in &inventory.skills {
            referenced.insert((skill.skill_id.clone(), skill.content_hash.clone()));
        }
        for deployment in &registry.deployments {
            referenced.insert((deployment.skill_id.clone(), deployment.content_hash.clone()));
        }

        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let staging_names = root
            .names()?
            .into_iter()
            .filter(|name| name.to_string_lossy().starts_with(".staging-"))
            .collect::<Vec<_>>();
        if staging_names.len() > MAX_OWNED_STORE_VERSIONS {
            return Err(storage_limit_error("Skill staging 数量超过恢复上限"));
        }
        let mut recovered = 0_usize;
        for name in staging_names {
            let staging = root.open_child(&name)?;
            let marker_data = staging.read_file(STORE_MARKER_FILE.as_ref(), 4_096)?;
            let marker: StoreMarker =
                serde_json::from_slice(&marker_data).map_err(|_| store_conflict())?;
            verify_owned_store_version(&marker.skill_id, &marker.content_hash, &staging)?;
            if referenced.contains(&(marker.skill_id.clone(), marker.content_hash.clone())) {
                return Err(store_conflict());
            }
            let captured = CapturedTree::capture(&staging)?;
            captured.verify_unchanged()?;
            root.verify_child_identity(&name, &staging)?;
            captured.remove_contents()?;
            root.remove_empty_verified_child(&name, &staging)?;
            recovered += 1;
        }

        let store = root.open_child("store".as_ref())?;
        let mut orphan_versions = Vec::new();
        for id_name in store.names()? {
            let skill_id = SkillId::parse(id_name.to_string_lossy().to_string())
                .map_err(|_| store_conflict())?;
            let id = store.open_child(&id_name)?;
            id.validate_owned()?;
            for hash_name in id.names()? {
                let hash = hash_name.to_str().ok_or_else(store_conflict)?.to_string();
                let version = id.open_child(&hash_name)?;
                verify_owned_store_version(&skill_id, &hash, &version)?;
                if !referenced.contains(&(skill_id.clone(), hash.clone())) {
                    orphan_versions.push((skill_id.clone(), hash));
                }
            }
        }
        if orphan_versions.len() > MAX_OWNED_STORE_VERSIONS {
            return Err(storage_limit_error("Skill orphan 数量超过恢复上限"));
        }
        for (skill_id, hash) in orphan_versions {
            self.remove_exact_store_version(&skill_id, &hash)?;
            recovered += 1;
        }
        Ok(recovered)
    }

    pub(crate) fn status(
        &self,
        data_dir: &Path,
        science_state: ScienceProbeState,
        science_version: Option<&str>,
    ) -> SkillResult<SkillManagerStatus> {
        let _guard = acquire_write_lock()?;
        let inventory = self.load_inventory()?;
        let service = DeploymentService::new(self.config_dir.clone(), data_dir.to_path_buf());
        let deployments = service.load_registry()?;
        let reconcile = service.reconcile(&inventory.skills, true, "status", |skill| {
            self.payload_files(skill)
        });
        let runtime_fingerprint = service.runtime_fingerprint().ok();
        let evidence = self.load_discovery_evidence()?;
        Ok(evaluate_status(
            &inventory,
            &deployments,
            &evidence,
            &reconcile,
            science_state,
            science_version,
            runtime_fingerprint.as_deref(),
        ))
    }

    #[allow(dead_code, reason = "called by the Stage 3.2 isolated discovery probe")]
    pub(crate) fn record_discovery_observation(
        &self,
        data_dir: &Path,
        skill_id: &SkillId,
        science_version: &str,
        discovered: bool,
        observed_at: u64,
    ) -> SkillResult<DiscoveryEvidence> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        validate_science_version(science_version)?;
        if observed_at == 0 {
            return Err(inventory_invalid());
        }
        let inventory = self.load_inventory()?;
        let skill = inventory
            .skills
            .iter()
            .find(|skill| &skill.skill_id == skill_id)
            .ok_or_else(|| skill_not_found(skill_id))?;
        if !skill.enabled || skill.restart_required {
            return Err(discovery_not_current(skill_id));
        }
        self.verify_store_version(skill)?;
        let service = DeploymentService::new(self.config_dir.clone(), data_dir.to_path_buf());
        let verification = service.reconcile(&inventory.skills, true, "discovery_probe", |item| {
            self.payload_files(item)
        });
        if !verification.errors.is_empty()
            || verification.restart_required
            || verification
                .planned
                .iter()
                .any(|item| item.skill_id == *skill_id)
        {
            return Err(discovery_not_current(skill_id));
        }
        let deployments = service.load_registry()?;
        if !deployments.deployments.iter().any(|record| {
            record.skill_id == *skill_id
                && record.runtime_name == skill.runtime_name
                && record.content_hash == skill.content_hash
        }) {
            return Err(discovery_not_current(skill_id));
        }
        let observation = DiscoveryEvidence {
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            content_hash: skill.content_hash.clone(),
            science_version: science_version.to_string(),
            runtime_fingerprint: service.runtime_fingerprint()?,
            discovered,
            observed_at,
        };
        let installed_ids = inventory
            .skills
            .iter()
            .map(|item| item.skill_id.clone())
            .collect::<BTreeSet<_>>();
        let mut evidence = self.load_discovery_evidence()?;
        evidence
            .evidence
            .retain(|item| installed_ids.contains(&item.skill_id) && item.skill_id != *skill_id);
        evidence.evidence.push(observation.clone());
        evidence
            .evidence
            .sort_by(|left, right| left.skill_id.cmp(&right.skill_id));
        self.save_discovery_evidence(&evidence)?;
        Ok(observation)
    }

    pub(crate) fn import_source(&self, source: &Path) -> SkillResult<InstallOutcome> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let inspection = self.inspect(source)?;
        let mut inventory = self.load_inventory()?;
        if let Some(index) = inventory
            .skills
            .iter()
            .position(|skill| skill.content_hash == inspection.summary.content_hash)
        {
            let mut verified = inventory.skills[index].clone();
            verified.requirements = inspection.summary.requirements.clone();
            self.verify_store_version(&verified)?;
            let changed = inventory.skills[index].requirements != verified.requirements;
            if changed {
                verified.compatibility_acknowledgment = None;
                verified.updated_at = now_ms().max(verified.installed_at);
                verified.discovery_status = DiscoveryStatus::Unknown;
                if verified.enabled {
                    verified.deployment_status = DeploymentStatus::Pending;
                    verified.restart_required = true;
                }
                inventory.skills[index] = verified.clone();
                self.save_inventory(&inventory)?;
            }
            return Ok(InstallOutcome {
                skill: verified,
                changed,
            });
        }

        let skill_id = SkillId::new_random().map_err(|_| atomic_error())?;
        let installed_at = now_ms();
        let skill = InstalledSkill {
            runtime_name: runtime_name(&inspection.summary.manifest.name, &skill_id)
                .map_err(|_| inventory_invalid())?,
            skill_id: skill_id.clone(),
            manifest: inspection.summary.manifest.clone(),
            source: SkillSource::LocalDirectory {
                display_path: safe_source_label(source),
            },
            content_hash: inspection.summary.content_hash.clone(),
            requirements: inspection.summary.requirements.clone(),
            compatibility_acknowledgment: None,
            installed_at,
            updated_at: installed_at,
            enabled: false,
            validation_status: ValidationStatus::Valid,
            deployment_status: DeploymentStatus::NotDeployed,
            discovery_status: DiscoveryStatus::Unknown,
            restart_required: false,
            last_error: None,
        };
        let preflight = self.preflight_store_version(&skill)?;
        if preflight == StoreVersionPreflight::Absent {
            self.enforce_global_store_quota(inspection.summary.total_size, false, true)?;
        }
        let store_commit = self.commit_store_version(&skill, &inspection, preflight)?;
        inventory.skills.push(skill.clone());
        self.save_inventory_after_store_commit(&inventory, &skill, store_commit)?;
        Ok(InstallOutcome {
            skill,
            changed: true,
        })
    }

    #[allow(
        dead_code,
        reason = "single-source primitive retained for focused tests and recovery tools"
    )]
    pub(crate) fn sync_external_home_inspection(
        &self,
        directory_name: &str,
        inspection: InspectionResult,
    ) -> SkillResult<InstallOutcome> {
        let mut batch = self.begin_external_scan_batch()?;
        batch.sync(directory_name, inspection)
    }

    pub(crate) fn begin_external_scan_batch(&self) -> SkillResult<ExternalScanBatch<'_>> {
        let guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let inventory = self.load_inventory()?;
        let external_skills = inventory
            .skills
            .iter()
            .filter(|skill| matches!(skill.source, SkillSource::ExternalHomeDirectory { .. }))
            .count();
        let usage = self.owned_store_usage()?;
        Ok(ExternalScanBatch {
            manager: self,
            _guard: guard,
            quota: ExternalBatchQuota {
                external_skills,
                usage,
                valid: true,
            },
        })
    }

    fn sync_external_home_inspection_locked(
        &self,
        directory_name: &str,
        inspection: InspectionResult,
        quota: &mut ExternalBatchQuota,
    ) -> SkillResult<InstallOutcome> {
        if directory_name.is_empty()
            || directory_name.starts_with('.')
            || directory_name.len() > 255
            || directory_name.chars().any(char::is_control)
            || Path::new(directory_name).components().count() != 1
            || !matches!(
                Path::new(directory_name).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(SkillManagerError::new(
                SkillErrorCode::UnsafePath,
                "外部 Skill 目录名不安全",
                "仅支持 ~/.claude/skills 下的直接、非隐藏子目录",
            ));
        }

        let mut inventory = self.load_inventory()?;
        let existing = inventory.skills.iter().position(|skill| {
            matches!(
                &skill.source,
                SkillSource::ExternalHomeDirectory { directory_name: existing }
                    if existing == directory_name
            )
        });

        let migration_candidates = inventory
            .skills
            .iter()
            .enumerate()
            .filter(|(_, skill)| {
                matches!(
                    &skill.source,
                    SkillSource::LocalDirectory { display_path }
                        if display_path == directory_name
                            && skill.content_hash == inspection.summary.content_hash
                )
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        if existing.is_none() && migration_candidates.len() == 1 {
            quota.enforce(0, true, false)?;
            let index = migration_candidates[0];
            let mut migrated = inventory.skills[index].clone();
            self.verify_store_version(&migrated)?;
            migrated.source = SkillSource::ExternalHomeDirectory {
                directory_name: directory_name.to_string(),
            };
            migrated.updated_at = now_ms().max(migrated.installed_at);
            inventory.skills[index] = migrated.clone();
            if let Err(error) = self.save_inventory(&inventory) {
                quota.refresh(self)?;
                return Err(error);
            }
            quota.record(0, true, false)?;
            return Ok(InstallOutcome {
                skill: migrated,
                changed: true,
            });
        }

        if let Some(index) = existing {
            let old = inventory.skills[index].clone();
            if old.content_hash == inspection.summary.content_hash {
                let mut verified = old.clone();
                verified.requirements = inspection.summary.requirements.clone();
                self.verify_store_version(&verified)?;
                let changed = old.requirements != verified.requirements;
                if changed {
                    verified.compatibility_acknowledgment = None;
                    verified.updated_at = now_ms().max(verified.installed_at);
                    verified.discovery_status = DiscoveryStatus::Unknown;
                    if verified.enabled {
                        verified.deployment_status = DeploymentStatus::Pending;
                        verified.restart_required = true;
                    }
                    inventory.skills[index] = verified.clone();
                    self.save_inventory(&inventory)?;
                }
                return Ok(InstallOutcome {
                    skill: verified,
                    changed,
                });
            }

            let mut updated = old.clone();
            updated.manifest = inspection.summary.manifest.clone();
            updated.content_hash = inspection.summary.content_hash.clone();
            updated.requirements = inspection.summary.requirements.clone();
            updated.compatibility_acknowledgment = None;
            updated.runtime_name = runtime_name(&updated.manifest.name, &updated.skill_id)
                .map_err(|_| inventory_invalid())?;
            updated.updated_at = now_ms().max(updated.installed_at);
            updated.validation_status = ValidationStatus::Valid;
            updated.deployment_status = if updated.enabled {
                DeploymentStatus::Pending
            } else {
                DeploymentStatus::NotDeployed
            };
            updated.discovery_status = DiscoveryStatus::Unknown;
            updated.restart_required = updated.enabled;
            updated.last_error = None;
            let preflight = self.preflight_store_version(&updated)?;
            if preflight == StoreVersionPreflight::Absent {
                self.enforce_external_version_quota(&old, inspection.summary.total_size)?;
                quota.enforce(inspection.summary.total_size, false, true)?;
            }
            let store_commit = match self.commit_store_version(&updated, &inspection, preflight) {
                Ok(outcome) => outcome,
                Err(error) => {
                    quota.refresh(self)?;
                    return Err(error);
                }
            };
            inventory.skills[index] = updated.clone();
            if let Err(error) =
                self.save_inventory_after_store_commit(&inventory, &updated, store_commit)
            {
                quota.refresh(self)?;
                return Err(error);
            }
            if store_commit == StoreVersionCommit::Created {
                quota.record(inspection.summary.total_size, false, true)?;
            }
            return Ok(InstallOutcome {
                skill: updated,
                changed: true,
            });
        }

        let skill_id = SkillId::new_random().map_err(|_| atomic_error())?;
        let installed_at = now_ms();
        let skill = InstalledSkill {
            runtime_name: runtime_name(&inspection.summary.manifest.name, &skill_id)
                .map_err(|_| inventory_invalid())?,
            skill_id,
            manifest: inspection.summary.manifest.clone(),
            source: SkillSource::ExternalHomeDirectory {
                directory_name: directory_name.to_string(),
            },
            content_hash: inspection.summary.content_hash.clone(),
            requirements: inspection.summary.requirements.clone(),
            compatibility_acknowledgment: None,
            installed_at,
            updated_at: installed_at,
            enabled: true,
            validation_status: ValidationStatus::Valid,
            deployment_status: DeploymentStatus::Pending,
            discovery_status: DiscoveryStatus::Unknown,
            restart_required: true,
            last_error: None,
        };
        let preflight = self.preflight_store_version(&skill)?;
        let adding_version = preflight == StoreVersionPreflight::Absent;
        quota.enforce(
            if adding_version {
                inspection.summary.total_size
            } else {
                0
            },
            true,
            adding_version,
        )?;
        let store_commit = match self.commit_store_version(&skill, &inspection, preflight) {
            Ok(outcome) => outcome,
            Err(error) => {
                quota.refresh(self)?;
                return Err(error);
            }
        };
        inventory.skills.push(skill.clone());
        if let Err(error) = self.save_inventory_after_store_commit(&inventory, &skill, store_commit)
        {
            quota.refresh(self)?;
            return Err(error);
        }
        quota.record(
            if store_commit == StoreVersionCommit::Created {
                inspection.summary.total_size
            } else {
                0
            },
            true,
            store_commit == StoreVersionCommit::Created,
        )?;
        Ok(InstallOutcome {
            skill,
            changed: true,
        })
    }

    pub(crate) fn update_source(
        &self,
        skill_id: &SkillId,
        source: &Path,
        allow_downgrade: bool,
    ) -> SkillResult<InstallOutcome> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let inspection = self.inspect(source)?;
        let mut inventory = self.load_inventory()?;
        let index = inventory
            .skills
            .iter()
            .position(|skill| &skill.skill_id == skill_id)
            .ok_or_else(|| skill_not_found(skill_id))?;
        let old = inventory.skills[index].clone();
        if old.content_hash == inspection.summary.content_hash {
            let mut verified = old.clone();
            verified.requirements = inspection.summary.requirements.clone();
            self.verify_store_version(&verified)?;
            let changed = old.requirements != verified.requirements;
            if changed {
                verified.compatibility_acknowledgment = None;
                verified.updated_at = now_ms().max(verified.installed_at);
                verified.discovery_status = DiscoveryStatus::Unknown;
                if verified.enabled {
                    verified.deployment_status = DeploymentStatus::Pending;
                    verified.restart_required = true;
                }
                inventory.skills[index] = verified.clone();
                self.save_inventory(&inventory)?;
            }
            return Ok(InstallOutcome {
                skill: verified,
                changed,
            });
        }
        if !allow_downgrade
            && is_declared_downgrade(
                old.manifest.declared_version.as_deref(),
                inspection.summary.manifest.declared_version.as_deref(),
            )
        {
            return Err(SkillManagerError::new(
                SkillErrorCode::DowngradeConfirmationRequired,
                "Skill 更新看起来是版本降级，需要显式确认",
                "确认来源可信后，以允许降级选项重试",
            ));
        }
        let mut updated = old.clone();
        updated.manifest = inspection.summary.manifest.clone();
        updated.content_hash = inspection.summary.content_hash.clone();
        updated.requirements = inspection.summary.requirements.clone();
        updated.compatibility_acknowledgment = None;
        updated.runtime_name =
            runtime_name(&updated.manifest.name, skill_id).map_err(|_| inventory_invalid())?;
        if matches!(updated.source, SkillSource::LocalDirectory { .. }) {
            updated.source = SkillSource::LocalDirectory {
                display_path: safe_source_label(source),
            };
        }
        updated.updated_at = now_ms().max(updated.installed_at);
        updated.validation_status = ValidationStatus::Valid;
        updated.deployment_status = if updated.enabled {
            DeploymentStatus::Pending
        } else {
            DeploymentStatus::NotDeployed
        };
        updated.discovery_status = DiscoveryStatus::Unknown;
        updated.restart_required = updated.enabled;
        updated.last_error = None;
        let preflight = self.preflight_store_version(&updated)?;
        if preflight == StoreVersionPreflight::Absent {
            if matches!(old.source, SkillSource::ExternalHomeDirectory { .. }) {
                self.enforce_external_version_quota(&old, inspection.summary.total_size)?;
            }
            self.enforce_global_store_quota(inspection.summary.total_size, false, true)?;
        }
        let store_commit = self.commit_store_version(&updated, &inspection, preflight)?;
        inventory.skills[index] = updated.clone();
        self.save_inventory_after_store_commit(&inventory, &updated, store_commit)?;
        Ok(InstallOutcome {
            skill: updated,
            changed: true,
        })
    }

    #[allow(
        dead_code,
        reason = "raw lifecycle primitive retained for focused filesystem tests"
    )]
    pub(crate) fn set_enabled(
        &self,
        skill_id: &SkillId,
        enabled: bool,
    ) -> SkillResult<EnableOutcome> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let mut inventory = self.load_inventory()?;
        let index = inventory
            .skills
            .iter()
            .position(|skill| &skill.skill_id == skill_id)
            .ok_or_else(|| skill_not_found(skill_id))?;
        self.verify_store_version(&inventory.skills[index])?;
        if inventory.skills[index].enabled == enabled
            && (enabled
                || inventory.skills[index]
                    .compatibility_acknowledgment
                    .is_none())
        {
            return Ok(EnableOutcome {
                skill: inventory.skills[index].clone(),
                changed: false,
            });
        }
        let skill = &mut inventory.skills[index];
        skill.enabled = enabled;
        if !enabled {
            skill.compatibility_acknowledgment = None;
        }
        skill.deployment_status = DeploymentStatus::Pending;
        skill.discovery_status = DiscoveryStatus::Unknown;
        skill.restart_required = true;
        skill.updated_at = now_ms().max(skill.installed_at);
        skill.last_error = None;
        let updated = skill.clone();
        self.save_inventory(&inventory)?;
        Ok(EnableOutcome {
            skill: updated,
            changed: true,
        })
    }

    pub(crate) fn set_enabled_with_compatibility(
        &self,
        skill_id: &SkillId,
        enabled: bool,
        acknowledged_rule_ids: &[String],
        context: &RuntimeContext,
        catalog: &CapabilityCatalog,
    ) -> SkillResult<EnableOutcome> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let mut inventory = self.load_inventory()?;
        let index = inventory
            .skills
            .iter()
            .position(|skill| &skill.skill_id == skill_id)
            .ok_or_else(|| skill_not_found(skill_id))?;
        self.verify_store_version(&inventory.skills[index])?;

        let acknowledgment = if enabled {
            let gate = evaluate_compatibility_gate(&inventory.skills[index], context, catalog)
                .map_err(|error| compatibility_error(error.code, skill_id))?;
            if gate.full_verdict.status == CompatibilityStatus::Unsupported
                || gate.capability_verdict.status == CompatibilityStatus::Unsupported
            {
                return Err(compatibility_unsupported(skill_id));
            }
            if inventory.skills[index].enabled
                && acknowledged_rule_ids != gate.required_rule_ids
                && inventory.skills[index]
                    .compatibility_acknowledgment
                    .as_ref()
                    .is_some_and(|existing| {
                        existing.last_action_rule_ids == acknowledged_rule_ids
                            && existing.capability_rule_ids == gate.capability_rule_ids
                            && existing.capability_fingerprint == gate.capability_fingerprint
                    })
            {
                return Ok(EnableOutcome {
                    skill: inventory.skills[index].clone(),
                    changed: false,
                });
            }
            acknowledgment_for(&gate, acknowledged_rule_ids, now_ms())
                .map_err(|_| compatibility_ack_required(skill_id))?
        } else {
            None
        };

        if inventory.skills[index].enabled == enabled
            && inventory.skills[index]
                .compatibility_acknowledgment
                .as_ref()
                .map(|ack| {
                    (
                        &ack.capability_rule_ids,
                        &ack.last_action_rule_ids,
                        &ack.capability_fingerprint,
                    )
                })
                == acknowledgment.as_ref().map(|ack| {
                    (
                        &ack.capability_rule_ids,
                        &ack.last_action_rule_ids,
                        &ack.capability_fingerprint,
                    )
                })
        {
            return Ok(EnableOutcome {
                skill: inventory.skills[index].clone(),
                changed: false,
            });
        }

        let old = &inventory.skills[index];
        let changed = old.enabled != enabled || old.compatibility_acknowledgment != acknowledgment;
        if !changed {
            return Ok(EnableOutcome {
                skill: old.clone(),
                changed: false,
            });
        }
        let skill = &mut inventory.skills[index];
        skill.enabled = enabled;
        skill.compatibility_acknowledgment = acknowledgment;
        skill.deployment_status = DeploymentStatus::Pending;
        skill.discovery_status = DiscoveryStatus::Unknown;
        skill.restart_required = true;
        skill.updated_at = now_ms().max(skill.installed_at);
        skill.last_error = None;
        let updated = skill.clone();
        self.save_inventory(&inventory)?;
        Ok(EnableOutcome {
            skill: updated,
            changed: true,
        })
    }

    pub(crate) fn uninstall(
        &self,
        skill_id: &SkillId,
        data_dir: &Path,
    ) -> SkillResult<UninstallOutcome> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let mut inventory = self.load_inventory()?;
        let index = inventory
            .skills
            .iter()
            .position(|skill| &skill.skill_id == skill_id)
            .ok_or_else(|| skill_not_found(skill_id))?;
        let skill = inventory.skills[index].clone();
        self.verify_store_version(&skill)?;
        let deployments = DeploymentService::new(self.config_dir.clone(), data_dir.to_path_buf());
        let runtime_removed = deployments
            .remove_owned_runtime_and_record_with_payload(&skill, |stored| {
                self.payload_files(stored)
            })?;
        inventory.skills.remove(index);
        self.save_inventory(&inventory)?;
        let store_gc_pending = self.remove_skill_store(&skill).is_err();
        Ok(UninstallOutcome {
            skill_id: skill.skill_id,
            changed: true,
            runtime_removed,
            store_gc_pending,
        })
    }

    #[allow(
        dead_code,
        reason = "raw lifecycle primitive retained for focused filesystem tests"
    )]
    pub(crate) fn reconcile(
        &self,
        data_dir: &Path,
        dry_run: bool,
        reason: &str,
    ) -> SkillResult<ReconcileReport> {
        let _guard = acquire_write_lock()?;
        self.reconcile_locked(data_dir, dry_run, reason)
    }

    pub(crate) fn reconcile_with_compatibility(
        &self,
        data_dir: &Path,
        dry_run: bool,
        reason: &str,
        catalog: &CapabilityCatalog,
        context_for: impl Fn(&InstalledSkill) -> SkillResult<RuntimeContext>,
    ) -> SkillResult<ReconcileReport> {
        let _guard = acquire_write_lock()?;
        let inventory = self.load_inventory()?;
        for skill in inventory.skills.iter().filter(|skill| skill.enabled) {
            let context = context_for(skill)?;
            let gate = evaluate_compatibility_gate(skill, &context, catalog)
                .map_err(|error| compatibility_error(error.code, &skill.skill_id))?;
            enforce_gate(skill, &gate)?;
        }
        self.reconcile_locked(data_dir, dry_run, reason)
    }

    fn reconcile_locked(
        &self,
        data_dir: &Path,
        dry_run: bool,
        reason: &str,
    ) -> SkillResult<ReconcileReport> {
        if !dry_run {
            self.ensure_layout()?;
        }
        let mut inventory = self.load_inventory()?;
        let service = DeploymentService::new(self.config_dir.clone(), data_dir.to_path_buf());
        let report = service.reconcile(&inventory.skills, dry_run, reason, |skill| {
            self.payload_files(skill)
        });
        if dry_run {
            return Ok(report);
        }
        let registry = service.load_registry()?;
        for skill in &mut inventory.skills {
            let record = registry
                .deployments
                .iter()
                .find(|record| record.skill_id == skill.skill_id);
            let error = report.errors.iter().find(|error| {
                error
                    .skill_id
                    .as_ref()
                    .is_none_or(|id| id == &skill.skill_id)
            });
            if let Some(error) = error {
                skill.deployment_status = DeploymentStatus::Failed;
                skill.last_error = Some(error.code.clone());
            } else if skill.enabled
                && record.is_some_and(|record| {
                    record.runtime_name == skill.runtime_name
                        && record.content_hash == skill.content_hash
                })
            {
                skill.deployment_status = DeploymentStatus::Deployed;
                skill.last_error = None;
            } else if !skill.enabled && record.is_none() {
                skill.deployment_status = DeploymentStatus::NotDeployed;
                skill.last_error = None;
            } else {
                skill.deployment_status = DeploymentStatus::Pending;
            }
            if report
                .applied
                .iter()
                .any(|item| item.skill_id == skill.skill_id)
            {
                skill.restart_required = true;
                skill.discovery_status = DiscoveryStatus::Unknown;
            }
        }
        self.save_inventory(&inventory)?;
        let _ = self.gc_unreferenced_store_versions(&inventory, &registry);
        Ok(report)
    }

    pub(crate) fn has_pending_restart(&self) -> SkillResult<bool> {
        Ok(self
            .load_inventory()?
            .skills
            .iter()
            .any(|skill| skill.restart_required))
    }

    pub(crate) fn mark_science_started(&self, data_dir: &Path) -> SkillResult<Vec<SkillId>> {
        let _guard = acquire_write_lock()?;
        self.ensure_layout()?;
        let mut inventory = self.load_inventory()?;
        let service = DeploymentService::new(self.config_dir.clone(), data_dir.to_path_buf());
        let verification =
            service.reconcile(&inventory.skills, true, "post_start_verify", |skill| {
                self.payload_files(skill)
            });
        if !verification.errors.is_empty()
            || !verification.planned.is_empty()
            || verification.restart_required
        {
            let skill_id = verification
                .errors
                .first()
                .and_then(|error| error.skill_id.clone())
                .or_else(|| {
                    verification
                        .planned
                        .first()
                        .map(|item| item.skill_id.clone())
                });
            let error = SkillManagerError::new(
                SkillErrorCode::DeploymentConflict,
                "Science 启动后的 Skill 运行副本未通过完整性复验",
                "停止隔离 Science，修复冲突并重新 reconcile 后重试",
            );
            return Err(match skill_id {
                Some(skill_id) => error.with_skill_id(skill_id),
                None => error,
            });
        }
        let registry = service.load_registry()?;
        let mut cleared = Vec::new();
        for skill in &mut inventory.skills {
            let record = registry
                .deployments
                .iter()
                .find(|record| record.skill_id == skill.skill_id);
            let consistent = if skill.enabled {
                record.is_some_and(|record| {
                    record.runtime_name == skill.runtime_name
                        && record.content_hash == skill.content_hash
                })
            } else {
                record.is_none()
            };
            if !consistent {
                return Err(SkillManagerError::new(
                    SkillErrorCode::DeploymentConflict,
                    "Science 启动后的 Skill 部署登记与 inventory 不一致",
                    "停止隔离 Science，运行 reconcile 后重试",
                )
                .with_skill_id(skill.skill_id.clone()));
            }
            if skill.restart_required {
                cleared.push(skill.skill_id.clone());
            }
            skill.restart_required = false;
            skill.deployment_status = if skill.enabled {
                DeploymentStatus::Deployed
            } else {
                DeploymentStatus::NotDeployed
            };
            skill.discovery_status = DiscoveryStatus::Unknown;
            skill.last_error = None;
        }
        self.save_inventory(&inventory)?;
        Ok(cleared)
    }

    fn ensure_layout(&self) -> SkillResult<()> {
        let config_fd = SafeDir::ensure_absolute(&self.config_dir)?;
        config_fd.validate_owned()?;
        let skills_name = std::ffi::OsStr::new("skills");
        let skills_c = os_cstring(skills_name)?;
        let root_fd = if config_fd.child_stat(&skills_c)?.is_some() {
            let root = config_fd.open_child(skills_name)?;
            root.validate_owned()?;
            verify_root_marker_fd(&root)?;
            root
        } else {
            let staging_name = format!(".skills-init-{}", random_suffix()?);
            let staging_fd = config_fd.create_child(staging_name.as_ref())?;
            let result = (|| {
                self.fail_if(FaultPoint::BeforeRootMarker)?;
                let marker = serde_json::to_vec(&RootMarker {
                    schema_version: 1,
                    owner: ROOT_MARKER_OWNER.to_string(),
                })
                .map_err(|_| atomic_error())?;
                staging_fd.create_file(ROOT_MARKER_FILE.as_ref(), &marker)?;
                self.fail_if(FaultPoint::AfterRootMarker)?;
                staging_fd.sync()?;
                rename_noreplace(&config_fd, staging_name.as_ref(), &config_fd, skills_name)?;
                config_fd.sync()?;
                Ok(())
            })();
            if let Err(error) = result {
                let _ = config_fd.remove_tree_child(staging_name.as_ref());
                return Err(error);
            }
            config_fd.open_child(skills_name)?
        };
        root_fd.validate_owned()?;
        let store_fd = root_fd.open_or_create_child("store".as_ref())?;
        store_fd.validate_owned()?;
        Ok(())
    }

    fn preflight_store_version(
        &self,
        skill: &InstalledSkill,
    ) -> SkillResult<StoreVersionPreflight> {
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        let id_name = os_cstring(skill.skill_id.as_str().as_ref())?;
        if store.child_stat(&id_name)?.is_none() {
            return Ok(StoreVersionPreflight::Absent);
        }
        let id = store.open_child(skill.skill_id.as_str().as_ref())?;
        id.validate_owned()?;
        let version_name = os_cstring(skill.content_hash.as_ref())?;
        if id.child_stat(&version_name)?.is_none() {
            return Ok(StoreVersionPreflight::Absent);
        }
        let version = id.open_child(skill.content_hash.as_ref())?;
        verify_store_version_fd(skill, &version)?;
        Ok(StoreVersionPreflight::AlreadyPresent)
    }

    fn commit_store_version(
        &self,
        skill: &InstalledSkill,
        inspection: &InspectionResult,
        preflight: StoreVersionPreflight,
    ) -> SkillResult<StoreVersionCommit> {
        let skills_fd = SafeDir::open_absolute(&self.paths.root)?;
        skills_fd.validate_owned()?;
        verify_root_marker_fd(&skills_fd)?;
        let store_fd = skills_fd.open_child("store".as_ref())?;
        store_fd.validate_owned()?;
        let id_fd = store_fd.open_or_create_child(skill.skill_id.as_str().as_ref())?;
        id_fd.validate_owned()?;
        let version_name = os_cstring(skill.content_hash.as_ref())?;
        if id_fd.child_stat(&version_name)?.is_some() {
            let version_fd = id_fd.open_child(skill.content_hash.as_ref())?;
            verify_store_version_fd(skill, &version_fd)?;
            return Ok(StoreVersionCommit::AlreadyPresent);
        }
        if preflight == StoreVersionPreflight::AlreadyPresent {
            return Err(store_conflict());
        }
        let staging_name = format!(".staging-{}-{}", skill.skill_id.short(), random_suffix()?);
        let staging_fd = skills_fd.create_child(staging_name.as_ref())?;
        let result = (|| {
            let marker = marker_bytes(skill)?;
            staging_fd.create_file(STORE_MARKER_FILE.as_ref(), &marker)?;
            let payload_fd = staging_fd.create_child("payload".as_ref())?;
            for inspected in &inspection.files {
                if inspected.content.len() as u64 != inspected.size {
                    return Err(atomic_error());
                }
                let mut current = payload_fd.try_clone()?;
                let mut components = inspected.relative_path.components().peekable();
                while let Some(component) = components.next() {
                    let Component::Normal(name) = component else {
                        return Err(store_conflict());
                    };
                    if components.peek().is_some() {
                        current = current.open_or_create_child(name)?;
                    } else {
                        current.create_file_mode(name, &inspected.content, inspected.executable)?;
                    }
                }
            }
            payload_fd.sync()?;
            staging_fd.sync()?;
            self.fail_if(FaultPoint::AfterStaging)?;
            self.fail_if(FaultPoint::BeforeStoreRename)?;
            rename_noreplace(
                &skills_fd,
                staging_name.as_ref(),
                &id_fd,
                skill.content_hash.as_ref(),
            )?;
            if self.fault == Some(FaultPoint::AfterStoreRename) {
                return Err(durability_uncertain());
            }
            id_fd.sync()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = skills_fd.remove_tree_child(staging_name.as_ref());
        }
        result.map(|()| StoreVersionCommit::Created)
    }

    fn verify_store_version(&self, skill: &InstalledSkill) -> SkillResult<()> {
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        let id = store.open_child(skill.skill_id.as_str().as_ref())?;
        let version = id.open_child(skill.content_hash.as_ref())?;
        verify_store_version_fd(skill, &version)
    }

    fn save_inventory(&self, inventory: &Inventory) -> SkillResult<()> {
        inventory.validate().map_err(|_| inventory_invalid())?;
        let data = serde_json::to_vec_pretty(inventory).map_err(|_| inventory_invalid())?;
        if data.len() as u64 > MAX_INVENTORY_SIZE {
            return Err(inventory_invalid());
        }
        let skills_fd = SafeDir::open_absolute(&self.paths.root)?;
        skills_fd.validate_owned()?;
        verify_root_marker_fd(&skills_fd)?;
        assert_inventory_target_safe(&skills_fd)?;
        let temporary_name = format!(
            ".{INVENTORY_FILE}.tmp-{}-{}",
            std::process::id(),
            random_suffix()?
        );
        let mut temporary_created = false;
        let result = (|| {
            skills_fd.create_file(temporary_name.as_ref(), &data)?;
            temporary_created = true;
            if self.fault == Some(FaultPoint::BeforeInventoryRenameCleanupReadFails) {
                return Err(atomic_error());
            }
            self.fail_if(FaultPoint::BeforeInventoryRename)?;
            assert_inventory_target_safe(&skills_fd)?;
            skills_fd.replace_child(temporary_name.as_ref(), INVENTORY_FILE.as_ref())?;
            if self.fault == Some(FaultPoint::AfterInventoryRename) {
                return Err(durability_uncertain());
            }
            skills_fd.sync()?;
            Ok(())
        })();
        if result.is_err() && temporary_created {
            let _ = skills_fd.unlink_file(temporary_name.as_ref());
        }
        result
    }

    fn save_inventory_after_store_commit(
        &self,
        inventory: &Inventory,
        committed: &InstalledSkill,
        store_commit: StoreVersionCommit,
    ) -> SkillResult<()> {
        match self.save_inventory(inventory) {
            Ok(()) => Ok(()),
            Err(error) => {
                if store_commit == StoreVersionCommit::AlreadyPresent {
                    return Err(error);
                }
                let current =
                    if self.fault == Some(FaultPoint::BeforeInventoryRenameCleanupReadFails) {
                        Err(inventory_invalid())
                    } else {
                        self.load_inventory()
                    };
                if let Ok(current) = current {
                    let referenced = current.skills.iter().any(|skill| {
                        skill.skill_id == committed.skill_id
                            && skill.content_hash == committed.content_hash
                    });
                    if !referenced {
                        let _ = self.remove_exact_store_version(
                            &committed.skill_id,
                            &committed.content_hash,
                        );
                    }
                }
                Err(error)
            }
        }
    }

    fn remove_exact_store_version(&self, skill_id: &SkillId, hash: &str) -> SkillResult<()> {
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        let id = store.open_child(skill_id.as_str().as_ref())?;
        id.validate_owned()?;
        let version = id.open_child(hash.as_ref())?;
        let captured = CapturedTree::capture(&version)?;
        verify_owned_store_version(skill_id, hash, &version)?;
        captured.verify_unchanged()?;
        id.verify_child_identity(hash.as_ref(), &version)?;
        captured.remove_contents()?;
        id.remove_empty_verified_child(hash.as_ref(), &version)?;
        if id.names()?.is_empty() {
            store.remove_empty_verified_child(skill_id.as_str().as_ref(), &id)?;
        }
        Ok(())
    }

    #[allow(dead_code, reason = "called by record_discovery_observation")]
    fn save_discovery_evidence(&self, evidence: &DiscoveryEvidenceRegistry) -> SkillResult<()> {
        evidence.validate()?;
        let data = serde_json::to_vec_pretty(evidence).map_err(|_| inventory_invalid())?;
        if data.len() as u64 > MAX_DISCOVERY_SIZE {
            return Err(inventory_invalid());
        }
        let skills_fd = SafeDir::open_absolute(&self.paths.root)?;
        skills_fd.validate_owned()?;
        verify_root_marker_fd(&skills_fd)?;
        assert_regular_owned_target_safe(&skills_fd, DISCOVERY_FILE)?;
        let temporary_name = format!(
            ".{DISCOVERY_FILE}.tmp-{}-{}",
            std::process::id(),
            random_suffix()?
        );
        let mut temporary_created = false;
        let result = (|| {
            skills_fd.create_file(temporary_name.as_ref(), &data)?;
            temporary_created = true;
            assert_regular_owned_target_safe(&skills_fd, DISCOVERY_FILE)?;
            skills_fd.replace_child(temporary_name.as_ref(), DISCOVERY_FILE.as_ref())?;
            skills_fd.sync()?;
            Ok(())
        })();
        if result.is_err() && temporary_created {
            let _ = skills_fd.unlink_file(temporary_name.as_ref());
        }
        result
    }

    fn remove_skill_store(&self, skill: &InstalledSkill) -> SkillResult<()> {
        self.remove_skill_store_with_hook(skill, &|| {})
    }

    fn enforce_external_version_quota(
        &self,
        skill: &InstalledSkill,
        incoming_size: u64,
    ) -> SkillResult<()> {
        self.enforce_external_version_quota_with_limits(
            skill,
            incoming_size,
            MAX_EXTERNAL_VERSIONS_PER_SKILL,
            MAX_EXTERNAL_STORED_BYTES_PER_SKILL,
        )
    }

    fn enforce_global_store_quota(
        &self,
        incoming_size: u64,
        adding_external_skill: bool,
        adding_version: bool,
    ) -> SkillResult<()> {
        self.enforce_global_store_quota_with_limits(
            incoming_size,
            adding_external_skill,
            adding_version,
            MAX_EXTERNAL_SKILL_COUNT,
            MAX_OWNED_STORE_VERSIONS,
            MAX_OWNED_STORE_BYTES,
        )
    }

    fn enforce_global_store_quota_with_limits(
        &self,
        incoming_size: u64,
        adding_external_skill: bool,
        adding_version: bool,
        max_external_skills: usize,
        max_versions: usize,
        max_bytes: u64,
    ) -> SkillResult<()> {
        let inventory = self.load_inventory()?;
        let external_skills = inventory
            .skills
            .iter()
            .filter(|skill| matches!(skill.source, SkillSource::ExternalHomeDirectory { .. }))
            .count();
        if adding_external_skill && external_skills >= max_external_skills {
            return Err(storage_limit_error(
                "外部 Skill 数量达到全局配额；现有来源消失的 Skill 会继续保留",
            ));
        }
        let usage = self.owned_store_usage()?;
        if adding_version
            && (usage.versions >= max_versions
                || usage
                    .bytes
                    .checked_add(incoming_size)
                    .is_none_or(|total| total > max_bytes))
        {
            return Err(storage_limit_error(
                "CSSwitch owned Skill store 达到全局配额",
            ));
        }
        Ok(())
    }

    fn owned_store_usage(&self) -> SkillResult<OwnedStoreUsage> {
        #[cfg(test)]
        OWNED_STORE_AUDIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        if root
            .names()?
            .iter()
            .any(|name| name.to_string_lossy().starts_with(".staging-"))
        {
            return Err(store_conflict());
        }
        let store = root.open_child("store".as_ref())?;
        store.validate_owned()?;
        let mut usage = OwnedStoreUsage::default();
        for id_name in store.names()? {
            let skill_id = SkillId::parse(id_name.to_string_lossy().to_string())
                .map_err(|_| store_conflict())?;
            let id = store.open_child(&id_name)?;
            id.validate_owned()?;
            for hash_name in id.names()? {
                let hash = hash_name.to_str().ok_or_else(store_conflict)?;
                let version = id.open_child(&hash_name)?;
                let (_, size) = verify_owned_store_version(&skill_id, hash, &version)?;
                usage.versions = usage
                    .versions
                    .checked_add(1)
                    .ok_or_else(|| storage_limit_error("owned store 版本数量无效"))?;
                usage.bytes = usage
                    .bytes
                    .checked_add(size)
                    .ok_or_else(|| storage_limit_error("owned store 累计大小无效"))?;
            }
        }
        Ok(usage)
    }

    fn enforce_external_version_quota_with_limits(
        &self,
        skill: &InstalledSkill,
        incoming_size: u64,
        max_versions: usize,
        max_bytes: u64,
    ) -> SkillResult<()> {
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        let id = store.open_child(skill.skill_id.as_str().as_ref())?;
        id.validate_owned()?;
        let mut versions = 0_usize;
        let mut bytes = 0_u64;
        for name in id.names()? {
            let hash = name.to_str().ok_or_else(store_conflict)?.to_string();
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(store_conflict());
            }
            let version = id.open_child(&name)?;
            let mut version_skill = skill.clone();
            version_skill.content_hash = hash;
            version_skill.requirements = requirements_from_store_version(&version)?;
            verify_store_version_fd(&version_skill, &version)?;
            let payload = version.open_child("payload".as_ref())?;
            let scan = scan_stored_payload(&payload)?;
            versions = versions
                .checked_add(1)
                .ok_or_else(|| storage_limit_error("外部 Skill store 版本数量无效"))?;
            bytes = bytes
                .checked_add(scan.total_size)
                .ok_or_else(|| storage_limit_error("外部 Skill store 累计大小无效"))?;
        }
        if versions >= max_versions
            || bytes
                .checked_add(incoming_size)
                .is_none_or(|total| total > max_bytes)
        {
            return Err(storage_limit_error(
                "外部 Skill 历史版本达到本地存储配额；请先完成 reconcile 后重试",
            ));
        }
        Ok(())
    }

    fn gc_unreferenced_store_versions(
        &self,
        inventory: &Inventory,
        registry: &DeploymentRegistry,
    ) -> SkillResult<()> {
        let mut keep = BTreeMap::<SkillId, BTreeSet<String>>::new();
        for skill in &inventory.skills {
            keep.entry(skill.skill_id.clone())
                .or_default()
                .insert(skill.content_hash.clone());
        }
        for deployment in &registry.deployments {
            keep.entry(deployment.skill_id.clone())
                .or_default()
                .insert(deployment.content_hash.clone());
        }

        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        for skill in inventory
            .skills
            .iter()
            .filter(|skill| matches!(skill.source, SkillSource::ExternalHomeDirectory { .. }))
        {
            let id = store.open_child(skill.skill_id.as_str().as_ref())?;
            id.validate_owned()?;
            for name in id.names()? {
                let hash = name.to_str().ok_or_else(store_conflict)?.to_string();
                if keep
                    .get(&skill.skill_id)
                    .is_some_and(|hashes| hashes.contains(&hash))
                {
                    continue;
                }
                let version = id.open_child(&name)?;
                let mut version_skill = skill.clone();
                version_skill.content_hash = hash;
                version_skill.requirements = requirements_from_store_version(&version)?;
                let captured = CapturedTree::capture(&version)?;
                verify_store_version_fd(&version_skill, &version)?;
                captured.verify_unchanged()?;
                self.fail_if(FaultPoint::BeforeVersionGc)?;
                id.verify_child_identity(&name, &version)?;
                captured.verify_unchanged()?;
                captured.remove_contents()?;
                id.remove_empty_verified_child(&name, &version)?;
            }
        }
        Ok(())
    }

    fn remove_skill_store_with_hook(
        &self,
        skill: &InstalledSkill,
        after_version_verify: &dyn Fn(),
    ) -> SkillResult<()> {
        let root = SafeDir::open_absolute(&self.paths.root)?;
        root.validate_owned()?;
        verify_root_marker_fd(&root)?;
        let store = root.open_child("store".as_ref())?;
        let id = store.open_child(skill.skill_id.as_str().as_ref())?;
        id.validate_owned()?;
        for name in id.names()? {
            let hash = name.to_string_lossy().to_string();
            let mut version_skill = skill.clone();
            version_skill.content_hash = hash.clone();
            let version = id.open_child(&name)?;
            version_skill.requirements = requirements_from_store_version(&version)?;
            let captured = CapturedTree::capture(&version)?;
            verify_store_version_fd(&version_skill, &version)?;
            captured.verify_unchanged()?;
            after_version_verify();
            id.verify_child_identity(&name, &version)?;
            captured.verify_unchanged()?;
            captured.remove_contents()?;
            id.remove_empty_verified_child(&name, &version)?;
        }
        if !id.names()?.is_empty() {
            return Err(store_conflict());
        }
        store.remove_empty_verified_child(skill.skill_id.as_str().as_ref(), &id)?;
        Ok(())
    }

    fn fail_if(&self, point: FaultPoint) -> SkillResult<()> {
        if self.fault == Some(point) {
            Err(atomic_error())
        } else {
            Ok(())
        }
    }
}

impl ExternalScanBatch<'_> {
    pub(crate) fn sync(
        &mut self,
        directory_name: &str,
        inspection: InspectionResult,
    ) -> SkillResult<InstallOutcome> {
        self.manager.sync_external_home_inspection_locked(
            directory_name,
            inspection,
            &mut self.quota,
        )
    }
}

impl ExternalBatchQuota {
    fn enforce(
        &self,
        incoming_size: u64,
        adding_external_skill: bool,
        adding_version: bool,
    ) -> SkillResult<()> {
        if !self.valid {
            return Err(store_conflict());
        }
        if adding_external_skill && self.external_skills >= MAX_EXTERNAL_SKILL_COUNT {
            return Err(storage_limit_error(
                "外部 Skill 数量达到全局配额；现有来源消失的 Skill 会继续保留",
            ));
        }
        if adding_version
            && (self.usage.versions >= MAX_OWNED_STORE_VERSIONS
                || self
                    .usage
                    .bytes
                    .checked_add(incoming_size)
                    .is_none_or(|total| total > MAX_OWNED_STORE_BYTES))
        {
            return Err(storage_limit_error(
                "CSSwitch owned Skill store 达到全局配额",
            ));
        }
        Ok(())
    }

    fn record(
        &mut self,
        incoming_size: u64,
        added_external_skill: bool,
        added_version: bool,
    ) -> SkillResult<()> {
        if added_external_skill {
            self.external_skills = self
                .external_skills
                .checked_add(1)
                .ok_or_else(|| storage_limit_error("外部 Skill 数量无效"))?;
        }
        if added_version {
            self.usage.versions = self
                .usage
                .versions
                .checked_add(1)
                .ok_or_else(|| storage_limit_error("owned store 版本数量无效"))?;
            self.usage.bytes = self
                .usage
                .bytes
                .checked_add(incoming_size)
                .ok_or_else(|| storage_limit_error("owned store 累计大小无效"))?;
        }
        Ok(())
    }

    fn refresh(&mut self, manager: &SkillManager) -> SkillResult<()> {
        match (manager.load_inventory(), manager.owned_store_usage()) {
            (Ok(inventory), Ok(usage)) => {
                self.external_skills = inventory
                    .skills
                    .iter()
                    .filter(|skill| {
                        matches!(skill.source, SkillSource::ExternalHomeDirectory { .. })
                    })
                    .count();
                self.usage = usage;
                self.valid = true;
                Ok(())
            }
            _ => {
                self.valid = false;
                Err(store_conflict())
            }
        }
    }
}

pub(super) struct SafeDir {
    file: File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SafeFileIdentity {
    device: u64,
    inode: u64,
}

pub(super) struct CapturedTree {
    directory: SafeDir,
    snapshot: libc::stat,
    entries: Vec<CapturedEntry>,
}

enum CapturedEntry {
    File {
        name: std::ffi::OsString,
        file: File,
        snapshot: libc::stat,
    },
    Directory {
        name: std::ffi::OsString,
        tree: Box<CapturedTree>,
    },
}

impl CapturedTree {
    pub(super) fn capture(directory: &SafeDir) -> SkillResult<Self> {
        let held = directory.try_clone()?;
        let snapshot = file_stat(&held.file)?;
        let mut entries = Vec::new();
        for name in directory.names()? {
            let stat = directory
                .child_stat(&os_cstring(&name)?)?
                .ok_or_else(store_conflict)?;
            match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => entries.push(CapturedEntry::File {
                    file: directory.open_regular_file(&name)?,
                    snapshot: stat,
                    name,
                }),
                libc::S_IFDIR => {
                    let child = directory.open_child(&name)?;
                    child.validate_owned()?;
                    entries.push(CapturedEntry::Directory {
                        name,
                        tree: Box::new(Self::capture(&child)?),
                    });
                }
                _ => return Err(store_conflict()),
            }
        }
        Ok(Self {
            directory: held,
            snapshot,
            entries,
        })
    }

    pub(super) fn verify_unchanged(&self) -> SkillResult<()> {
        if !same_file_snapshot(&self.snapshot, &file_stat(&self.directory.file)?) {
            return Err(store_conflict());
        }
        self.verify_entries_unchanged()
    }

    pub(super) fn verify_entries_unchanged(&self) -> SkillResult<()> {
        let current_names = self.directory.names()?.into_iter().collect::<BTreeSet<_>>();
        let captured_names = self
            .entries
            .iter()
            .map(|entry| match entry {
                CapturedEntry::File { name, .. } | CapturedEntry::Directory { name, .. } => {
                    name.clone()
                }
            })
            .collect::<BTreeSet<_>>();
        if current_names != captured_names {
            return Err(store_conflict());
        }
        for entry in &self.entries {
            match entry {
                CapturedEntry::File {
                    name,
                    file,
                    snapshot,
                } => {
                    let current = self
                        .directory
                        .child_stat(&os_cstring(name)?)?
                        .ok_or_else(store_conflict)?;
                    let opened = file_stat(file)?;
                    if !same_file_snapshot(snapshot, &current)
                        || !same_file_snapshot(snapshot, &opened)
                    {
                        return Err(store_conflict());
                    }
                }
                CapturedEntry::Directory { name, tree } => {
                    let current = self
                        .directory
                        .child_stat(&os_cstring(name)?)?
                        .ok_or_else(store_conflict)?;
                    if !same_file_snapshot(&tree.snapshot, &current) {
                        return Err(store_conflict());
                    }
                    tree.verify_unchanged()?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn remove_contents(self) -> SkillResult<()> {
        for entry in self.entries {
            match entry {
                CapturedEntry::File {
                    name,
                    file,
                    snapshot,
                } => {
                    let current = self
                        .directory
                        .child_stat(&os_cstring(&name)?)?
                        .ok_or_else(store_conflict)?;
                    if !same_file_snapshot(&snapshot, &current)
                        || !same_file_snapshot(&snapshot, &file_stat(&file)?)
                    {
                        return Err(store_conflict());
                    }
                    self.directory.remove_verified_file(&name, &file)?;
                }
                CapturedEntry::Directory { name, tree } => {
                    tree.verify_unchanged()?;
                    let child = tree.directory.try_clone()?;
                    tree.remove_contents()?;
                    self.directory.remove_empty_verified_child(&name, &child)?;
                }
            }
        }
        Ok(())
    }
}

impl SafeDir {
    pub(super) fn ensure_absolute(path: &Path) -> SkillResult<Self> {
        Self::ensure_absolute_with_hook(path, &|_| {})
    }

    fn ensure_absolute_with_hook(
        path: &Path,
        before_component: &dyn Fn(&Path),
    ) -> SkillResult<Self> {
        if !path.is_absolute() {
            return Err(store_conflict());
        }
        let root = CString::new("/").expect("root has no NUL");
        // SAFETY: root is NUL terminated and flags only open an existing directory.
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd was returned by open and ownership moves into File.
        let mut current = Self {
            file: unsafe { File::from_raw_fd(fd) },
        };
        let mut walked = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    walked.push(name);
                    before_component(&walked);
                    let name_c = os_cstring(name)?;
                    current = if current.child_stat(&name_c)?.is_some() {
                        current.open_child(name)?
                    } else {
                        current.create_child(name)?
                    };
                }
                _ => return Err(store_conflict()),
            }
        }
        Ok(current)
    }

    pub(super) fn open_absolute(path: &Path) -> SkillResult<Self> {
        if !path.is_absolute() {
            return Err(store_conflict());
        }
        let root = CString::new("/").expect("root has no NUL");
        // SAFETY: root is NUL terminated and open flags only access an existing directory.
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd was returned by open and ownership moves into File.
        let mut current = Self {
            file: unsafe { File::from_raw_fd(fd) },
        };
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => current = current.open_child(name)?,
                _ => return Err(store_conflict()),
            }
        }
        Ok(current)
    }

    pub(super) fn try_clone(&self) -> SkillResult<Self> {
        self.file
            .try_clone()
            .map(|file| Self { file })
            .map_err(|_| atomic_error())
    }

    pub(super) fn same_identity(&self, other: &Self) -> SkillResult<bool> {
        let left = file_stat(&self.file)?;
        let right = file_stat(&other.file)?;
        Ok(left.st_dev == right.st_dev && left.st_ino == right.st_ino)
    }

    pub(super) fn validate_owned(&self) -> SkillResult<()> {
        let stat = file_stat(&self.file)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_uid != current_euid()
            || stat.st_mode & 0o777 != 0o700
        {
            return Err(store_conflict());
        }
        Ok(())
    }

    pub(super) fn validate_external_owned(&self) -> SkillResult<()> {
        let stat = file_stat(&self.file)?;
        let mode = stat.st_mode & 0o777;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_uid != current_euid()
            || mode & 0o022 != 0
        {
            return Err(store_conflict());
        }
        Ok(())
    }

    pub(super) fn open_child(&self, name: &std::ffi::OsStr) -> SkillResult<Self> {
        let name = os_cstring(name)?;
        let before = self.child_stat(&name)?.ok_or_else(store_conflict)?;
        if before.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(store_conflict());
        }
        // SAFETY: parent fd and name are valid; O_NOFOLLOW rejects a symlink leaf.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd was returned by openat and ownership moves into File.
        let file = unsafe { File::from_raw_fd(fd) };
        let opened = file_stat(&file)?;
        if before.st_dev != opened.st_dev || before.st_ino != opened.st_ino {
            return Err(store_conflict());
        }
        Ok(Self { file })
    }

    pub(super) fn create_child(&self, name: &std::ffi::OsStr) -> SkillResult<Self> {
        let name_c = os_cstring(name)?;
        if self.child_stat(&name_c)?.is_some() {
            return Err(store_conflict());
        }
        // SAFETY: parent fd/name are valid and mode is restricted to owner access.
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err(atomic_error());
        }
        self.file.sync_all().map_err(|_| durability_uncertain())?;
        let child = self.open_child(name)?;
        // SAFETY: child fd is open and fchmod only changes that anchored directory.
        if unsafe { libc::fchmod(child.file.as_raw_fd(), 0o700) } != 0 {
            return Err(atomic_error());
        }
        Ok(child)
    }

    pub(super) fn open_or_create_child(&self, name: &std::ffi::OsStr) -> SkillResult<Self> {
        let name_c = os_cstring(name)?;
        if self.child_stat(&name_c)?.is_some() {
            self.open_child(name)
        } else {
            self.create_child(name)
        }
    }

    pub(super) fn child_stat(&self, name: &CString) -> SkillResult<Option<libc::stat>> {
        // SAFETY: zeroed stat is an output buffer for fstatat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: parent/name are valid; AT_SYMLINK_NOFOLLOW inspects the entry itself.
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(Some(stat));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(store_conflict())
        }
    }

    pub(super) fn verify_child_identity(&self, name: &OsStr, child: &SafeDir) -> SkillResult<()> {
        let current = self
            .child_stat(&os_cstring(name)?)?
            .ok_or_else(store_conflict)?;
        let opened = file_stat(&child.file)?;
        if current.st_mode & libc::S_IFMT != libc::S_IFDIR
            || current.st_dev != opened.st_dev
            || current.st_ino != opened.st_ino
        {
            return Err(store_conflict());
        }
        Ok(())
    }

    pub(super) fn read_bound_file(
        &self,
        name: &std::ffi::OsStr,
        max_size: u64,
    ) -> SkillResult<(Vec<u8>, SafeFileIdentity)> {
        let name_c = os_cstring(name)?;
        let before = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        let data = self.read_file(name, max_size)?;
        let after = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
            return Err(store_conflict());
        }
        Ok((
            data,
            SafeFileIdentity {
                device: before.st_dev as u64,
                inode: before.st_ino as u64,
            },
        ))
    }

    pub(super) fn read_bound_external_file(
        &self,
        name: &std::ffi::OsStr,
        max_size: u64,
    ) -> SkillResult<Vec<u8>> {
        let name = os_cstring(name)?;
        let expected = self.child_stat(&name)?.ok_or_else(store_conflict)?;
        let mode = expected.st_mode & 0o777;
        if expected.st_mode & libc::S_IFMT != libc::S_IFREG
            || expected.st_nlink != 1
            || expected.st_size < 0
            || expected.st_size as u64 > max_size
            || expected.st_uid != current_euid()
            || mode & 0o400 == 0
            || mode & 0o022 != 0
        {
            return Err(store_conflict());
        }
        // SAFETY: the directory fd is anchored; O_NOFOLLOW rejects a leaf symlink.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd is newly owned by this File.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let opened = file_stat(&file)?;
        if !same_file_snapshot(&expected, &opened) {
            return Err(store_conflict());
        }
        let mut data = Vec::with_capacity(usize::try_from(opened.st_size).unwrap_or(0));
        std::io::Read::by_ref(&mut file)
            .take(max_size + 1)
            .read_to_end(&mut data)
            .map_err(|_| store_conflict())?;
        let after = file_stat(&file)?;
        if data.len() as i64 != opened.st_size || !same_file_snapshot(&opened, &after) {
            return Err(store_conflict());
        }
        Ok(data)
    }

    pub(super) fn verify_file_identity(
        &self,
        name: &std::ffi::OsStr,
        identity: SafeFileIdentity,
    ) -> SkillResult<()> {
        let current = self
            .child_stat(&os_cstring(name)?)?
            .ok_or_else(store_conflict)?;
        if current.st_mode & libc::S_IFMT != libc::S_IFREG
            || current.st_dev as u64 != identity.device
            || current.st_ino as u64 != identity.inode
        {
            return Err(store_conflict());
        }
        Ok(())
    }

    pub(super) fn create_file(&self, name: &std::ffi::OsStr, content: &[u8]) -> SkillResult<()> {
        self.create_file_mode(name, content, false)
    }

    pub(super) fn create_file_mode(
        &self,
        name: &std::ffi::OsStr,
        content: &[u8],
        executable: bool,
    ) -> SkillResult<()> {
        let name = os_cstring(name)?;
        // SAFETY: parent/name are valid; EXCL and NOFOLLOW prevent replacing/following entries.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd was returned by openat and ownership moves into File.
        let mut file = unsafe { File::from_raw_fd(fd) };
        // SAFETY: fd remains open and fchmod anchors the permission change to this file.
        let mode = if executable { 0o700 } else { 0o600 };
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
            return Err(atomic_error());
        }
        file.write_all(content).map_err(|_| atomic_error())?;
        file.sync_all().map_err(|_| atomic_error())?;
        self.file.sync_all().map_err(|_| durability_uncertain())?;
        Ok(())
    }

    pub(super) fn read_payload_file(
        &self,
        name: &std::ffi::OsStr,
        max_size: u64,
    ) -> SkillResult<(Vec<u8>, bool)> {
        let name_c = os_cstring(name)?;
        let expected = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        let mode = expected.st_mode & 0o777;
        if expected.st_mode & libc::S_IFMT != libc::S_IFREG
            || expected.st_nlink != 1
            || expected.st_size < 0
            || expected.st_size as u64 > max_size
            || expected.st_uid != current_euid()
            || !matches!(mode, 0o600 | 0o700)
        {
            return Err(store_conflict());
        }
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let opened = file_stat(&file)?;
        if !same_file_snapshot(&expected, &opened) {
            return Err(store_conflict());
        }
        let mut content = Vec::with_capacity(expected.st_size as usize);
        Read::by_ref(&mut file)
            .take(max_size + 1)
            .read_to_end(&mut content)
            .map_err(|_| store_conflict())?;
        let after = file_stat(&file)?;
        if !same_file_snapshot(&opened, &after) || content.len() as i64 != expected.st_size {
            return Err(store_conflict());
        }
        Ok((content, mode == 0o700))
    }

    pub(super) fn read_file(&self, name: &std::ffi::OsStr, max_size: u64) -> SkillResult<Vec<u8>> {
        let name = os_cstring(name)?;
        let expected = self.child_stat(&name)?.ok_or_else(store_conflict)?;
        if expected.st_mode & libc::S_IFMT != libc::S_IFREG
            || expected.st_nlink != 1
            || expected.st_size < 0
            || expected.st_size as u64 > max_size
            || expected.st_uid != current_euid()
            || expected.st_mode & 0o777 != 0o600
        {
            return Err(store_conflict());
        }
        // SAFETY: parent/name are valid; O_NOFOLLOW prevents following a leaf symlink.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd was returned by openat and ownership moves into File.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let opened = file_stat(&file)?;
        if !same_file_snapshot(&expected, &opened) {
            return Err(store_conflict());
        }
        let mut data = Vec::with_capacity(usize::try_from(opened.st_size).unwrap_or(0));
        std::io::Read::by_ref(&mut file)
            .take(max_size + 1)
            .read_to_end(&mut data)
            .map_err(|_| store_conflict())?;
        let after = file_stat(&file)?;
        if data.len() as i64 != opened.st_size || !same_file_snapshot(&opened, &after) {
            return Err(store_conflict());
        }
        Ok(data)
    }

    fn open_regular_file(&self, name: &OsStr) -> SkillResult<File> {
        let name = os_cstring(name)?;
        // SAFETY: self is an owned directory fd; O_NOFOLLOW rejects symlinks.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(store_conflict());
        }
        // SAFETY: fd was returned by openat and ownership transfers to File.
        let file = unsafe { File::from_raw_fd(fd) };
        let stat = file_stat(&file)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 1 {
            return Err(store_conflict());
        }
        Ok(file)
    }

    fn remove_verified_file(&self, name: &OsStr, verified: &File) -> SkillResult<()> {
        let name_c = os_cstring(name)?;
        let opened = file_stat(verified)?;
        let current = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        if current.st_mode & libc::S_IFMT != libc::S_IFREG
            || current.st_dev != opened.st_dev
            || current.st_ino != opened.st_ino
        {
            return Err(store_conflict());
        }
        // SAFETY: unlinkat removes only the inode-verified entry and never follows a symlink.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
            return Err(store_conflict());
        }
        Ok(())
    }

    pub(super) fn replace_child(
        &self,
        source_name: &std::ffi::OsStr,
        target_name: &std::ffi::OsStr,
    ) -> SkillResult<()> {
        let source_name = os_cstring(source_name)?;
        let target_name = os_cstring(target_name)?;
        // SAFETY: both names are relative to the same anchored directory; renameat never follows
        // the target entry and atomically replaces only that directory entry.
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                source_name.as_ptr(),
                self.file.as_raw_fd(),
                target_name.as_ptr(),
            )
        } != 0
        {
            return Err(atomic_error());
        }
        Ok(())
    }

    fn unlink_file(&self, name: &std::ffi::OsStr) -> SkillResult<()> {
        let name = os_cstring(name)?;
        // SAFETY: name is relative to the anchored directory and unlinkat removes only that entry.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(atomic_error());
        }
        Ok(())
    }

    pub(super) fn remove_tree_child(&self, name: &std::ffi::OsStr) -> SkillResult<()> {
        let name_c = os_cstring(name)?;
        let Some(stat) = self.child_stat(&name_c)? else {
            return Ok(());
        };
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = self.open_child(name)?;
            return self.remove_verified_child(name, &child);
        } else {
            // SAFETY: unlinkat removes the entry itself and never follows symlinks.
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
                return Err(atomic_error());
            }
        }
        self.file.sync_all().map_err(|_| durability_uncertain())?;
        Ok(())
    }

    pub(super) fn remove_verified_child(
        &self,
        name: &std::ffi::OsStr,
        verified: &SafeDir,
    ) -> SkillResult<()> {
        let name_c = os_cstring(name)?;
        let opened = file_stat(&verified.file)?;
        let current = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        if current.st_mode & libc::S_IFMT != libc::S_IFDIR
            || current.st_dev != opened.st_dev
            || current.st_ino != opened.st_ino
        {
            return Err(store_conflict());
        }
        for entry in directory_names(&verified.file)? {
            verified.remove_tree_child(&entry)?;
        }
        let current = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        if current.st_dev != opened.st_dev || current.st_ino != opened.st_ino {
            return Err(store_conflict());
        }
        // SAFETY: the parent entry is still the same verified, now-empty directory; unlinkat
        // removes only that entry and never follows a symlink.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) }
            != 0
        {
            return Err(atomic_error());
        }
        self.file.sync_all().map_err(|_| durability_uncertain())?;
        Ok(())
    }

    pub(super) fn remove_empty_verified_child(
        &self,
        name: &std::ffi::OsStr,
        verified: &SafeDir,
    ) -> SkillResult<()> {
        let name_c = os_cstring(name)?;
        let opened = file_stat(&verified.file)?;
        let current = self.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        if current.st_mode & libc::S_IFMT != libc::S_IFDIR
            || current.st_dev != opened.st_dev
            || current.st_ino != opened.st_ino
        {
            return Err(store_conflict());
        }
        // SAFETY: unlinkat with AT_REMOVEDIR is atomic with respect to directory contents. If
        // anything appears after the checks above, the kernel returns ENOTEMPTY instead of
        // recursively deleting data that was not part of this operation.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) }
            != 0
        {
            return Err(store_conflict());
        }
        self.file.sync_all().map_err(|_| durability_uncertain())?;
        Ok(())
    }

    pub(super) fn sync(&self) -> SkillResult<()> {
        self.file.sync_all().map_err(|_| durability_uncertain())
    }

    pub(super) fn names(&self) -> SkillResult<Vec<std::ffi::OsString>> {
        directory_names(&self.file)
    }
}

fn file_stat(file: &File) -> SkillResult<libc::stat> {
    // SAFETY: zeroed stat is a valid fstat output buffer.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: file owns a valid fd and stat is writable.
    if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(store_conflict());
    }
    Ok(stat)
}

fn same_file_snapshot(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_mode == right.st_mode
        && left.st_uid == right.st_uid
        && left.st_nlink == right.st_nlink
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn directory_names(file: &File) -> SkillResult<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: dup returns an independent fd or -1; fdopendir owns it on success.
    let duplicate = unsafe { libc::dup(file.as_raw_fd()) };
    if duplicate < 0 {
        return Err(atomic_error());
    }
    // SAFETY: duplicate refers to an open directory.
    let directory = unsafe { libc::fdopendir(duplicate) };
    if directory.is_null() {
        // SAFETY: fdopendir failed and did not consume duplicate.
        unsafe { libc::close(duplicate) };
        return Err(atomic_error());
    }
    // SAFETY: directory is a valid DIR pointer. dup shares the original fd offset, so every
    // enumeration must rewind before reading or later ownership checks could see an empty list.
    unsafe { libc::rewinddir(directory) };
    let mut names = Vec::new();
    let result = loop {
        set_store_errno(0);
        // SAFETY: directory is valid until closedir below.
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            break if store_errno() == 0 {
                Ok(())
            } else {
                Err(atomic_error())
            };
        }
        // SAFETY: readdir supplies a NUL-terminated d_name for this call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(OsStr::from_bytes(name.to_bytes()).to_os_string());
        }
    };
    // SAFETY: directory came from fdopendir and is closed once.
    let close_result = unsafe { libc::closedir(directory) };
    result?;
    if close_result != 0 {
        return Err(atomic_error());
    }
    Ok(names)
}

#[cfg(target_os = "macos")]
fn store_errno_pointer() -> *mut libc::c_int {
    // SAFETY: __error returns the current thread's errno storage on macOS.
    unsafe { libc::__error() }
}

#[cfg(not(target_os = "macos"))]
fn store_errno_pointer() -> *mut libc::c_int {
    // SAFETY: __errno_location returns current thread errno storage on supported Unix CI.
    unsafe { libc::__errno_location() }
}

fn set_store_errno(value: libc::c_int) {
    // SAFETY: pointer addresses writable thread-local errno storage.
    unsafe { *store_errno_pointer() = value };
}

fn store_errno() -> libc::c_int {
    // SAFETY: pointer addresses readable thread-local errno storage.
    unsafe { *store_errno_pointer() }
}

pub(super) fn os_cstring(value: &std::ffi::OsStr) -> SkillResult<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(value.as_bytes()).map_err(|_| store_conflict())
}

pub(super) fn rename_noreplace(
    source: &SafeDir,
    source_name: &std::ffi::OsStr,
    target: &SafeDir,
    target_name: &std::ffi::OsStr,
) -> SkillResult<()> {
    let source_name = os_cstring(source_name)?;
    let target_name = os_cstring(target_name)?;
    #[cfg(target_os = "macos")]
    // SAFETY: directory fds and names are valid; RENAME_EXCL prevents target replacement.
    let result = unsafe {
        libc::renameatx_np(
            source.file.as_raw_fd(),
            source_name.as_ptr(),
            target.file.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    // SAFETY: syscall receives valid directory fds/names and RENAME_NOREPLACE.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source.file.as_raw_fd(),
            source_name.as_ptr(),
            target.file.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        ) as libc::c_int
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = -1;
    if result != 0 {
        return Err(store_conflict());
    }
    Ok(())
}

pub(super) fn rename_swap(
    left_parent: &SafeDir,
    left_name: &std::ffi::OsStr,
    right_parent: &SafeDir,
    right_name: &std::ffi::OsStr,
) -> SkillResult<()> {
    let left_name = os_cstring(left_name)?;
    let right_name = os_cstring(right_name)?;
    #[cfg(target_os = "macos")]
    // SAFETY: directory fds and names are valid; RENAME_SWAP atomically exchanges entries.
    let result = unsafe {
        libc::renameatx_np(
            left_parent.file.as_raw_fd(),
            left_name.as_ptr(),
            right_parent.file.as_raw_fd(),
            right_name.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    #[cfg(target_os = "linux")]
    // SAFETY: syscall receives valid directory fds/names and RENAME_EXCHANGE.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_parent.file.as_raw_fd(),
            left_name.as_ptr(),
            right_parent.file.as_raw_fd(),
            right_name.as_ptr(),
            libc::RENAME_EXCHANGE,
        ) as libc::c_int
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = -1;
    if result != 0 {
        return Err(store_conflict());
    }
    Ok(())
}

fn acquire_write_lock() -> SkillResult<MutexGuard<'static, ()>> {
    match WRITE_LOCK.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) | Err(TryLockError::Poisoned(_)) => {
            Err(SkillManagerError::new(
                SkillErrorCode::ManagerBusy,
                "另一个 Skill 写操作正在进行",
                "请等待当前操作完成后重试",
            ))
        }
    }
}

fn assert_path_chain_no_symlink(path: &Path) -> SkillResult<()> {
    if !path.is_absolute() {
        return Err(store_conflict());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(Path::new("/")),
            Component::Normal(value) => {
                current.push(value);
                let metadata = fs::symlink_metadata(&current).map_err(|_| store_conflict())?;
                if metadata.file_type().is_symlink() {
                    return Err(store_conflict());
                }
            }
            _ => return Err(store_conflict()),
        }
    }
    Ok(())
}

fn assert_inventory_target_safe(root: &SafeDir) -> SkillResult<()> {
    assert_regular_owned_target_safe(root, INVENTORY_FILE)
}

fn assert_regular_owned_target_safe(root: &SafeDir, target: &str) -> SkillResult<()> {
    let name = os_cstring(target.as_ref())?;
    if let Some(stat) = root.child_stat(&name)? {
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat.st_nlink != 1
            || stat.st_uid != current_euid()
            || stat.st_mode & 0o777 != 0o600
        {
            return Err(store_conflict());
        }
    }
    Ok(())
}

#[allow(dead_code, reason = "called by record_discovery_observation")]
fn discovery_not_current(skill_id: &SkillId) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::DeploymentConflict,
        "Skill discovery 证据与当前部署状态不一致",
        "先停止 Science 并完成 reconcile，再重新运行 discovery probe",
    )
    .with_skill_id(skill_id.clone())
}

fn marker_bytes(skill: &InstalledSkill) -> SkillResult<Vec<u8>> {
    let marker = StoreMarker {
        schema_version: 1,
        owner: STORE_MARKER_OWNER.to_string(),
        skill_id: skill.skill_id.clone(),
        content_hash: skill.content_hash.clone(),
    };
    serde_json::to_vec(&marker).map_err(|_| atomic_error())
}

pub(super) fn verify_root_marker_fd(directory: &SafeDir) -> SkillResult<()> {
    let data = directory.read_file(ROOT_MARKER_FILE.as_ref(), 4_096)?;
    let marker: RootMarker = serde_json::from_slice(&data).map_err(|_| store_conflict())?;
    if marker.schema_version != 1 || marker.owner != ROOT_MARKER_OWNER {
        return Err(store_conflict());
    }
    Ok(())
}

fn verify_store_version_fd(skill: &InstalledSkill, version: &SafeDir) -> SkillResult<()> {
    let (requirements, _) =
        verify_owned_store_version(&skill.skill_id, &skill.content_hash, version)?;
    if requirements != skill.requirements {
        return Err(store_conflict());
    }
    Ok(())
}

fn verify_owned_store_version(
    skill_id: &SkillId,
    content_hash: &str,
    version: &SafeDir,
) -> SkillResult<(SkillRequirements, u64)> {
    version.validate_owned()?;
    let marker_data = version.read_file(STORE_MARKER_FILE.as_ref(), 4_096)?;
    let marker: StoreMarker = serde_json::from_slice(&marker_data).map_err(|_| store_conflict())?;
    if marker.schema_version != 1
        || marker.owner != STORE_MARKER_OWNER
        || marker.skill_id != *skill_id
        || marker.content_hash != content_hash
    {
        return Err(store_conflict());
    }
    let payload = version.open_child("payload".as_ref())?;
    payload.validate_owned()?;
    let scan = scan_stored_payload(&payload)?;
    if hash_stored_payload(&scan) != content_hash
        && hash_stored_payload_legacy(&scan) != content_hash
    {
        return Err(store_conflict());
    }
    let requirements = SkillRequirements::from_public_json(
        scan.files
            .get(b"csswitch.skill.json".as_slice())
            .map(|file| file.content.as_slice()),
    )
    .map_err(|_| store_conflict())?;
    Ok((requirements, scan.total_size))
}

fn requirements_from_store_version(version: &SafeDir) -> SkillResult<SkillRequirements> {
    let payload = version.open_child("payload".as_ref())?;
    payload.validate_owned()?;
    let scan = scan_stored_payload(&payload)?;
    SkillRequirements::from_public_json(
        scan.files
            .get(b"csswitch.skill.json".as_slice())
            .map(|file| file.content.as_slice()),
    )
    .map_err(|_| store_conflict())
}

#[derive(Default)]
struct StoredPayloadScan {
    files: BTreeMap<Vec<u8>, StoredPayloadFile>,
    collisions: BTreeSet<String>,
    total_size: u64,
    directory_count: usize,
}

struct StoredPayloadFile {
    content: Vec<u8>,
    executable: bool,
}

fn scan_stored_payload(payload: &SafeDir) -> SkillResult<StoredPayloadScan> {
    let mut scan = StoredPayloadScan::default();
    walk_stored_payload(payload, Path::new(""), 0, &mut scan)?;
    Ok(scan)
}

fn hash_stored_payload(scan: &StoredPayloadScan) -> String {
    let mut digest = Sha256::new();
    digest.update(b"CSSWITCH-SKILL-CONTENT-V2\0");
    digest.update((scan.files.len() as u64).to_be_bytes());
    for (path, file) in &scan.files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path);
        digest.update((file.content.len() as u64).to_be_bytes());
        digest.update([u8::from(file.executable)]);
        digest.update(&file.content);
    }
    let bytes = digest.finalize();
    let mut hash = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut hash, "{byte:02x}").expect("writing to String cannot fail");
    }
    hash
}

fn hash_stored_payload_legacy(scan: &StoredPayloadScan) -> String {
    let mut digest = Sha256::new();
    digest.update(b"CSSWITCH-SKILL-CONTENT-V1\0");
    digest.update((scan.files.len() as u64).to_be_bytes());
    for (path, file) in &scan.files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path);
        digest.update((file.content.len() as u64).to_be_bytes());
        digest.update(&file.content);
    }
    let bytes = digest.finalize();
    let mut hash = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut hash, "{byte:02x}").expect("writing to String cannot fail");
    }
    hash
}

fn walk_stored_payload(
    directory: &SafeDir,
    relative: &Path,
    depth: usize,
    scan: &mut StoredPayloadScan,
) -> SkillResult<()> {
    use std::os::unix::ffi::OsStrExt;

    if depth > MAX_PATH_DEPTH {
        return Err(store_conflict());
    }
    let mut names = directory_names(&directory.file)?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for name in names {
        let bytes = name.as_bytes();
        if bytes.is_empty() || !bytes.is_ascii() || bytes == b"." || bytes == b".." {
            return Err(store_conflict());
        }
        let child_relative = relative.join(&name);
        let path_bytes = child_relative.as_os_str().as_bytes();
        if path_bytes.len() > MAX_PATH_BYTES || depth + 1 > MAX_PATH_DEPTH {
            return Err(store_conflict());
        }
        let collision = path_bytes
            .iter()
            .map(u8::to_ascii_lowercase)
            .map(char::from)
            .collect::<String>();
        if !scan.collisions.insert(collision) {
            return Err(store_conflict());
        }
        let name_c = os_cstring(&name)?;
        let stat = directory.child_stat(&name_c)?.ok_or_else(store_conflict)?;
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                if scan.directory_count >= MAX_DIRECTORY_COUNT {
                    return Err(store_conflict());
                }
                scan.directory_count += 1;
                let child = directory.open_child(&name)?;
                child.validate_owned()?;
                walk_stored_payload(&child, &child_relative, depth + 1, scan)?;
            }
            libc::S_IFREG => {
                if scan.files.len() >= MAX_FILE_COUNT || stat.st_nlink != 1 || stat.st_size < 0 {
                    return Err(store_conflict());
                }
                let size = stat.st_size as u64;
                if size > MAX_FILE_SIZE
                    || (child_relative == Path::new("SKILL.md") && size > MAX_SKILL_MD_SIZE)
                {
                    return Err(store_conflict());
                }
                scan.total_size = scan
                    .total_size
                    .checked_add(size)
                    .ok_or_else(store_conflict)?;
                if scan.total_size > MAX_TOTAL_SIZE {
                    return Err(store_conflict());
                }
                let (content, executable) = directory.read_payload_file(&name, MAX_FILE_SIZE)?;
                scan.files.insert(
                    path_bytes.to_vec(),
                    StoredPayloadFile {
                        content,
                        executable,
                    },
                );
            }
            _ => return Err(store_conflict()),
        }
    }
    Ok(())
}

fn random_suffix() -> SkillResult<String> {
    SkillId::new_random()
        .map(|id| id.as_str().trim_start_matches("sk_").to_string())
        .map_err(|_| atomic_error())
}

fn safe_source_label(source: &Path) -> String {
    source
        .file_name()
        .map(|value| value.to_string_lossy().chars().take(255).collect())
        .filter(|value: &String| !value.is_empty())
        .unwrap_or_else(|| "local-folder".to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    unsafe { libc::geteuid() }
}

fn is_declared_downgrade(old: Option<&str>, new: Option<&str>) -> bool {
    match (
        old.and_then(parse_numeric_version),
        new.and_then(parse_numeric_version),
    ) {
        (Some(old), Some(new)) => new < old,
        _ => false,
    }
}

fn parse_numeric_version(value: &str) -> Option<Vec<u64>> {
    let trimmed = value.trim().strip_prefix('v').unwrap_or(value.trim());
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

fn inventory_invalid() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::InventoryInvalid,
        "Skill inventory 缺失完整性或版本校验",
        "请保留现有文件并运行 Skill Manager 诊断",
    )
}

fn storage_limit_error(message: &str) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::LimitExceeded,
        message,
        "完成 Skill reconcile 以回收未引用历史版本，或精简 Skill 内容后重试",
    )
}

fn skill_not_found(skill_id: &SkillId) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::SkillNotFound,
        "找不到指定的 CSSwitch Skill",
        "请刷新 Skill 列表后重试",
    )
    .with_skill_id(skill_id.clone())
}

fn enforce_gate(skill: &InstalledSkill, gate: &CompatibilityGate) -> SkillResult<()> {
    if gate.full_verdict.status == CompatibilityStatus::Unsupported
        || gate.capability_verdict.status == CompatibilityStatus::Unsupported
    {
        return Err(compatibility_unsupported(&skill.skill_id));
    }
    // Limited/unknown capability results remain visible as diagnostics, but are not a
    // deployment veto. Third-party Skill behavior is the installing user's trust decision;
    // CSSwitch blocks only an explicit hard incompatibility.
    Ok(())
}

fn compatibility_error(code: CompatibilityErrorCode, skill_id: &SkillId) -> SkillManagerError {
    match code {
        CompatibilityErrorCode::InvalidCatalog | CompatibilityErrorCode::MissingCatalogRule => {
            SkillManagerError::new(
                SkillErrorCode::CompatibilityCatalogInvalid,
                "Skill 兼容性 catalog 未通过完整性校验",
                "恢复随应用发布的 capability catalog 后重试；现有库存未被修改",
            )
        }
        CompatibilityErrorCode::AcknowledgmentRequired => compatibility_ack_required(skill_id),
        CompatibilityErrorCode::InvalidSkill | CompatibilityErrorCode::InvalidRuntimeContext => {
            SkillManagerError::new(
                SkillErrorCode::CompatibilityAcknowledgmentRequired,
                "无法为当前 Skill 建立安全的兼容性上下文",
                "刷新运行时诊断并重新评估当前兼容性规则",
            )
        }
    }
    .with_skill_id(skill_id.clone())
}

fn compatibility_unsupported(skill_id: &SkillId) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::CompatibilityUnsupported,
        "当前运行能力不支持启用或部署此 Skill",
        "根据兼容性诊断补齐能力，或保持此 Skill 停用",
    )
    .with_skill_id(skill_id.clone())
}

fn compatibility_ack_required(skill_id: &SkillId) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::CompatibilityAcknowledgmentRequired,
        "当前兼容性限制尚未得到有效确认",
        "重新评估并确认返回的完整规则 ID 集合后重试",
    )
    .with_skill_id(skill_id.clone())
}

fn store_conflict() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::StoreConflict,
        "Skill 存储路径的类型、所有权标记或内容不匹配",
        "请不要手工修改 CSSwitch Skill 存储，并运行诊断",
    )
}

fn atomic_error() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::AtomicCommitFailed,
        "Skill 原子写入未完成，旧库存保持不变",
        "请确认磁盘空间和目录权限后重试",
    )
}

fn durability_uncertain() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::CommitDurabilityUncertain,
        "Skill 写入已提交，但持久化确认失败",
        "请不要重复删除文件；重新打开 CSSwitch 并运行 Skill Manager 诊断",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    static TEST_SERIAL: &Mutex<()> = &TEST_OPERATION_LOCK;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = crate::test_temp_root().join(format!(
                "csswitch-skill-store-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn skill(name: &str, version: &str, body: &str) -> Self {
            let dir = Self::new("source");
            write_skill(&dir.0, name, version, body);
            dir
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_skill(path: &Path, name: &str, version: &str, body: &str) {
        fs::write(
            path.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Store test skill\nversion: {version}\n---\n{body}\n"
            ),
        )
        .unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn repeated_import_is_idempotent_and_permissions_are_normalized() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let source = TestDir::skill("Probe", "1.0.0", "body");
        let script = source.0.join("run.sh");
        fs::write(&script, b"echo test\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));

        let first = manager.import_source(&source.0).unwrap();
        let second = manager.import_source(&source.0).unwrap();
        assert!(first.changed);
        assert!(!second.changed);
        assert_eq!(first.skill.skill_id, second.skill.skill_id);
        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills.len(), 1);
        assert_eq!(mode(&manager.paths.root), 0o700);
        assert_eq!(mode(&manager.paths.root.join(ROOT_MARKER_FILE)), 0o600);
        assert_eq!(mode(&manager.paths.inventory), 0o600);
        let payload = manager
            .paths
            .payload(&first.skill.skill_id, &first.skill.content_hash);
        assert_eq!(mode(&payload), 0o700);
        assert_eq!(mode(&payload.join("SKILL.md")), 0o600);
        assert_eq!(mode(&payload.join("run.sh")), 0o700);
    }

    #[test]
    fn requirements_round_trip_on_import_and_update_without_entering_ownership_marker() {
        use crate::skill_manager::requirements::{RequirementFlag, RequirementSource};

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("requirements-config");
        let source = TestDir::skill("Requirements", "1.0.0", "body");
        fs::write(
            source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_ssh":true}}"#,
        )
        .unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        assert_eq!(
            installed.requirements.needs_ssh.value,
            RequirementFlag::True
        );
        assert_eq!(
            installed.requirements.needs_ssh.source,
            RequirementSource::Declared
        );
        assert_eq!(manager.load_inventory().unwrap().skills[0], installed);
        let marker = fs::read(
            manager
                .paths
                .payload(&installed.skill_id, &installed.content_hash)
                .parent()
                .unwrap()
                .join(STORE_MARKER_FILE),
        )
        .unwrap();
        assert!(!String::from_utf8_lossy(&marker).contains("requirements"));
        assert!(!String::from_utf8_lossy(&marker).contains("needs_ssh"));

        write_skill(&source.0, "Requirements", "1.1.0", "updated");
        fs::write(
            source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_ssh":false,"needs_mcp":true}}"#,
        )
        .unwrap();
        let updated = manager
            .update_source(&installed.skill_id, &source.0, false)
            .unwrap()
            .skill;
        assert_eq!(updated.requirements.needs_ssh.value, RequirementFlag::False);
        assert_eq!(updated.requirements.needs_mcp.value, RequirementFlag::True);
        assert_eq!(manager.load_inventory().unwrap().skills[0], updated);
    }

    #[test]
    fn stored_requirements_bind_inventory_and_same_hash_revalidation_repairs_legacy_state() {
        use crate::skill_manager::requirements::{
            FlagRequirement, RequirementFlag, RequirementSource,
        };

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("requirements-integrity-config");
        let source = TestDir::skill("Requirements Integrity", "1.0.0", "body");
        fs::write(
            source.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_ssh":true}}"#,
        )
        .unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;

        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&manager.paths.inventory).unwrap()).unwrap();
        legacy["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("requirements");
        fs::write(
            &manager.paths.inventory,
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();
        let legacy_skill = manager.load_inventory().unwrap().skills.remove(0);
        assert_eq!(
            legacy_skill.requirements.needs_ssh.value,
            RequirementFlag::Unknown
        );
        assert_eq!(
            manager.verify_skill_store(&legacy_skill).unwrap_err().code,
            SkillErrorCode::StoreConflict
        );
        let migrated = manager
            .update_source(&installed.skill_id, &source.0, false)
            .unwrap();
        assert!(migrated.changed);
        assert_eq!(
            migrated.skill.requirements.needs_ssh.value,
            RequirementFlag::True
        );
        manager.verify_skill_store(&migrated.skill).unwrap();

        let mut tampered = manager.load_inventory().unwrap();
        tampered.skills[0].requirements.needs_ssh = FlagRequirement {
            value: RequirementFlag::False,
            source: RequirementSource::Declared,
        };
        fs::write(
            &manager.paths.inventory,
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();
        let tampered_skill = manager.load_inventory().unwrap().skills.remove(0);
        assert_eq!(
            manager
                .verify_skill_store(&tampered_skill)
                .unwrap_err()
                .code,
            SkillErrorCode::StoreConflict
        );
        let repaired = manager.import_source(&source.0).unwrap();
        assert!(repaired.changed);
        assert_eq!(
            repaired.skill.requirements.needs_ssh.value,
            RequirementFlag::True
        );
        manager.verify_skill_store(&repaired.skill).unwrap();
    }

    #[test]
    fn same_name_different_content_gets_distinct_skill_ids() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let first = TestDir::skill("Same Name", "1.0.0", "one");
        let second = TestDir::skill("Same Name", "1.0.1", "two");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let one = manager.import_source(&first.0).unwrap().skill;
        let two = manager.import_source(&second.0).unwrap().skill;
        assert_ne!(one.skill_id, two.skill_id);
        assert_ne!(one.runtime_name, two.runtime_name);
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 2);
    }

    #[test]
    fn updates_are_immutable_and_downgrades_require_confirmation() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let source = TestDir::skill("Versioned", "2.0.0", "two");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let original = manager.import_source(&source.0).unwrap().skill;
        let old_payload = manager
            .paths
            .payload(&original.skill_id, &original.content_hash);

        write_skill(&source.0, "Versioned", "1.0.0", "one");
        assert_eq!(
            manager
                .update_source(&original.skill_id, &source.0, false)
                .unwrap_err()
                .code,
            SkillErrorCode::DowngradeConfirmationRequired
        );
        let downgraded = manager
            .update_source(&original.skill_id, &source.0, true)
            .unwrap()
            .skill;
        assert_ne!(original.content_hash, downgraded.content_hash);
        assert!(old_payload.is_dir());
        assert!(manager
            .paths
            .payload(&downgraded.skill_id, &downgraded.content_hash)
            .is_dir());
    }

    #[test]
    fn inventory_commit_failure_preserves_old_pointer_and_payload() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let source = TestDir::skill("Atomic", "1.0.0", "old");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let old = manager.import_source(&source.0).unwrap().skill;
        write_skill(&source.0, "Atomic", "1.1.0", "new");
        let faulty = SkillManager::new(root.0.join(".csswitch"))
            .with_fault(FaultPoint::BeforeInventoryRename);
        assert_eq!(
            faulty
                .update_source(&old.skill_id, &source.0, false)
                .unwrap_err()
                .code,
            SkillErrorCode::AtomicCommitFailed
        );
        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills[0].content_hash, old.content_hash);
        manager.verify_store_version(&inventory.skills[0]).unwrap();
        assert!(!fs::read_dir(&manager.paths.root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn external_home_update_commit_failure_preserves_stable_identity_and_old_pointer() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("external-atomic-config");
        let source = TestDir::skill("External Atomic", "1.0.0", "old");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let original = manager
            .sync_external_home_inspection("external-atomic", manager.inspect(&source.0).unwrap())
            .unwrap()
            .skill;
        write_skill(&source.0, "External Atomic", "1.1.0", "new");
        let faulty = SkillManager::new(root.0.join(".csswitch"))
            .with_fault(FaultPoint::BeforeInventoryRename);
        let error = faulty
            .sync_external_home_inspection("external-atomic", faulty.inspect(&source.0).unwrap())
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::AtomicCommitFailed);
        let retained = manager.load_inventory().unwrap().skills.remove(0);
        assert_eq!(retained.skill_id, original.skill_id);
        assert_eq!(retained.content_hash, original.content_hash);
        assert!(matches!(
            retained.source,
            SkillSource::ExternalHomeDirectory { ref directory_name }
                if directory_name == "external-atomic"
        ));
        manager.verify_skill_store(&retained).unwrap();
    }

    #[test]
    fn external_versions_are_quota_bounded_and_gc_waits_for_reconcile() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("external-quota-config");
        let source_root = TestDir::new("external-quota-source-root");
        let source = source_root.0.join("quota-skill");
        fs::create_dir(&source).unwrap();
        write_skill(&source, "Quota Skill", "1.0.0", "v0");
        let data = TestDir::new("external-quota-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let original = manager
            .sync_external_home_inspection("quota-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        manager.reconcile(&data.0, false, "initial").unwrap();

        write_skill(&source, "Quota Skill", "1.0.1", "v1");
        let updated = manager
            .sync_external_home_inspection("quota-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        let id_store = manager.paths.store.join(original.skill_id.as_str());
        assert!(id_store.join(&original.content_hash).is_dir());
        assert!(id_store.join(&updated.content_hash).is_dir());
        assert_eq!(manager.load_discovery_evidence().unwrap().evidence.len(), 0);
        assert_eq!(
            DeploymentService::new(manager.config_dir.clone(), data.0.clone())
                .load_registry()
                .unwrap()
                .deployments[0]
                .content_hash,
            original.content_hash
        );

        manager.reconcile(&data.0, false, "replace").unwrap();
        assert!(!id_store.join(&original.content_hash).exists());
        assert!(id_store.join(&updated.content_hash).is_dir());

        let mut current = updated;
        for version in 2..=MAX_EXTERNAL_VERSIONS_PER_SKILL {
            write_skill(
                &source,
                "Quota Skill",
                &format!("1.0.{version}"),
                &format!("v{version}"),
            );
            current = manager
                .sync_external_home_inspection("quota-skill", manager.inspect(&source).unwrap())
                .unwrap()
                .skill;
        }
        assert_eq!(
            fs::read_dir(&id_store).unwrap().count(),
            MAX_EXTERNAL_VERSIONS_PER_SKILL
        );
        write_skill(&source, "Quota Skill", "1.0.99", "over quota");
        let error = manager
            .sync_external_home_inspection("quota-skill", manager.inspect(&source).unwrap())
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::LimitExceeded);
        let update_error = manager
            .update_source(&current.skill_id, &source, true)
            .unwrap_err();
        assert_eq!(update_error.code, SkillErrorCode::LimitExceeded);
        assert_eq!(
            manager.load_inventory().unwrap().skills[0].content_hash,
            current.content_hash
        );
        assert!(manager
            .enforce_external_version_quota_with_limits(&current, 1, 100, 0)
            .is_err());
    }

    #[test]
    fn public_update_reuses_existing_hash_at_eight_versions_without_growth() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("existing-version-public-update-config");
        let source_root = TestDir::new("existing-version-public-update-source");
        let source = source_root.0.join("reuse-skill");
        fs::create_dir(&source).unwrap();
        write_skill(&source, "Reuse Skill", "1.0.0", "A");
        let a_bytes = fs::read(source.join("SKILL.md")).unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let original = manager
            .sync_external_home_inspection("reuse-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        for version in 1..MAX_EXTERNAL_VERSIONS_PER_SKILL {
            write_skill(
                &source,
                "Reuse Skill",
                &format!("1.0.{version}"),
                &format!("version-{version}"),
            );
            manager
                .update_source(&original.skill_id, &source, true)
                .unwrap();
        }
        let id_store = manager.paths.store.join(original.skill_id.as_str());
        assert_eq!(fs::read_dir(&id_store).unwrap().count(), 8);
        fs::write(source.join("SKILL.md"), a_bytes).unwrap();
        let rollback = manager
            .update_source(&original.skill_id, &source, true)
            .unwrap();
        assert!(rollback.changed);
        assert_eq!(rollback.skill.content_hash, original.content_hash);
        assert_eq!(fs::read_dir(&id_store).unwrap().count(), 8);
    }

    #[test]
    fn existing_registry_hash_is_never_cleaned_on_failed_inventory_rollback() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("existing-registry-rollback-config");
        let data = TestDir::new("existing-registry-rollback-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let source_root = TestDir::new("existing-registry-rollback-source");
        let source = source_root.0.join("registry-skill");
        fs::create_dir(&source).unwrap();
        write_skill(&source, "Registry Skill", "1.0.0", "A");
        let a_bytes = fs::read(source.join("SKILL.md")).unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let a = manager
            .sync_external_home_inspection("registry-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        manager.reconcile(&data.0, false, "deploy_a").unwrap();
        write_skill(&source, "Registry Skill", "1.1.0", "B");
        let b = manager
            .sync_external_home_inspection("registry-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        fs::write(source.join("SKILL.md"), a_bytes).unwrap();
        let faulty = SkillManager::new(root.0.join(".csswitch"))
            .with_fault(FaultPoint::BeforeInventoryRename);
        let error = faulty
            .update_source(&a.skill_id, &source, true)
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::AtomicCommitFailed);
        assert_eq!(
            manager.load_inventory().unwrap().skills[0].content_hash,
            b.content_hash
        );
        assert!(manager
            .paths
            .store
            .join(a.skill_id.as_str())
            .join(&a.content_hash)
            .is_dir());
        assert_eq!(
            DeploymentService::new(manager.config_dir.clone(), data.0.clone())
                .load_registry()
                .unwrap()
                .deployments[0]
                .content_hash,
            a.content_hash
        );
    }

    #[test]
    fn external_gc_failure_keeps_current_and_runtime_references_then_retries() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("external-gc-fault-config");
        let source_root = TestDir::new("external-gc-fault-source-root");
        let source = source_root.0.join("gc-skill");
        fs::create_dir(&source).unwrap();
        write_skill(&source, "GC Skill", "1.0.0", "old");
        let data = TestDir::new("external-gc-fault-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let old = manager
            .sync_external_home_inspection("gc-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        manager.reconcile(&data.0, false, "initial").unwrap();
        write_skill(&source, "GC Skill", "1.1.0", "new");
        let new = manager
            .sync_external_home_inspection("gc-skill", manager.inspect(&source).unwrap())
            .unwrap()
            .skill;
        let faulty =
            SkillManager::new(root.0.join(".csswitch")).with_fault(FaultPoint::BeforeVersionGc);
        let report = faulty.reconcile(&data.0, false, "replace").unwrap();
        assert!(report.errors.is_empty());
        let id_store = manager.paths.store.join(old.skill_id.as_str());
        assert!(id_store.join(&old.content_hash).is_dir());
        assert!(id_store.join(&new.content_hash).is_dir());
        assert!(data
            .0
            .join("skills")
            .join(&new.runtime_name)
            .join("SKILL.md")
            .is_file());
        assert_eq!(
            manager.load_inventory().unwrap().skills[0].content_hash,
            new.content_hash
        );

        manager.reconcile(&data.0, false, "gc_retry").unwrap();
        assert!(!id_store.join(&old.content_hash).exists());
        assert!(id_store.join(&new.content_hash).is_dir());
    }

    #[test]
    fn inventory_fault_retries_do_not_accumulate_random_skill_id_orphans() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("external-inventory-retry-config");
        let source = TestDir::skill("Retry Skill", "1.0.0", "body");
        let faulty = SkillManager::new(root.0.join(".csswitch"))
            .with_fault(FaultPoint::BeforeInventoryRename);
        for _ in 0..3 {
            let error = faulty
                .sync_external_home_inspection("retry-skill", faulty.inspect(&source.0).unwrap())
                .unwrap_err();
            assert_eq!(error.code, SkillErrorCode::AtomicCommitFailed);
            assert!(faulty.load_inventory().unwrap().skills.is_empty());
            assert_eq!(fs::read_dir(&faulty.paths.store).unwrap().count(), 0);
        }
    }

    #[test]
    fn uncertain_cleanup_inventory_read_never_deletes_committed_payload() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("uncertain-cleanup-read-config");
        let data = TestDir::new("uncertain-cleanup-read-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let source = TestDir::skill("Uncertain Skill", "1.0.0", "body");
        let faulty = SkillManager::new(root.0.join(".csswitch"))
            .with_fault(FaultPoint::BeforeInventoryRenameCleanupReadFails);
        let expected = faulty.inspect(&source.0).unwrap();
        let error = faulty
            .sync_external_home_inspection("uncertain-skill", expected.clone())
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::AtomicCommitFailed);
        assert!(faulty.load_inventory().unwrap().skills.is_empty());
        let id_path = fs::read_dir(&faulty.paths.store)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(id_path.join(&expected.summary.content_hash).is_dir());
        assert_eq!(faulty.recover_store_orphans(&data.0).unwrap(), 1);
        assert_eq!(fs::read_dir(&faulty.paths.store).unwrap().count(), 0);
    }

    #[test]
    fn crash_orphan_recovery_preserves_registry_reference_then_cleans_unreferenced() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("external-crash-orphan-config");
        let data = TestDir::new("external-crash-orphan-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let source = TestDir::skill("Crash Skill", "1.0.0", "body");
        let faulty =
            SkillManager::new(root.0.join(".csswitch")).with_fault(FaultPoint::AfterStoreRename);
        let expected = faulty.inspect(&source.0).unwrap();
        let error = faulty
            .sync_external_home_inspection("crash-skill", expected.clone())
            .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::CommitDurabilityUncertain);
        assert!(faulty.load_inventory().unwrap().skills.is_empty());
        let id_name = fs::read_dir(&faulty.paths.store)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .to_string();
        let skill_id = SkillId::parse(id_name).unwrap();
        let service = DeploymentService::new(faulty.config_dir.clone(), data.0.clone());
        service
            .save_registry(&DeploymentRegistry {
                schema_version: 1,
                deployments: vec![super::super::deployment::DeploymentRecord {
                    skill_id: skill_id.clone(),
                    runtime_name: "crash-skill--00000000".to_string(),
                    content_hash: expected.summary.content_hash.clone(),
                }],
            })
            .unwrap();
        assert_eq!(faulty.recover_store_orphans(&data.0).unwrap(), 0);
        assert!(faulty
            .paths
            .store
            .join(skill_id.as_str())
            .join(&expected.summary.content_hash)
            .is_dir());
        service
            .save_registry(&DeploymentRegistry::default())
            .unwrap();
        assert_eq!(faulty.recover_store_orphans(&data.0).unwrap(), 1);
        assert_eq!(fs::read_dir(&faulty.paths.store).unwrap().count(), 0);
    }

    #[test]
    fn owned_crash_staging_blocks_growth_and_is_recovered_safely() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("owned-crash-staging-config");
        let data = TestDir::new("owned-crash-staging-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let source = TestDir::skill("Staging Skill", "1.0.0", "body");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        let inspection = manager.inspect(&source.0).unwrap();
        let mut orphan = installed.clone();
        orphan.skill_id = SkillId::new_random().unwrap();
        orphan.runtime_name = runtime_name(&orphan.manifest.name, &orphan.skill_id).unwrap();
        let skills = SafeDir::open_absolute(&manager.paths.root).unwrap();
        let staging = skills
            .create_child(".staging-crash-owned".as_ref())
            .unwrap();
        staging
            .create_file(STORE_MARKER_FILE.as_ref(), &marker_bytes(&orphan).unwrap())
            .unwrap();
        let payload = staging.create_child("payload".as_ref()).unwrap();
        for file in inspection.files {
            let mut current = payload.try_clone().unwrap();
            let mut components = file.relative_path.components().peekable();
            while let Some(Component::Normal(name)) = components.next() {
                if components.peek().is_some() {
                    current = current.open_or_create_child(name).unwrap();
                } else {
                    current
                        .create_file_mode(name, &file.content, file.executable)
                        .unwrap();
                }
            }
        }
        assert!(manager.owned_store_usage().is_err());
        assert_eq!(manager.recover_store_orphans(&data.0).unwrap(), 1);
        assert!(!manager.paths.root.join(".staging-crash-owned").exists());
        manager.verify_skill_store(&installed).unwrap();
        assert!(manager.owned_store_usage().is_ok());
    }

    #[test]
    fn global_store_quota_rejects_without_mutating_inventory() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("external-global-quota-config");
        let source = TestDir::skill("Global Skill", "1.0.0", "body");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager
            .sync_external_home_inspection("global-skill", manager.inspect(&source.0).unwrap())
            .unwrap()
            .skill;
        let before = fs::read(&manager.paths.inventory).unwrap();
        assert!(manager
            .enforce_global_store_quota_with_limits(1, true, true, 1, 100, u64::MAX)
            .is_err());
        assert!(manager
            .enforce_global_store_quota_with_limits(1, false, true, 100, 1, u64::MAX)
            .is_err());
        assert!(manager
            .enforce_global_store_quota_with_limits(
                30 * 1024 * 1024,
                false,
                true,
                100,
                100,
                MAX_OWNED_STORE_BYTES,
            )
            .is_ok());
        assert_eq!(before, fs::read(&manager.paths.inventory).unwrap());
        manager.verify_skill_store(&skill).unwrap();
    }

    #[test]
    fn staging_and_rename_faults_leave_no_installed_inventory_entry() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for fault in [FaultPoint::AfterStaging, FaultPoint::BeforeStoreRename] {
            let root = TestDir::new("config");
            let source = TestDir::skill("Fault", "1.0.0", "body");
            let manager = SkillManager::new(root.0.join(".csswitch")).with_fault(fault);
            assert_eq!(
                manager.import_source(&source.0).unwrap_err().code,
                SkillErrorCode::AtomicCommitFailed
            );
            assert!(manager.load_inventory().unwrap().skills.is_empty());
            assert!(!fs::read_dir(&manager.paths.root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-")));
        }
    }

    #[test]
    fn invalid_inventory_marker_and_symlinked_intermediate_fail_closed() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let source = TestDir::skill("Guard", "1.0.0", "body");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        let version = manager
            .paths
            .payload(&installed.skill_id, &installed.content_hash)
            .parent()
            .unwrap()
            .to_path_buf();
        fs::write(version.join(STORE_MARKER_FILE), b"{}").unwrap();
        assert_eq!(
            manager.verify_store_version(&installed).unwrap_err().code,
            SkillErrorCode::StoreConflict
        );

        let other = TestDir::new("other");
        let guarded = TestDir::new("guarded");
        let config = guarded.0.join(".csswitch");
        fs::create_dir(&config).unwrap();
        symlink(&other.0, config.join("skills")).unwrap();
        let manager = SkillManager::new(config);
        assert_eq!(
            manager.import_source(&source.0).unwrap_err().code,
            SkillErrorCode::StoreConflict
        );
    }

    #[test]
    fn schema_and_concurrent_writes_fail_closed() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let config = root.0.join(".csswitch");
        let manager = SkillManager::new(config);
        manager.ensure_layout().unwrap();
        fs::write(
            &manager.paths.inventory,
            br#"{"schema_version":2,"skills":[]}"#,
        )
        .unwrap();
        assert_eq!(
            manager.load_inventory().unwrap_err().code,
            SkillErrorCode::InventoryInvalid
        );

        let source = TestDir::skill("Busy", "1.0.0", "body");
        let _guard = acquire_write_lock().unwrap();
        assert_eq!(
            manager.import_source(&source.0).unwrap_err().code,
            SkillErrorCode::ManagerBusy
        );
    }

    #[test]
    fn uninitialized_inventory_is_empty_and_read_is_side_effect_free() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let config = root.0.join("not-created").join(".csswitch");
        let manager = SkillManager::new(config.clone());
        assert!(manager.load_inventory().unwrap().skills.is_empty());
        assert!(!config.exists());
    }

    #[test]
    fn inventory_post_rename_failure_reports_uncertain_and_new_pointer_is_observable() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let source = TestDir::skill("Durable", "1.0.0", "old");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let old = manager.import_source(&source.0).unwrap().skill;
        write_skill(&source.0, "Durable", "1.1.0", "new");
        let expected_hash = manager.inspect(&source.0).unwrap().summary.content_hash;
        let faulty = SkillManager::new(root.0.join(".csswitch"))
            .with_fault(FaultPoint::AfterInventoryRename);
        assert_eq!(
            faulty
                .update_source(&old.skill_id, &source.0, false)
                .unwrap_err()
                .code,
            SkillErrorCode::CommitDurabilityUncertain
        );
        assert_eq!(
            manager.load_inventory().unwrap().skills[0].content_hash,
            expected_hash
        );
        assert!(manager
            .paths
            .store
            .join(old.skill_id.as_str())
            .join(&expected_hash)
            .join("payload/SKILL.md")
            .is_file());
    }

    #[test]
    fn broken_symlink_and_unmanaged_version_targets_are_never_replaced() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("config");
        let source = TestDir::skill("Collision", "1.0.0", "old");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let installed = manager.import_source(&source.0).unwrap().skill;
        write_skill(&source.0, "Collision", "1.1.0", "new");
        let new_hash = manager.inspect(&source.0).unwrap().summary.content_hash;
        let target = manager
            .paths
            .store
            .join(installed.skill_id.as_str())
            .join(&new_hash);
        symlink(root.0.join("missing"), &target).unwrap();
        assert_eq!(
            manager
                .update_source(&installed.skill_id, &source.0, false)
                .unwrap_err()
                .code,
            SkillErrorCode::StoreConflict
        );
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"unmanaged").unwrap();
        assert_eq!(
            manager
                .update_source(&installed.skill_id, &source.0, false)
                .unwrap_err()
                .code,
            SkillErrorCode::StoreConflict
        );
        assert_eq!(fs::read(&target).unwrap(), b"unmanaged");
    }

    #[test]
    fn fd_anchored_writes_ignore_replaced_intermediate_path() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("fd-anchor");
        let external = TestDir::new("external");
        let root_fd = SafeDir::open_absolute(&root.0).unwrap();
        let payload_fd = root_fd.create_child("payload".as_ref()).unwrap();
        let held = root.0.join("held");
        fs::rename(root.0.join("payload"), &held).unwrap();
        symlink(&external.0, root.0.join("payload")).unwrap();
        payload_fd
            .create_file("proof.txt".as_ref(), b"anchored")
            .unwrap();
        assert_eq!(fs::read(held.join("proof.txt")).unwrap(), b"anchored");
        assert!(!external.0.join("proof.txt").exists());
    }

    #[test]
    fn no_replace_rename_preserves_existing_target() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("rename");
        let root_fd = SafeDir::open_absolute(&root.0).unwrap();
        let source = root_fd.create_child("source".as_ref()).unwrap();
        let target = root_fd.create_child("target".as_ref()).unwrap();
        source.create_file("stage".as_ref(), b"new").unwrap();
        target
            .create_file("hash".as_ref(), b"owned elsewhere")
            .unwrap();
        assert_eq!(
            rename_noreplace(&source, "stage".as_ref(), &target, "hash".as_ref())
                .unwrap_err()
                .code,
            SkillErrorCode::StoreConflict
        );
        assert_eq!(
            target.read_file("hash".as_ref(), 100).unwrap(),
            b"owned elsewhere"
        );
    }

    #[test]
    fn fd_anchored_layout_creation_does_not_follow_swapped_ancestor() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outer = TestDir::new("layout-anchor");
        let real = outer.0.join("real");
        let held = outer.0.join("held");
        let external = outer.0.join("external");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&external).unwrap();
        let target = real.join(".csswitch");
        let swapped = std::sync::atomic::AtomicBool::new(false);
        let real_hook = real.clone();
        let held_hook = held.clone();
        let external_hook = external.clone();
        let hook = |walked: &Path| {
            if walked == target && !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&real_hook, &held_hook).unwrap();
                symlink(&external_hook, &real_hook).unwrap();
            }
        };
        SafeDir::ensure_absolute_with_hook(&target, &hook).unwrap();
        assert!(held.join(".csswitch").is_dir());
        assert!(!external.join(".csswitch").exists());
    }

    #[test]
    fn fd_anchored_cleanup_does_not_touch_external_same_name() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outer = TestDir::new("cleanup-anchor");
        let skills = outer.0.join("skills");
        let held = outer.0.join("held");
        let external = outer.0.join("external");
        fs::create_dir(&skills).unwrap();
        fs::create_dir(&external).unwrap();
        let skills_fd = SafeDir::open_absolute(&skills).unwrap();
        let staging = skills_fd.create_child(".staging-proof".as_ref()).unwrap();
        staging.create_file("owned".as_ref(), b"owned").unwrap();
        fs::create_dir(external.join(".staging-proof")).unwrap();
        fs::write(external.join(".staging-proof/victim"), b"keep").unwrap();
        fs::rename(&skills, &held).unwrap();
        symlink(&external, &skills).unwrap();
        skills_fd
            .remove_tree_child(".staging-proof".as_ref())
            .unwrap();
        assert!(!held.join(".staging-proof").exists());
        assert_eq!(
            fs::read(external.join(".staging-proof/victim")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn root_marker_initialization_failures_are_retryable() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for fault in [FaultPoint::BeforeRootMarker, FaultPoint::AfterRootMarker] {
            let root = TestDir::new("root-retry");
            let config = root.0.join(".csswitch");
            let source = TestDir::skill("Retry", "1.0.0", "body");
            let faulty = SkillManager::new(config.clone()).with_fault(fault);
            assert_eq!(
                faulty.import_source(&source.0).unwrap_err().code,
                SkillErrorCode::AtomicCommitFailed
            );
            assert!(!config.join("skills").exists());
            assert!(!fs::read_dir(&config)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".skills-init-")));
            let manager = SkillManager::new(config);
            assert!(manager.import_source(&source.0).unwrap().changed);
        }
    }

    #[test]
    fn verified_root_fd_remains_the_only_inventory_and_store_authority_after_path_swap() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outer = TestDir::new("authority-swap");
        let source = TestDir::skill("Authority", "1.0.0", "body");
        let config = outer.0.join(".csswitch");
        let manager = SkillManager::new(config.clone());
        let installed = manager.import_source(&source.0).unwrap().skill;

        let root_fd = SafeDir::open_absolute(&manager.paths.root).unwrap();
        root_fd.validate_owned().unwrap();
        verify_root_marker_fd(&root_fd).unwrap();
        let inventory = root_fd
            .read_file(INVENTORY_FILE.as_ref(), MAX_INVENTORY_SIZE)
            .unwrap();

        let held = config.join("skills-held");
        let replacement = config.join("skills");
        fs::rename(&replacement, &held).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();

        root_fd
            .create_file(".proof.tmp".as_ref(), &inventory)
            .unwrap();
        root_fd
            .replace_child(".proof.tmp".as_ref(), INVENTORY_FILE.as_ref())
            .unwrap();
        let store = root_fd.open_child("store".as_ref()).unwrap();
        let id = store
            .open_child(installed.skill_id.as_str().as_ref())
            .unwrap();
        let version = id.open_child(installed.content_hash.as_ref()).unwrap();
        verify_store_version_fd(&installed, &version).unwrap();

        assert!(fs::read_dir(&replacement).unwrap().next().is_none());
        assert_eq!(
            manager.load_inventory().unwrap_err().code,
            SkillErrorCode::InventoryInvalid
        );
        assert!(held.join(INVENTORY_FILE).is_file());
    }

    #[test]
    fn gc_never_deletes_items_inserted_or_replaced_after_version_verification() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("gc-race");
        let source = TestDir::skill("GC", "1.0.0", "body");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let id_path = manager.paths.store.join(skill.skill_id.as_str());
        let inserted = std::sync::atomic::AtomicBool::new(false);
        let hook = || {
            if !inserted.swap(true, Ordering::SeqCst) {
                fs::write(id_path.join("unmanaged"), b"keep").unwrap();
            }
        };
        assert!(manager.remove_skill_store_with_hook(&skill, &hook).is_err());
        assert_eq!(fs::read(id_path.join("unmanaged")).unwrap(), b"keep");

        fs::remove_file(id_path.join("unmanaged")).unwrap();
        write_skill(&source.0, "GC", "1.1.0", "new");
        let updated = manager
            .update_source(&skill.skill_id, &source.0, false)
            .unwrap()
            .skill;
        let version_path = id_path.join(&updated.content_hash);
        let original_payload = fs::read(version_path.join("payload/SKILL.md")).unwrap();
        let original_marker = fs::read(version_path.join(STORE_MARKER_FILE)).unwrap();
        let held = id_path.join("held-version");
        let swapped = std::sync::atomic::AtomicBool::new(false);
        let hook = || {
            if !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&version_path, &held).unwrap();
                fs::create_dir(&version_path).unwrap();
                fs::set_permissions(&version_path, fs::Permissions::from_mode(0o700)).unwrap();
                fs::write(version_path.join("manual"), b"keep").unwrap();
            }
        };
        assert!(manager
            .remove_skill_store_with_hook(&updated, &hook)
            .is_err());
        assert_eq!(fs::read(version_path.join("manual")).unwrap(), b"keep");
        assert_eq!(
            fs::read(held.join("payload/SKILL.md")).unwrap(),
            original_payload
        );
        assert_eq!(
            fs::read(held.join(STORE_MARKER_FILE)).unwrap(),
            original_marker
        );
    }

    #[test]
    fn gc_never_deletes_files_inserted_inside_verified_version() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("gc-inner-race");
        let source = TestDir::skill("GC Inner", "1.0.0", "body");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let version = manager
            .paths
            .store
            .join(skill.skill_id.as_str())
            .join(&skill.content_hash);
        let hook = || {
            fs::write(version.join("late-unmanaged"), b"keep").unwrap();
        };
        assert!(manager.remove_skill_store_with_hook(&skill, &hook).is_err());
        assert_eq!(fs::read(version.join("late-unmanaged")).unwrap(), b"keep");
        assert!(version.join(STORE_MARKER_FILE).is_file());
        assert!(version.join("payload/SKILL.md").is_file());
    }

    #[test]
    fn update_and_uninstall_reject_when_manager_lock_is_held() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("busy-lifecycle");
        let data = TestDir::new("busy-data");
        let source = TestDir::skill("Busy Lifecycle", "1.0.0", "body");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        let _guard = acquire_write_lock().unwrap();
        assert_eq!(
            manager
                .update_source(&skill.skill_id, &source.0, false)
                .unwrap_err()
                .code,
            SkillErrorCode::ManagerBusy
        );
        assert_eq!(
            manager
                .uninstall(&skill.skill_id, &data.0)
                .unwrap_err()
                .code,
            SkillErrorCode::ManagerBusy
        );
    }

    #[test]
    fn discovery_observation_requires_current_deployment_and_is_version_bound() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("discovery-record");
        let data = TestDir::new("discovery-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let source = TestDir::skill("Discovery", "1.0.0", "marker");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;

        assert_eq!(
            manager
                .record_discovery_observation(&data.0, &skill.skill_id, "science 1.0", true, 10)
                .unwrap_err()
                .code,
            SkillErrorCode::DeploymentConflict
        );
        manager.set_enabled(&skill.skill_id, true).unwrap();
        let reconcile = manager.reconcile(&data.0, false, "test").unwrap();
        assert!(reconcile.errors.is_empty(), "{reconcile:?}");
        manager.mark_science_started(&data.0).unwrap();
        let recorded = manager
            .record_discovery_observation(&data.0, &skill.skill_id, "science 1.0", true, 10)
            .unwrap();
        assert!(recorded.discovered);
        let evidence_path = manager.paths.root.join(DISCOVERY_FILE);
        assert_eq!(
            fs::metadata(&evidence_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let current = manager
            .status(&data.0, ScienceProbeState::Running, Some("science 1.0"))
            .unwrap();
        assert_eq!(
            current.skills[0].discovery_status,
            DiscoveryStatus::Discovered
        );
        manager
            .record_discovery_observation(&data.0, &skill.skill_id, "science 1.0", false, 11)
            .unwrap();
        let negative = manager
            .status(&data.0, ScienceProbeState::Running, Some("science 1.0"))
            .unwrap();
        assert_eq!(
            negative.skills[0].discovery_status,
            DiscoveryStatus::NotDiscovered
        );
        let changed_version = manager
            .status(&data.0, ScienceProbeState::Running, Some("science 2.0"))
            .unwrap();
        assert_eq!(
            changed_version.skills[0].discovery_status,
            DiscoveryStatus::Unknown
        );
    }

    #[test]
    fn discovery_evidence_target_symlink_fails_closed_without_touching_target() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("discovery-symlink");
        let data = TestDir::new("discovery-symlink-data");
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let source = TestDir::skill("Discovery Safe", "1.0.0", "marker");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        manager.reconcile(&data.0, false, "test").unwrap();
        manager.mark_science_started(&data.0).unwrap();

        let target = root.0.join("external-evidence");
        fs::write(&target, b"keep").unwrap();
        symlink(&target, manager.paths.root.join(DISCOVERY_FILE)).unwrap();
        assert_eq!(
            manager
                .record_discovery_observation(&data.0, &skill.skill_id, "science 1.0", true, 10)
                .unwrap_err()
                .code,
            SkillErrorCode::StoreConflict
        );
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[test]
    fn conflicting_store_is_quarantined_and_valid_payload_is_restored() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("store-recovery");
        let source = TestDir::skill("Recovery Probe", "1.0.0", "recover me");
        let manager = SkillManager::new(root.0.join(".csswitch"));
        let skill = manager.import_source(&source.0).unwrap().skill;
        manager.set_enabled(&skill.skill_id, true).unwrap();
        fs::write(
            manager
                .paths
                .store
                .join(skill.skill_id.as_str())
                .join(&skill.content_hash)
                .join(STORE_MARKER_FILE),
            b"corrupt",
        )
        .unwrap();
        assert_eq!(
            manager.verify_skill_store(&skill).unwrap_err().code,
            SkillErrorCode::StoreConflict
        );

        let recovery = manager.quarantine_and_restore_store().unwrap();
        assert_eq!(recovery.recovered, 1);
        assert_eq!(recovery.skipped, 0);
        assert!(recovery.quarantine_path.is_dir());
        let restored = manager.load_inventory().unwrap();
        assert_eq!(restored.skills.len(), 1);
        assert!(restored.skills[0].enabled);
        manager.verify_skill_store(&restored.skills[0]).unwrap();
    }
}
