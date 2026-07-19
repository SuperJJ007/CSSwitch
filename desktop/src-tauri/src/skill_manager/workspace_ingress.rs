use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use super::error::{SkillErrorCode, SkillManagerError, SkillResult};
use super::model::SkillId;
use super::store::{os_cstring, SafeDir, SkillManager};

const MAX_WORKSPACES: usize = 256;
const MAX_CANDIDATES: usize = 256;
const MAX_SKILL_MD_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceIngressReport {
    pub(crate) discovered: usize,
    pub(crate) imported: usize,
    pub(crate) unchanged: usize,
    pub(crate) skill_ids: Vec<SkillId>,
    pub(crate) diagnostics: Vec<WorkspaceIngressDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceIngressDiagnostic {
    pub(crate) file_name: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Deserialize)]
struct ActiveOrg {
    org_uuid: String,
}

pub(crate) fn scan_workspace_skill_files(
    manager: &SkillManager,
    data_dir: &Path,
) -> SkillResult<WorkspaceIngressReport> {
    if !data_dir.is_absolute() {
        return Err(unsafe_path("Science data-dir 必须是绝对路径"));
    }
    let data =
        SafeDir::open_absolute(data_dir).map_err(|_| unsafe_path("Science data-dir 不安全"))?;
    data.validate_external_owned()
        .map_err(|_| unsafe_path("Science data-dir 所有权无效"))?;
    let active_name = OsStr::new("active-org.json");
    if data.child_stat(&os_cstring(active_name)?)?.is_none() {
        return Ok(WorkspaceIngressReport::default());
    }
    let (active_contents, active_identity) = data
        .read_bound_file(active_name, 64 * 1024)
        .map_err(|_| unsafe_path("active-org.json 不是安全的普通文件"))?;
    let active: ActiveOrg = serde_json::from_slice(&active_contents)
        .map_err(|_| unsafe_path("active-org.json 内容无效"))?;
    if !valid_uuid(&active.org_uuid) {
        return Err(unsafe_path("active-org.json 的组织标识无效"));
    }

    let orgs = open_optional_child(&data, OsStr::new("orgs"))?;
    let Some(orgs) = orgs else {
        return Ok(WorkspaceIngressReport::default());
    };
    let org = open_optional_child(&orgs, OsStr::new(&active.org_uuid))?;
    let Some(org) = org else {
        return Ok(WorkspaceIngressReport::default());
    };
    let workspaces = open_optional_child(&org, OsStr::new("workspaces"))?;
    let Some(workspaces) = workspaces else {
        return Ok(WorkspaceIngressReport::default());
    };

    let workspace_dirs = workspaces.names()?;
    if workspace_dirs.len() > MAX_WORKSPACES {
        return Err(limit_error("Science workspace 数量超过 256 个上限"));
    }

    let mut report = WorkspaceIngressReport::default();
    for workspace_name in workspace_dirs {
        if !valid_child_name(&workspace_name) {
            continue;
        }
        let workspace = match workspaces.open_child(&workspace_name) {
            Ok(directory) => directory,
            Err(_) => continue,
        };
        workspace
            .validate_external_owned()
            .map_err(|_| unsafe_path("Science workspace 所有权无效"))?;
        for name in workspace.names()? {
            let Some(name_text) = name.to_str() else {
                continue;
            };
            if !name_text.ends_with(".skill.md") || !valid_child_name(&name) {
                continue;
            }
            report.discovered += 1;
            if report.discovered > MAX_CANDIDATES {
                return Err(limit_error("workspace Skill 候选超过 256 个上限"));
            }
            let content = match workspace.read_bound_external_file(&name, MAX_SKILL_MD_SIZE) {
                Ok(content) => content,
                Err(error) => {
                    report.diagnostics.push(WorkspaceIngressDiagnostic {
                        file_name: name_text.to_string(),
                        code: error.code.as_str().to_string(),
                        message: "workspace Skill 必须是稳定的普通文件".to_string(),
                    });
                    continue;
                }
            };
            match import_candidate(
                manager,
                &content,
                workspace_name.to_string_lossy().as_ref(),
                name_text,
            ) {
                Ok((skill_id, changed)) => {
                    report.skill_ids.push(skill_id);
                    if changed {
                        report.imported += 1
                    } else {
                        report.unchanged += 1
                    }
                }
                Err(error) => report.diagnostics.push(WorkspaceIngressDiagnostic {
                    file_name: name_text.to_string(),
                    code: error.code.as_str().to_string(),
                    message: error.message,
                }),
            }
        }
        workspaces.verify_child_identity(&workspace_name, &workspace)?;
    }
    data.verify_file_identity(active_name, active_identity)?;
    data.verify_child_identity(OsStr::new("orgs"), &orgs)?;
    orgs.verify_child_identity(OsStr::new(&active.org_uuid), &org)?;
    org.verify_child_identity(OsStr::new("workspaces"), &workspaces)?;
    let reopened = SafeDir::open_absolute(data_dir)?;
    if !data.same_identity(&reopened)? {
        return Err(SkillManagerError::new(
            SkillErrorCode::SourceChanged,
            "Science data-dir 在扫描期间发生变化",
            "重新一键开始",
        ));
    }
    report.skill_ids.sort();
    report.skill_ids.dedup();
    Ok(report)
}

fn import_candidate(
    manager: &SkillManager,
    content: &[u8],
    workspace_name: &str,
    file_name: &str,
) -> SkillResult<(SkillId, bool)> {
    let staging_root = std::env::temp_dir()
        .canonicalize()
        .map_err(|_| io_error())?;
    let staging = staging_root.join(format!(
        "csswitch-workspace-skill-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir(&staging).map_err(|_| io_error())?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).map_err(|_| io_error())?;
    let result = (|| {
        let target = staging.join("SKILL.md");
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)
            .map_err(|_| io_error())?;
        output.write_all(content).map_err(|_| io_error())?;
        output.sync_all().map_err(|_| io_error())?;
        let inspection = manager.inspect(&staging)?;
        let source_key = format!("workspace--{workspace_name}--{file_name}");
        let mut batch = manager.begin_external_scan_batch()?;
        let outcome = batch.sync(&source_key, inspection)?;
        Ok((outcome.skill.skill_id, outcome.changed))
    })();
    let _ = fs::remove_file(staging.join("SKILL.md"));
    let _ = fs::remove_dir(&staging);
    result
}

fn open_optional_child(parent: &SafeDir, name: &OsStr) -> SkillResult<Option<SafeDir>> {
    if parent.child_stat(&os_cstring(name)?)?.is_none() {
        return Ok(None);
    }
    let child = parent.open_child(name)?;
    child.validate_external_owned()?;
    Ok(Some(child))
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn valid_child_name(value: &OsStr) -> bool {
    let path = Path::new(value);
    matches!(
        path.components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    ) && value.to_str().is_some_and(|text| {
        !text.is_empty()
            && !text.starts_with('.')
            && text.len() <= 255
            && !text.chars().any(char::is_control)
    })
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn unsafe_path(message: &str) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::UnsafePath,
        message,
        "不要手工链接或替换 Science workspace 路径",
    )
}

fn io_error() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::IoFailed,
        "无法安全读取 workspace Skill",
        "确认文件写入完成后重新一键开始",
    )
}

fn limit_error(message: &str) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::LimitExceeded,
        message,
        "减少候选文件后重新一键开始",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct TestDir(PathBuf);

    use std::path::PathBuf;

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = crate::test_temp_root().join(format!(
                "csswitch-workspace-ingress-{label}-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn science_workspace(data: &Path) -> PathBuf {
        let org = "11111111-1111-4111-8111-111111111111";
        fs::set_permissions(data, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            data.join("active-org.json"),
            format!(r#"{{"org_uuid":"{org}"}}"#),
        )
        .unwrap();
        fs::set_permissions(
            data.join("active-org.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let workspace = data.join("orgs").join(org).join("workspaces").join("ws-1");
        fs::create_dir_all(&workspace).unwrap();
        let mut current = data.to_path_buf();
        for component in ["orgs", org, "workspaces", "ws-1"] {
            current.push(component);
            fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).unwrap();
        }
        workspace
    }

    #[test]
    fn imports_single_file_once_and_store_survives_source_removal() {
        let _serial = super::super::store::TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("import");
        let config = root.0.join(".csswitch");
        let data = root.0.join("science");
        fs::create_dir_all(&data).unwrap();
        let workspace = science_workspace(&data);
        let source = workspace.join("probe.skill.md");
        fs::write(
            &source,
            b"---\nname: Workspace Probe\ndescription: Agent-created probe\nversion: 1.0.0\n---\nReturn WORKSPACE_PROBE.\n",
        )
        .unwrap();
        let manager = SkillManager::new(config);

        let first = scan_workspace_skill_files(&manager, &data).unwrap();
        assert_eq!(first.discovered, 1);
        assert_eq!(first.imported, 1, "{first:?}");
        assert!(first.diagnostics.is_empty());
        let second = scan_workspace_skill_files(&manager, &data).unwrap();
        assert_eq!(second.unchanged, 1);

        fs::write(
            &source,
            b"---\nname: Workspace Probe\ndescription: Agent-created probe\nversion: 1.1.0\n---\nReturn WORKSPACE_PROBE_V2.\n",
        )
        .unwrap();
        let updated = scan_workspace_skill_files(&manager, &data).unwrap();
        assert_eq!(updated.imported, 1, "{updated:?}");
        assert_eq!(updated.skill_ids, first.skill_ids);
        assert_eq!(manager.load_inventory().unwrap().skills.len(), 1);

        fs::remove_file(source).unwrap();
        let missing = scan_workspace_skill_files(&manager, &data).unwrap();
        assert_eq!(missing.discovered, 0);
        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills.len(), 1);
        assert!(inventory.skills[0].enabled);
        manager.verify_skill_store(&inventory.skills[0]).unwrap();
    }

    #[test]
    fn rejects_symlink_candidate_without_importing() {
        let _serial = super::super::store::TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestDir::new("symlink");
        let config = root.0.join(".csswitch");
        let data = root.0.join("science");
        fs::create_dir_all(&data).unwrap();
        let workspace = science_workspace(&data);
        let outside = root.0.join("outside.md");
        fs::write(
            &outside,
            b"---\nname: Outside\ndescription: Must not import\n---\nno\n",
        )
        .unwrap();
        symlink(outside, workspace.join("outside.skill.md")).unwrap();
        let manager = SkillManager::new(config);

        let report = scan_workspace_skill_files(&manager, &data).unwrap();
        assert_eq!(report.discovered, 1);
        assert_eq!(report.imported, 0);
        assert_eq!(report.diagnostics[0].code, "STORE_CONFLICT");
        assert!(manager.load_inventory().unwrap().skills.is_empty());
    }
}
