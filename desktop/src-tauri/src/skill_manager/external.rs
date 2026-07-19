use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use super::error::{SkillErrorCode, SkillManagerError, SkillResult};
use super::inspection::{AnchoredSourceRoot, InspectionResult};
use super::model::{SkillId, SkillSource};
use super::store::SkillManager;

const MAX_EXTERNAL_SKILLS: usize = 256;
const MAX_EXTERNAL_ROOT_ENTRIES: usize = 4_096;
const MAX_EXTERNAL_SCAN_TOTAL_SIZE: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalSkillScanDiagnostic {
    pub(crate) directory_name: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalSkillScanReport {
    pub(crate) root_present: bool,
    pub(crate) discovered: usize,
    pub(crate) imported: usize,
    pub(crate) updated: usize,
    pub(crate) unchanged: usize,
    pub(crate) retained_missing: usize,
    pub(crate) skill_ids: Vec<SkillId>,
    pub(crate) diagnostics: Vec<ExternalSkillScanDiagnostic>,
}

struct Candidate {
    directory_name: String,
    inspection: InspectionResult,
}

pub(crate) fn external_skills_root_from_process_home() -> SkillResult<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        SkillManagerError::new(
            SkillErrorCode::InvalidSource,
            "无法确定桌面进程的真实 HOME",
            "请从正常用户会话启动 CSSwitch",
        )
    })?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(SkillManagerError::new(
            SkillErrorCode::UnsafePath,
            "桌面进程 HOME 不是绝对路径",
            "请从正常用户会话启动 CSSwitch",
        ));
    }
    Ok(home.join(".claude").join("skills"))
}

pub(crate) fn scan_external_home_skills(
    manager: &SkillManager,
    root: &Path,
) -> SkillResult<ExternalSkillScanReport> {
    scan_external_home_skills_with_hooks(manager, root, None, &ScanHooks::default())
}

#[cfg(test)]
pub(crate) fn scan_named_external_home_skill_for_test(
    manager: &SkillManager,
    root: &Path,
    directory_name: &str,
) -> SkillResult<ExternalSkillScanReport> {
    if !valid_direct_child_name(directory_name) || directory_name.starts_with('.') {
        return Err(root_error(
            SkillErrorCode::UnsafePath,
            "受控外部 Skill 测试目录名无效",
        ));
    }
    scan_external_home_skills_with_hooks(manager, root, Some(directory_name), &ScanHooks::default())
}

#[derive(Default)]
struct ScanHooks<'a> {
    after_root_open: Option<&'a dyn Fn()>,
    after_entries: Option<&'a dyn Fn()>,
    after_candidate_open: Option<&'a dyn Fn(&str)>,
}

fn scan_external_home_skills_with_hooks(
    manager: &SkillManager,
    root: &Path,
    selected_directory: Option<&str>,
    hooks: &ScanHooks<'_>,
) -> SkillResult<ExternalSkillScanReport> {
    if !root.is_absolute() {
        return Err(root_error(
            SkillErrorCode::UnsafePath,
            "外部 Skill 根目录必须是绝对路径",
        ));
    }

    let existing = manager.load_inventory()?;
    let existing_by_name = existing
        .skills
        .iter()
        .filter_map(|skill| match &skill.source {
            SkillSource::ExternalHomeDirectory { directory_name } => selected_directory
                .is_none_or(|selected| directory_name == selected)
                .then(|| (directory_name.clone(), skill.skill_id.clone())),
            SkillSource::LocalDirectory { .. } => None,
        })
        .collect::<BTreeMap<_, _>>();
    let existing_ids = existing
        .skills
        .iter()
        .map(|skill| skill.skill_id.clone())
        .collect::<BTreeSet<_>>();

    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExternalSkillScanReport {
                retained_missing: existing_by_name.len(),
                ..ExternalSkillScanReport::default()
            })
        }
        Err(_) => {
            return Err(root_error(
                SkillErrorCode::IoFailed,
                "无法读取外部 Skill 根目录",
            ))
        }
        Ok(_) => {}
    }
    let anchored_root = AnchoredSourceRoot::open(root)
        .map_err(|error| root_error(error.code, "外部 Skill 根目录不是安全的真实目录"))?;
    if let Some(hook) = hooks.after_root_open {
        hook();
    }
    let entries = anchored_root
        .entries()
        .map_err(|error| root_error(error.code, "无法安全枚举外部 Skill 根目录"))?;
    if entries.len() > MAX_EXTERNAL_ROOT_ENTRIES {
        return Err(root_error(
            SkillErrorCode::LimitExceeded,
            "外部 Skill 根目录条目超过 4,096 个扫描上限",
        ));
    }
    if let Some(hook) = hooks.after_entries {
        hook();
    }

    let mut report = ExternalSkillScanReport {
        root_present: true,
        ..ExternalSkillScanReport::default()
    };
    let mut candidates = Vec::new();
    let mut present_names = BTreeSet::new();
    let mut total_size = 0_u64;
    for entry in entries {
        let Some(name) = entry.name().to_str() else {
            continue;
        };
        if selected_directory.is_some_and(|selected| name != selected) {
            continue;
        }
        if name.starts_with('.') || !valid_direct_child_name(name) {
            continue;
        }
        if entry.is_symlink() {
            report.diagnostics.push(diagnostic(
                name,
                SkillErrorCode::UnsafePath,
                "直接子目录不能是符号链接",
            ));
            continue;
        }
        if !entry.is_directory() {
            continue;
        }
        let source = match anchored_root.open_skill(&entry) {
            Ok(source) => source,
            Err(error) => {
                report.diagnostics.push(diagnostic(
                    name,
                    error.code,
                    "扫描期间候选 Skill 目录发生变化",
                ));
                continue;
            }
        };
        if let Some(hook) = hooks.after_candidate_open {
            hook(name);
        }
        let skill_md_kind = match source.top_level_skill_md_kind() {
            Ok(kind) => kind,
            Err(error) => {
                report
                    .diagnostics
                    .push(diagnostic(name, error.code, "无法安全检查顶层 SKILL.md"));
                continue;
            }
        };
        let Some(skill_md_kind) = skill_md_kind else {
            continue;
        };
        present_names.insert(name.to_string());
        report.discovered += 1;
        if report.discovered > MAX_EXTERNAL_SKILLS {
            return Err(root_error(
                SkillErrorCode::LimitExceeded,
                "外部 Skill 数量超过 256 个扫描上限",
            ));
        }
        if skill_md_kind != libc::S_IFREG {
            report.diagnostics.push(diagnostic(
                name,
                SkillErrorCode::UnsupportedFileType,
                "顶层 SKILL.md 必须是普通文件",
            ));
            continue;
        }
        match source.inspect() {
            Ok(inspection) => {
                if let Err(error) = anchored_root.verify_entry_linked(&source) {
                    report.diagnostics.push(diagnostic(
                        name,
                        error.code,
                        "扫描期间候选 Skill 目录发生变化",
                    ));
                    continue;
                }
                total_size = total_size
                    .checked_add(inspection.summary.total_size)
                    .ok_or_else(|| {
                        root_error(SkillErrorCode::LimitExceeded, "外部 Skill 扫描总大小无效")
                    })?;
                if total_size > MAX_EXTERNAL_SCAN_TOTAL_SIZE {
                    return Err(root_error(
                        SkillErrorCode::LimitExceeded,
                        "外部 Skill 扫描总大小超过 512 MiB",
                    ));
                }
                candidates.push(Candidate {
                    directory_name: name.to_string(),
                    inspection,
                });
            }
            Err(error) => report.diagnostics.push(ExternalSkillScanDiagnostic {
                directory_name: name.to_string(),
                code: error.code.as_str().to_string(),
                message: error.message,
            }),
        }
    }

    anchored_root
        .verify_path_unchanged()
        .map_err(|error| root_error(error.code, "扫描期间外部 Skill 根目录发生变化"))?;

    let mut batch = manager.begin_external_scan_batch()?;
    for candidate in candidates {
        match batch.sync(&candidate.directory_name, candidate.inspection) {
            Ok(outcome) => {
                let existed = existing_ids.contains(&outcome.skill.skill_id);
                report.skill_ids.push(outcome.skill.skill_id);
                match (existed, outcome.changed) {
                    (false, true) => report.imported += 1,
                    (true, true) => report.updated += 1,
                    (_, false) => report.unchanged += 1,
                }
            }
            Err(error) => report.diagnostics.push(ExternalSkillScanDiagnostic {
                directory_name: candidate.directory_name,
                code: error.code.as_str().to_string(),
                message: error.message,
            }),
        }
    }
    report.skill_ids.sort();
    report.retained_missing = existing_by_name
        .keys()
        .filter(|name| !present_names.contains(*name))
        .count();
    Ok(report)
}

fn valid_direct_child_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.chars().any(char::is_control)
        && matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        )
}

fn diagnostic(
    directory_name: &str,
    code: SkillErrorCode,
    message: &str,
) -> ExternalSkillScanDiagnostic {
    ExternalSkillScanDiagnostic {
        directory_name: directory_name.to_string(),
        code: code.as_str().to_string(),
        message: message.to_string(),
    }
}

fn root_error(code: SkillErrorCode, message: &str) -> SkillManagerError {
    SkillManagerError::new(
        code,
        message,
        "CSSwitch 未读取其他 HOME 内容；请修复 ~/.claude/skills 根目录后重试",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_manager::deployment::ReconcileAction;
    use crate::skill_manager::store::TEST_OPERATION_LOCK;
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = crate::test_temp_root().join(format!(
                "csswitch-external-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_skill(root: &Path, name: &str, body: &str) -> PathBuf {
        let skill = root.join(name);
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: external test\n---\n{body}\n"),
        )
        .unwrap();
        skill
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn scans_only_direct_visible_skill_directories_and_rejects_escape_entries() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("discovery-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        write_skill(&root, "visible", "body");
        write_skill(&root, ".hidden", "hidden");
        fs::write(root.join(".DS_Store"), b"metadata").unwrap();
        fs::write(root.join("plain-file"), b"not a skill").unwrap();
        fs::create_dir(root.join("no-manifest")).unwrap();
        let outside = TestDir::new("outside");
        write_skill(&outside.0, "escaped", "outside");
        symlink(outside.0.join("escaped"), root.join("linked-skill")).unwrap();
        let config = TestDir::new("discovery-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));

        let report = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(report.discovered, 1);
        assert_eq!(report.imported, 1);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].directory_name, "linked-skill");
        assert_eq!(report.diagnostics[0].code, "UNSAFE_PATH");
        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills.len(), 1);
        assert!(matches!(
            &inventory.skills[0].source,
            SkillSource::ExternalHomeDirectory { directory_name } if directory_name == "visible"
        ));
        assert!(inventory.skills[0].enabled);
    }

    #[test]
    fn stable_identity_update_missing_retention_and_rebuild_restore() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("lifecycle-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        let source = write_skill(&root, "nature-skill", "first");
        let script = source.join("scripts/render.sh");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let config = TestDir::new("lifecycle-config");
        let data = TestDir::new("lifecycle-data");
        let manager = SkillManager::new(config.0.join(".csswitch"));

        let first = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!((first.imported, first.updated, first.unchanged), (1, 0, 0));
        let original = manager.load_inventory().unwrap().skills.remove(0);
        let payload = manager
            .paths
            .payload(&original.skill_id, &original.content_hash);
        assert_eq!(mode(&payload.join("SKILL.md")), 0o600);
        assert_eq!(mode(&payload.join("scripts/render.sh")), 0o700);

        let repeat = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(
            (repeat.imported, repeat.updated, repeat.unchanged),
            (0, 0, 1)
        );
        assert_eq!(repeat.skill_ids, vec![original.skill_id.clone()]);

        fs::write(
            source.join("SKILL.md"),
            "---\nname: nature-skill\ndescription: external test\n---\nsecond\n",
        )
        .unwrap();
        let update = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(
            (update.imported, update.updated, update.unchanged),
            (0, 1, 0)
        );
        let updated = manager.load_inventory().unwrap().skills.remove(0);
        assert_eq!(updated.skill_id, original.skill_id);
        assert_ne!(updated.content_hash, original.content_hash);

        let deployed = manager.reconcile(&data.0, false, "before_start").unwrap();
        assert!(deployed.errors.is_empty(), "{:?}", deployed.errors);
        assert_eq!(deployed.applied.len(), 1);
        assert_eq!(deployed.applied[0].action, ReconcileAction::Deploy);
        let runtime = data.0.join("skills").join(&updated.runtime_name);
        assert_eq!(mode(&runtime.join("scripts/render.sh")), 0o700);

        fs::remove_dir_all(&source).unwrap();
        let missing = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(missing.retained_missing, 1);
        assert_eq!(
            manager.load_inventory().unwrap().skills[0].skill_id,
            original.skill_id
        );
        manager.verify_skill_store(&updated).unwrap();

        fs::remove_dir_all(&data.0).unwrap();
        fs::create_dir_all(&data.0).unwrap();
        fs::set_permissions(&data.0, fs::Permissions::from_mode(0o700)).unwrap();
        let restored = manager
            .reconcile(&data.0, false, "sandbox_rebuild")
            .unwrap();
        assert_eq!(restored.applied.len(), 1);
        assert!(runtime.join("SKILL.md").is_file());
        assert_eq!(mode(&runtime.join("scripts/render.sh")), 0o700);
    }

    #[test]
    fn candidate_special_files_and_size_limits_are_diagnostics_not_inventory_loss() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("limits-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        write_skill(&root, "good", "body");
        let oversized = write_skill(&root, "oversized", "body");
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(oversized.join("large.bin"))
            .unwrap()
            .set_len(super::super::inspection::MAX_FILE_SIZE + 1)
            .unwrap();
        let special = root.join("special");
        fs::create_dir(&special).unwrap();
        let fifo = special.join("SKILL.md");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
        assert_eq!(result, 0);
        let config = TestDir::new("limits-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));

        let report = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(report.discovered, 3);
        assert_eq!(report.imported, 1);
        assert_eq!(report.diagnostics.len(), 2);
        assert!(report
            .diagnostics
            .iter()
            .any(|item| item.directory_name == "oversized" && item.code == "LIMIT_EXCEEDED"));
        assert!(report.diagnostics.iter().any(|item| {
            item.directory_name == "special" && item.code == "UNSUPPORTED_FILE_TYPE"
        }));
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 1);
    }

    #[test]
    fn symlinked_root_is_a_fail_closed_root_error() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("root-link-home");
        let real = TestDir::new("root-link-real");
        fs::create_dir_all(home.0.join(".claude")).unwrap();
        symlink(&real.0, home.0.join(".claude/skills")).unwrap();
        let config = TestDir::new("root-link-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let error =
            scan_external_home_skills(&manager, &home.0.join(".claude/skills")).unwrap_err();
        assert_eq!(error.code, SkillErrorCode::UnsafePath);
        assert!(manager.load_inventory().unwrap().skills.is_empty());
    }

    #[test]
    fn anchored_scan_fails_closed_when_root_is_replaced_after_open() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("root-swap-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        write_skill(&root, "original", "body");
        let held = home.0.join("held-skills");
        let swap = || {
            fs::rename(&root, &held).unwrap();
            fs::create_dir_all(&root).unwrap();
            write_skill(&root, "replacement", "body");
        };
        let config = TestDir::new("root-swap-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let error = scan_external_home_skills_with_hooks(
            &manager,
            &root,
            None,
            &ScanHooks {
                after_root_open: Some(&swap),
                ..ScanHooks::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::SourceChanged);
        assert!(manager.load_inventory().unwrap().skills.is_empty());
    }

    #[test]
    fn anchored_scan_fails_closed_when_candidate_changes_after_enumeration_or_open() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for stage in ["after_entries", "after_candidate_open"] {
            let home = TestDir::new(stage);
            let root = home.0.join(".claude/skills");
            fs::create_dir_all(&root).unwrap();
            let source = write_skill(&root, "candidate", "original");
            let held = root.join("held-candidate");
            let swap = || {
                fs::rename(&source, &held).unwrap();
                write_skill(&root, "candidate", "replacement");
            };
            let config = TestDir::new(&format!("{stage}-config"));
            let manager = SkillManager::new(config.0.join(".csswitch"));
            let after_candidate = |name: &str| {
                if name == "candidate" {
                    swap();
                }
            };
            let hooks = if stage == "after_entries" {
                ScanHooks {
                    after_entries: Some(&swap),
                    ..ScanHooks::default()
                }
            } else {
                ScanHooks {
                    after_candidate_open: Some(&after_candidate),
                    ..ScanHooks::default()
                }
            };
            let error =
                scan_external_home_skills_with_hooks(&manager, &root, None, &hooks).unwrap_err();
            assert_eq!(error.code, SkillErrorCode::SourceChanged);
            assert!(manager.load_inventory().unwrap().skills.is_empty());
        }
    }

    #[test]
    fn matching_manual_import_migrates_to_external_identity_without_duplicate_id() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("manual-migration-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        let source = write_skill(&root, "nature-skill", "body");
        let config = TestDir::new("manual-migration-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let manual = manager.import_source(&source).unwrap().skill;

        let report = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(report.updated, 1);
        assert_eq!(report.imported, 0);
        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills.len(), 1);
        assert_eq!(inventory.skills[0].skill_id, manual.skill_id);
        assert!(matches!(
            &inventory.skills[0].source,
            SkillSource::ExternalHomeDirectory { directory_name }
                if directory_name == "nature-skill"
        ));
    }

    #[test]
    fn ordinary_update_preserves_external_identity_and_next_scan_keeps_skill_id() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("external-update-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        let source = write_skill(&root, "nature-skill", "old");
        let config = TestDir::new("external-update-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let original = scan_external_home_skills(&manager, &root)
            .unwrap()
            .skill_ids
            .remove(0);
        let alternate = TestDir::new("external-update-alternate");
        fs::write(
            alternate.0.join("SKILL.md"),
            "---\nname: nature-skill\ndescription: external test\n---\nnew\n",
        )
        .unwrap();
        let updated = manager
            .update_source(&original, &alternate.0, true)
            .unwrap()
            .skill;
        assert!(matches!(
            &updated.source,
            SkillSource::ExternalHomeDirectory { directory_name }
                if directory_name == "nature-skill"
        ));
        fs::write(
            source.join("SKILL.md"),
            fs::read(alternate.0.join("SKILL.md")).unwrap(),
        )
        .unwrap();
        let rescanned = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(rescanned.skill_ids, vec![original.clone()]);
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 1);
        assert_eq!(
            manager.load_inventory().unwrap().skills[0].skill_id,
            original
        );
    }

    #[test]
    fn automatic_scan_a_to_b_to_a_reuses_store_version_without_growth() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("scan-version-reuse-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        let source = write_skill(&root, "reuse-skill", "A");
        let a_bytes = fs::read(source.join("SKILL.md")).unwrap();
        let config = TestDir::new("scan-version-reuse-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let first = scan_external_home_skills(&manager, &root).unwrap();
        let skill_id = first.skill_ids[0].clone();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: reuse-skill\ndescription: external test\n---\nB\n",
        )
        .unwrap();
        assert_eq!(
            scan_external_home_skills(&manager, &root).unwrap().updated,
            1
        );
        let id_store = manager.paths.store.join(skill_id.as_str());
        assert_eq!(fs::read_dir(&id_store).unwrap().count(), 2);
        fs::write(source.join("SKILL.md"), a_bytes).unwrap();
        let rollback = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(rollback.updated, 1);
        assert_eq!(rollback.skill_ids, vec![skill_id]);
        assert_eq!(fs::read_dir(&id_store).unwrap().count(), 2);
    }

    #[test]
    fn long_directory_name_manual_import_migrates_without_truncation() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("long-migration-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        let directory_name = "n".repeat(200);
        let source = write_skill(&root, &directory_name, "body");
        fs::write(
            source.join("SKILL.md"),
            "---\nname: long-name-skill\ndescription: external test\n---\nbody\n",
        )
        .unwrap();
        let config = TestDir::new("long-migration-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let manual = manager.import_source(&source).unwrap().skill;
        assert_eq!(
            manual.source,
            SkillSource::LocalDirectory {
                display_path: directory_name.clone()
            }
        );
        let scan = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(scan.updated, 1);
        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills.len(), 1);
        assert_eq!(inventory.skills[0].skill_id, manual.skill_id);
        assert_eq!(
            inventory.skills[0].source,
            SkillSource::ExternalHomeDirectory { directory_name }
        );
    }

    #[test]
    fn directory_name_churn_hits_global_count_quota_without_deleting_missing_sources() {
        let _serial = TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = TestDir::new("global-count-home");
        let root = home.0.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        for index in 0..=super::super::store::MAX_EXTERNAL_SKILL_COUNT {
            write_skill(&root, &format!("skill-{index:03}"), "body");
        }
        let config = TestDir::new("global-count-config");
        let manager = SkillManager::new(config.0.join(".csswitch"));
        let first = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(
            first.imported,
            super::super::store::MAX_EXTERNAL_SKILL_COUNT
        );
        assert_eq!(first.diagnostics.len(), 1);
        assert_eq!(first.diagnostics[0].code, "LIMIT_EXCEEDED");
        assert_eq!(
            manager.load_inventory().unwrap().skills.len(),
            super::super::store::MAX_EXTERNAL_SKILL_COUNT
        );

        let migration_source = root.join(format!(
            "skill-{:03}",
            super::super::store::MAX_EXTERNAL_SKILL_COUNT
        ));
        let manual = manager.import_source(&migration_source).unwrap().skill;
        let before_migration = fs::read(&manager.paths.inventory).unwrap();
        let migration_scan = scan_external_home_skills(&manager, &root).unwrap();
        assert!(migration_scan.diagnostics.iter().any(|diagnostic| {
            diagnostic.directory_name
                == format!("skill-{:03}", super::super::store::MAX_EXTERNAL_SKILL_COUNT)
                && diagnostic.code == "LIMIT_EXCEEDED"
        }));
        assert_eq!(
            before_migration,
            fs::read(&manager.paths.inventory).unwrap()
        );
        assert!(matches!(
            manager
                .load_inventory()
                .unwrap()
                .skills
                .iter()
                .find(|skill| skill.skill_id == manual.skill_id)
                .unwrap()
                .source,
            SkillSource::LocalDirectory { .. }
        ));

        for index in 0..super::super::store::MAX_EXTERNAL_SKILL_COUNT {
            let source = root.join(format!("skill-{index:03}"));
            fs::write(
                source.join("SKILL.md"),
                format!("---\nname: skill-{index:03}\ndescription: external test\n---\nchanged\n"),
            )
            .unwrap();
        }
        super::super::store::OWNED_STORE_AUDIT_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
        let changed = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(
            changed.updated,
            super::super::store::MAX_EXTERNAL_SKILL_COUNT
        );
        assert_eq!(
            super::super::store::OWNED_STORE_AUDIT_COUNT.load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        fs::remove_dir_all(root.join("skill-000")).unwrap();
        write_skill(&root, "skill-new", "body");
        let churn = scan_external_home_skills(&manager, &root).unwrap();
        assert_eq!(churn.retained_missing, 1);
        assert!(churn
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.directory_name == "skill-new"
                && diagnostic.code == "LIMIT_EXCEEDED"));
        assert_eq!(
            manager.load_inventory().unwrap().skills.len(),
            super::super::store::MAX_EXTERNAL_SKILL_COUNT + 1
        );
    }
}
