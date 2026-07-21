use std::collections::HashSet;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const SSH_STUB_V1_MARKER: &str = "# CSSwitch managed system SSH config bridge v1";
const SSH_STUB_V2_MARKER: &str = "# CSSwitch managed system SSH config bridge v2";
const SSH_STUB_MAX_ALIASES: usize = 512;
const SSH_STUB_MAX_BYTES: u64 = 65_536;

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
    if !metadata.is_file() || metadata.uid() != uid || metadata.len() > SSH_STUB_MAX_BYTES {
        return Err("隔离 SSH config 不是 CSSwitch 管理的安全普通文件".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("读取隔离 SSH config 失败：{error}"))?;
    let escaped = expected_system_config
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    if !is_managed_sandbox_ssh_stub_text(&text, &escaped) {
        return Err("隔离 SSH config 不是 CSSwitch 管理的入口，拒绝删除".into());
    }
    std::fs::remove_file(&config).map_err(|error| format!("撤销隔离 SSH config 失败：{error}"))?;
    let _ = std::fs::remove_dir(&ssh_dir);
    Ok(())
}

fn is_managed_sandbox_ssh_stub_text(text: &str, escaped_system_config: &str) -> bool {
    let include = format!("Include \"{escaped_system_config}\"");
    let v1 = format!("{SSH_STUB_V1_MARKER}\n{include}\n");
    if text == v1 {
        return true;
    }
    if text.len() as u64 > SSH_STUB_MAX_BYTES || !text.ends_with('\n') {
        return false;
    }

    let mut lines = text.lines();
    if lines.next() != Some(SSH_STUB_V2_MARKER) || lines.next() != Some(include.as_str()) {
        return false;
    }

    let mut aliases = HashSet::new();
    for line in lines {
        let Some(alias) = line.strip_prefix("Host ") else {
            return false;
        };
        if !is_safe_projected_ssh_alias(alias)
            || !aliases.insert(alias)
            || aliases.len() > SSH_STUB_MAX_ALIASES
        {
            return false;
        }
    }
    true
}

fn is_safe_projected_ssh_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.bytes().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'-'
                    | b'.'
                    | b'_'
                    | b':'
                    | b'@'
                    | b'%'
                    | b'+'
            )
        })
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
    use std::os::unix::fs::FileTypeExt;

    use super::{
        is_managed_sandbox_ssh_stub_text, remove_managed_sandbox_ssh_stub_for_config,
        system_ssh_config_path_for_home, validate_runtime_ports, SSH_STUB_V1_MARKER,
        SSH_STUB_V2_MARKER,
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
                "{SSH_STUB_V1_MARKER}\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        remove_managed_sandbox_ssh_stub_for_config(&home, &expected_system_config).unwrap();
        assert!(!config.exists());

        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_V2_MARKER}\nInclude \"{}\"\nHost alpha\nHost beta-2\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        remove_managed_sandbox_ssh_stub_for_config(&home, &expected_system_config).unwrap();
        assert!(!config.exists());

        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            &config,
            format!("{SSH_STUB_V1_MARKER}\nInclude \"/different/config\"\n\nHost foreign\n"),
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
            format!("{SSH_STUB_V1_MARKER}\nInclude \"{}\"\n", expected.display()),
        )
        .unwrap();
        let linked_home = base.join("linked-home");
        std::os::unix::fs::symlink(&outside, &linked_home).unwrap();
        assert!(remove_managed_sandbox_ssh_stub_for_config(&linked_home, &expected).is_err());
        assert!(outside.join(".ssh/config").is_file());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn managed_v2_stub_accepts_only_canonical_safe_unique_aliases() {
        let escaped = "/tmp/real-home/.ssh/config";
        let valid =
            format!("{SSH_STUB_V2_MARKER}\nInclude \"{escaped}\"\nHost alpha\nHost beta-2\n");
        assert!(is_managed_sandbox_ssh_stub_text(&valid, escaped));

        for invalid in [
            format!("{SSH_STUB_V2_MARKER}\nInclude \"{escaped}\"\nHost alpha\nHost alpha\n"),
            format!("{SSH_STUB_V2_MARKER}\nInclude \"{escaped}\"\nHost *.example\n"),
            format!("{SSH_STUB_V2_MARKER}\nInclude \"{escaped}\"\nHost alpha\nUser foreign\n"),
            format!("{SSH_STUB_V2_MARKER}\nInclude \"/different/config\"\nHost alpha\n"),
            format!("{SSH_STUB_V2_MARKER}\nInclude \"{escaped}\"\nHost alpha"),
        ] {
            assert!(!is_managed_sandbox_ssh_stub_text(&invalid, escaped));
        }

        let too_many_aliases = (0..=super::SSH_STUB_MAX_ALIASES)
            .map(|index| format!("Host alias-{index}\n"))
            .collect::<String>();
        let over_limit = format!("{SSH_STUB_V2_MARKER}\nInclude \"{escaped}\"\n{too_many_aliases}");
        assert!(!is_managed_sandbox_ssh_stub_text(&over_limit, escaped));
    }
}
