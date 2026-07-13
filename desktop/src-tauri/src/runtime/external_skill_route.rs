use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub(crate) const SKILL_NAME: &str = "csswitch-external-skill-tools";
const IMPORT_ORIGIN_FILE: &str = ".import-origin";
const MARKETPLACE: &str = "csswitch-system-bridge";
const SKILL_BODY: &str =
    include_str!("../../resources/skills/csswitch-external-skill-tools/SKILL.md");

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
    let target = route_skill_path(data_dir)?;
    if !target.exists() && fs::symlink_metadata(&target).is_err() {
        return Ok(false);
    }
    route_skill_matches(&target)
}

fn route_skill_path(data_dir: &Path) -> Result<PathBuf, String> {
    let active_org = read_active_org(data_dir)?;
    let skills_root = data_dir.join("orgs").join(active_org).join("skills");
    ensure_safe_skills_root(data_dir, &skills_root)?;
    Ok(skills_root.join(SKILL_NAME))
}

fn read_active_org(data_dir: &Path) -> Result<String, String> {
    if !data_dir.is_absolute() {
        return Err("Science data-dir 必须是绝对路径".into());
    }
    reject_symlink_path(data_dir)?;
    let active = data_dir.join("active-org.json");
    reject_symlink_path(&active)?;
    let body = fs::read(&active).map_err(|_| "读取 Science active-org.json 失败")?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| "Science active-org.json 非法")?;
    let org = value
        .get("org_uuid")
        .and_then(Value::as_str)
        .ok_or("active-org.json 缺少 org_uuid")?;
    let valid = !org.is_empty()
        && org.len() <= 128
        && org
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
    if !valid {
        return Err("active org 标识非法".into());
    }
    Ok(org.to_string())
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
    if fs::read(&body_path).ok().as_deref() != Some(SKILL_BODY.as_bytes()) {
        return Ok(false);
    }
    let marker: Value = match fs::read(&marker_path)
        .ok()
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

    fn write_managed_route(data: &Path) -> PathBuf {
        let target = route_skill_path(data).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("SKILL.md"), SKILL_BODY).unwrap();
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
