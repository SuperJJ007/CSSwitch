use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use tauri::Manager;

use crate::config;

const OPERATION_LOG_MAX_BYTES: u64 = 1_048_576;

/// Locate the CSSwitch repository root containing `proxy/csswitch_proxy.py`.
/// Prefer `CSSWITCH_REPO`; otherwise walk upwards from the executable path.
pub(crate) fn repo_root() -> Option<PathBuf> {
    let marker = Path::new("proxy/csswitch_proxy.py");
    if let Some(r) = std::env::var_os("CSSWITCH_REPO") {
        if let Ok(p) = std::fs::canonicalize(PathBuf::from(r)) {
            if p.join(marker).is_file() {
                return Some(p);
            }
        }
    }
    // Only walk from the executable path. current_dir is intentionally ignored:
    // the launch directory can be influenced, and must not select a foreign
    // proxy script that receives provider keys through env.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir: Option<&Path> = exe.parent();
        while let Some(d) = dir {
            if d.join(marker).is_file() {
                return Some(d.to_path_buf());
            }
            dir = d.parent();
        }
    }
    None
}

/// Locate the asset root containing `proxy/` and `scripts/`.
/// Packaged apps use `Contents/Resources`; dev builds fall back to repo root.
pub(crate) fn asset_root(app: &tauri::AppHandle) -> Option<PathBuf> {
    let marker = Path::new("proxy/csswitch_proxy.py");
    if let Ok(res) = app.path().resource_dir() {
        if res.join(marker).is_file() {
            return Some(res);
        }
    }
    repo_root()
}

pub(crate) fn log_path(name: &str) -> PathBuf {
    config::default_dir().join("logs").join(name)
}

/// Platform `O_NOFOLLOW` without adding libc. macOS/BSD=0x0100, Linux=0x20000.
const fn libc_o_nofollow() -> i32 {
    if cfg!(target_os = "linux") {
        0x2_0000
    } else {
        0x0100
    }
}

/// Open/truncate a child-process log, ensuring parent dir is 0700 and file is 0600.
pub(crate) fn open_log(name: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let p = log_path(name);
    if let Some(parent) = p.parent() {
        config::assert_not_symlink(parent)?;
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    config::assert_not_symlink(&p)?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc_o_nofollow())
        .open(&p)?;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    Ok(f)
}

/// Append a redaction-safe operation event to `operation.log`.
/// Callers must pass only coarse stage metadata, never keys, secrets, base URLs, or request bodies.
pub(crate) fn append_operation_log(line: &str) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let p = log_path("operation.log");
    let Some(parent) = p.parent() else {
        return;
    };
    if config::assert_not_symlink(parent).is_err() || config::assert_not_symlink(&p).is_err() {
        return;
    }
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    rotate_operation_log_if_needed(&p, line.len() as u64 + 1);
    let mut f = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc_o_nofollow())
        .open(&p)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    let _ = writeln!(f, "{line}");
}

fn operation_log_archive_path(p: &Path) -> PathBuf {
    p.with_file_name("operation.log.1")
}

fn should_rotate_operation_log(current_bytes: u64, incoming_bytes: u64) -> bool {
    current_bytes.saturating_add(incoming_bytes) > OPERATION_LOG_MAX_BYTES
}

fn rotate_operation_log_if_needed(p: &Path, incoming_bytes: u64) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(md) = std::fs::metadata(p) else {
        return;
    };
    if !should_rotate_operation_log(md.len(), incoming_bytes) {
        return;
    }
    let archive = operation_log_archive_path(p);
    if config::assert_not_symlink(&archive).is_err() {
        return;
    }
    let _ = std::fs::remove_file(&archive);
    if std::fs::rename(p, &archive).is_ok() {
        let _ = std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o600));
    }
}

/// Redact a path-secret before returning child-process log tails to the frontend.
pub(crate) fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        s.to_string()
    } else {
        s.replace(secret, "****")
    }
}

pub(crate) fn tail_file(path: &Path, max: usize) -> String {
    match std::fs::read(path) {
        Ok(b) => {
            let start = b.len().saturating_sub(max);
            String::from_utf8_lossy(&b[start..]).trim().to_string()
        }
        Err(_) => String::new(),
    }
}

pub(crate) fn kill_child(slot: &mut Option<Child>) {
    if let Some(mut c) = slot.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Open a URL with the system browser (macOS `open`) and verify exit status.
pub(crate) fn open_in_browser(url: &str) -> Result<(), String> {
    let st = Command::new("open")
        .arg(url)
        .status()
        .map_err(|e| format!("打开浏览器失败：{e}"))?;
    if !st.success() {
        return Err(format!("open 非零退出（{:?}）", st.code()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{operation_log_archive_path, redact, should_rotate_operation_log};
    use std::path::Path;

    #[test]
    fn redact_replaces_nonempty_secret_only() {
        assert_eq!(redact("abc secret abc", "secret"), "abc **** abc");
        assert_eq!(redact("abc", ""), "abc");
    }

    #[test]
    fn operation_log_rotation_threshold_counts_incoming_line() {
        assert!(!should_rotate_operation_log(1_048_575, 1));
        assert!(should_rotate_operation_log(1_048_575, 2));
        assert!(should_rotate_operation_log(u64::MAX, 1));
    }

    #[test]
    fn operation_log_archive_is_single_sibling_file() {
        assert_eq!(
            operation_log_archive_path(Path::new("/tmp/operation.log")),
            Path::new("/tmp/operation.log.1")
        );
    }
}
