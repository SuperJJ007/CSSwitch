use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::archive::canonical_content_sha256;
use crate::install::{
    acquire_install_lock, create_private_directories, create_private_staging, reject_symlink_path,
    rename_no_replace, rfc3339_now, scan_installed_payload_with_limits, skills_root,
    sync_directory, sync_tree, verify_csswitch_import_origin, write_new_file, InstallAction,
    SourceDescriptor,
};
use crate::{
    active_org, error, BundleMember, InstallError, PackageFile, SourceKind, ValidatedBundle,
    CSSWITCH_MARKETPLACE, IMPORT_ORIGIN_FILE, MAX_BUNDLE_FILES, MAX_BUNDLE_TOTAL_BYTES,
    MAX_IMPORT_ORIGIN_BYTES,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleMemberCommit {
    pub skill_name: String,
    pub content_sha256: String,
    pub install_action: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleCommit {
    pub bundle_id: String,
    pub bundle_name: String,
    pub source_kind: SourceKind,
    pub active_org: String,
    pub bundle_content_sha256: String,
    pub source_digest_sha256: Option<String>,
    pub resolved_commit_sha: Option<String>,
    pub source_repo: String,
    pub source_path: String,
    pub skill_names: Vec<String>,
    pub support_paths: Vec<String>,
    pub members: Vec<BundleMemberCommit>,
    pub action: InstallAction,
    pub directory_commit: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleUninstall {
    pub bundle_id: String,
    pub bundle_name: String,
    pub active_org: String,
    pub skill_names: Vec<String>,
    pub top_level_paths: Vec<String>,
    pub manifest_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleUninstallCommit {
    pub bundle_id: String,
    pub bundle_name: String,
    pub skill_names: Vec<String>,
    pub quarantined_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BundleManifest {
    schema_version: u64,
    bundle_id: String,
    bundle_name: String,
    source_kind: String,
    source_repo: String,
    source_sha: String,
    source_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_sha256: Option<String>,
    bundle_content_sha256: String,
    skill_names: Vec<String>,
    support_paths: Vec<String>,
    top_level_paths: Vec<String>,
    member_hashes: BTreeMap<String, String>,
    path_hashes: BTreeMap<String, String>,
    state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BundleJournal {
    schema_version: u64,
    bundle_id: String,
    state: String,
    staging: PathBuf,
    backup: PathBuf,
    installed: Vec<String>,
    moved_old: Vec<String>,
}

pub(crate) fn bundle_id_for_github(repo: &str, collection_path: &str) -> String {
    stable_bundle_id(&format!("github\0{repo}\0{collection_path}"))
}

pub(crate) fn bundle_id_for_local(bundle_name: &str, collection_path: &str) -> String {
    stable_bundle_id(&format!("local_zip\0{bundle_name}\0{collection_path}"))
}

pub(crate) fn recover_existing_github_bundle(
    data_dir: &Path,
    active_org: &str,
    source_repo: &str,
    resolved_commit: &str,
    requested_path: &str,
) -> Result<Option<BundleCommit>, InstallError> {
    let state_root = bundle_state_root(data_dir, active_org)?;
    let entries = match fs::read_dir(&state_root) {
        Ok(entries) => entries,
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(error(
                "BUNDLE_STATE_INVALID",
                "无法读取 bundle 状态目录",
                "recovery",
            ))
        }
    };
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| {
            error(
                "BUNDLE_STATE_INVALID",
                "读取 bundle 状态目录失败",
                "recovery",
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(bundle_id) = name.strip_suffix(".json") else {
            continue;
        };
        if !valid_bundle_id(bundle_id) {
            continue;
        }
        let manifest = read_manifest(&entry.path())?;
        if github_manifest_matches(&manifest, source_repo, resolved_commit, requested_path) {
            matches.push(manifest);
        }
    }
    if matches.len() > 1 {
        return Err(error(
            "MULTIPLE_BUNDLE_CANDIDATES",
            "同一 GitHub 来源匹配到多个已安装 bundle，拒绝猜测",
            "recovery",
        ));
    }
    let Some(discovered_manifest) = matches.into_iter().next() else {
        return Ok(None);
    };
    let skills = skills_root(data_dir, active_org)?;
    let bundle_id = discovered_manifest.bundle_id;
    let _bundle_lock = acquire_install_lock(&state_root.join(format!("{bundle_id}.lock")))?;
    recover_journal(
        &skills,
        &state_root.join(format!("{bundle_id}.journal.json")),
    )?;
    let manifest = read_manifest(&state_root.join(format!("{bundle_id}.json")))?;
    if !github_manifest_matches(&manifest, source_repo, resolved_commit, requested_path) {
        return Ok(None);
    }
    verify_installed_manifest(&skills, &manifest)?;
    let members = manifest
        .skill_names
        .iter()
        .map(|skill_name| {
            let content_sha256 =
                manifest
                    .member_hashes
                    .get(skill_name)
                    .cloned()
                    .ok_or_else(|| {
                        error(
                            "BUNDLE_STATE_INVALID",
                            "bundle manifest 缺少成员摘要",
                            "recovery",
                        )
                    })?;
            Ok(BundleMemberCommit {
                skill_name: skill_name.clone(),
                content_sha256,
                install_action: InstallAction::ReusedVerified.as_str().to_string(),
            })
        })
        .collect::<Result<Vec<_>, InstallError>>()?;
    Ok(Some(BundleCommit {
        bundle_id: manifest.bundle_id,
        bundle_name: manifest.bundle_name,
        source_kind: SourceKind::Github,
        active_org: active_org.to_string(),
        bundle_content_sha256: manifest.bundle_content_sha256,
        source_digest_sha256: None,
        resolved_commit_sha: Some(manifest.source_sha),
        source_repo: manifest.source_repo,
        source_path: manifest.source_path,
        skill_names: manifest.skill_names,
        support_paths: manifest.support_paths,
        members,
        action: InstallAction::ReusedVerified,
        directory_commit: false,
    }))
}

fn github_manifest_matches(
    manifest: &BundleManifest,
    source_repo: &str,
    resolved_commit: &str,
    requested_path: &str,
) -> bool {
    let requested_match = requested_path.is_empty()
        || manifest.source_path == requested_path
        || manifest
            .source_path
            .strip_prefix(requested_path)
            .is_some_and(|suffix| suffix.starts_with('/'));
    manifest.source_kind == SourceKind::Github.as_str()
        && manifest.source_repo == source_repo
        && manifest.source_sha == resolved_commit
        && requested_match
}

fn stable_bundle_id(identity: &str) -> String {
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

pub(crate) fn install_validated_bundle(
    data_dir: &Path,
    bundle: ValidatedBundle,
    descriptor: SourceDescriptor,
    initial_org: &str,
    bundle_id: String,
) -> Result<BundleCommit, InstallError> {
    if active_org(data_dir)? != initial_org {
        return Err(active_org_changed());
    }
    let skills = skills_root(data_dir, initial_org)?;
    fs::create_dir_all(&skills).map_err(|_| {
        error(
            "SKILLS_ROOT_CREATE_FAILED",
            "创建 Science Skills 目录失败",
            "commit",
        )
    })?;
    reject_symlink_path(&skills)?;
    let state_root = bundle_state_root(data_dir, initial_org)?;
    fs::create_dir_all(&state_root).map_err(|_| {
        error(
            "BUNDLE_STATE_WRITE_FAILED",
            "创建 bundle 状态目录失败",
            "commit",
        )
    })?;
    let _bundle_lock = acquire_install_lock(&state_root.join(format!("{bundle_id}.lock")))?;
    recover_journal(
        &skills,
        &state_root.join(format!("{bundle_id}.journal.json")),
    )?;

    let top_level_paths = top_level_paths(&bundle.files);
    let manifest_path = state_root.join(format!("{bundle_id}.json"));
    let existing_manifest = read_manifest_optional(&manifest_path)?;
    let mut lock_paths = top_level_paths.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(existing) = &existing_manifest {
        lock_paths.extend(existing.top_level_paths.iter().cloned());
    }
    let mut locks = Vec::new();
    for name in &lock_paths {
        locks.push(acquire_install_lock(
            &state_root.join(format!("path-{}.lock", lock_component(name))),
        )?);
    }

    if let Some(existing) = &existing_manifest {
        if existing.source_repo != descriptor.repo || existing.source_path != descriptor.path {
            return Err(error(
                "BUNDLE_PATH_CONFLICT",
                "bundle 标识已由其他来源占用",
                "recovery",
            ));
        }
        verify_installed_manifest(&skills, existing)?;
        if existing.bundle_content_sha256 == bundle.content_sha256 {
            return Ok(bundle_commit_result(
                &bundle,
                &descriptor,
                initial_org,
                bundle_id,
                InstallAction::ReusedVerified,
                false,
            ));
        }
        let owned = existing
            .top_level_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        for name in &top_level_paths {
            if !owned.contains(name) && fs::symlink_metadata(skills.join(name)).is_ok() {
                return Err(error(
                    "BUNDLE_PATH_CONFLICT",
                    format!("bundle 更新目标路径已由其他来源占用：{name}"),
                    "commit",
                ));
            }
        }
    } else {
        for name in &top_level_paths {
            if fs::symlink_metadata(skills.join(name)).is_ok() {
                return Err(error(
                    "BUNDLE_PATH_CONFLICT",
                    format!("Science skills 顶层路径已存在：{name}"),
                    "commit",
                ));
            }
        }
    }

    let staging = state_root.join(format!(
        ".staging-{bundle_id}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let backup = state_root.join(format!(
        ".backup-{bundle_id}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    create_private_staging(&staging)?;
    create_private_staging(&backup)?;
    stage_bundle(&staging, &bundle, &descriptor, &bundle_id)?;
    sync_tree(&staging)?;
    if active_org(data_dir)? != initial_org {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(&backup);
        return Err(active_org_changed());
    }

    let manifest = build_manifest(&bundle, &descriptor, &bundle_id, &top_level_paths, &staging)?;
    let journal_path = state_root.join(format!("{bundle_id}.journal.json"));
    let mut journal = BundleJournal {
        schema_version: 1,
        bundle_id: bundle_id.clone(),
        state: "committing".to_string(),
        staging: staging.clone(),
        backup: backup.clone(),
        installed: Vec::new(),
        moved_old: Vec::new(),
    };
    write_json_atomic(&journal_path, &journal)?;

    let transaction = (|| -> Result<(), InstallError> {
        if let Some(existing) = &existing_manifest {
            for name in &existing.top_level_paths {
                let target = skills.join(name);
                if fs::symlink_metadata(&target).is_ok() {
                    fs::rename(&target, backup.join(name)).map_err(|_| {
                        error(
                            "BUNDLE_COMMIT_FAILED",
                            format!("无法备份旧 bundle 路径：{name}"),
                            "commit",
                        )
                    })?;
                    journal.moved_old.push(name.clone());
                    write_json_atomic(&journal_path, &journal)?;
                }
            }
        }
        for name in &top_level_paths {
            rename_no_replace(&staging.join(name), &skills.join(name))?;
            journal.installed.push(name.clone());
            write_json_atomic(&journal_path, &journal)?;
        }
        sync_directory(&skills)?;
        write_json_atomic(&manifest_path, &manifest)?;
        journal.state = "committed".to_string();
        write_json_atomic(&journal_path, &journal)?;
        Ok(())
    })();
    if let Err(problem) = transaction {
        rollback_journal(&skills, &journal)?;
        let _ = fs::remove_file(&journal_path);
        return Err(problem);
    }
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&backup);
    let _ = fs::remove_file(&journal_path);
    drop(locks);
    Ok(bundle_commit_result(
        &bundle,
        &descriptor,
        initial_org,
        bundle_id,
        InstallAction::Committed,
        true,
    ))
}

fn stage_bundle(
    staging: &Path,
    bundle: &ValidatedBundle,
    descriptor: &SourceDescriptor,
    bundle_id: &str,
) -> Result<(), InstallError> {
    for file in &bundle.files {
        let destination = staging.join(&file.path);
        if let Some(parent) = destination.parent() {
            create_private_directories(staging, parent)?;
        }
        write_new_file(&destination, &file.content, file.executable)?;
    }
    for member in &bundle.members {
        let skill_dir = staging.join(&member.member_path);
        let marker = bundle_marker(bundle, member, descriptor, bundle_id);
        let mut body = serde_json::to_vec(&marker)
            .map_err(|_| error("MARKER_WRITE_FAILED", "编码 bundle marker 失败", "commit"))?;
        body.push(b'\n');
        if body.len() > MAX_IMPORT_ORIGIN_BYTES {
            return Err(error(
                "MARKER_WRITE_FAILED",
                "bundle marker 超过大小限制",
                "commit",
            ));
        }
        write_new_file(&skill_dir.join(IMPORT_ORIGIN_FILE), &body, false)?;
    }
    Ok(())
}

fn bundle_marker(
    bundle: &ValidatedBundle,
    member: &BundleMember,
    descriptor: &SourceDescriptor,
    bundle_id: &str,
) -> Value {
    let member_source_path = if descriptor.path.is_empty() {
        member.member_path.to_string_lossy().to_string()
    } else {
        format!(
            "{}/{}",
            descriptor.path,
            member.member_path.to_string_lossy()
        )
    };
    let mut marker = json!({
        "version": 1,
        "repo": descriptor.repo,
        "sha": descriptor.sha,
        "plugin": member.skill_name,
        "marketplace": CSSWITCH_MARKETPLACE,
        "path": member_source_path,
        "importedAt": rfc3339_now(),
        "license": "NOASSERTION",
        "csswitch_revision": 2,
        "source_kind": descriptor.kind.as_str(),
        "content_sha256": member.content_sha256,
        "bundle_id": bundle_id,
        "bundle_name": bundle.bundle_name,
        "bundle_content_sha256": bundle.content_sha256,
        "bundle_member_path": member.member_path.to_string_lossy(),
    });
    if let Some(digest) = &descriptor.archive_sha256 {
        marker["archive_sha256"] = Value::String(digest.clone());
    }
    marker
}

fn build_manifest(
    bundle: &ValidatedBundle,
    descriptor: &SourceDescriptor,
    bundle_id: &str,
    top_level_paths: &[String],
    staged: &Path,
) -> Result<BundleManifest, InstallError> {
    let member_hashes = bundle
        .members
        .iter()
        .map(|member| (member.skill_name.clone(), member.content_sha256.clone()))
        .collect();
    let mut path_hashes = BTreeMap::new();
    for name in top_level_paths {
        path_hashes.insert(name.clone(), hash_installed_path(&staged.join(name), true)?);
    }
    Ok(BundleManifest {
        schema_version: 1,
        bundle_id: bundle_id.to_string(),
        bundle_name: bundle.bundle_name.clone(),
        source_kind: descriptor.kind.as_str().to_string(),
        source_repo: descriptor.repo.clone(),
        source_sha: descriptor.sha.clone(),
        source_path: descriptor.path.clone(),
        archive_sha256: descriptor.archive_sha256.clone(),
        bundle_content_sha256: bundle.content_sha256.clone(),
        skill_names: bundle
            .members
            .iter()
            .map(|member| member.skill_name.clone())
            .collect(),
        support_paths: bundle.support_paths.clone(),
        top_level_paths: top_level_paths.to_vec(),
        member_hashes,
        path_hashes,
        state: "installed".to_string(),
    })
}

fn bundle_commit_result(
    bundle: &ValidatedBundle,
    descriptor: &SourceDescriptor,
    active_org: &str,
    bundle_id: String,
    action: InstallAction,
    directory_commit: bool,
) -> BundleCommit {
    let action_name = action.as_str().to_string();
    BundleCommit {
        bundle_id,
        bundle_name: bundle.bundle_name.clone(),
        source_kind: descriptor.kind.clone(),
        active_org: active_org.to_string(),
        bundle_content_sha256: bundle.content_sha256.clone(),
        source_digest_sha256: descriptor.archive_sha256.clone(),
        resolved_commit_sha: matches!(descriptor.kind, SourceKind::Github)
            .then(|| descriptor.sha.clone()),
        source_repo: descriptor.repo.clone(),
        source_path: descriptor.path.clone(),
        skill_names: bundle
            .members
            .iter()
            .map(|member| member.skill_name.clone())
            .collect(),
        support_paths: bundle.support_paths.clone(),
        members: bundle
            .members
            .iter()
            .map(|member| BundleMemberCommit {
                skill_name: member.skill_name.clone(),
                content_sha256: member.content_sha256.clone(),
                install_action: action_name.clone(),
            })
            .collect(),
        action,
        directory_commit,
    }
}

pub fn find_bundle_for_skill(
    data_dir: &Path,
    skill_name: &str,
) -> Result<Option<BundleUninstall>, InstallError> {
    let org = active_org(data_dir)?;
    let target = skills_root(data_dir, &org)?.join(skill_name);
    if fs::symlink_metadata(&target).is_err() {
        return Ok(None);
    }
    reject_symlink_path(&target)?;
    verify_csswitch_import_origin(&target, skill_name)?;
    let marker_body = fs::read(target.join(IMPORT_ORIGIN_FILE)).map_err(|_| {
        error(
            "INVALID_IMPORT_ORIGIN",
            "无法读取 Skill marker",
            "uninstall",
        )
    })?;
    let marker: Value = serde_json::from_slice(&marker_body).map_err(|_| {
        error(
            "INVALID_IMPORT_ORIGIN",
            "Skill marker JSON 非法",
            "uninstall",
        )
    })?;
    let Some(bundle_id) = marker.get("bundle_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !valid_bundle_id(bundle_id) {
        return Err(error(
            "INVALID_IMPORT_ORIGIN",
            "Skill bundle_id 非法",
            "uninstall",
        ));
    }
    let manifest_path = bundle_state_root(data_dir, &org)?.join(format!("{bundle_id}.json"));
    let manifest = read_manifest(&manifest_path)?;
    verify_installed_manifest(&skills_root(data_dir, &org)?, &manifest)?;
    Ok(Some(BundleUninstall {
        bundle_id: manifest.bundle_id,
        bundle_name: manifest.bundle_name,
        active_org: org,
        skill_names: manifest.skill_names,
        top_level_paths: manifest.top_level_paths,
        manifest_path,
    }))
}

pub fn quarantine_bundle(
    data_dir: &Path,
    uninstall: &BundleUninstall,
) -> Result<BundleUninstallCommit, InstallError> {
    if active_org(data_dir)? != uninstall.active_org {
        return Err(active_org_changed());
    }
    let skills = skills_root(data_dir, &uninstall.active_org)?;
    let state_root = bundle_state_root(data_dir, &uninstall.active_org)?;
    let _bundle_lock =
        acquire_install_lock(&state_root.join(format!("{}.lock", uninstall.bundle_id)))?;
    let mut path_locks = Vec::new();
    for name in uninstall.top_level_paths.iter().collect::<BTreeSet<_>>() {
        path_locks.push(acquire_install_lock(
            &state_root.join(format!("path-{}.lock", lock_component(name))),
        )?);
    }
    let manifest = read_manifest(&uninstall.manifest_path)?;
    if manifest.bundle_id != uninstall.bundle_id
        || manifest.bundle_name != uninstall.bundle_name
        || manifest.skill_names != uninstall.skill_names
        || manifest.top_level_paths != uninstall.top_level_paths
    {
        return Err(error(
            "BUNDLE_STATE_CHANGED",
            "bundle manifest 已变化；拒绝沿用旧确认",
            "uninstall",
        ));
    }
    verify_installed_manifest(&skills, &manifest)?;
    if active_org(data_dir)? != uninstall.active_org {
        return Err(active_org_changed());
    }
    let quarantine = state_root.join(format!(
        ".quarantine-{}-{}",
        uninstall.bundle_id,
        unique_suffix()
    ));
    create_private_staging(&quarantine)?;
    let moved = move_bundle_paths_transactional(&skills, &quarantine, &uninstall.top_level_paths)?;
    if active_org(data_dir)? != uninstall.active_org {
        rollback_bundle_paths(&skills, &quarantine, &moved)?;
        let _ = fs::remove_dir(&quarantine);
        return Err(active_org_changed());
    }
    if fs::rename(
        &uninstall.manifest_path,
        quarantine.join("bundle-manifest.json"),
    )
    .is_err()
    {
        rollback_bundle_paths(&skills, &quarantine, &moved)?;
        let _ = fs::remove_dir(&quarantine);
        return Err(error(
            "UNINSTALL_FAILED",
            "无法隔离 bundle manifest；已恢复全部 bundle 路径",
            "uninstall",
        ));
    }
    if sync_directory(&skills).is_err() {
        let _ = fs::rename(
            quarantine.join("bundle-manifest.json"),
            &uninstall.manifest_path,
        );
        rollback_bundle_paths(&skills, &quarantine, &moved)?;
        let _ = fs::remove_dir(&quarantine);
        return Err(error(
            "UNINSTALL_FAILED",
            "无法同步 bundle 卸载；已恢复全部 bundle 路径",
            "uninstall",
        ));
    }
    Ok(BundleUninstallCommit {
        bundle_id: uninstall.bundle_id.clone(),
        bundle_name: uninstall.bundle_name.clone(),
        skill_names: uninstall.skill_names.clone(),
        quarantined_path: quarantine,
    })
}

fn move_bundle_paths_transactional(
    skills: &Path,
    quarantine: &Path,
    names: &[String],
) -> Result<Vec<String>, InstallError> {
    let mut moved = Vec::new();
    for name in names {
        if fs::rename(skills.join(name), quarantine.join(name)).is_err() {
            rollback_bundle_paths(skills, quarantine, &moved)?;
            let _ = fs::remove_dir(quarantine);
            return Err(error(
                "UNINSTALL_FAILED",
                format!("无法隔离 bundle 路径：{name}；已恢复此前移动的路径"),
                "uninstall",
            ));
        }
        moved.push(name.clone());
    }
    Ok(moved)
}

fn rollback_bundle_paths(
    skills: &Path,
    quarantine: &Path,
    moved: &[String],
) -> Result<(), InstallError> {
    for name in moved.iter().rev() {
        fs::rename(quarantine.join(name), skills.join(name)).map_err(|_| {
            error(
                "UNINSTALL_ROLLBACK_FAILED",
                format!("恢复 bundle 路径失败：{name}"),
                "uninstall",
            )
        })?;
    }
    Ok(())
}

fn verify_installed_manifest(root: &Path, manifest: &BundleManifest) -> Result<(), InstallError> {
    for (name, expected) in &manifest.path_hashes {
        let path = root.join(name);
        if fs::symlink_metadata(&path).is_err() || hash_installed_path(&path, true)? != *expected {
            return Err(error(
                "INSTALLED_CONTENT_CHANGED",
                format!("bundle 已安装路径被修改：{name}"),
                "recovery",
            ));
        }
    }
    Ok(())
}

fn hash_installed_path(path: &Path, include_marker: bool) -> Result<String, InstallError> {
    if path.is_file() {
        let metadata = fs::metadata(path).map_err(|_| {
            error(
                "INSTALLED_CONTENT_CHANGED",
                "读取 bundle 文件失败",
                "recovery",
            )
        })?;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let file = PackageFile {
            path: PathBuf::from(path.file_name().and_then(|v| v.to_str()).unwrap_or("file")),
            content: fs::read(path).map_err(|_| {
                error(
                    "INSTALLED_CONTENT_CHANGED",
                    "读取 bundle 文件失败",
                    "recovery",
                )
            })?,
            #[cfg(unix)]
            executable: metadata.permissions().mode() & 0o111 != 0,
            #[cfg(not(unix))]
            executable: false,
        };
        return Ok(canonical_content_sha256(&[file]));
    }
    let mut files = scan_installed_payload_with_limits(
        path,
        MAX_BUNDLE_FILES,
        MAX_BUNDLE_TOTAL_BYTES,
        include_marker,
    )?;
    let prefix = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    for file in &mut files {
        file.path = PathBuf::from(prefix).join(&file.path);
    }
    Ok(canonical_content_sha256(&files))
}

fn top_level_paths(files: &[PackageFile]) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| file.path.components().next())
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn bundle_state_root(data_dir: &Path, org: &str) -> Result<PathBuf, InstallError> {
    if data_dir.file_name().and_then(|value| value.to_str()) != Some(".claude-science") {
        return Err(error(
            "SCIENCE_DATA_DIR_INVALID",
            "Science data-dir 不是 CSSwitch 管理路径",
            "commit",
        ));
    }
    let home = data_dir.parent().ok_or_else(|| {
        error(
            "SCIENCE_DATA_DIR_INVALID",
            "Science data-dir 缺少 sandbox HOME",
            "commit",
        )
    })?;
    let sandbox = home.parent().ok_or_else(|| {
        error(
            "SCIENCE_DATA_DIR_INVALID",
            "Science data-dir 缺少 sandbox 根目录",
            "commit",
        )
    })?;
    let root = sandbox.join("skill-bundles").join(org);
    reject_symlink_path(&root)?;
    Ok(root)
}

fn read_manifest_optional(path: &Path) -> Result<Option<BundleManifest>, InstallError> {
    reject_symlink_path(path)?;
    match fs::read(path) {
        Ok(body) => {
            let manifest: BundleManifest = serde_json::from_slice(&body)
                .map_err(|_| error("BUNDLE_STATE_INVALID", "bundle manifest 非法", "recovery"))?;
            validate_manifest(&manifest)?;
            Ok(Some(manifest))
        }
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(error(
            "BUNDLE_STATE_INVALID",
            "无法读取 bundle manifest",
            "recovery",
        )),
    }
}

fn validate_manifest(manifest: &BundleManifest) -> Result<(), InstallError> {
    let safe_top_level = |value: &str| {
        !value.is_empty() && !matches!(value, "." | "..") && !value.contains(['/', '\\', '\0'])
    };
    let valid_sha = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    let valid = manifest.schema_version == 1
        && valid_bundle_id(&manifest.bundle_id)
        && !manifest.bundle_name.is_empty()
        && manifest.bundle_name.len() <= 120
        && matches!(manifest.source_kind.as_str(), "github" | "local_zip")
        && manifest.source_sha.len() == 40
        && manifest
            .source_sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && valid_sha(&manifest.bundle_content_sha256)
        && manifest.archive_sha256.as_deref().is_none_or(valid_sha)
        && manifest.state == "installed"
        && !manifest.skill_names.is_empty()
        && manifest
            .skill_names
            .iter()
            .all(|name| crate::archive::validate_skill_name(name).is_ok())
        && manifest
            .support_paths
            .iter()
            .all(|path| safe_top_level(path))
        && manifest
            .top_level_paths
            .iter()
            .all(|path| safe_top_level(path))
        && manifest.member_hashes.values().all(|hash| valid_sha(hash))
        && manifest.path_hashes.iter().all(|(path, hash)| {
            safe_top_level(path) && valid_sha(hash) && manifest.top_level_paths.contains(path)
        });
    if !valid {
        return Err(error(
            "BUNDLE_STATE_INVALID",
            "bundle manifest 字段或摘要非法",
            "recovery",
        ));
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<BundleManifest, InstallError> {
    read_manifest_optional(path)?
        .ok_or_else(|| error("BUNDLE_STATE_INVALID", "bundle manifest 不存在", "recovery"))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), InstallError> {
    reject_symlink_path(path)?;
    let body = serde_json::to_vec_pretty(value).map_err(|_| {
        error(
            "BUNDLE_STATE_WRITE_FAILED",
            "编码 bundle 状态失败",
            "commit",
        )
    })?;
    let temporary = path.with_extension(format!("tmp-{}", unique_suffix()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| {
        error(
            "BUNDLE_STATE_WRITE_FAILED",
            "创建 bundle 状态文件失败",
            "commit",
        )
    })?;
    file.write_all(&body)
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            error(
                "BUNDLE_STATE_WRITE_FAILED",
                "写入 bundle 状态失败",
                "commit",
            )
        })?;
    fs::rename(&temporary, path).map_err(|_| {
        error(
            "BUNDLE_STATE_WRITE_FAILED",
            "提交 bundle 状态失败",
            "commit",
        )
    })?;
    sync_directory(path.parent().expect("state parent"))
}

fn recover_journal(skills: &Path, path: &Path) -> Result<(), InstallError> {
    reject_symlink_path(path)?;
    let Ok(body) = fs::read(path) else {
        return Ok(());
    };
    let journal: BundleJournal = serde_json::from_slice(&body).map_err(|_| {
        error(
            "BUNDLE_STATE_INVALID",
            "bundle transaction journal 非法",
            "recovery",
        )
    })?;
    if journal.state == "committed" {
        let _ = fs::remove_dir_all(&journal.backup);
        let _ = fs::remove_dir_all(&journal.staging);
        let _ = fs::remove_file(path);
        return Ok(());
    }
    rollback_journal(skills, &journal)?;
    let _ = fs::remove_file(path);
    Ok(())
}

fn rollback_journal(skills: &Path, journal: &BundleJournal) -> Result<(), InstallError> {
    for name in journal.installed.iter().rev() {
        let target = skills.join(name);
        if fs::symlink_metadata(&target).is_ok() {
            fs::remove_dir_all(&target)
                .or_else(|_| fs::remove_file(&target))
                .map_err(|_| {
                    error(
                        "BUNDLE_RECOVERY_FAILED",
                        "无法回滚新 bundle 路径",
                        "recovery",
                    )
                })?;
        }
    }
    for name in journal.moved_old.iter().rev() {
        let backup = journal.backup.join(name);
        if fs::symlink_metadata(&backup).is_ok() {
            fs::rename(&backup, skills.join(name)).map_err(|_| {
                error(
                    "BUNDLE_RECOVERY_FAILED",
                    "无法恢复旧 bundle 路径",
                    "recovery",
                )
            })?;
        }
    }
    let _ = fs::remove_dir_all(&journal.staging);
    let _ = fs::remove_dir_all(&journal.backup);
    sync_directory(skills)
}

fn lock_component(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..24].to_string()
}

fn valid_bundle_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn active_org_changed() -> InstallError {
    error(
        "ACTIVE_ORG_CHANGED",
        "Science active org 在 bundle 操作期间发生变化",
        "commit",
    )
    .retryable(true)
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_data_dir(label: &str) -> PathBuf {
        let root = crate::test_temp_root().join(format!(
            "csswitch-bundle-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let data = root.join("sandbox/home/.claude-science");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("active-org.json"), br#"{"org_uuid":"org-test"}"#).unwrap();
        data
    }

    fn validated_bundle(shared: &[u8]) -> ValidatedBundle {
        let files = vec![
            PackageFile {
                path: PathBuf::from("demo/SKILL.md"),
                content: b"# Demo".to_vec(),
                executable: false,
            },
            PackageFile {
                path: PathBuf::from("_shared/helper.py"),
                content: shared.to_vec(),
                executable: false,
            },
        ];
        let member_files = vec![PackageFile {
            path: PathBuf::from("SKILL.md"),
            content: b"# Demo".to_vec(),
            executable: false,
        }];
        ValidatedBundle {
            bundle_name: "demo-bundle".to_string(),
            collection_path: "skills".to_string(),
            members: vec![BundleMember {
                skill_name: "demo".to_string(),
                member_path: PathBuf::from("demo"),
                content_sha256: canonical_content_sha256(&member_files),
            }],
            support_paths: vec!["_shared".to_string()],
            content_sha256: canonical_content_sha256(&files),
            files,
        }
    }

    fn source(sha: char) -> SourceDescriptor {
        SourceDescriptor {
            kind: SourceKind::Github,
            repo: "owner/repo".to_string(),
            sha: std::iter::repeat_n(sha, 40).collect(),
            path: "skills".to_string(),
            archive_sha256: None,
        }
    }

    #[test]
    fn bundle_commit_is_idempotent_updates_atomically_and_detects_tamper() {
        let data = test_data_dir("lifecycle");
        let id = bundle_id_for_github("owner/repo", "skills");
        let first = install_validated_bundle(
            &data,
            validated_bundle(b"v1"),
            source('a'),
            "org-test",
            id.clone(),
        )
        .unwrap();
        assert!(first.directory_commit);
        assert!(data
            .join("orgs/org-test/skills/demo/.import-origin")
            .is_file());
        assert_eq!(
            fs::read(data.join("orgs/org-test/skills/_shared/helper.py")).unwrap(),
            b"v1"
        );
        let repeated = install_validated_bundle(
            &data,
            validated_bundle(b"v1"),
            source('a'),
            "org-test",
            id.clone(),
        )
        .unwrap();
        assert_eq!(repeated.action, InstallAction::ReusedVerified);

        let updated = install_validated_bundle(
            &data,
            validated_bundle(b"v2"),
            source('b'),
            "org-test",
            id.clone(),
        )
        .unwrap();
        assert!(updated.directory_commit);
        assert_eq!(
            fs::read(data.join("orgs/org-test/skills/_shared/helper.py")).unwrap(),
            b"v2"
        );

        fs::write(
            data.join("orgs/org-test/skills/_shared/helper.py"),
            b"tampered",
        )
        .unwrap();
        let error =
            install_validated_bundle(&data, validated_bundle(b"v2"), source('b'), "org-test", id)
                .unwrap_err();
        assert_eq!(error.code, "INSTALLED_CONTENT_CHANGED");
        fs::remove_dir_all(data.parent().unwrap().parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn uninstall_from_any_member_quarantines_members_support_and_manifest() {
        let data = test_data_dir("uninstall");
        let id = bundle_id_for_github("owner/repo", "skills");
        install_validated_bundle(
            &data,
            validated_bundle(b"shared"),
            source('a'),
            "org-test",
            id.clone(),
        )
        .unwrap();
        let uninstall = find_bundle_for_skill(&data, "demo").unwrap().unwrap();
        assert_eq!(uninstall.bundle_id, id);
        let result = quarantine_bundle(&data, &uninstall).unwrap();
        assert_eq!(result.skill_names, vec!["demo"]);
        let skills = data.join("orgs/org-test/skills");
        assert!(!skills.join("demo").exists());
        assert!(!skills.join("_shared").exists());
        assert!(result.quarantined_path.join("demo/SKILL.md").is_file());
        assert!(result.quarantined_path.join("_shared/helper.py").is_file());
        assert!(result
            .quarantined_path
            .join("bundle-manifest.json")
            .is_file());
        fs::remove_dir_all(data.parent().unwrap().parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_bundle_move_rolls_back_every_previously_moved_path() {
        let root = test_data_dir("uninstall-rollback")
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let skills = root.join("skills");
        let quarantine = root.join("quarantine");
        fs::create_dir_all(skills.join("first")).unwrap();
        fs::create_dir(&quarantine).unwrap();
        fs::write(skills.join("first/payload"), b"intact").unwrap();

        let error = move_bundle_paths_transactional(
            &skills,
            &quarantine,
            &["first".into(), "missing".into()],
        )
        .unwrap_err();

        assert_eq!(error.code, "UNINSTALL_FAILED");
        assert_eq!(fs::read(skills.join("first/payload")).unwrap(), b"intact");
        assert!(!quarantine.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_update_journal_restores_old_paths_before_retry() {
        let data = test_data_dir("journal-recovery");
        let id = bundle_id_for_github("owner/repo", "skills");
        let bundle = validated_bundle(b"shared");
        install_validated_bundle(&data, bundle.clone(), source('a'), "org-test", id.clone())
            .unwrap();
        let skills = data.join("orgs/org-test/skills");
        let state = bundle_state_root(&data, "org-test").unwrap();
        let backup = state.join("simulated-backup");
        let staging = state.join("simulated-staging");
        create_private_staging(&backup).unwrap();
        create_private_staging(&staging).unwrap();
        fs::rename(skills.join("demo"), backup.join("demo")).unwrap();
        let journal = BundleJournal {
            schema_version: 1,
            bundle_id: id.clone(),
            state: "committing".to_string(),
            staging,
            backup,
            installed: Vec::new(),
            moved_old: vec!["demo".to_string()],
        };
        write_json_atomic(&state.join(format!("{id}.journal.json")), &journal).unwrap();
        assert!(!skills.join("demo").exists());

        let repeated =
            install_validated_bundle(&data, bundle, source('a'), "org-test", id).unwrap();
        assert_eq!(repeated.action, InstallAction::ReusedVerified);
        assert!(skills.join("demo/SKILL.md").is_file());
        fs::remove_dir_all(data.parent().unwrap().parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn fixed_github_bundle_retry_recovers_from_manifest_without_archive() {
        let data = test_data_dir("github-recovery");
        let id = bundle_id_for_github("owner/repo", "skills");
        install_validated_bundle(
            &data,
            validated_bundle(b"shared"),
            source('a'),
            "org-test",
            id.clone(),
        )
        .unwrap();
        let recovered =
            recover_existing_github_bundle(&data, "org-test", "owner/repo", &"a".repeat(40), "")
                .unwrap()
                .unwrap();
        assert_eq!(recovered.bundle_id, id);
        assert_eq!(recovered.action, InstallAction::ReusedVerified);
        assert!(!recovered.directory_commit);
        assert_eq!(recovered.skill_names, vec!["demo"]);
        fs::remove_dir_all(data.parent().unwrap().parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn fixed_github_bundle_retry_recovers_interrupted_journal_before_manifest_verify() {
        let data = test_data_dir("github-journal-recovery");
        let id = bundle_id_for_github("owner/repo", "skills");
        install_validated_bundle(
            &data,
            validated_bundle(b"shared"),
            source('a'),
            "org-test",
            id.clone(),
        )
        .unwrap();
        let skills = data.join("orgs/org-test/skills");
        let state = bundle_state_root(&data, "org-test").unwrap();
        let backup = state.join("simulated-github-backup");
        let staging = state.join("simulated-github-staging");
        create_private_staging(&backup).unwrap();
        create_private_staging(&staging).unwrap();
        fs::rename(skills.join("demo"), backup.join("demo")).unwrap();
        write_json_atomic(
            &state.join(format!("{id}.journal.json")),
            &BundleJournal {
                schema_version: 1,
                bundle_id: id.clone(),
                state: "committing".to_string(),
                staging,
                backup,
                installed: Vec::new(),
                moved_old: vec!["demo".to_string()],
            },
        )
        .unwrap();

        let recovered =
            recover_existing_github_bundle(&data, "org-test", "owner/repo", &"a".repeat(40), "")
                .unwrap()
                .unwrap();

        assert_eq!(recovered.action, InstallAction::ReusedVerified);
        assert!(skills.join("demo/SKILL.md").is_file());
        assert!(!state.join(format!("{id}.journal.json")).exists());
        fs::remove_dir_all(data.parent().unwrap().parent().unwrap().parent().unwrap()).unwrap();
    }
}
