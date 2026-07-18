use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};
use unicode_normalization::UnicodeNormalization;

use crate::archive::{
    archive_sha256, canonical_content_sha256, package_or_bundle_from_local_archive,
    read_archive_file,
};
use crate::bundle::{bundle_id_for_local, install_validated_bundle, BundleCommit};
use crate::{
    error, InstallError, PackageFile, SourceKind, ValidatedArchive, ValidatedPackage,
    CATALOG_STAMP_FILE, CSSWITCH_MARKETPLACE, IMPORT_ORIGIN_FILE, MAX_FILES, MAX_FILE_BYTES,
    MAX_IMPORT_ORIGIN_BYTES, MAX_PATH_BYTES, MAX_PATH_DEPTH, MAX_TOTAL_BYTES,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallAction {
    Committed,
    ReusedVerified,
    LegacyMarkerUpgraded,
}

impl InstallAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Committed => "COMMITTED",
            Self::ReusedVerified => "REUSED_VERIFIED",
            Self::LegacyMarkerUpgraded => "LEGACY_MARKER_UPGRADED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstallCommit {
    pub skill_name: String,
    pub source_kind: SourceKind,
    pub active_org: String,
    pub content_sha256: String,
    pub source_digest_sha256: Option<String>,
    pub resolved_commit_sha: Option<String>,
    pub source_repo: String,
    pub source_path: String,
    pub dependency_scan: &'static str,
    pub action: InstallAction,
    pub directory_commit: bool,
}

#[derive(Clone, Debug)]
pub enum InstalledPackage {
    Skill(InstallCommit),
    Bundle(BundleCommit),
}

#[derive(Debug)]
pub struct LocalArchiveInput<'a> {
    pub file: &'a mut File,
    pub archive_name: &'a str,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceDescriptor {
    pub kind: SourceKind,
    pub repo: String,
    pub sha: String,
    pub path: String,
    pub archive_sha256: Option<String>,
}

pub fn install_local_skill(
    data_dir: &Path,
    input: LocalArchiveInput<'_>,
) -> Result<InstallCommit, InstallError> {
    match install_local_package(data_dir, input)? {
        InstalledPackage::Skill(commit) => Ok(commit),
        InstalledPackage::Bundle(_) => Err(error(
            "BUNDLE_STRUCTURE_UNSUPPORTED",
            "该 archive 是 bundle，请使用 bundle-aware 安装入口",
            "archive",
        )),
    }
}

pub fn install_local_package(
    data_dir: &Path,
    input: LocalArchiveInput<'_>,
) -> Result<InstalledPackage, InstallError> {
    let initial_org = active_org(data_dir)?;
    let extension = Path::new(input.archive_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "zip" | "skill") {
        return Err(error(
            "UNSUPPORTED_ARCHIVE_EXTENSION",
            "只支持 .zip 或 .skill 文件",
            "archive",
        ));
    }
    let stem = Path::new(input.archive_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| error("INVALID_SKILL_NAME", "archive 文件名不是 UTF-8", "archive"))?;
    let bytes = read_archive_file(input.file)?;
    let digest = archive_sha256(&bytes);
    match package_or_bundle_from_local_archive(&bytes, stem)? {
        ValidatedArchive::Skill(package) => {
            let descriptor = SourceDescriptor {
                kind: SourceKind::LocalZip,
                repo: "csswitch/local-archive".to_string(),
                sha: digest[..40].to_string(),
                path: package.skill_name.clone(),
                archive_sha256: Some(digest.clone()),
            };
            let mut commit = commit_package(data_dir, package, descriptor, &initial_org)?;
            commit.source_digest_sha256 = Some(digest);
            Ok(InstalledPackage::Skill(commit))
        }
        ValidatedArchive::Bundle(bundle) => {
            let descriptor = SourceDescriptor {
                kind: SourceKind::LocalZip,
                repo: "csswitch/local-archive".to_string(),
                sha: digest[..40].to_string(),
                path: if bundle.collection_path.is_empty() {
                    bundle.bundle_name.clone()
                } else {
                    bundle.collection_path.clone()
                },
                archive_sha256: Some(digest.clone()),
            };
            let bundle_id = bundle_id_for_local(&bundle.bundle_name, &bundle.collection_path);
            install_validated_bundle(data_dir, bundle, descriptor, &initial_org, bundle_id)
                .map(InstalledPackage::Bundle)
        }
    }
}

pub(crate) enum ExistingGithubInstall {
    Missing,
    Verified(InstallCommit),
    Legacy(SourceDescriptor),
}

pub(crate) fn inspect_existing_github_install(
    data_dir: &Path,
    expected_org: &str,
    skill_name: &str,
    source_repo: &str,
    source_path: &str,
    requested_commit: Option<&str>,
) -> Result<ExistingGithubInstall, InstallError> {
    let org = active_org(data_dir)?;
    if org != expected_org {
        return Err(active_org_changed());
    }
    let target = skills_root(data_dir, &org)?.join(skill_name);
    if fs::symlink_metadata(&target).is_err() {
        return Ok(ExistingGithubInstall::Missing);
    }
    reject_symlink_path(&target)?;
    let marker = read_and_validate_marker(&target, skill_name)?;
    let marker_repo = marker.get("repo").and_then(Value::as_str).unwrap_or("");
    let marker_sha = marker.get("sha").and_then(Value::as_str).unwrap_or("");
    let marker_path = marker.get("path").and_then(Value::as_str).unwrap_or("");
    let marker_kind = marker.get("source_kind").and_then(Value::as_str);
    if marker_repo != source_repo
        || marker_path != source_path
        || marker_kind.is_some_and(|kind| kind != SourceKind::Github.as_str())
        || requested_commit.is_some_and(|requested| requested != marker_sha)
    {
        return Err(error(
            "SKILL_NAME_CONFLICT",
            format!("Skill '{skill_name}' 已由其他来源占用"),
            "recovery",
        ));
    }
    let descriptor = SourceDescriptor {
        kind: SourceKind::Github,
        repo: marker_repo.to_string(),
        sha: marker_sha.to_string(),
        path: marker_path.to_string(),
        archive_sha256: None,
    };
    let Some(expected_hash) = marker.get("content_sha256").and_then(Value::as_str) else {
        return Ok(ExistingGithubInstall::Legacy(descriptor));
    };
    let files = scan_installed_payload(&target)?;
    let actual_hash = canonical_content_sha256(&files);
    if expected_hash != actual_hash {
        return Err(error(
            "INSTALLED_CONTENT_CHANGED",
            format!("Skill '{skill_name}' 的已安装内容与 marker 不一致"),
            "recovery",
        ));
    }
    Ok(ExistingGithubInstall::Verified(InstallCommit {
        skill_name: skill_name.to_string(),
        source_kind: SourceKind::Github,
        active_org: org,
        content_sha256: actual_hash,
        source_digest_sha256: None,
        resolved_commit_sha: Some(descriptor.sha.clone()),
        source_repo: descriptor.repo,
        source_path: descriptor.path,
        dependency_scan: "BEST_EFFORT",
        action: InstallAction::ReusedVerified,
        directory_commit: false,
    }))
}

pub(crate) fn commit_package(
    data_dir: &Path,
    package: ValidatedPackage,
    descriptor: SourceDescriptor,
    initial_org: &str,
) -> Result<InstallCommit, InstallError> {
    if active_org(data_dir)? != initial_org {
        return Err(active_org_changed());
    }
    let skills_root = skills_root(data_dir, initial_org)?;
    fs::create_dir_all(&skills_root).map_err(|_| {
        error(
            "SKILLS_ROOT_CREATE_FAILED",
            "创建 Science Skills 目录失败",
            "commit",
        )
    })?;
    reject_symlink_path(&skills_root)?;
    let target = skills_root.join(&package.skill_name);
    let lock_path = skills_root.join(format!(".csswitch-install-{}.lock", package.skill_name));
    let _lock = acquire_lock(&lock_path)?;
    if fs::symlink_metadata(&target).is_ok() {
        let marker = read_and_validate_marker(&target, &package.skill_name)?;
        let legacy_marker = marker.get("content_sha256").is_none();
        if !marker_matches_source(&marker, &descriptor) {
            return Err(error(
                "SKILL_NAME_CONFLICT",
                format!("Skill '{}' 已由其他来源占用", package.skill_name),
                "recovery",
            ));
        }
        let installed = scan_installed_payload(&target)?;
        let installed_hash = canonical_content_sha256(&installed);
        if installed_hash != package.content_sha256 {
            return Err(
                if legacy_marker && matches!(descriptor.kind, SourceKind::Github) {
                    error(
                        "LEGACY_INTEGRITY_UNVERIFIED",
                        format!(
                            "Skill '{}' 的旧 marker 内容无法与固定 GitHub commit 对齐",
                            package.skill_name
                        ),
                        "recovery",
                    )
                } else {
                    error(
                        "INSTALLED_CONTENT_CHANGED",
                        format!("Skill '{}' 的已安装内容已变化", package.skill_name),
                        "recovery",
                    )
                },
            );
        }
        let action = if marker.get("content_sha256").and_then(Value::as_str)
            == Some(package.content_sha256.as_str())
        {
            InstallAction::ReusedVerified
        } else if matches!(descriptor.kind, SourceKind::Github) {
            write_marker_atomic(&target, &package, &descriptor, marker.get("importedAt"))?;
            InstallAction::LegacyMarkerUpgraded
        } else {
            return Err(error(
                "LEGACY_INTEGRITY_UNVERIFIED",
                "本地 archive marker 缺少完整内容摘要",
                "recovery",
            ));
        };
        return Ok(commit_result(
            &package,
            &descriptor,
            initial_org.to_string(),
            action,
            false,
        ));
    }
    let temp = skills_root.join(format!(
        ".csswitch-install-{}-{}-{}",
        package.skill_name,
        std::process::id(),
        unique_suffix()
    ));
    create_private_staging(&temp)?;
    let staged = (|| -> Result<(), InstallError> {
        for file in &package.files {
            let destination = temp.join(&file.path);
            if let Some(parent) = destination.parent() {
                create_private_directories(&temp, parent)?;
            }
            write_new_file(&destination, &file.content, file.executable)?;
        }
        write_marker(&temp, &package, &descriptor, None)?;
        sync_tree(&temp)?;
        if active_org(data_dir)? != initial_org {
            return Err(active_org_changed());
        }
        rename_no_replace(&temp, &target)?;
        sync_directory(&skills_root)?;
        Ok(())
    })();
    if staged.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    staged?;
    Ok(commit_result(
        &package,
        &descriptor,
        initial_org.to_string(),
        InstallAction::Committed,
        true,
    ))
}

fn active_org_changed() -> InstallError {
    error(
        "ACTIVE_ORG_CHANGED",
        "Science active org 在安装期间发生变化",
        "commit",
    )
    .retryable(true)
}

fn commit_result(
    package: &ValidatedPackage,
    descriptor: &SourceDescriptor,
    active_org: String,
    action: InstallAction,
    directory_commit: bool,
) -> InstallCommit {
    InstallCommit {
        skill_name: package.skill_name.clone(),
        source_kind: descriptor.kind.clone(),
        active_org,
        content_sha256: package.content_sha256.clone(),
        source_digest_sha256: descriptor.archive_sha256.clone(),
        resolved_commit_sha: matches!(descriptor.kind, SourceKind::Github)
            .then(|| descriptor.sha.clone()),
        source_repo: descriptor.repo.clone(),
        source_path: descriptor.path.clone(),
        dependency_scan: "BEST_EFFORT",
        action,
        directory_commit,
    }
}

fn marker_matches_source(marker: &Value, source: &SourceDescriptor) -> bool {
    marker.get("repo").and_then(Value::as_str) == Some(source.repo.as_str())
        && marker.get("sha").and_then(Value::as_str) == Some(source.sha.as_str())
        && marker.get("path").and_then(Value::as_str) == Some(source.path.as_str())
        && marker
            .get("source_kind")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == source.kind.as_str())
        && source.archive_sha256.as_ref().is_none_or(|expected| {
            marker.get("archive_sha256").and_then(Value::as_str) == Some(expected.as_str())
        })
}

fn write_marker_atomic(
    skill_dir: &Path,
    package: &ValidatedPackage,
    descriptor: &SourceDescriptor,
    imported_at: Option<&Value>,
) -> Result<(), InstallError> {
    let marker_path = skill_dir.join(IMPORT_ORIGIN_FILE);
    let temporary = skill_dir.join(format!(".{IMPORT_ORIGIN_FILE}.{}", unique_suffix()));
    write_marker_to_path(&temporary, package, descriptor, imported_at)?;
    fs::rename(&temporary, &marker_path).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        error("MARKER_WRITE_FAILED", "升级 Skill marker 失败", "recovery")
    })?;
    sync_directory(skill_dir)
}

fn write_marker(
    skill_dir: &Path,
    package: &ValidatedPackage,
    descriptor: &SourceDescriptor,
    imported_at: Option<&Value>,
) -> Result<(), InstallError> {
    write_marker_to_path(
        &skill_dir.join(IMPORT_ORIGIN_FILE),
        package,
        descriptor,
        imported_at,
    )
}

fn write_marker_to_path(
    path: &Path,
    package: &ValidatedPackage,
    descriptor: &SourceDescriptor,
    imported_at: Option<&Value>,
) -> Result<(), InstallError> {
    let mut marker = json!({
        "version": 1,
        "repo": descriptor.repo,
        "sha": descriptor.sha,
        "plugin": package.skill_name,
        "marketplace": CSSWITCH_MARKETPLACE,
        "path": descriptor.path,
        "importedAt": imported_at.and_then(Value::as_str).map(str::to_owned).unwrap_or_else(rfc3339_now),
        "license": "NOASSERTION",
        "csswitch_revision": 2,
        "source_kind": descriptor.kind.as_str(),
        "content_sha256": package.content_sha256,
    });
    if let Some(archive_sha256) = &descriptor.archive_sha256 {
        marker["archive_sha256"] = Value::String(archive_sha256.clone());
    }
    let mut body = serde_json::to_vec(&marker)
        .map_err(|_| error("MARKER_WRITE_FAILED", "编码 Skill marker 失败", "commit"))?;
    body.push(b'\n');
    if body.len() > MAX_IMPORT_ORIGIN_BYTES {
        return Err(error(
            "MARKER_WRITE_FAILED",
            "Skill marker 超过大小限制",
            "commit",
        ));
    }
    write_new_file(path, &body, false)
}

pub fn verify_csswitch_import_origin(
    skill_dir: &Path,
    skill_name: &str,
) -> Result<Value, InstallError> {
    read_and_validate_marker(skill_dir, skill_name)
}

fn read_and_validate_marker(skill_dir: &Path, skill_name: &str) -> Result<Value, InstallError> {
    reject_symlink_path(skill_dir)?;
    let marker_path = skill_dir.join(IMPORT_ORIGIN_FILE);
    reject_symlink_path(&marker_path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut marker_file = options.open(&marker_path).map_err(|_| {
        error(
            "SKILL_NAME_CONFLICT",
            format!("Skill '{skill_name}' 没有 CSSwitch marker"),
            "recovery",
        )
    })?;
    let metadata = marker_file
        .metadata()
        .map_err(|_| error("INVALID_IMPORT_ORIGIN", "无法检查 Skill marker", "recovery"))?;
    if !metadata.is_file() || metadata.len() as usize > MAX_IMPORT_ORIGIN_BYTES {
        return Err(error(
            "INVALID_IMPORT_ORIGIN",
            "Skill marker 不是受支持的普通文件",
            "recovery",
        ));
    }
    let mut body = Vec::new();
    Read::by_ref(&mut marker_file)
        .take((MAX_IMPORT_ORIGIN_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| {
            error(
                "INVALID_IMPORT_ORIGIN",
                "读取 Skill marker 失败",
                "recovery",
            )
        })?;
    if body.len() > MAX_IMPORT_ORIGIN_BYTES {
        return Err(error(
            "INVALID_IMPORT_ORIGIN",
            "Skill marker 超过大小限制",
            "recovery",
        ));
    }
    let marker: Value = serde_json::from_slice(&body).map_err(|_| {
        error(
            "INVALID_IMPORT_ORIGIN",
            "Skill marker JSON 非法",
            "recovery",
        )
    })?;
    let repo = marker.get("repo").and_then(Value::as_str).unwrap_or("");
    let sha = marker.get("sha").and_then(Value::as_str).unwrap_or("");
    let plugin = marker.get("plugin").and_then(Value::as_str).unwrap_or("");
    let marketplace = marker
        .get("marketplace")
        .and_then(Value::as_str)
        .unwrap_or("");
    let path = marker.get("path").and_then(Value::as_str).unwrap_or("");
    let imported_at = marker
        .get("importedAt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let license = marker.get("license").and_then(Value::as_str).unwrap_or("");
    let repo_valid = repo.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && owner.len() <= 100
            && !matches!(owner, "." | "..")
            && !name.is_empty()
            && name.len() <= 100
            && !matches!(name, "." | "..")
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            && !name.contains('/')
    });
    let valid = marker.get("version").and_then(Value::as_u64) == Some(1)
        && repo_valid
        && sha.len() == 40
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && plugin == skill_name
        && marketplace == CSSWITCH_MARKETPLACE
        && !path.is_empty()
        && path.len() <= 500
        && path.split('/').all(safe_component)
        && !imported_at.is_empty()
        && imported_at.len() <= 100
        && !license.is_empty()
        && license.len() <= 100
        && marker
            .get("csswitch_revision")
            .and_then(Value::as_u64)
            .is_none_or(|revision| revision == 2)
        && marker
            .get("source_kind")
            .and_then(Value::as_str)
            .is_none_or(|kind| matches!(kind, "github" | "local_zip"))
        && marker
            .get("content_sha256")
            .and_then(Value::as_str)
            .is_none_or(valid_sha256)
        && marker
            .get("archive_sha256")
            .and_then(Value::as_str)
            .is_none_or(valid_sha256)
        && marker
            .get("bundle_id")
            .and_then(Value::as_str)
            .is_none_or(valid_sha256)
        && marker
            .get("bundle_content_sha256")
            .and_then(Value::as_str)
            .is_none_or(valid_sha256)
        && marker
            .get("bundle_name")
            .and_then(Value::as_str)
            .is_none_or(|name| !name.is_empty() && name.len() <= 120)
        && marker
            .get("bundle_member_path")
            .and_then(Value::as_str)
            .is_none_or(|path| {
                !path.is_empty() && path.len() <= 500 && path.split('/').all(safe_component)
            });
    if !valid {
        return Err(error(
            "INVALID_IMPORT_ORIGIN",
            format!("Skill '{skill_name}' 不是可验证的 CSSwitch 导入"),
            "recovery",
        ));
    }
    Ok(marker)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn register_installed_path(
    collision_keys: &mut BTreeMap<String, (PathBuf, bool)>,
    relative: &Path,
    is_file: bool,
) -> Result<(), InstallError> {
    let relative_text = relative.to_str().ok_or_else(|| {
        error(
            "INSTALLED_CONTENT_CHANGED",
            "已安装 Skill 含非 UTF-8 路径",
            "recovery",
        )
    })?;
    let key = relative_text.nfc().collect::<String>().to_lowercase();
    if let Some((previous, previous_is_file)) = collision_keys.get(&key) {
        if previous != relative || *previous_is_file != is_file {
            return Err(error(
                "INSTALLED_CONTENT_CHANGED",
                "已安装 Skill 含大小写、Unicode 或类型冲突路径",
                "recovery",
            ));
        }
    } else {
        collision_keys.insert(key, (relative.to_path_buf(), is_file));
    }
    Ok(())
}

fn scan_installed_payload(root: &Path) -> Result<Vec<PackageFile>, InstallError> {
    scan_installed_payload_with_limits(root, MAX_FILES, MAX_TOTAL_BYTES, false)
}

pub(crate) fn scan_installed_payload_with_limits(
    root: &Path,
    max_files: usize,
    max_total_bytes: usize,
    include_marker: bool,
) -> Result<Vec<PackageFile>, InstallError> {
    let mut pending = vec![(root.to_path_buf(), PathBuf::new())];
    let mut files = Vec::new();
    let mut total = 0usize;
    let mut collision_keys = BTreeMap::new();
    while let Some((directory, relative_dir)) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|_| {
            error(
                "INSTALLED_CONTENT_CHANGED",
                "读取已安装 Skill 失败",
                "recovery",
            )
        })? {
            let entry = entry.map_err(|_| {
                error(
                    "INSTALLED_CONTENT_CHANGED",
                    "读取已安装 Skill 失败",
                    "recovery",
                )
            })?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含非 UTF-8 路径",
                    "recovery",
                )
            })?;
            let relative = relative_dir.join(name);
            if relative == Path::new(IMPORT_ORIGIN_FILE) && !include_marker {
                continue;
            }
            if relative == Path::new(CATALOG_STAMP_FILE) {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含不允许的 catalog stamp",
                    "recovery",
                ));
            }
            let file_type = entry.file_type().map_err(|_| {
                error(
                    "INSTALLED_CONTENT_CHANGED",
                    "读取已安装 Skill 失败",
                    "recovery",
                )
            })?;
            if file_type.is_symlink() {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含符号链接",
                    "recovery",
                ));
            }
            let metadata = entry.metadata().map_err(|_| {
                error(
                    "INSTALLED_CONTENT_CHANGED",
                    "读取已安装 Skill 失败",
                    "recovery",
                )
            })?;
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含特殊文件",
                    "recovery",
                ));
            }
            if relative.as_os_str().as_bytes().len() > MAX_PATH_BYTES
                || relative.components().count() > MAX_PATH_DEPTH
            {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 路径超限",
                    "recovery",
                ));
            }
            let relative_text = relative.to_str().ok_or_else(|| {
                error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含非 UTF-8 路径",
                    "recovery",
                )
            })?;
            if relative_text.nfc().collect::<String>() != relative_text {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含非 NFC 路径",
                    "recovery",
                ));
            }
            register_installed_path(&mut collision_keys, &relative, metadata.is_file())?;
            if metadata.is_dir() {
                pending.push((entry.path(), relative));
                continue;
            }
            if metadata.len() as usize > MAX_FILE_BYTES {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 含超限文件",
                    "recovery",
                ));
            }
            total = total.saturating_add(metadata.len() as usize);
            if total > max_total_bytes || files.len() >= max_files {
                return Err(error(
                    "INSTALLED_CONTENT_CHANGED",
                    "已安装 Skill 超过大小或文件数限制",
                    "recovery",
                ));
            }
            #[cfg(unix)]
            use std::os::unix::fs::PermissionsExt;
            files.push(PackageFile {
                path: relative,
                content: fs::read(entry.path()).map_err(|_| {
                    error(
                        "INSTALLED_CONTENT_CHANGED",
                        "读取已安装文件失败",
                        "recovery",
                    )
                })?,
                #[cfg(unix)]
                executable: metadata.permissions().mode() & 0o111 != 0,
                #[cfg(not(unix))]
                executable: false,
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

pub fn active_org(data_dir: &Path) -> Result<String, InstallError> {
    const MAX_ACTIVE_ORG_BYTES: usize = 16 * 1024;
    if !data_dir.is_absolute() {
        return Err(error(
            "SCIENCE_DATA_DIR_INVALID",
            "Science data-dir 必须是绝对路径",
            "science_state",
        ));
    }
    reject_symlink_path(data_dir)?;
    let active = data_dir.join("active-org.json");
    reject_symlink_path(&active)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut active_file = options.open(&active).map_err(|_| {
        error(
            "SCIENCE_NOT_READY",
            "读取 Science active-org.json 失败",
            "science_state",
        )
        .retryable(true)
    })?;
    let metadata = active_file.metadata().map_err(|_| {
        error(
            "SCIENCE_NOT_READY",
            "无法检查 Science active-org.json",
            "science_state",
        )
        .retryable(true)
    })?;
    if !metadata.is_file() || metadata.len() as usize > MAX_ACTIVE_ORG_BYTES {
        return Err(error(
            "SCIENCE_NOT_READY",
            "Science active-org.json 超过大小限制或不是普通文件",
            "science_state",
        )
        .retryable(true));
    }
    let mut body = Vec::new();
    Read::by_ref(&mut active_file)
        .take((MAX_ACTIVE_ORG_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| {
            error(
                "SCIENCE_NOT_READY",
                "读取 Science active-org.json 失败",
                "science_state",
            )
            .retryable(true)
        })?;
    if body.len() > MAX_ACTIVE_ORG_BYTES {
        return Err(error(
            "SCIENCE_NOT_READY",
            "Science active-org.json 超过大小限制",
            "science_state",
        )
        .retryable(true));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        error(
            "SCIENCE_NOT_READY",
            "Science active-org.json 非法",
            "science_state",
        )
        .retryable(true)
    })?;
    let org = value
        .get("org_uuid")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            error(
                "SCIENCE_NOT_READY",
                "active-org.json 缺少 org_uuid",
                "science_state",
            )
            .retryable(true)
        })?;
    let valid = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("static regex");
    if !valid.is_match(org) {
        return Err(error(
            "SCIENCE_NOT_READY",
            "active org 标识非法",
            "science_state",
        ));
    }
    Ok(org.to_string())
}

pub(crate) fn skills_root(data_dir: &Path, org: &str) -> Result<PathBuf, InstallError> {
    let orgs = data_dir.join("orgs");
    let root = orgs.join(org).join("skills");
    if root.strip_prefix(&orgs).is_err() {
        return Err(error("UNSAFE_TARGET", "Skills 目标目录越界", "commit"));
    }
    reject_symlink_path(data_dir)?;
    reject_symlink_path(&root)?;
    Ok(root)
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

pub(crate) fn reject_symlink_path(path: &Path) -> Result<(), InstallError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(error("UNSAFE_TARGET", "路径包含符号链接", "filesystem"))
            }
            Ok(_) => {}
            Err(problem) if problem.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(error("UNSAFE_TARGET", "无法检查目标路径", "filesystem")),
        }
    }
    Ok(())
}

pub(crate) fn create_private_directories(root: &Path, target: &Path) -> Result<(), InstallError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| error("UNSAFE_TARGET", "staging 子目录越界", "commit"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(error("UNSAFE_TARGET", "staging 子目录非法", "commit"));
        }
        current.push(component.as_os_str());
        if !current.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&current).map_err(|_| {
                    error("STAGING_WRITE_FAILED", "创建 staging 子目录失败", "commit")
                })?;
            }
            #[cfg(not(unix))]
            fs::create_dir(&current)
                .map_err(|_| error("STAGING_WRITE_FAILED", "创建 staging 子目录失败", "commit"))?;
        }
        reject_symlink_path(&current)?;
    }
    Ok(())
}

pub(crate) fn create_private_staging(path: &Path) -> Result<(), InstallError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .map_err(|_| error("STAGING_CREATE_FAILED", "创建 Skill staging 失败", "commit"))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            error(
                "STAGING_CREATE_FAILED",
                "设置 Skill staging 权限失败",
                "commit",
            )
        })?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)
        .map_err(|_| error("STAGING_CREATE_FAILED", "创建 Skill staging 失败", "commit"))?;
    Ok(())
}

pub(crate) fn write_new_file(
    path: &Path,
    content: &[u8],
    executable: bool,
) -> Result<(), InstallError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .map_err(|_| error("STAGING_WRITE_FAILED", "创建 Skill 文件失败", "commit"))?;
    file.write_all(content)
        .and_then(|_| file.sync_all())
        .map_err(|_| error("STAGING_WRITE_FAILED", "写入 Skill 文件失败", "commit"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )
        .map_err(|_| error("STAGING_WRITE_FAILED", "设置 Skill 权限失败", "commit"))?;
    }
    Ok(())
}

pub(crate) struct InstallLock {
    _file: File,
}

fn acquire_lock(path: &Path) -> Result<InstallLock, InstallError> {
    reject_symlink_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| error("INSTALL_BUSY", "同名 Skill 正在安装", "commit"))?;
    file.try_lock()
        .map_err(|_| error("INSTALL_BUSY", "同名 Skill 正在安装", "commit"))?;
    Ok(InstallLock { _file: file })
}

pub(crate) fn acquire_install_lock(path: &Path) -> Result<InstallLock, InstallError> {
    acquire_lock(path)
}

pub(crate) fn sync_tree(root: &Path) -> Result<(), InstallError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|_| error("DURABILITY_SYNC_FAILED", "读取 staging 失败", "commit"))?
        {
            let path = entry
                .map_err(|_| error("DURABILITY_SYNC_FAILED", "读取 staging 失败", "commit"))?
                .path();
            if path.is_dir() {
                pending.push(path);
            }
        }
        sync_directory(&directory)?;
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<(), InstallError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| error("DURABILITY_SYNC_FAILED", "同步 Skill 目录失败", "commit"))
}

#[cfg(target_os = "macos")]
pub(crate) fn rename_no_replace(source: &Path, target: &Path) -> Result<(), InstallError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn renameatx_np(fromfd: i32, from: *const i8, tofd: i32, to: *const i8, flags: u32) -> i32;
    }
    const AT_FDCWD: i32 = -2;
    const RENAME_EXCL: u32 = 0x0000_0004;
    let from = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| error("UNSAFE_TARGET", "staging 路径非法", "commit"))?;
    let to = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| error("UNSAFE_TARGET", "目标路径非法", "commit"))?;
    if unsafe { renameatx_np(AT_FDCWD, from.as_ptr(), AT_FDCWD, to.as_ptr(), RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(error(
            "SKILL_NAME_CONFLICT",
            "原子提交失败，目标可能已存在",
            "commit",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn rename_no_replace(source: &Path, target: &Path) -> Result<(), InstallError> {
    if target.exists() {
        return Err(error("SKILL_NAME_CONFLICT", "目标 Skill 已存在", "commit"));
    }
    fs::rename(source, target).map_err(|_| error("COMMIT_FAILED", "提交 Skill 目录失败", "commit"))
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(crate) fn rfc3339_now() -> String {
    rfc3339_from_unix(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
}

fn rfc3339_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let second_of_day = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "csswitch-skill-core-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8], u32)]) {
        let file = File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        for (name, content, mode) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default().unix_permissions(*mode))
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn installed_path_collision_check_includes_directories() {
        let mut collisions = BTreeMap::new();
        register_installed_path(&mut collisions, Path::new("Scripts"), false).unwrap();
        let error =
            register_installed_path(&mut collisions, Path::new("scripts"), false).unwrap_err();
        assert_eq!(error.code, "INSTALLED_CONTENT_CHANGED");
    }

    fn data_dir(root: &Path) -> PathBuf {
        let data = root.join("home/.claude-science");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("active-org.json"), br#"{"org_uuid":"org-test"}"#).unwrap();
        data
    }

    #[test]
    fn active_org_rejects_oversized_file_during_bounded_read() {
        let root = test_root("active-org-limit");
        let data = root.join("home/.claude-science");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("active-org.json"), vec![b'x'; 16 * 1024 + 1]).unwrap();
        assert_eq!(active_org(&data).unwrap_err().code, "SCIENCE_NOT_READY");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zip_marker_projection_is_v1_compatible() {
        let package = ValidatedPackage {
            skill_name: "demo".into(),
            files: vec![PackageFile {
                path: PathBuf::from("SKILL.md"),
                content: b"demo".to_vec(),
                executable: false,
            }],
            content_sha256: "a".repeat(64),
        };
        let descriptor = SourceDescriptor {
            kind: SourceKind::LocalZip,
            repo: "csswitch/local-archive".into(),
            sha: "b".repeat(40),
            path: "demo".into(),
            archive_sha256: Some("b".repeat(64)),
        };
        let marker = json!({
            "version": 1,
            "repo": descriptor.repo,
            "sha": descriptor.sha,
            "plugin": package.skill_name,
            "marketplace": CSSWITCH_MARKETPLACE,
            "path": descriptor.path,
            "importedAt": "2026-01-01T00:00:00Z",
            "license": "NOASSERTION"
        });
        assert_eq!(marker["sha"].as_str().unwrap().len(), 40);
        assert_eq!(marker["path"], "demo");
    }

    #[test]
    fn local_install_commits_private_files_and_recovers_idempotently() {
        let root = test_root("local-commit");
        let data = data_dir(&root);
        let archive = root.join("demo.zip");
        write_zip(
            &archive,
            &[
                ("SKILL.md", b"# Demo", 0o644),
                ("scripts/run.sh", b"echo ok", 0o755),
            ],
        );
        let mut first_file = File::open(&archive).unwrap();
        let first = install_local_skill(
            &data,
            LocalArchiveInput {
                file: &mut first_file,
                archive_name: "demo.zip",
            },
        )
        .unwrap();
        assert_eq!(first.action, InstallAction::Committed);
        assert!(first.directory_commit);
        let target = data.join("orgs/org-test/skills/demo");
        let marker = verify_csswitch_import_origin(&target, "demo").unwrap();
        assert_eq!(marker["version"], 1);
        assert_eq!(marker["marketplace"], CSSWITCH_MARKETPLACE);
        assert_eq!(marker["repo"], "csswitch/local-archive");
        assert_eq!(marker["path"], "demo");
        assert_eq!(marker["csswitch_revision"], 2);
        assert_eq!(marker["source_kind"], "local_zip");
        assert_eq!(marker["sha"].as_str().unwrap().len(), 40);
        assert_eq!(marker["archive_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(marker["content_sha256"], first.content_sha256);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(target.join("SKILL.md"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(target.join("scripts/run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        let mut second_file = File::open(&archive).unwrap();
        let second = install_local_skill(
            &data,
            LocalArchiveInput {
                file: &mut second_file,
                archive_name: "demo.zip",
            },
        )
        .unwrap();
        assert_eq!(second.action, InstallAction::ReusedVerified);
        assert!(!second.directory_commit);

        fs::write(target.join("SKILL.md"), b"changed").unwrap();
        let mut changed_file = File::open(&archive).unwrap();
        let error = install_local_skill(
            &data,
            LocalArchiveInput {
                file: &mut changed_file,
                archive_name: "demo.zip",
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "INSTALLED_CONTENT_CHANGED");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_github_marker_upgrades_only_after_content_match() {
        let root = test_root("legacy-marker");
        let data = data_dir(&root);
        let package = ValidatedPackage {
            skill_name: "demo".into(),
            files: vec![PackageFile {
                path: PathBuf::from("SKILL.md"),
                content: b"demo".to_vec(),
                executable: false,
            }],
            content_sha256: canonical_content_sha256(&[PackageFile {
                path: PathBuf::from("SKILL.md"),
                content: b"demo".to_vec(),
                executable: false,
            }]),
        };
        let descriptor = SourceDescriptor {
            kind: SourceKind::Github,
            repo: "owner/repo".into(),
            sha: "a".repeat(40),
            path: "skills/demo".into(),
            archive_sha256: None,
        };
        commit_package(&data, package.clone(), descriptor.clone(), "org-test").unwrap();
        let marker_path = data.join("orgs/org-test/skills/demo/.import-origin");
        let mut marker: Value = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.as_object_mut().unwrap().remove("content_sha256");
        marker.as_object_mut().unwrap().remove("csswitch_revision");
        marker.as_object_mut().unwrap().remove("source_kind");
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();

        fs::write(data.join("orgs/org-test/skills/demo/SKILL.md"), b"changed").unwrap();
        let mismatch =
            commit_package(&data, package.clone(), descriptor.clone(), "org-test").unwrap_err();
        assert_eq!(mismatch.code, "LEGACY_INTEGRITY_UNVERIFIED");

        fs::write(data.join("orgs/org-test/skills/demo/SKILL.md"), b"demo").unwrap();
        let upgraded = commit_package(&data, package, descriptor, "org-test").unwrap();
        assert_eq!(upgraded.action, InstallAction::LegacyMarkerUpgraded);
        let marker = verify_csswitch_import_origin(marker_path.parent().unwrap(), "demo").unwrap();
        assert_eq!(marker["csswitch_revision"], 2);
        assert_eq!(marker["source_kind"], "github");
        assert_eq!(marker["content_sha256"], upgraded.content_sha256);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_refuses_active_org_change_before_staging() {
        let root = test_root("org-change");
        let data = data_dir(&root);
        fs::write(data.join("active-org.json"), br#"{"org_uuid":"org-other"}"#).unwrap();
        let package = ValidatedPackage {
            skill_name: "demo".into(),
            files: vec![PackageFile {
                path: PathBuf::from("SKILL.md"),
                content: b"demo".to_vec(),
                executable: false,
            }],
            content_sha256: canonical_content_sha256(&[PackageFile {
                path: PathBuf::from("SKILL.md"),
                content: b"demo".to_vec(),
                executable: false,
            }]),
        };
        let descriptor = SourceDescriptor {
            kind: SourceKind::Github,
            repo: "owner/repo".into(),
            sha: "a".repeat(40),
            path: "skills/demo".into(),
            archive_sha256: None,
        };
        let error = commit_package(&data, package, descriptor, "org-test").unwrap_err();
        assert_eq!(error.code, "ACTIVE_ORG_CHANGED");
        assert!(!data.join("orgs/org-other/skills/demo").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "explicit local Nature ZIP acceptance; temporary data-dir only"]
    fn local_nature_bundle_archive_smoke() {
        let archive_path = std::env::var_os("CSSWITCH_LOCAL_NATURE_BUNDLE_ARCHIVE")
            .map(PathBuf::from)
            .expect("set CSSWITCH_LOCAL_NATURE_BUNDLE_ARCHIVE");
        let archive_name = archive_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap();
        let root = test_root("local-nature-bundle");
        let data = data_dir(&root);
        let mut file = File::open(&archive_path).unwrap();
        let result = install_local_package(
            &data,
            LocalArchiveInput {
                file: &mut file,
                archive_name,
            },
        )
        .unwrap();
        let InstalledPackage::Bundle(bundle) = result else {
            panic!("expected local Nature bundle");
        };
        assert!(bundle.skill_names.len() > 1);
        assert!(bundle.support_paths.iter().any(|path| path == "_shared"));
        let skills = data.join("orgs/org-test/skills");
        assert!(skills.join("_shared").is_dir());
        for skill_name in &bundle.skill_names {
            assert!(skills.join(skill_name).join("SKILL.md").is_file());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
