use std::path::{Path, PathBuf};
use std::process::Command;

const TRUSTED_SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

fn copy_locale_environment(command: &mut Command) {
    for key in ["LANG", "LC_ALL", "LC_CTYPE"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if !path.is_absolute() {
        return Err("Science 隔离目录必须是绝对路径".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match current.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Science 隔离目录路径包含符号链接".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("无法检查 Science 隔离目录".into()),
        }
    }
    std::fs::create_dir_all(path).map_err(|_| "无法创建 Science 隔离目录".to_string())?;
    let metadata = path
        .symlink_metadata()
        .map_err(|_| "无法检查 Science 隔离目录".to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Science 隔离目录不是安全的普通目录".into());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "无法收紧 Science 隔离目录权限".to_string())
}

pub fn configure_science_command_environment(
    command: &mut Command,
    home: &Path,
    strict_linux: bool,
) -> Result<(), String> {
    if !strict_linux {
        command.env("HOME", home);
        return Ok(());
    }
    let config = home.join(".config");
    let data = home.join(".local/share");
    let cache = home.join(".cache");
    let state = home.join(".local/state");
    let runtime = home.join(".xdg-runtime");
    let temporary = home.join(".tmp");
    for directory in [home, &config, &data, &cache, &state, &runtime, &temporary] {
        ensure_private_directory(directory)?;
    }
    command.env_clear();
    command
        .env("HOME", home)
        .env("PATH", TRUSTED_SYSTEM_PATH)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_DATA_HOME", data)
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("TMPDIR", temporary);
    copy_locale_environment(command);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::configure_science_command_environment;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(label: &str) -> PathBuf {
        let private_tmp = PathBuf::from("/private/tmp");
        let base = if private_tmp.is_dir() {
            private_tmp
        } else {
            std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir())
        };
        base.join(format!(
            "csswitch-science-env-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn strict_linux_environment_is_private_complete_and_secret_free() {
        let home = test_dir("strict");
        let mut command = Command::new("/usr/bin/env");
        command
            .env("OPENAI_API_KEY", "host-secret")
            .env("ANTHROPIC_AUTH_TOKEN", "host-secret")
            .env("GIT_SSH_COMMAND", "host-secret")
            .env("SSH_AUTH_SOCK", "/host/agent.sock")
            .env("XDG_CONFIG_HOME", "/host/config");
        configure_science_command_environment(&mut command, &home, true).unwrap();
        let output = command.output().unwrap();
        assert!(output.status.success());
        let environment = String::from_utf8(output.stdout).unwrap();
        for expected in [
            format!("HOME={}", home.display()),
            "PATH=/usr/local/bin:/usr/bin:/bin".to_string(),
            format!("XDG_CONFIG_HOME={}", home.join(".config").display()),
            format!("XDG_DATA_HOME={}", home.join(".local/share").display()),
            format!("XDG_CACHE_HOME={}", home.join(".cache").display()),
            format!("XDG_STATE_HOME={}", home.join(".local/state").display()),
            format!("XDG_RUNTIME_DIR={}", home.join(".xdg-runtime").display()),
            format!("TMPDIR={}", home.join(".tmp").display()),
        ] {
            assert!(environment.contains(&expected), "missing {expected}");
        }
        assert!(!environment.contains("host-secret"));
        assert!(!environment.contains("/host/config"));
        assert!(!environment.contains("SSH_AUTH_SOCK="));
        for relative in [
            ".config",
            ".local/share",
            ".cache",
            ".local/state",
            ".xdg-runtime",
            ".tmp",
        ] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }
}
