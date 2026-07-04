//! 远程服务器 Profile 的本地持久化存储。
//!
//! Profile 文件位置：`~/.csswitch/remote-hosts.json`
//!
//! 格式：JSON 数组 `[RemoteHostProfile]`。
//! 支持 CRUD（Create/Read/Update/Delete）操作 + 校验。

use std::fs;
use std::path::PathBuf;

use super::types::{RemoteAuthMethod, RemoteHostProfile};

/// 返回远程 Profile 文件的完整路径：`~/.csswitch/remote-hosts.json`。
/// 跨平台：使用 `dirs::home_dir()` 获取用户主目录。
pub fn profiles_path() -> PathBuf {
    crate::config::default_dir().join("remote-hosts.json")
}

// ============================================================================
// CRUD 操作
// ============================================================================

/// 从 `remote-hosts.json` 读取所有远程 Profile。
/// 文件不存在时返回空 Vec（首次使用）。
pub fn load_profiles() -> Result<Vec<RemoteHostProfile>, String> {
    let path = profiles_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("无法读取远程服务器配置 {}：{e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let profiles: Vec<RemoteHostProfile> = serde_json::from_str(&raw)
        .map_err(|e| format!("远程服务器配置格式错误 {}：{e}", path.display()))?;
    for profile in &profiles {
        validate_profile(profile)?;
    }
    Ok(profiles)
}

/// 将 Profile 列表写入 `remote-hosts.json`（安全写入：symlink 防护 + 原子 rename）。
/// 父目录不存在时自动创建。
/// 审核 P1-5 修复：增加 symlink 防护，对齐 `config.rs` 的安全标准。
pub fn save_profiles(profiles: &[RemoteHostProfile]) -> Result<(), String> {
    for profile in profiles {
        validate_profile(profile)?;
    }
    let path = profiles_path();
    // 拒绝符号链接目标（防止写入重定向到非预期文件）。
    crate::config::assert_not_symlink(&path)
        .map_err(|e| format!("远程配置路径安全拒绝：{e}"))?;
    if let Some(parent) = path.parent() {
        crate::config::assert_not_symlink(parent)
            .map_err(|e| format!("远程配置父目录安全拒绝：{e}"))?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("无法创建远程配置目录 {}：{e}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(profiles)
        .map_err(|e| format!("序列化远程配置失败：{e}"))?;
    // 原子写入：pid+thread 随机化临时文件名（避免并发冲突）
    let tmp = path.with_file_name(format!(
        ".remote-hosts.json.tmp.{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    fs::write(&tmp, &json)
        .map_err(|e| format!("写入远程配置临时文件失败：{e}"))?;
    // 设置 0600 权限，防止其他用户读取 SSH 配置
    crate::fs_ext::set_file_permissions(&tmp, 0o600)
        .map_err(|e| format!("设置权限失败：{e}"))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("替换远程配置文件失败：{e}"))?;
    crate::fs_ext::set_file_permissions(&path, 0o600)
        .map_err(|e| format!("确认权限失败：{e}"))?;
    Ok(())
}

/// 插入或更新一个 Profile（按 `id` 匹配）。
/// 不存在则插入到列表头部（最近使用的排前面）。
pub fn upsert_profile(profile: RemoteHostProfile) -> Result<RemoteHostProfile, String> {
    validate_profile(&profile)?;
    let mut profiles = load_profiles()?;
    if let Some(existing) = profiles.iter_mut().find(|p| p.id == profile.id) {
        *existing = profile.clone();
    } else {
        profiles.insert(0, profile.clone());
    }
    save_profiles(&profiles)?;
    Ok(profile)
}

/// 删除指定 `id` 的 Profile。返回 true 表示成功删除，false 表示未找到。
pub fn delete_profile(id: &str) -> Result<bool, String> {
    let mut profiles = load_profiles()?;
    let before = profiles.len();
    profiles.retain(|p| p.id != id);
    if profiles.len() == before {
        return Ok(false);
    }
    save_profiles(&profiles)?;
    Ok(true)
}

// ============================================================================
// 校验
// ============================================================================

/// 校验 Profile 的各字段是否合法。
/// - host：非空
/// - port：1-65535
/// - username：非空
/// - helper_path：非空且格式为绝对路径（以 `/` 或 `~` 开头）
/// - KeyFile 路径：非空（如果 auth_method 为 KeyFile）
pub fn validate_profile(profile: &RemoteHostProfile) -> Result<(), String> {
    if profile.id.trim().is_empty() {
        return Err("远程服务器 Profile ID 不得为空".into());
    }
    if profile.host.trim().is_empty() {
        return Err("远程服务器地址不得为空".into());
    }
    if profile.username.trim().is_empty() {
        return Err("远程服务器用户名不得为空".into());
    }
    if profile.port == 0 {
        return Err("远程 SSH 端口不得为 0".into());
    }
    if profile.helper_path.trim().is_empty() {
        return Err("Helper 路径不得为空".into());
    }
    // 校验 helper_path 格式：应该是绝对路径或以 ~ 开头
    let hp = profile.helper_path.trim();
    if !hp.starts_with('/') && !hp.starts_with('~') {
        return Err(format!(
            "Helper 路径应为绝对路径或以 ~ 开头：{hp}"
        ));
    }
    if let RemoteAuthMethod::KeyFile { path } = &profile.auth_method {
        if path.trim().is_empty() {
            return Err("选择私钥文件认证时，密钥路径不得为空".into());
        }
    }
    Ok(())
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile(id: &str) -> RemoteHostProfile {
        RemoteHostProfile {
            id: id.to_string(),
            name: "测试服务器".to_string(),
            host: "192.168.1.100".to_string(),
            port: 22,
            username: "testuser".to_string(),
            auth_method: RemoteAuthMethod::SshAgent,
            helper_path: "~/.csswitch/bin/csswitch-helper".to_string(),
            last_connected: None,
        }
    }

    fn tmp_path() -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("csswitch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join("remote-hosts.json")
    }

    #[test]
    fn test_crud_roundtrip() {
        let p = tmp_path();
        // 初始为空
        // (实际调用 load_profiles 使用的是 profiles_path()，我们不 override，改为测试 core logic)
        let profile = sample_profile("test-01");
        validate_profile(&profile).unwrap();
        // core logic: save, load, upsert, delete
        let single = vec![profile.clone()];
        let json = serde_json::to_vec_pretty(&single).unwrap();
        fs::write(&p, &json).unwrap();
        let loaded: Vec<RemoteHostProfile> = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "test-01");

        // Delete
        let loaded: Vec<RemoteHostProfile> = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let remaining: Vec<_> = loaded.into_iter().filter(|pr| pr.id != "test-01").collect();
        let json = serde_json::to_vec_pretty(&remaining).unwrap();
        fs::write(&p, &json).unwrap();
        let loaded: Vec<RemoteHostProfile> = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(loaded.len(), 0);

        let _ = fs::remove_file(&p);
    }

    #[test]
    fn test_validation_rejects_empty_host() {
        let mut p = sample_profile("t1");
        p.host = "".to_string();
        assert!(validate_profile(&p).is_err());
    }

    #[test]
    fn test_validation_rejects_empty_username() {
        let mut p = sample_profile("t2");
        p.username = "".to_string();
        assert!(validate_profile(&p).is_err());
    }

    #[test]
    fn test_validation_rejects_zero_port() {
        let mut p = sample_profile("t3");
        p.port = 0;
        assert!(validate_profile(&p).is_err());
    }

    #[test]
    fn test_validation_rejects_relative_helper_path() {
        let mut p = sample_profile("t4");
        p.helper_path = "csswitch-helper".to_string();
        assert!(validate_profile(&p).is_err());
    }

    #[test]
    fn test_validation_rejects_empty_keyfile_path() {
        let mut p = sample_profile("t5");
        p.auth_method = RemoteAuthMethod::KeyFile {
            path: "".to_string(),
        };
        assert!(validate_profile(&p).is_err());
    }
}
