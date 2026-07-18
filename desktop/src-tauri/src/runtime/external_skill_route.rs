use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use csswitch_skill_install_core::MAX_IMPORT_ORIGIN_BYTES;
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(crate) const SKILL_NAME: &str = "csswitch-external-skill-tools";
const IMPORT_ORIGIN_FILE: &str = ".import-origin";
const MARKETPLACE: &str = "csswitch-system-bridge";
const MAX_ROUTE_SKILL_BODY_BYTES: usize = 64 * 1024;
const SKILL_BODY: &str =
    include_str!("../../resources/skills/csswitch-external-skill-tools/SKILL.md");
const LEGACY_SPLIT_CONNECTORS_SHA256: &str =
    "a4f6700a69ce83664cb620791362a5f380455113e0231a8ec6f39db12c16e269";
const LEGACY_REVISION_2_SHA256: &str =
    "867c11ff6d738326cb66a34583f3eff5eba58ead4ea40fb3f4a2b6e7242b6cee";
const LEGACY_SINGLE_SKILL_REVISION_SHA256: &str =
    "04d46b508ea682eda4e3f3cfd95ca1f7743213620f82dc3a10f5f09001f39999";
const LEGACY_BUNDLE_PRE_PROGRESS_SHA256: &str =
    "4bdff8f40a18af106a88f1489ae409b9c4df2ba24762fe806fd7f4152979da31";
const LEGACY_BUNDLE_PROGRESS_V1_SHA256: &str =
    "60555234c2b6aa63ce263f6ce09e9cfad09b4b16f923ec2d4331fb856eb3078c";
const LEGACY_BUNDLE_PROGRESS_V2_SHA256: &str =
    "bc13e93dda04d54faa6b81518ad306283b5f55b40c4da587faec1ca8d4a922ae";
const LEGACY_BUNDLE_NO_UNINSTALL_CONFIRMATION_SHA256: &str =
    "9c80e2c45f2471f408df8d18eee6746a29adc7f51e07212102636ba46273a29e";

/// Atomically install the tiny CSSwitch routing Skill into the active org.
///
/// The caller must still attach it to OPERON through Science's local control
/// plane. A same-name user or modified directory is never overwritten.
pub(crate) fn ensure_route_skill(data_dir: &Path) -> Result<bool, String> {
    let target = route_skill_path(data_dir)?;
    if target.exists() || fs::symlink_metadata(&target).is_ok() {
        if route_skill_matches(&target)? {
            return Ok(false);
        }
        if legacy_route_skill_matches(&target)? {
            migrate_legacy_route_body(&target)?;
            return Ok(true);
        }
        return Err(format!(
            "Skill '{SKILL_NAME}' 已存在且不是当前 CSSwitch 路由，已拒绝覆盖"
        ));
    }
    let skills_root = target.parent().ok_or("路由 Skill 缺少 skills 父目录")?;
    fs::create_dir_all(skills_root).map_err(|e| format!("创建 Skills 目录失败：{e}"))?;
    reject_symlink_path(skills_root)?;
    let temp = skills_root.join(format!(
        ".csswitch-route-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let result = (|| -> Result<(), String> {
        fs::create_dir(&temp).map_err(|e| format!("创建路由 Skill 临时目录失败：{e}"))?;
        write_new_file(&temp.join("SKILL.md"), SKILL_BODY.as_bytes())?;
        let mut marker = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "repo": "csswitch/local",
            "sha": "0000000000000000000000000000000000000000",
            "plugin": SKILL_NAME,
            "marketplace": MARKETPLACE,
            "path": "embedded/csswitch-external-skill-tools",
            "importedAt": rfc3339_now(),
            "license": "MIT"
        }))
        .map_err(|e| format!("编码路由 Skill 来源标记失败：{e}"))?;
        marker.push(b'\n');
        write_new_file(&temp.join(IMPORT_ORIGIN_FILE), &marker)?;
        File::open(&temp)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("同步路由 Skill 临时目录失败：{e}"))?;
        rename_no_replace(&temp, &target)?;
        File::open(skills_root)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("同步 Skills 目录失败：{e}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result.map(|_| true)
}

pub(crate) fn inspect_route_skill(data_dir: &Path) -> Result<bool, String> {
    let active_org = read_active_org(data_dir)?;
    inspect_route_skill_for_org(data_dir, &active_org)
}

pub(crate) fn inspect_route_skill_for_org(
    data_dir: &Path,
    expected_org: &str,
) -> Result<bool, String> {
    let target = route_skill_path_for_org(data_dir, expected_org)?;
    if !target.exists() && fs::symlink_metadata(&target).is_err() {
        return Ok(false);
    }
    route_skill_matches(&target)
}

fn route_skill_path(data_dir: &Path) -> Result<PathBuf, String> {
    let active_org = read_active_org(data_dir)?;
    route_skill_path_for_org(data_dir, &active_org)
}

fn route_skill_path_for_org(data_dir: &Path, active_org: &str) -> Result<PathBuf, String> {
    if !valid_active_org(active_org) {
        return Err("active org 标识非法".into());
    }
    let skills_root = data_dir.join("orgs").join(active_org).join("skills");
    ensure_safe_skills_root(data_dir, &skills_root)?;
    Ok(skills_root.join(SKILL_NAME))
}

fn read_active_org(data_dir: &Path) -> Result<String, String> {
    csswitch_skill_install_core::active_org(data_dir).map_err(|error| error.message)
}

fn valid_active_org(org: &str) -> bool {
    !org.is_empty()
        && org.len() <= 128
        && org
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn ensure_safe_skills_root(data_dir: &Path, skills_root: &Path) -> Result<(), String> {
    let orgs = data_dir.join("orgs");
    if skills_root.strip_prefix(&orgs).is_err() {
        return Err("路由 Skill 目标目录越界".into());
    }
    reject_symlink_path(data_dir)?;
    reject_symlink_path(&orgs)?;
    reject_symlink_path(skills_root)?;
    Ok(())
}

fn route_skill_matches(target: &Path) -> Result<bool, String> {
    reject_symlink_path(target)?;
    if !fs::metadata(target)
        .map_err(|e| format!("检查路由 Skill 失败：{e}"))?
        .is_dir()
    {
        return Ok(false);
    }
    let body_path = target.join("SKILL.md");
    let marker_path = target.join(IMPORT_ORIGIN_FILE);
    reject_symlink_path(&body_path)?;
    reject_symlink_path(&marker_path)?;
    if read_regular_file_limited(&body_path, MAX_ROUTE_SKILL_BODY_BYTES)?.as_deref()
        != Some(SKILL_BODY.as_bytes())
    {
        return Ok(false);
    }
    route_marker_matches(&marker_path)
}

fn legacy_route_skill_matches(target: &Path) -> Result<bool, String> {
    reject_symlink_path(target)?;
    if !fs::metadata(target)
        .map_err(|e| format!("检查旧路由 Skill 失败：{e}"))?
        .is_dir()
    {
        return Ok(false);
    }
    let body_path = target.join("SKILL.md");
    let marker_path = target.join(IMPORT_ORIGIN_FILE);
    reject_symlink_path(&body_path)?;
    reject_symlink_path(&marker_path)?;
    let body = match read_regular_file_limited(&body_path, MAX_ROUTE_SKILL_BODY_BYTES)? {
        Some(body) => body,
        None => return Ok(false),
    };
    let digest = format!("{:x}", Sha256::digest(&body));
    if !matches!(
        digest.as_str(),
        LEGACY_SPLIT_CONNECTORS_SHA256
            | LEGACY_REVISION_2_SHA256
            | LEGACY_SINGLE_SKILL_REVISION_SHA256
            | LEGACY_BUNDLE_PRE_PROGRESS_SHA256
            | LEGACY_BUNDLE_PROGRESS_V1_SHA256
            | LEGACY_BUNDLE_PROGRESS_V2_SHA256
            | LEGACY_BUNDLE_NO_UNINSTALL_CONFIRMATION_SHA256
    ) {
        return Ok(false);
    }
    route_marker_matches(&marker_path)
}

fn route_marker_matches(marker_path: &Path) -> Result<bool, String> {
    let marker: Value = match read_regular_file_limited(marker_path, MAX_IMPORT_ORIGIN_BYTES)?
        .and_then(|body| serde_json::from_slice(&body).ok())
    {
        Some(value) => value,
        None => return Ok(false),
    };
    Ok(marker.get("version").and_then(Value::as_u64) == Some(1)
        && marker.get("repo").and_then(Value::as_str) == Some("csswitch/local")
        && marker.get("sha").and_then(Value::as_str)
            == Some("0000000000000000000000000000000000000000")
        && marker.get("plugin").and_then(Value::as_str) == Some(SKILL_NAME)
        && marker.get("marketplace").and_then(Value::as_str) == Some(MARKETPLACE)
        && marker.get("path").and_then(Value::as_str)
            == Some("embedded/csswitch-external-skill-tools")
        && marker
            .get("importedAt")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && marker.get("license").and_then(Value::as_str) == Some("MIT"))
}

fn read_regular_file_limited(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.len() <= limit as u64 =>
        {
            metadata
        }
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };
    if !metadata.is_file() {
        return Ok(None);
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let opened = match file.metadata() {
        Ok(metadata) if metadata.is_file() && metadata.len() <= limit as u64 => metadata,
        _ => return Ok(None),
    };
    if !opened.is_file() {
        return Ok(None);
    }
    let mut body = Vec::new();
    if Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut body)
        .is_err()
        || body.len() > limit
    {
        return Ok(None);
    }
    Ok(Some(body))
}

fn migrate_legacy_route_body(target: &Path) -> Result<(), String> {
    let body_path = target.join("SKILL.md");
    reject_symlink_path(&body_path)?;
    let temporary = target.join(format!(".SKILL.md.csswitch-{}", unique_suffix()));
    let result = (|| -> Result<(), String> {
        write_new_file(&temporary, SKILL_BODY.as_bytes())?;
        fs::rename(&temporary, &body_path)
            .map_err(|error| format!("升级 CSSwitch 路由 Skill 失败：{error}"))?;
        File::open(target)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("同步 CSSwitch 路由 Skill 升级失败：{error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn reject_symlink_path(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("路由 Skill 路径包含符号链接".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查路由 Skill 路径失败：{error}")),
        }
    }
    Ok(())
}

fn write_new_file(path: &Path, body: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("创建路由 Skill 文件失败：{e}"))?;
    file.write_all(body)
        .map_err(|e| format!("写入路由 Skill 文件失败：{e}"))?;
    file.sync_all()
        .map_err(|e| format!("同步路由 Skill 文件失败：{e}"))
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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

#[cfg(target_os = "macos")]
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn renameatx_np(fromfd: i32, from: *const i8, tofd: i32, to: *const i8, flags: u32) -> i32;
    }
    const AT_FDCWD: i32 = -2;
    const RENAME_EXCL: u32 = 0x0000_0004;
    let from = CString::new(source.as_os_str().as_bytes()).map_err(|_| "临时路径非法")?;
    let to = CString::new(target.as_os_str().as_bytes()).map_err(|_| "目标路径非法")?;
    let result =
        unsafe { renameatx_np(AT_FDCWD, from.as_ptr(), AT_FDCWD, to.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "原子提交路由 Skill 失败：{}",
            io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const i8,
            newdirfd: i32,
            newpath: *const i8,
            flags: u32,
        ) -> i32;
    }
    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;
    let from = CString::new(source.as_os_str().as_bytes()).map_err(|_| "临时路径非法")?;
    let to = CString::new(target.as_os_str().as_bytes()).map_err(|_| "目标路径非法")?;
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            from.as_ptr(),
            AT_FDCWD,
            to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "原子提交路由 Skill 失败：{}",
            io::Error::last_os_error()
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() || fs::symlink_metadata(target).is_ok() {
        return Err("路由 Skill 已存在；拒绝覆盖".into());
    }
    fs::rename(source, target).map_err(|e| format!("提交路由 Skill 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_data(label: &str) -> (PathBuf, PathBuf) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/private/tmp").join(format!(
            "csswitch-route-{label}-{}-{suffix}",
            std::process::id()
        ));
        let data = root.join("sandbox/home/.claude-science");
        fs::create_dir_all(data.join("orgs/org-test/skills")).unwrap();
        fs::write(data.join("active-org.json"), br#"{"org_uuid":"org-test"}"#).unwrap();
        (root, data)
    }

    fn write_managed_route_with_body(data: &Path, body: &[u8]) -> PathBuf {
        let target = route_skill_path(data).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("SKILL.md"), body).unwrap();
        fs::write(
            target.join(IMPORT_ORIGIN_FILE),
            serde_json::to_vec(&json!({
                "version": 1,
                "repo": "csswitch/local",
                "sha": "0000000000000000000000000000000000000000",
                "plugin": SKILL_NAME,
                "marketplace": MARKETPLACE,
                "path": "embedded/csswitch-external-skill-tools",
                "importedAt": "2026-07-13T00:00:00Z",
                "license": "MIT"
            }))
            .unwrap(),
        )
        .unwrap();
        target
    }

    fn write_managed_route(data: &Path) -> PathBuf {
        write_managed_route_with_body(data, SKILL_BODY.as_bytes())
    }

    fn route_before_bundle_uninstall_confirmation() -> String {
        let current = std::str::from_utf8(SKILL_BODY.as_bytes()).unwrap();
        let confirmation_protocol = "If the result is `BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED`, do not call any\nuninstall or detach tool again yet. Show the user the returned `bundle_name`\nand complete `affected_skill_names` list, explain that the operation is\nwhole-bundle only, and ask for an explicit confirm or cancel decision. If the\nuser cancels, stop without another tool call. If the user explicitly confirms,\ncall the same tool once more with the same `skill_name` and the exact returned\n`bundle_id` as `confirm_bundle_id`:\n\n```python\nhost.mcp(\n    \"csswitch-skill-installer\",\n    \"uninstall_external_skill\",\n    skill_name=skill_name,\n    confirm_bundle_id=bundle_id,\n)\n```\n\nThe confirmed call re-finds and re-verifies the installed bundle. If it returns\na new `BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED`, show the new membership and ask\nagain. Never infer or retain an older bundle ID, never confirm on the user's\nbehalf, and never offer partial physical deletion of bundle members.\n\n";
        let current_result = "For single-Skill uninstall, follow the native detach step explicitly returned\nby the connector, then verify that `skill(skill_name)` no longer loads. A\n`BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED` result is non-mutating and requires the\nexplicit two-step flow above. A `BUNDLE_UNINSTALLED_DETACHED` result already\nmeans the entire owning bundle was batch-detached and quarantined; do not detach\nits members again. Report every response faithfully.";
        let prior_result = "For single-Skill uninstall, follow the native detach step explicitly returned\nby the connector, then verify that `skill(skill_name)` no longer loads. A\n`BUNDLE_UNINSTALLED_DETACHED` result already means the entire owning bundle was\nbatch-detached and quarantined; do not detach its members again. Report every\nresponse faithfully.";
        assert!(current.contains(confirmation_protocol));
        assert!(current.contains(current_result));
        current
            .replace(confirmation_protocol, "")
            .replace(current_result, prior_result)
    }

    #[test]
    fn ensures_route_atomically_and_idempotently() {
        let (root, data) = test_data("ensure");
        assert!(ensure_route_skill(&data).unwrap());
        let target = route_skill_path(&data).unwrap();
        assert!(target.is_dir());
        assert!(inspect_route_skill(&data).unwrap());
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_same_name_user_or_modified_content_untouched() {
        let (root, data) = test_data("preserve");
        let target = route_skill_path(&data).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("SKILL.md"), b"user content").unwrap();
        assert!(ensure_route_skill(&data).is_err());
        assert_eq!(fs::read(target.join("SKILL.md")).unwrap(), b"user content");
        fs::remove_dir_all(&target).unwrap();

        let target = write_managed_route(&data);
        fs::write(target.join("SKILL.md"), b"modified").unwrap();
        assert!(ensure_route_skill(&data).is_err());
        assert_eq!(fs::read(target.join("SKILL.md")).unwrap(), b"modified");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_oversized_route_body_and_marker_without_unbounded_reads() {
        let (root, data) = test_data("oversized-route-files");
        let target = write_managed_route(&data);
        fs::write(
            target.join("SKILL.md"),
            vec![b'x'; MAX_ROUTE_SKILL_BODY_BYTES + 1],
        )
        .unwrap();
        assert!(!inspect_route_skill(&data).unwrap());

        fs::write(target.join("SKILL.md"), SKILL_BODY.as_bytes()).unwrap();
        fs::write(
            target.join(IMPORT_ORIGIN_FILE),
            vec![b'x'; MAX_IMPORT_ORIGIN_BYTES + 1],
        )
        .unwrap();
        assert!(!inspect_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_only_exact_revision_two_managed_route() {
        let (root, data) = test_data("migrate-revision-two");
        let legacy = include_bytes!(
            "../../resources/skills/csswitch-external-skill-tools/legacy-revision2.md"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(legacy)),
            LEGACY_REVISION_2_SHA256
        );
        let target = write_managed_route_with_body(&data, legacy);
        assert!(ensure_route_skill(&data).unwrap());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            SKILL_BODY.as_bytes()
        );
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_exact_split_connector_managed_route() {
        let (root, data) = test_data("migrate-split-connectors");
        let legacy = include_bytes!(
            "../../resources/skills/csswitch-external-skill-tools/legacy-split-connectors.md"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(legacy)),
            LEGACY_SPLIT_CONNECTORS_SHA256
        );
        let target = write_managed_route_with_body(&data, legacy);
        assert!(ensure_route_skill(&data).unwrap());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            SKILL_BODY.as_bytes()
        );
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_exact_bundle_route_before_progress_protocol() {
        let (root, data) = test_data("migrate-bundle-pre-progress");
        let legacy = include_bytes!(
            "../../resources/skills/csswitch-external-skill-tools/legacy-bundle-pre-progress.md"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(legacy)),
            LEGACY_BUNDLE_PRE_PROGRESS_SHA256
        );
        let target = write_managed_route_with_body(&data, legacy);
        assert!(ensure_route_skill(&data).unwrap());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            SKILL_BODY.as_bytes()
        );
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_exact_bundle_route_before_fast_polling_protocol() {
        let (root, data) = test_data("migrate-bundle-progress-v1");
        let legacy = include_bytes!(
            "../../resources/skills/csswitch-external-skill-tools/legacy-bundle-progress-v1.md"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(legacy)),
            LEGACY_BUNDLE_PROGRESS_V1_SHA256
        );
        let target = write_managed_route_with_body(&data, legacy);
        assert!(ensure_route_skill(&data).unwrap());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            SKILL_BODY.as_bytes()
        );
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_exact_bundle_route_before_gateway_long_polling() {
        let (root, data) = test_data("migrate-bundle-progress-v2");
        let current = route_before_bundle_uninstall_confirmation();
        let poll_protocol = "For `HOST_ACCESS_REQUIRED`, submit the returned `request.payload` exactly once\nunder its original request ID. Then use `poll_external_skill_request` with that\nsame `request_id`; omit `last_sequence` on the first call, and pass the previous\n`PROCESSING.sequence` on later calls. The polling tool waits inside the gateway\nand returns bounded `phase`, heartbeat timestamps, elapsed time, and\n`deadline_at`, or the final response. Do not read the bridge files repeatedly,\nrun `sleep`, or start a shell/Python polling loop. Never write the request again\nand never call the install/uninstall tool again merely because a bundle is still\ndownloading. Success, failure, timeout, and interrupted-host recovery all arrive\nas a final response after `.processing` is cleared.";
        let prior_protocol = "For `HOST_ACCESS_REQUIRED`, submit the returned `request.payload` exactly once\nunder its original request ID. Immediately try that ID's `response_filename`;\nif it is absent, read `status_filename`: `PROCESSING` includes a bounded\n`phase`, heartbeat timestamps, elapsed time, and `deadline_at`. Poll only the\nsame response filename. Do not run a dedicated `sleep` command or a long-running\nshell/Python polling loop; every poll must be one quick file read. Never write\nthe request again and never call the MCP tool again merely because a bundle is\nstill downloading. Success, failure, timeout, and interrupted-host recovery all\narrive as a final response after `.processing` is cleared.";
        assert!(current.contains(poll_protocol));
        let legacy = current.replace(poll_protocol, prior_protocol);
        assert_eq!(
            format!("{:x}", Sha256::digest(legacy.as_bytes())),
            LEGACY_BUNDLE_PROGRESS_V2_SHA256
        );
        let target = write_managed_route_with_body(&data, legacy.as_bytes());
        assert!(ensure_route_skill(&data).unwrap());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            SKILL_BODY.as_bytes()
        );
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_exact_bundle_route_before_uninstall_confirmation() {
        let (root, data) = test_data("migrate-bundle-uninstall-confirmation");
        let legacy = route_before_bundle_uninstall_confirmation();
        assert_eq!(
            format!("{:x}", Sha256::digest(legacy.as_bytes())),
            LEGACY_BUNDLE_NO_UNINSTALL_CONFIRMATION_SHA256
        );
        let target = write_managed_route_with_body(&data, legacy.as_bytes());
        assert!(ensure_route_skill(&data).unwrap());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            SKILL_BODY.as_bytes()
        );
        assert!(!ensure_route_skill(&data).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_org_without_removing_target() {
        use std::os::unix::fs::symlink;

        let (root, data) = test_data("symlink");
        let org = data.join("orgs/org-test");
        fs::remove_dir_all(&org).unwrap();
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, &org).unwrap();
        assert!(ensure_route_skill(&data).is_err());
        assert!(outside.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
