use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const SSH_STUB_MARKER: &str = "# CSSwitch managed system SSH config bridge v1";
const SSH_STUB_MARKER_V2: &str = "# CSSwitch managed system SSH config bridge v2";

fn managed_ssh_stub_text(text: &str, expected_system_config: &Path) -> bool {
    let escaped = expected_system_config
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    if text == format!("{SSH_STUB_MARKER}\nInclude \"{escaped}\"\n") {
        return true;
    }
    let lines = text.lines().collect::<Vec<_>>();
    lines.len() == 3
        && lines[0] == SSH_STUB_MARKER_V2
        && lines[1].strip_prefix("Host ").is_some_and(|hosts| {
            let aliases = hosts.split_ascii_whitespace().collect::<Vec<_>>();
            !aliases.is_empty()
                && aliases
                    .iter()
                    .all(|alias| crate::runtime::ssh_bridge::is_concrete_alias(alias))
        })
        && lines[2] == format!("Include \"{escaped}\"")
}

fn system_ssh_config_path_for_home(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能启用系统 SSH 配置。".into());
    }
    let config = home.join(".ssh").join("config");
    if !config.is_file() {
        return Err("未找到系统 ~/.ssh/config，不能启用系统 SSH 配置。".into());
    }
    Ok(config)
}

pub(crate) fn system_ssh_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能启用系统 SSH 配置。")?;
    system_ssh_config_path_for_home(&home)
}

/// Revoke only the narrow config stub created by CSSwitch. Foreign files,
/// symlinks and special files fail closed instead of being deleted or exposed
/// to a later isolated Science launch.
pub(crate) fn remove_managed_sandbox_ssh_stub(sandbox_home: &Path) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能撤销系统 SSH 配置。")?;
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能撤销系统 SSH 配置。".into());
    }
    remove_managed_sandbox_ssh_stub_for_config(sandbox_home, &home.join(".ssh/config"))
}

pub(crate) fn validate_managed_sandbox_ssh_stub(
    sandbox_home: &Path,
    expected_hosts: &[String],
) -> Result<(), String> {
    let expected_system_config = system_ssh_config_path()?;
    validate_managed_sandbox_ssh_stub_for_config(
        sandbox_home,
        &expected_system_config,
        expected_hosts,
    )
}

fn validate_managed_sandbox_ssh_stub_for_config(
    sandbox_home: &Path,
    expected_system_config: &Path,
    expected_hosts: &[String],
) -> Result<(), String> {
    if expected_hosts.is_empty()
        || !expected_hosts
            .iter()
            .all(|host| crate::runtime::ssh_bridge::is_concrete_alias(host))
    {
        return Err("没有可供 Science 校验的安全 SSH Host alias".into());
    }
    let ssh_dir = sandbox_home.join(".ssh");
    let dir_metadata = std::fs::symlink_metadata(&ssh_dir)
        .map_err(|_| "隔离 SSH 配置目录缺失，拒绝复用运行中的 Science")?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !dir_metadata.file_type().is_dir()
        || dir_metadata.file_type().is_symlink()
        || dir_metadata.uid() != uid
        || dir_metadata.mode() & 0o022 != 0
    {
        return Err("隔离 SSH 配置目录不安全，拒绝复用运行中的 Science".into());
    }
    let config = ssh_dir.join("config");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&config)
        .map_err(|_| "隔离 SSH config 缺失或不安全，拒绝复用运行中的 Science")?;
    let metadata = file.metadata().map_err(|_| "无法检查隔离 SSH config")?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 128 * 1024
    {
        return Err("隔离 SSH config 不是安全的 CSSwitch 管理文件".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| "无法读取隔离 SSH config")?;
    let expected_host_line = format!("Host {}", expected_hosts.join(" "));
    let lines = text.lines().collect::<Vec<_>>();
    if !managed_ssh_stub_text(&text, expected_system_config)
        || lines.first().copied() != Some(SSH_STUB_MARKER_V2)
        || lines.get(1).copied() != Some(expected_host_line.as_str())
    {
        return Err("隔离 SSH config 与当前 CSSwitch SSH bridge 不一致".into());
    }
    Ok(())
}

fn remove_managed_sandbox_ssh_stub_for_config(
    sandbox_home: &Path,
    expected_system_config: &Path,
) -> Result<(), String> {
    let mut ancestor = Some(sandbox_home);
    for _ in 0..3 {
        let Some(path) = ancestor else { break };
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("隔离 SSH 配置路径包含符号链接，拒绝撤销授权".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查隔离 SSH 配置路径失败：{error}")),
        }
        ancestor = path.parent();
    }
    let ssh_dir = sandbox_home.join(".ssh");
    let dir_metadata = match std::fs::symlink_metadata(&ssh_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("检查隔离 SSH 配置目录失败：{error}")),
    };
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !dir_metadata.file_type().is_dir() || dir_metadata.uid() != uid {
        return Err("隔离 SSH 配置目录不安全，拒绝撤销授权".into());
    }
    let config = ssh_dir.join("config");
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&config)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_dir(&ssh_dir);
            return Ok(());
        }
        Err(error) => return Err(format!("检查隔离 SSH config 失败：{error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("检查隔离 SSH config 失败：{error}"))?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.len() > 128 * 1024 {
        return Err("隔离 SSH config 不是 CSSwitch 管理的安全普通文件".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("读取隔离 SSH config 失败：{error}"))?;
    if !managed_ssh_stub_text(&text, expected_system_config) {
        return Err("隔离 SSH config 不是 CSSwitch 管理的入口，拒绝删除".into());
    }
    std::fs::remove_file(&config).map_err(|error| format!("撤销隔离 SSH config 失败：{error}"))?;
    let _ = std::fs::remove_dir(&ssh_dir);
    Ok(())
}

pub(crate) fn validate_runtime_ports(proxy_port: u16, sandbox_port: u16) -> Result<(), String> {
    crate::config::validate_runtime_ports(proxy_port, sandbox_port)?;
    let preview_port = sandbox_port
        .checked_add(1)
        .ok_or("沙箱端口必须小于 65535，才能分配隔离预览端口。")?;
    if preview_port == 8765 {
        return Err("沙箱预览端口会命中真实 Science 保留端口 8765。".into());
    }
    if preview_port == proxy_port {
        return Err("代理端口不能与沙箱预览端口相同。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    use super::{
        remove_managed_sandbox_ssh_stub_for_config, system_ssh_config_path_for_home,
        validate_managed_sandbox_ssh_stub_for_config, validate_runtime_ports, SSH_STUB_MARKER,
        SSH_STUB_MARKER_V2,
    };

    #[test]
    fn validate_runtime_ports_rejects_reserved_real_science_port() {
        assert!(validate_runtime_ports(8765, 18991).is_err());
        assert!(validate_runtime_ports(18991, 8765).is_err());
    }

    #[test]
    fn validate_runtime_ports_rejects_zero_and_same_port() {
        assert!(validate_runtime_ports(0, 18991).is_err());
        assert!(validate_runtime_ports(18991, 0).is_err());
        assert!(validate_runtime_ports(18991, 18991).is_err());
        assert!(validate_runtime_ports(8991, 8990).is_err());
        assert!(validate_runtime_ports(18991, 8764).is_err());
        assert!(validate_runtime_ports(18991, u16::MAX).is_err());
        assert!(
            crate::config::validate_runtime_ports(8991, 8990).is_ok(),
            "legacy config must remain readable so the UI can repair it"
        );
    }

    #[test]
    fn validate_runtime_ports_accepts_distinct_nonreserved_ports() {
        assert!(validate_runtime_ports(18991, 18992).is_ok());
    }

    #[test]
    fn system_ssh_config_requires_an_absolute_home_and_regular_target() {
        assert!(system_ssh_config_path_for_home(std::path::Path::new("relative-home")).is_err());
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-config-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        assert!(system_ssh_config_path_for_home(&home).is_err());
        std::fs::write(home.join(".ssh/config"), "Host test\n").unwrap();
        assert_eq!(
            system_ssh_config_path_for_home(&home).unwrap(),
            home.join(".ssh/config")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn managed_sandbox_ssh_stub_is_revoked_without_touching_foreign_files() {
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-stub-test-{}",
            crate::config::new_id()
        ));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let config = home.join(".ssh/config");
        let expected_system_config = home.join("real-home/.ssh/config");
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_MARKER}\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        remove_managed_sandbox_ssh_stub_for_config(&home, &expected_system_config).unwrap();
        assert!(!config.exists());

        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            &config,
            format!("{SSH_STUB_MARKER}\nInclude \"/different/config\"\n\nHost foreign\n"),
        )
        .unwrap();
        assert!(
            remove_managed_sandbox_ssh_stub_for_config(&home, &expected_system_config).is_err()
        );
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("Host foreign"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn running_stub_validation_requires_exact_v2_aliases_and_private_file() {
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-running-stub-test-{}",
            crate::config::new_id()
        ));
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let expected_system_config = home.join("real-home/.ssh/config");
        let config = ssh_dir.join("config");
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_MARKER_V2}\nHost alpha beta\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let expected = vec!["alpha".to_string(), "beta".to_string()];
        assert!(validate_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            &expected
        )
        .is_ok());
        assert!(validate_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            &["alpha".to_string()]
        )
        .is_err());
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            &expected
        )
        .is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn sandbox_ssh_revocation_rejects_fifo_and_symlinked_home_without_blocking() {
        let base = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-special-test-{}",
            crate::config::new_id()
        ));
        let home = base.join("home");
        let expected = base.join("real/.ssh/config");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let fifo = home.join(".ssh/config");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: fifo_c is a valid NUL-terminated path and mode is conventional.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(remove_managed_sandbox_ssh_stub_for_config(&home, &expected).is_err());
        assert!(std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo());

        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join(".ssh")).unwrap();
        std::fs::write(
            outside.join(".ssh/config"),
            format!("{SSH_STUB_MARKER}\nInclude \"{}\"\n", expected.display()),
        )
        .unwrap();
        let linked_home = base.join("linked-home");
        std::os::unix::fs::symlink(&outside, &linked_home).unwrap();
        assert!(remove_managed_sandbox_ssh_stub_for_config(&linked_home, &expected).is_err());
        assert!(outside.join(".ssh/config").is_file());
        let _ = std::fs::remove_dir_all(base);
    }
}
