use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::archive::validate_skill_name;
use crate::install::{active_org, reject_symlink_path, skills_root};
use crate::{
    error, verify_csswitch_import_origin, InstallError, IMPORT_ORIGIN_FILE, MAX_IMPORT_ORIGIN_BYTES,
};

pub const MAX_LISTED_SKILLS: usize = 2_000;
pub const MAX_SKILL_FRONTMATTER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstalledSkillSource {
    CsswitchSystem,
    CsswitchLocal,
    CsswitchGithub,
    ScienceLocal,
    Unverified,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledSkillSummary {
    pub skill_id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub source_kind: InstalledSkillSource,
    pub bundle_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillListWarning {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFilesystemSnapshot {
    pub active_org: String,
    pub items: Vec<InstalledSkillSummary>,
    pub warnings: Vec<SkillListWarning>,
}

pub fn inspect_active_org_skills(data_dir: &Path) -> Result<SkillFilesystemSnapshot, InstallError> {
    let initial_org = active_org(data_dir)?;
    let root = skills_root(data_dir, &initial_org)?;
    let mut items = Vec::new();
    let mut warnings = Vec::new();

    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(error(
                "SKILL_ROOT_INVALID",
                "Science Skills 根目录不是安全目录",
                "listing",
            ));
        }
        Ok(_) => {}
        Err(problem) if problem.kind() == io::ErrorKind::NotFound => {
            return finish_snapshot(data_dir, initial_org, items, warnings);
        }
        Err(_) => {
            return Err(error(
                "SKILL_ROOT_UNREADABLE",
                "无法读取 Science Skills 根目录",
                "listing",
            ));
        }
    }

    reject_symlink_path(&root)?;
    let entries = fs::read_dir(&root).map_err(|_| {
        error(
            "SKILL_ROOT_UNREADABLE",
            "无法枚举 Science Skills 根目录",
            "listing",
        )
    })?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_LISTED_SKILLS {
            return Err(error(
                "SKILL_LIST_LIMIT_EXCEEDED",
                "Science Skill 数量超过列表安全上限",
                "listing",
            ));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                warnings.push(warning(
                    "SKILL_ENTRY_UNREADABLE",
                    None,
                    "已跳过一个无法读取的 Skill 条目",
                ));
                continue;
            }
        };
        let Some(skill_id) = entry.file_name().to_str().map(str::to_owned) else {
            warnings.push(warning(
                "SKILL_ID_INVALID",
                None,
                "已跳过一个名称不是 UTF-8 的 Skill 条目",
            ));
            continue;
        };
        if validate_skill_name(&skill_id).is_err() {
            warnings.push(warning(
                "SKILL_ID_INVALID",
                None,
                "已跳过一个名称不符合安全规则的 Skill 条目",
            ));
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(value) => value,
            Err(_) => {
                warnings.push(warning(
                    "SKILL_ENTRY_UNREADABLE",
                    Some(&skill_id),
                    "无法确认该 Skill 条目的文件类型",
                ));
                continue;
            }
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            warnings.push(warning(
                "SKILL_ENTRY_UNSAFE",
                Some(&skill_id),
                "该 Skill 条目不是安全目录，已跳过",
            ));
            continue;
        }
        let skill_dir = entry.path();
        if reject_symlink_path(&skill_dir).is_err() {
            warnings.push(warning(
                "SKILL_ENTRY_UNSAFE",
                Some(&skill_id),
                "该 Skill 目录包含符号链接路径，已跳过",
            ));
            continue;
        }
        let manifest_path = skill_dir.join("SKILL.md");
        let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                warnings.push(warning(
                    "SKILL_MANIFEST_UNSAFE",
                    Some(&skill_id),
                    "该 Skill 的 SKILL.md 不是安全普通文件，已跳过",
                ));
                continue;
            }
            Ok(metadata) => metadata,
            Err(_) => {
                warnings.push(warning(
                    "SKILL_MANIFEST_MISSING",
                    Some(&skill_id),
                    "该目录缺少可读取的 SKILL.md，已跳过",
                ));
                continue;
            }
        };
        if manifest_metadata.len() as usize > MAX_SKILL_FRONTMATTER_BYTES {
            warnings.push(warning(
                "SKILL_MANIFEST_TOO_LARGE",
                Some(&skill_id),
                "SKILL.md 超过展示元数据读取上限，已使用目录名",
            ));
        }
        let (display_name, description) = match read_frontmatter(&manifest_path, &skill_id) {
            Ok(value) => value,
            Err(()) => {
                warnings.push(warning(
                    "SKILL_MANIFEST_UNREADABLE",
                    Some(&skill_id),
                    "无法安全读取 SKILL.md 展示元数据，已使用目录名",
                ));
                (skill_id.clone(), None)
            }
        };
        let (source_kind, bundle_name, source_warning) = classify_source(&skill_dir, &skill_id);
        if let Some(source_warning) = source_warning {
            warnings.push(source_warning);
        }
        items.push(InstalledSkillSummary {
            skill_id,
            display_name,
            description,
            source_kind,
            bundle_name,
        });
    }
    items.sort_by(|left, right| left.skill_id.cmp(&right.skill_id));
    finish_snapshot(data_dir, initial_org, items, warnings)
}

fn finish_snapshot(
    data_dir: &Path,
    initial_org: String,
    items: Vec<InstalledSkillSummary>,
    warnings: Vec<SkillListWarning>,
) -> Result<SkillFilesystemSnapshot, InstallError> {
    if active_org(data_dir)? != initial_org {
        return Err(error(
            "ACTIVE_ORG_CHANGED",
            "Science active org 在列表读取期间发生变化",
            "listing",
        )
        .retryable(true));
    }
    Ok(SkillFilesystemSnapshot {
        active_org: initial_org,
        items,
        warnings,
    })
}

fn classify_source(
    skill_dir: &Path,
    skill_id: &str,
) -> (
    InstalledSkillSource,
    Option<String>,
    Option<SkillListWarning>,
) {
    let marker_path = skill_dir.join(IMPORT_ORIGIN_FILE);
    let marker_metadata = match fs::symlink_metadata(&marker_path) {
        Ok(metadata) => metadata,
        Err(problem) if problem.kind() == io::ErrorKind::NotFound => {
            return (InstalledSkillSource::ScienceLocal, None, None)
        }
        Err(_) => {
            return (
                InstalledSkillSource::Unverified,
                None,
                Some(warning(
                    "SKILL_SOURCE_UNREADABLE",
                    Some(skill_id),
                    "无法读取该 Skill 的来源标记",
                )),
            )
        }
    };
    if marker_metadata.file_type().is_symlink()
        || !marker_metadata.is_file()
        || marker_metadata.len() as usize > MAX_IMPORT_ORIGIN_BYTES
    {
        return (
            InstalledSkillSource::Unverified,
            None,
            Some(warning(
                "SKILL_SOURCE_UNVERIFIED",
                Some(skill_id),
                "该 Skill 的来源标记无法验证",
            )),
        );
    }
    match verify_csswitch_import_origin(skill_dir, skill_id) {
        Ok(marker) => {
            let source_kind = marker.get("source_kind").and_then(Value::as_str);
            let repo = marker.get("repo").and_then(Value::as_str);
            let source = match (source_kind, repo) {
                (Some("local_zip"), Some("csswitch/local-archive"))
                | (None, Some("csswitch/local-archive")) => InstalledSkillSource::CsswitchLocal,
                (Some("github"), Some(repo)) | (None, Some(repo))
                    if repo != "csswitch/local-archive" =>
                {
                    InstalledSkillSource::CsswitchGithub
                }
                _ => {
                    return (
                        InstalledSkillSource::Unverified,
                        None,
                        Some(warning(
                            "SKILL_SOURCE_UNVERIFIED",
                            Some(skill_id),
                            "该 Skill 的来源类型与仓库标记不一致",
                        )),
                    );
                }
            };
            let bundle_name = marker
                .get("bundle_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (source, bundle_name, None)
        }
        Err(_) => (
            InstalledSkillSource::Unverified,
            None,
            Some(warning(
                "SKILL_SOURCE_UNVERIFIED",
                Some(skill_id),
                "该 Skill 的来源标记无法验证",
            )),
        ),
    }
}

fn read_frontmatter(path: &Path, fallback: &str) -> Result<(String, Option<String>), ()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).map_err(|_| ())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SKILL_FRONTMATTER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > MAX_SKILL_FRONTMATTER_BYTES {
        return Ok((fallback.to_string(), None));
    }
    let body = std::str::from_utf8(&bytes).map_err(|_| ())?;
    let lines = body.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return Ok((fallback.to_string(), None));
    }
    let Some(end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == "---").then_some(index))
    else {
        return Ok((fallback.to_string(), None));
    };
    let mut name = None;
    let mut description = None;
    let mut index = 1;
    while index < end {
        let line = lines[index];
        if line.starts_with(char::is_whitespace) || !line.contains(':') {
            index += 1;
            continue;
        }
        let (key, raw_value) = line.split_once(':').unwrap_or(("", ""));
        let key = key.trim();
        let raw_value = raw_value.trim();
        if key == "name" {
            name = scalar(raw_value).map(|value| display_text(&value, 120));
        } else if key == "description" {
            if matches!(raw_value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
                let folded = raw_value.starts_with('>');
                let mut parts = Vec::new();
                index += 1;
                while index < end {
                    let continuation = lines[index];
                    if !continuation.trim().is_empty()
                        && !continuation.starts_with(char::is_whitespace)
                    {
                        index -= 1;
                        break;
                    }
                    parts.push(continuation.trim());
                    index += 1;
                }
                let separator = if folded { " " } else { "\n" };
                let value = display_text(&parts.join(separator), 500);
                if !value.is_empty() {
                    description = Some(value);
                }
            } else if let Some(value) = scalar(raw_value) {
                let value = display_text(&value, 500);
                if !value.is_empty() {
                    description = Some(value);
                }
            }
        }
        index += 1;
    }
    Ok((
        name.filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string()),
        description,
    ))
}

fn scalar(value: &str) -> Option<String> {
    if value.is_empty() || matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
        return None;
    }
    let unquoted = if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };
    Some(unquoted.to_string())
}

fn display_text(value: &str, max_chars: usize) -> String {
    let normalized = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    let trimmed = normalized.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().take(max_chars).collect::<String>()
}

fn warning(code: &str, skill_id: Option<&str>, message: &str) -> SkillListWarning {
    SkillListWarning {
        code: code.to_string(),
        skill_id: skill_id.map(str::to_owned),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            loop {
                let unique = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
                let path = crate::test_temp_root().join(format!(
                    "csswitch-skill-listing-{}-{unique}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create isolated listing test dir: {error}"),
                }
            }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn prepare_data() -> TestDir {
        let root = TestDir::new();
        fs::write(root.0.join("active-org.json"), r#"{"org_uuid":"org-test"}"#).unwrap();
        fs::create_dir_all(root.0.join("orgs/org-test/skills")).unwrap();
        root
    }

    fn write_skill(root: &TestDir, name: &str, body: &str) -> PathBuf {
        let directory = root.0.join("orgs/org-test/skills").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("SKILL.md"), body).unwrap();
        directory
    }

    fn write_valid_marker(skill: &Path, name: &str, source_kind: &str, bundle_name: Option<&str>) {
        let mut marker = serde_json::json!({
            "version": 1,
            "repo": if source_kind == "local_zip" { "csswitch/local-archive" } else { "owner/repo" },
            "sha": "a".repeat(40),
            "plugin": name,
            "marketplace": crate::CSSWITCH_MARKETPLACE,
            "path": format!("skills/{name}"),
            "importedAt": "2026-07-18T00:00:00Z",
            "license": "NOASSERTION",
            "csswitch_revision": 2,
            "source_kind": source_kind,
            "content_sha256": "b".repeat(64),
        });
        if let Some(bundle_name) = bundle_name {
            marker["bundle_name"] = Value::String(bundle_name.to_string());
        }
        fs::write(
            skill.join(IMPORT_ORIGIN_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn lists_science_skill_and_parses_bounded_frontmatter() {
        let root = prepare_data();
        write_skill(
            &root,
            "demo",
            "---\nname: Demo Skill\ndescription: >-\n  A useful\n  bounded description.\n---\nBody\n",
        );
        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].skill_id, "demo");
        assert_eq!(snapshot.items[0].display_name, "Demo Skill");
        assert_eq!(
            snapshot.items[0].description.as_deref(),
            Some("A useful bounded description.")
        );
        assert_eq!(
            snapshot.items[0].source_kind,
            InstalledSkillSource::ScienceLocal
        );
    }

    #[test]
    fn invalid_marker_is_not_claimed_as_csswitch_source() {
        let root = prepare_data();
        let skill = write_skill(&root, "demo", "---\nname: Demo\n---\n");
        fs::write(skill.join(IMPORT_ORIGIN_FILE), b"{}").unwrap();
        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert_eq!(
            snapshot.items[0].source_kind,
            InstalledSkillSource::Unverified
        );
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.code == "SKILL_SOURCE_UNVERIFIED"));
    }

    #[test]
    fn classifies_verified_local_and_github_markers() {
        let root = prepare_data();
        let local = write_skill(&root, "local-demo", "---\nname: Local\n---\n");
        let github = write_skill(&root, "github-demo", "---\nname: GitHub\n---\n");
        write_valid_marker(&local, "local-demo", "local_zip", Some("demo-bundle"));
        write_valid_marker(&github, "github-demo", "github", None);

        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert_eq!(snapshot.items[0].skill_id, "github-demo");
        assert_eq!(
            snapshot.items[0].source_kind,
            InstalledSkillSource::CsswitchGithub
        );
        assert_eq!(snapshot.items[1].skill_id, "local-demo");
        assert_eq!(
            snapshot.items[1].source_kind,
            InstalledSkillSource::CsswitchLocal
        );
        assert_eq!(
            snapshot.items[1].bundle_name.as_deref(),
            Some("demo-bundle")
        );
    }

    #[test]
    fn inconsistent_marker_tuple_is_unverified() {
        let root = prepare_data();
        let skill = write_skill(&root, "mismatch", "---\nname: Mismatch\n---\n");
        write_valid_marker(&skill, "mismatch", "local_zip", None);
        let path = skill.join(IMPORT_ORIGIN_FILE);
        let mut marker: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        marker["repo"] = Value::String("owner/repo".to_string());
        fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();

        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert_eq!(
            snapshot.items[0].source_kind,
            InstalledSkillSource::Unverified
        );
        assert_eq!(snapshot.warnings[0].code, "SKILL_SOURCE_UNVERIFIED");
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlink_skill_and_manifest() {
        use std::os::unix::fs::symlink;

        let root = prepare_data();
        let external = root.0.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("SKILL.md"), b"demo").unwrap();
        symlink(&external, root.0.join("orgs/org-test/skills/symlink-skill")).unwrap();
        let manifest_link = root.0.join("manifest-target");
        fs::write(&manifest_link, b"demo").unwrap();
        let manifest_skill = root.0.join("orgs/org-test/skills/manifest-link");
        fs::create_dir(&manifest_skill).unwrap();
        symlink(&manifest_link, manifest_skill.join("SKILL.md")).unwrap();

        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert!(snapshot.items.is_empty());
        assert_eq!(snapshot.warnings.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn skips_special_entries_and_special_manifests() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        fn make_fifo(path: &Path) {
            let encoded = CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        }

        let root = prepare_data();
        let skills = root.0.join("orgs/org-test/skills");
        make_fifo(&skills.join("fifo-entry"));
        let manifest_skill = skills.join("socket-manifest");
        fs::create_dir(&manifest_skill).unwrap();
        make_fifo(&manifest_skill.join("SKILL.md"));

        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert!(snapshot.items.is_empty());
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.code == "SKILL_ENTRY_UNSAFE"));
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.code == "SKILL_MANIFEST_UNSAFE"));
    }

    #[test]
    fn enforces_entry_and_frontmatter_read_limits() {
        let root = prepare_data();
        let oversized = write_skill(&root, "oversized", "placeholder");
        fs::write(
            oversized.join("SKILL.md"),
            vec![b'x'; MAX_SKILL_FRONTMATTER_BYTES + 1],
        )
        .unwrap();
        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert_eq!(snapshot.items[0].display_name, "oversized");
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.code == "SKILL_MANIFEST_TOO_LARGE"));

        fs::remove_dir_all(root.0.join("orgs/org-test/skills")).unwrap();
        fs::create_dir_all(root.0.join("orgs/org-test/skills")).unwrap();
        for index in 0..=MAX_LISTED_SKILLS {
            fs::write(
                root.0
                    .join("orgs/org-test/skills")
                    .join(format!("entry-{index}")),
                b"not a directory",
            )
            .unwrap();
        }
        let error = inspect_active_org_skills(&root.0).unwrap_err();
        assert_eq!(error.code, "SKILL_LIST_LIMIT_EXCEEDED");
    }

    #[test]
    fn active_org_missing_invalid_and_changed_are_fail_closed() {
        let missing = TestDir::new();
        assert_eq!(
            inspect_active_org_skills(&missing.0).unwrap_err().code,
            "SCIENCE_NOT_READY"
        );

        let invalid = TestDir::new();
        fs::write(invalid.0.join("active-org.json"), b"not json").unwrap();
        assert_eq!(
            inspect_active_org_skills(&invalid.0).unwrap_err().code,
            "SCIENCE_NOT_READY"
        );

        let changed = prepare_data();
        fs::write(
            changed.0.join("active-org.json"),
            r#"{"org_uuid":"org-other"}"#,
        )
        .unwrap();
        assert_eq!(
            finish_snapshot(&changed.0, "org-test".to_string(), vec![], vec![])
                .unwrap_err()
                .code,
            "ACTIVE_ORG_CHANGED"
        );
    }

    #[test]
    fn missing_skills_root_is_an_empty_ready_snapshot() {
        let root = TestDir::new();
        fs::write(root.0.join("active-org.json"), r#"{"org_uuid":"org-test"}"#).unwrap();
        let snapshot = inspect_active_org_skills(&root.0).unwrap();
        assert!(snapshot.items.is_empty());
        assert_eq!(snapshot.active_org, "org-test");
    }
}
