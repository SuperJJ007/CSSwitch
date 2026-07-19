use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RuntimeEnvironmentBlocker {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

pub(crate) fn platform_capabilities() -> Value {
    let support_tier = if cfg!(target_os = "linux") {
        "beta"
    } else if cfg!(target_os = "macos") {
        "stable"
    } else {
        "unsupported"
    };
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "support_tier": support_tier,
        "official_mode_supported": cfg!(target_os = "macos"),
    })
}

pub(crate) const fn official_mode_supported() -> bool {
    cfg!(target_os = "macos")
}

const TRUSTED_SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub(crate) fn bash_bin() -> PathBuf {
    PathBuf::from("/bin/bash")
}

fn copy_locale_environment(command: &mut Command) {
    for key in ["LANG", "LC_ALL", "LC_CTYPE"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn configure_science_command_for_platform(
    command: &mut Command,
    home: &Path,
    strict_linux: bool,
) -> Result<(), String> {
    csswitch_skill_install_core::configure_science_command_environment(command, home, strict_linux)
}

pub(crate) fn configure_science_command(command: &mut Command, home: &Path) -> Result<(), String> {
    configure_science_command_for_platform(command, home, cfg!(target_os = "linux"))
}

fn configure_runtime_script_command_for_platform(
    command: &mut Command,
    allow_ssh_agent: bool,
    strict_linux: bool,
) -> Result<(), String> {
    if !strict_linux {
        return Ok(());
    }
    let real_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("Linux runtime 脚本缺少安全的外层 HOME")?;
    command.env_clear();
    command
        .env("HOME", real_home)
        .env("PATH", TRUSTED_SYSTEM_PATH);
    copy_locale_environment(command);
    if allow_ssh_agent {
        if let Some(raw) = std::env::var_os("SSH_AUTH_SOCK") {
            let socket = validate_ssh_agent_socket(Path::new(&raw))?;
            command.env("SSH_AUTH_SOCK", socket);
        }
    }
    Ok(())
}

fn validate_ssh_agent_socket(socket: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::FileTypeExt;

    if !socket.is_absolute() {
        return Err("系统 SSH agent socket 不是绝对路径".into());
    }
    let metadata = socket
        .symlink_metadata()
        .map_err(|_| "系统 SSH agent socket 不可访问")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        return Err("系统 SSH agent socket 不是安全的 Unix socket".into());
    }
    Ok(socket.to_path_buf())
}

pub(crate) fn configure_runtime_script_command(
    command: &mut Command,
    allow_ssh_agent: bool,
) -> Result<(), String> {
    configure_runtime_script_command_for_platform(
        command,
        allow_ssh_agent,
        cfg!(target_os = "linux"),
    )
}

pub(crate) fn installed_science_bin(home: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        return home.join(".local/bin/claude-science");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = home;
        PathBuf::from("/Applications/Claude Science.app/Contents/Resources/bin/claude-science")
    }
}

pub(crate) fn browser_open_bin() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/usr/bin/xdg-open");
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from("/usr/bin/open")
    }
}

pub(crate) fn lsof_bin() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/usr/bin/lsof");
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from("/usr/sbin/lsof")
    }
}

pub(crate) fn ps_bin() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/usr/bin/ps");
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from("/bin/ps")
    }
}

pub(crate) fn id_bin() -> PathBuf {
    PathBuf::from("/usr/bin/id")
}

pub(crate) fn kill_bin() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/usr/bin/kill");
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from("/bin/kill")
    }
}

#[cfg(target_os = "linux")]
fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_bwrap_version(output: &str) -> Option<(u32, u32, u32)> {
    let raw = output.split_whitespace().find(|part| {
        part.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
    })?;
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .and_then(|part| {
            let digits: String = part.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            (!digits.is_empty()).then_some(digits)
        })
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(target_os = "linux")]
fn linux_user_namespaces_available(bwrap: &Path) -> bool {
    let clone_enabled = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        .ok()
        .is_none_or(|value| value.trim() != "0");
    let namespace_capacity = std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_none_or(|value| value > 0);
    if !clone_enabled || !namespace_capacity {
        return false;
    }
    matches!(
        Command::new(bwrap)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-user",
                "--uid",
                "0",
                "--gid",
                "0",
                "/bin/true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
        Ok(status) if status.success()
    )
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug)]
struct LinuxPreflightFacts {
    x86_64: bool,
    bwrap_present: bool,
    bwrap_version: Option<(u32, u32, u32)>,
    userns_available: bool,
    socat_present: bool,
    lsof_present: bool,
}

#[cfg(any(target_os = "linux", test))]
fn linux_blockers_from(facts: LinuxPreflightFacts) -> Vec<RuntimeEnvironmentBlocker> {
    let mut blockers = Vec::new();
    if !facts.x86_64 {
        blockers.push(RuntimeEnvironmentBlocker {
            code: "unsupported_linux_arch",
            message: "Linux beta 当前只支持 x86_64 glibc。",
        });
    }
    if !facts.bwrap_present {
        blockers.push(RuntimeEnvironmentBlocker {
            code: "missing_bwrap",
            message: "缺少 Bubblewrap，不能安全启动 Claude Science。",
        });
    } else if facts
        .bwrap_version
        .is_none_or(|version| version < (0, 8, 0))
    {
        blockers.push(RuntimeEnvironmentBlocker {
            code: "bwrap_too_old",
            message: "Bubblewrap 必须为 0.8.0 或更高版本。",
        });
    } else if !facts.userns_available {
        blockers.push(RuntimeEnvironmentBlocker {
            code: "userns_unavailable",
            message: "当前系统不能创建非特权 user namespace，已拒绝无沙箱启动。",
        });
    }
    if !facts.socat_present {
        blockers.push(RuntimeEnvironmentBlocker {
            code: "missing_socat",
            message: "缺少 socat，Claude Science Linux runtime 不可用。",
        });
    }
    if !facts.lsof_present {
        blockers.push(RuntimeEnvironmentBlocker {
            code: "missing_lsof",
            message: "缺少 lsof，无法确认受管监听进程身份。",
        });
    }
    blockers
}

pub(crate) fn science_environment_blockers() -> Vec<RuntimeEnvironmentBlocker> {
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
    #[cfg(target_os = "linux")]
    {
        let bwrap = Path::new("/usr/bin/bwrap");
        let bwrap_present = executable(bwrap);
        let bwrap_version = if bwrap_present {
            Command::new(bwrap)
                .arg("--version")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|output| parse_bwrap_version(&output))
        } else {
            None
        };
        linux_blockers_from(LinuxPreflightFacts {
            x86_64: std::env::consts::ARCH == "x86_64",
            bwrap_present,
            bwrap_version,
            userns_available: bwrap_version.is_some_and(|version| version >= (0, 8, 0))
                && linux_user_namespaces_available(bwrap),
            socat_present: executable(Path::new("/usr/bin/socat")),
            lsof_present: executable(Path::new("/usr/bin/lsof")),
        })
    }
}

pub(crate) fn require_science_environment() -> Result<(), String> {
    let blockers = science_environment_blockers();
    if blockers.is_empty() {
        Ok(())
    } else {
        Err(blockers
            .iter()
            .map(|blocker| blocker.message)
            .collect::<Vec<_>>()
            .join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        configure_runtime_script_command_for_platform, configure_science_command_for_platform,
        linux_blockers_from, parse_bwrap_version, validate_ssh_agent_socket, LinuxPreflightFacts,
    };
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(label: &str) -> PathBuf {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        base.join(format!(
            "csswitch-platform-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn parses_bubblewrap_versions_without_accepting_malformed_output() {
        assert_eq!(parse_bwrap_version("bubblewrap 0.8.0\n"), Some((0, 8, 0)));
        assert_eq!(parse_bwrap_version("bwrap 0.11.0-2"), Some((0, 11, 0)));
        assert_eq!(parse_bwrap_version("bubblewrap unknown"), None);
    }

    #[test]
    fn missing_linux_sandbox_dependencies_return_bounded_codes() {
        let blockers = linux_blockers_from(LinuxPreflightFacts {
            x86_64: true,
            bwrap_present: false,
            bwrap_version: None,
            userns_available: false,
            socat_present: false,
            lsof_present: false,
        });
        assert_eq!(
            blockers
                .iter()
                .map(|blocker| blocker.code)
                .collect::<Vec<_>>(),
            vec!["missing_bwrap", "missing_socat", "missing_lsof"]
        );
    }

    #[test]
    fn unavailable_user_namespace_never_degrades_to_unsandboxed() {
        let blockers = linux_blockers_from(LinuxPreflightFacts {
            x86_64: true,
            bwrap_present: true,
            bwrap_version: Some((0, 8, 0)),
            userns_available: false,
            socat_present: true,
            lsof_present: true,
        });
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].code, "userns_unavailable");
    }

    #[test]
    fn strict_linux_science_environment_drops_host_credentials_and_xdg_roots() {
        let home = test_dir("science-env");
        let mut command = Command::new("/usr/bin/env");
        command
            .env("OPENAI_API_KEY", "host-secret")
            .env("ANTHROPIC_AUTH_TOKEN", "host-secret")
            .env("SSH_AUTH_SOCK", "/host/agent.sock")
            .env("XDG_CONFIG_HOME", "/host/config");
        configure_science_command_for_platform(&mut command, &home, true).unwrap();
        let output = command.output().unwrap();
        assert!(output.status.success());
        let environment = String::from_utf8(output.stdout).unwrap();
        assert!(environment.contains(&format!("HOME={}", home.display())));
        assert!(environment.contains(&format!(
            "XDG_CONFIG_HOME={}",
            home.join(".config").display()
        )));
        assert!(!environment.contains("host-secret"));
        assert!(!environment.contains("/host/config"));
        assert!(!environment.contains("SSH_AUTH_SOCK="));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn strict_linux_script_environment_never_inherits_ssh_by_default() {
        let mut command = Command::new("/usr/bin/env");
        command
            .env("SSH_AUTH_SOCK", "/host/agent.sock")
            .env("OPENAI_API_KEY", "host-secret");
        configure_runtime_script_command_for_platform(&mut command, false, true).unwrap();
        let output = command.output().unwrap();
        assert!(output.status.success());
        let environment = String::from_utf8(output.stdout).unwrap();
        assert!(!environment.contains("SSH_AUTH_SOCK="));
        assert!(!environment.contains("host-secret"));
    }

    #[test]
    fn ssh_agent_opt_in_accepts_only_an_existing_unix_socket() {
        use std::os::unix::net::UnixListener;

        let root = PathBuf::from(format!(
            "/tmp/csa-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let relative = PathBuf::from("relative-agent.sock");
        assert!(validate_ssh_agent_socket(&relative).is_err());
        let regular = root.join("regular-file");
        std::fs::write(&regular, b"not a socket").unwrap();
        assert!(validate_ssh_agent_socket(&regular).is_err());
        let socket = root.join("agent.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        assert_eq!(validate_ssh_agent_socket(&socket).unwrap(), socket);
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
}
