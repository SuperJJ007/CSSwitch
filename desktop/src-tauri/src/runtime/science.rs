use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::Runtime;

use crate::{config, proc};

use super::system::{asset_root, kill_child};

pub(crate) const SCIENCE_BIN: &str =
    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science";
pub(crate) const SCIENCE_DOWNLOAD_URL: &str = "https://claude.com/download";
pub(crate) const CACHED_ONCE_CHOICE: &str = "cached_once";
const OFFICIAL_UPDATED_RUNTIME_RELATIVE: &str = ".claude-science/bin/claude-science";
const OFFICIAL_SCIENCE_IDENTIFIER: &str = "com.anthropic.operon.cli";
const OFFICIAL_SCIENCE_TEAM_ID: &str = "Q6L2SF6YDW";
const MIN_SCIENCE_BINARY_SIZE: u64 = 1024 * 1024;
const MAX_SCIENCE_BINARY_SIZE: u64 = 512 * 1024 * 1024;
const OFFICIAL_UPDATED_SNAPSHOT_DIR: &str = "runtime-snapshots/science";
const SCIENCE_VERSION_TIMEOUT: Duration = Duration::from_secs(15);
static SCIENCE_VERSION_OUTPUT_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScienceRuntimeSource {
    Explicit,
    OfficialUpdated,
    InstalledApp,
    CachedOnce,
}

impl ScienceRuntimeSource {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::OfficialUpdated => "official_updated",
            Self::InstalledApp => "installed_app",
            Self::CachedOnce => "cached_once",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScienceRuntimeIdentity {
    pub(crate) path: PathBuf,
    pub(crate) source: ScienceRuntimeSource,
    pub(crate) version: Option<String>,
    fingerprint: ScienceExecutableFingerprint,
}

impl ScienceRuntimeIdentity {
    pub(crate) fn skill_install_host_context(
        &self,
        sandbox_port: u16,
    ) -> Result<csswitch_skill_install_core::ScienceHostContext, String> {
        let canonical = self
            .path
            .canonicalize()
            .map_err(|_| "Science binary 不可用，无法启用 Skill attach control")?;
        if canonical != self.path {
            return Err("Science binary 不是 canonical path，无法启用 Skill attach control".into());
        }
        let version = self
            .version
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .ok_or("Science 版本未确认，无法启用 Skill attach control")?
            .clone();
        if !self.is_current() {
            return Err("Science binary 在选择后发生变化，无法启用 Skill attach control".into());
        }
        let fingerprint = &self.fingerprint;
        Ok(csswitch_skill_install_core::ScienceHostContext {
            binary: canonical,
            version,
            fingerprint: csswitch_skill_install_core::ScienceExecutableFingerprint {
                device: fingerprint.device,
                inode: fingerprint.inode,
                size: fingerprint.size,
                modified_seconds: fingerprint.modified_seconds,
                modified_nanoseconds: fingerprint.modified_nanoseconds,
                mode: fingerprint.mode,
                sha256: fingerprint_sha256_hex(fingerprint),
            },
            home: sandbox_home(),
            data_dir: sandbox_data_dir(),
            sandbox_port,
        })
    }

    pub(crate) fn is_current(&self) -> bool {
        science_executable_fingerprint(&self.path).as_ref() == Some(&self.fingerprint)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScienceExecutableFingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    mode: u32,
    sha256: [u8; 32],
}

#[derive(Clone, Debug)]
struct ScienceVersionCacheEntry {
    fingerprint: ScienceExecutableFingerprint,
    version: String,
}

/// Successful Science `--version` results, shared for one CSSwitch process.
///
/// The dedicated inner lock serializes only the rare version probe. It never
/// holds the broader AppState lock while launching an external process.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScienceVersionCache {
    entries: Arc<Mutex<HashMap<PathBuf, ScienceVersionCacheEntry>>>,
}

impl ScienceVersionCache {
    fn version(&self, path: &Path) -> Option<String> {
        self.version_inner(path, false)
    }

    pub(crate) fn force_refresh(&self, path: &Path) -> Option<String> {
        self.version_inner(path, true)
    }

    fn version_inner(&self, path: &Path, force: bool) -> Option<String> {
        let mut force = force;
        for _ in 0..2 {
            let fingerprint = science_executable_fingerprint(path)?;
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if force {
                entries.remove(path);
                force = false;
            } else if let Some(entry) = entries.get(path) {
                if entry.fingerprint == fingerprint {
                    return Some(entry.version.clone());
                }
                entries.remove(path);
            }

            let version = safe_science_version(path)?;
            let Some(after) = science_executable_fingerprint(path) else {
                entries.remove(path);
                return None;
            };
            if after != fingerprint {
                entries.remove(path);
                continue;
            }
            entries.insert(
                path.to_path_buf(),
                ScienceVersionCacheEntry {
                    fingerprint,
                    version: version.clone(),
                },
            );
            return Some(version);
        }
        None
    }

    pub(crate) fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

/// 沙箱可写工作目录（独立 HOME）：由构建变体配置根派生；正式构建为
/// `~/.csswitch/sandbox/home`，Acceptance 为 `~/.csswitch-acceptance/sandbox/home`。
pub(crate) fn sandbox_home() -> PathBuf {
    config::default_dir().join("sandbox").join("home")
}

/// CSSwitch 隔离 Science 的持久化 data-dir；其中的 Skill 内容由 Science 自身管理。
pub(crate) fn sandbox_data_dir() -> PathBuf {
    sandbox_home().join(".claude-science")
}

/// 端口变更是否需要拆掉现有链路（纯函数，P1-c）。代理/沙箱任一端口变了，正在跑的代理就绑在
/// 旧端口、正在跑的沙箱又把旧代理 URL 烘死了，二者与新配置不一致 → 拆掉逼下次「一键开始」按新端口重建。
pub(crate) fn settings_change_needs_teardown(
    old_proxy: u16,
    new_proxy: u16,
    old_sandbox: u16,
    new_sandbox: u16,
) -> bool {
    old_proxy != new_proxy || old_sandbox != new_sandbox
}

/// 从 `claude-science url` 的 stdout 里取**第一条**合法 http(s) URL。
pub(crate) fn first_http_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let t = line.trim();
        if t.starts_with("http://") || t.starts_with("https://") {
            let url = t.split_whitespace().next().unwrap_or(t);
            return Some(url.to_string());
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match current.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    path.is_file()
        && path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn is_explicit_executable_file(path: &Path) -> bool {
    is_executable_file(path)
}

fn official_updated_identity_metadata_matches(path: &Path) -> bool {
    // Current upstream Science updater binaries expose the same embedded
    // identifier and Team ID as the App seed, but fail strict cryptographic
    // `codesign --verify`. Treat these fields only as format/identity guards;
    // the local trust boundary is the fixed user-owned path plus SHA-256-bound
    // runtime identity below, not a claim of verified official provenance.
    let output = Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=4"])
        .arg(path)
        .stdout(Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() || output.stderr.len() > 64 * 1024 {
        return false;
    }
    let details = String::from_utf8_lossy(&output.stderr);
    details
        .lines()
        .any(|line| line == format!("Identifier={OFFICIAL_SCIENCE_IDENTIFIER}"))
        && details
            .lines()
            .any(|line| line == format!("TeamIdentifier={OFFICIAL_SCIENCE_TEAM_ID}"))
}

fn file_is_macho(path: &Path) -> bool {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path);
    let Ok(mut file) = file else {
        return false;
    };
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        magic,
        [0xfe, 0xed, 0xfa, 0xce]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xcf, 0xfa, 0xed, 0xfe]
            | [0xca, 0xfe, 0xba, 0xbe]
            | [0xbe, 0xba, 0xfe, 0xca]
            | [0xca, 0xfe, 0xba, 0xbf]
            | [0xbf, 0xba, 0xfe, 0xca]
    )
}

fn official_updated_science_bin_for_home(
    home: &Path,
    verify_local_identity: bool,
) -> Option<PathBuf> {
    if !home.is_absolute() || home.canonicalize().ok().as_deref() != Some(home) {
        return None;
    }
    let science_dir = home.join(".claude-science");
    let bin_dir = science_dir.join("bin");
    let candidate = home.join(OFFICIAL_UPDATED_RUNTIME_RELATIVE);
    if !is_executable_file(&candidate) {
        return None;
    }
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    for directory in [home, &science_dir, &bin_dir] {
        let metadata = directory.symlink_metadata().ok()?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != uid
            || metadata.permissions().mode() & 0o022 != 0
        {
            return None;
        }
    }
    let metadata = candidate.symlink_metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        return None;
    }
    if verify_local_identity
        && (!(MIN_SCIENCE_BINARY_SIZE..=MAX_SCIENCE_BINARY_SIZE).contains(&metadata.len())
            || !file_is_macho(&candidate)
            || !official_updated_identity_metadata_matches(&candidate))
    {
        return None;
    }
    Some(candidate)
}

fn fingerprint_sha256_hex(fingerprint: &ScienceExecutableFingerprint) -> String {
    fingerprint
        .sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn secure_runtime_snapshot_root(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("Science runtime snapshot 目录不是绝对路径".into());
    }
    let mut cursor = Some(root);
    while let Some(path) = cursor {
        match path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Science runtime snapshot 目录路径包含 symlink，已拒绝使用".into())
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err("Science runtime snapshot 目录路径包含非目录文件".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查 Science runtime snapshot 目录失败：{error}")),
        }
        cursor = path.parent();
    }
    fs::create_dir_all(root)
        .map_err(|error| format!("创建 Science runtime snapshot 目录失败：{error}"))?;
    let canonical = root
        .canonicalize()
        .map_err(|error| format!("确认 Science runtime snapshot 目录失败：{error}"))?;
    if canonical != root {
        return Err("Science runtime snapshot 目录包含 symlink，已拒绝使用".into());
    }
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    let metadata = root
        .symlink_metadata()
        .map_err(|error| format!("读取 Science runtime snapshot 目录失败：{error}"))?;
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err("Science runtime snapshot 目录属主或权限不安全".into());
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("收紧 Science runtime snapshot 目录权限失败：{error}"))?;
    Ok(canonical)
}

fn official_updated_snapshot_from_process_paths(
    snapshot_root: &Path,
    process_paths: &[PathBuf],
    verify_local_identity: bool,
) -> Result<Option<PathBuf>, String> {
    let metadata = match snapshot_root.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取 Science runtime snapshot 目录失败：{error}")),
    };
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !snapshot_root.is_absolute()
        || metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o022 != 0
        || snapshot_root.canonicalize().ok().as_deref() != Some(snapshot_root)
    {
        return Err("Science runtime snapshot 目录身份或权限不安全".into());
    }
    let mut matching = process_paths
        .iter()
        .filter(|path| path.parent() == Some(snapshot_root));
    let Some(path) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err("Science listener 映射到多个 runtime snapshot，已拒绝恢复".into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Science runtime snapshot 文件名不可识别")?;
    let Some(expected_sha) = name.strip_prefix("claude-science-") else {
        return Err("Science listener executable 不是内容寻址 snapshot".into());
    };
    if expected_sha.len() != 64 || !expected_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Science runtime snapshot 文件名 SHA-256 非法".into());
    }
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("读取 Science runtime snapshot 文件失败：{error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
        || !(MIN_SCIENCE_BINARY_SIZE..=MAX_SCIENCE_BINARY_SIZE).contains(&metadata.len())
        || path.canonicalize().ok().as_deref() != Some(path.as_path())
    {
        return Err("Science runtime snapshot 文件身份或权限不安全".into());
    }
    let fingerprint =
        science_executable_fingerprint(path).ok_or("Science runtime snapshot 内容无法确认")?;
    if fingerprint_sha256_hex(&fingerprint) != expected_sha.to_ascii_lowercase() {
        return Err("Science runtime snapshot 文件名与内容 SHA-256 不一致".into());
    }
    if verify_local_identity
        && (!file_is_macho(path) || !official_updated_identity_metadata_matches(path))
    {
        return Err("Science runtime snapshot 未通过 Mach-O/embedded metadata 复核".into());
    }
    Ok(Some(path.clone()))
}

fn official_updated_snapshot_for_listener(
    port: u16,
    snapshot_root: &Path,
    verify_local_identity: bool,
) -> Result<Option<PathBuf>, String> {
    let Some(pid) = unique_listener_pid(port) else {
        return Ok(None);
    };
    let Some(process_paths) = process_text_paths(pid) else {
        return Ok(None);
    };
    official_updated_snapshot_from_process_paths(
        snapshot_root,
        &process_paths,
        verify_local_identity,
    )
}

fn official_updated_snapshot_for_home(
    home: &Path,
    snapshot_root: &Path,
    verify_local_identity: bool,
) -> Result<Option<PathBuf>, String> {
    let candidate = home.join(OFFICIAL_UPDATED_RUNTIME_RELATIVE);
    if !candidate.exists() {
        return Ok(None);
    }
    let candidate = official_updated_science_bin_for_home(home, false).ok_or(
        "检测到 updater Science executable，但固定路径、属主或权限校验未通过；已拒绝静默回退旧 App",
    )?;
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&candidate)
        .map_err(|error| format!("打开 updater Science executable 失败：{error}"))?;
    let source_before = source
        .metadata()
        .map_err(|error| format!("读取 updater Science executable 失败：{error}"))?;
    if !source_before.file_type().is_file()
        || !(MIN_SCIENCE_BINARY_SIZE..=MAX_SCIENCE_BINARY_SIZE).contains(&source_before.len())
    {
        return Err("updater Science executable 大小或文件类型不安全；已拒绝静默回退旧 App".into());
    }

    let snapshot_root = secure_runtime_snapshot_root(snapshot_root)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "系统时间异常，无法创建 Science runtime snapshot")?
        .as_nanos();
    let temporary = snapshot_root.join(format!(
        ".claude-science-{}-{nonce}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<PathBuf, String> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|error| format!("创建 Science runtime snapshot 临时文件失败：{error}"))?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = source
                .read(&mut buffer)
                .map_err(|error| format!("读取 updater Science executable 失败：{error}"))?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("写入 Science runtime snapshot 失败：{error}"))?;
        }
        output
            .sync_all()
            .map_err(|error| format!("持久化 Science runtime snapshot 失败：{error}"))?;
        drop(output);

        let source_after = source
            .metadata()
            .map_err(|error| format!("复核 updater Science executable 失败：{error}"))?;
        let current = candidate
            .symlink_metadata()
            .map_err(|error| format!("复核 updater Science executable 路径失败：{error}"))?;
        if source_before.dev() != source_after.dev()
            || source_before.ino() != source_after.ino()
            || source_before.size() != source_after.size()
            || source_before.mtime() != source_after.mtime()
            || source_before.mtime_nsec() != source_after.mtime_nsec()
            || source_before.mode() != source_after.mode()
            || source_after.dev() != current.dev()
            || source_after.ino() != current.ino()
            || official_updated_science_bin_for_home(home, false).as_deref()
                != Some(candidate.as_path())
        {
            return Err("updater Science executable 在快照期间发生变化；已拒绝启动，请重试".into());
        }
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o500))
            .map_err(|error| format!("收紧 Science runtime snapshot 权限失败：{error}"))?;
        if verify_local_identity
            && (!file_is_macho(&temporary)
                || !official_updated_identity_metadata_matches(&temporary))
        {
            return Err(
                "updater Science executable 未通过 Mach-O/embedded metadata 本地校验；已拒绝静默回退旧 App"
                    .into(),
            );
        }

        let sha256: [u8; 32] = digest.finalize().into();
        let name: String = sha256.iter().map(|byte| format!("{byte:02x}")).collect();
        let snapshot = snapshot_root.join(format!("claude-science-{name}"));
        match fs::hard_link(&temporary, &snapshot) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("提交 Science runtime snapshot 失败：{error}"));
            }
        }
        let fingerprint = science_executable_fingerprint(&snapshot)
            .ok_or("Science runtime snapshot 无法重新确认")?;
        if fingerprint.sha256 != sha256
            || fingerprint.size != source_after.size()
            || fingerprint.mode & 0o022 != 0
        {
            return Err("Science runtime snapshot 内容或权限与已验证候选不一致".into());
        }
        Ok(snapshot)
    })();
    let _ = fs::remove_file(&temporary);
    result.map(Some)
}

fn official_updated_science_bin() -> Result<Option<PathBuf>, String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(None);
    };
    let snapshot_root = config::default_dir().join(OFFICIAL_UPDATED_SNAPSHOT_DIR);
    official_updated_snapshot_for_home(&home, &snapshot_root, true)
}

fn science_executable_fingerprint(path: &Path) -> Option<ScienceExecutableFingerprint> {
    if !is_executable_file(path) {
        return None;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let before = file.metadata().ok()?;
    if !before.file_type().is_file() || before.permissions().mode() & 0o111 == 0 {
        return None;
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let after = file.metadata().ok()?;
    let path_metadata = path.symlink_metadata().ok()?;
    if path_metadata.file_type().is_symlink()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.size() != after.size()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.mode() != after.mode()
        || after.dev() != path_metadata.dev()
        || after.ino() != path_metadata.ino()
    {
        return None;
    }
    Some(ScienceExecutableFingerprint {
        device: after.dev(),
        inode: after.ino(),
        size: after.size(),
        modified_seconds: after.mtime(),
        modified_nanoseconds: after.mtime_nsec(),
        mode: after.mode(),
        sha256: digest.finalize().into(),
    })
}

fn cached_science_bin(data_dir: &Path) -> PathBuf {
    data_dir.join("bin").join("claude-science")
}

fn create_science_version_output() -> Option<(fs::File, PathBuf)> {
    for _ in 0..8 {
        let nonce = SCIENCE_VERSION_OUTPUT_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            ".csswitch-science-version-{}-{nonce}",
            std::process::id()
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => return Some((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn anchored_child_exited(pid: u32) -> Result<bool, i32> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    loop {
        // SAFETY: info points to writable siginfo_t storage. WNOWAIT observes
        // but does not reap the direct child, so its pid continues to anchor
        // the private process group until Child::wait is called.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: successful waitid initializes siginfo_t. si_pid == 0
            // is the specified WNOHANG result when no exit is pending.
            return Ok(unsafe { info.assume_init().si_pid() } != 0);
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

fn kill_anchored_science_version_group(pid: u32, leader_exited: bool) -> bool {
    let Ok(pgid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: waitid(WNOWAIT) or a still-running direct child keeps pid/pgid
    // reserved for the process group created by process_group(0).
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error();
    if errno == Some(libc::ESRCH) {
        return true;
    }
    // Darwin returns EPERM, rather than ESRCH, when the anchored process group
    // contains only the WNOWAIT zombie leader. A same-uid executable descendant
    // would make killpg succeed, so accept EPERM only after waitid confirmed the
    // leader's exit; never accept it on the timeout/live-leader path.
    cfg!(target_os = "macos") && leader_exited && errno == Some(libc::EPERM)
}

fn wait_science_version_probe(mut child: Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match anchored_child_exited(child.id()) {
            Ok(true) => {
                let group_stopped = kill_anchored_science_version_group(child.id(), true);
                let status = child.wait().ok()?;
                return group_stopped.then_some(status);
            }
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(false) => {
                if !kill_anchored_science_version_group(child.id(), false) {
                    let _ = child.kill();
                }
                let cleanup_deadline = Instant::now() + Duration::from_secs(1);
                while Instant::now() < cleanup_deadline {
                    match anchored_child_exited(child.id()) {
                        Ok(true) => {
                            let _ = child.wait();
                            return None;
                        }
                        Ok(false) => std::thread::sleep(Duration::from_millis(20)),
                        Err(_) => break,
                    }
                }
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return None;
            }
            Err(_) => return None,
        }
    }
}

fn safe_science_version_with_timeout(path: &Path, timeout: Duration) -> Option<String> {
    let (mut output_file, output_path) = create_science_version_output()?;
    let result = (|| {
        let stdout = output_file.try_clone().ok()?;
        let mut command = Command::new(path);
        command
            .arg("--version")
            .env("HOME", sandbox_home())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::null())
            .process_group(0);
        // SAFETY: setrlimit is async-signal-safe and the closure touches no
        // shared Rust state. The hard limit is inherited by descendants and
        // prevents a malformed version command from filling TMPDIR.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 1025 as libc::rlim_t,
                    rlim_max: 1025 as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let child = command.spawn().ok()?;
        let status = wait_science_version_probe(child, timeout)?;
        if !status.success() {
            return None;
        }
        output_file.seek(SeekFrom::Start(0)).ok()?;
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut output_file)
            .take(1025)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 1024 {
            return None;
        }
        let value = String::from_utf8(bytes).ok()?;
        let value = value.lines().next()?.trim();
        if value.is_empty()
            || value.len() > 160
            || !value
                .bytes()
                .all(|byte| byte == b' ' || (0x21..=0x7e).contains(&byte))
        {
            return None;
        }
        Some(value.to_string())
    })();
    drop(output_file);
    let _ = fs::remove_file(output_path);
    result
}

fn safe_science_version(path: &Path) -> Option<String> {
    safe_science_version_with_timeout(path, SCIENCE_VERSION_TIMEOUT)
}

fn runtime_identity(
    path: PathBuf,
    source: ScienceRuntimeSource,
    version_cache: &ScienceVersionCache,
) -> Option<ScienceRuntimeIdentity> {
    for _ in 0..2 {
        let before = science_executable_fingerprint(&path)?;
        let version = version_cache.version(&path)?;
        let after = science_executable_fingerprint(&path)?;
        if before == after {
            return Some(ScienceRuntimeIdentity {
                path,
                source,
                version: Some(version),
                fingerprint: after,
            });
        }
        let _ = version_cache.force_refresh(&path);
    }
    None
}

pub(crate) fn runtime_identity_is_current(runtime: &ScienceRuntimeIdentity) -> bool {
    runtime.is_current()
}

fn explicit_science_bin() -> Result<Option<PathBuf>, String> {
    let Some(path) = std::env::var_os("SCIENCE_BIN").map(PathBuf::from) else {
        return Ok(None);
    };
    if !is_explicit_executable_file(&path) {
        return Err("显式 SCIENCE_BIN 不是安全的绝对可执行文件；已拒绝回退".into());
    }
    Ok(Some(path))
}

#[cfg(test)]
fn science_runtime_preflight_for_paths(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    app_bin: &Path,
) -> Result<Value, String> {
    science_runtime_preflight_for_paths_cached(
        data_dir,
        explicit_bin,
        None,
        app_bin,
        &ScienceVersionCache::default(),
    )
}

#[cfg(test)]
fn science_runtime_preflight_for_paths_with_updated(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    official_updated_bin: Option<&Path>,
    app_bin: &Path,
) -> Result<Value, String> {
    science_runtime_preflight_for_paths_cached(
        data_dir,
        explicit_bin,
        official_updated_bin,
        app_bin,
        &ScienceVersionCache::default(),
    )
}

fn science_runtime_preflight_for_paths_cached(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    official_updated_bin: Option<&Path>,
    app_bin: &Path,
    version_cache: &ScienceVersionCache,
) -> Result<Value, String> {
    if let Some(bin) = explicit_bin {
        if !is_explicit_executable_file(bin) {
            return Err("显式 SCIENCE_BIN 不是安全的绝对可执行文件；已拒绝回退".into());
        }
        let runtime = runtime_identity(
            bin.to_path_buf(),
            ScienceRuntimeSource::Explicit,
            version_cache,
        )
        .ok_or("显式 SCIENCE_BIN 未通过版本预检；已拒绝回退")?;
        return Ok(json!({
            "status": "installed_ready",
            "selected_source": runtime.source.code(),
            "selected_version": runtime.version,
            "cached_version": Value::Null,
            "download_url": SCIENCE_DOWNLOAD_URL,
        }));
    }
    if let Some(bin) = official_updated_bin {
        let runtime = runtime_identity(
            bin.to_path_buf(),
            ScienceRuntimeSource::OfficialUpdated,
            version_cache,
        )
        .ok_or("updater Science snapshot 未通过版本预检；已拒绝回退旧 App")?;
        return Ok(json!({
            "status": "installed_ready",
            "selected_source": runtime.source.code(),
            "selected_version": runtime.version,
            "cached_version": Value::Null,
            "download_url": SCIENCE_DOWNLOAD_URL,
        }));
    }
    if let Some(runtime) = runtime_identity(
        app_bin.to_path_buf(),
        ScienceRuntimeSource::InstalledApp,
        version_cache,
    ) {
        return Ok(json!({
            "status": "installed_ready",
            "selected_source": runtime.source.code(),
            "selected_version": runtime.version,
            "cached_version": Value::Null,
            "download_url": SCIENCE_DOWNLOAD_URL,
        }));
    }
    let cached = cached_science_bin(data_dir);
    let cached_version = version_cache.version(&cached);
    if let Some(version) = cached_version {
        return Ok(json!({
            "status": "cached_choice_required",
            "selected_source": Value::Null,
            "selected_version": Value::Null,
            "cached_version": version,
            "download_url": SCIENCE_DOWNLOAD_URL,
        }));
    }
    Ok(json!({
        "status": "missing",
        "selected_source": Value::Null,
        "selected_version": Value::Null,
        "cached_version": Value::Null,
        "download_url": SCIENCE_DOWNLOAD_URL,
    }))
}

pub(crate) fn science_runtime_preflight(
    version_cache: &ScienceVersionCache,
    _confirmed_stopped: Option<&ScienceRuntimeIdentity>,
) -> Result<Value, String> {
    if let Ok(cfg) = config::load_from(&config::default_dir()) {
        let (state, runtime) = probe_sandbox_runtime_cached(cfg.sandbox_port, version_cache)?;
        if state == SandboxScienceState::RunningHealthy {
            let runtime = runtime.ok_or("Science 状态为运行中，但无法确认其 binary 身份")?;
            return Ok(json!({
                "status": "installed_ready",
                "selected_source": runtime.source.code(),
                "selected_version": runtime.version,
                "cached_version": Value::Null,
                "download_url": SCIENCE_DOWNLOAD_URL,
            }));
        }
    }
    let data_dir = sandbox_data_dir();
    let explicit = explicit_science_bin()?;
    let official_updated = official_updated_science_bin()?;
    science_runtime_preflight_for_paths_cached(
        &data_dir,
        explicit.as_deref(),
        official_updated.as_deref(),
        Path::new(SCIENCE_BIN),
        version_cache,
    )
}

#[cfg(test)]
fn select_science_runtime_for_paths(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    app_bin: &Path,
    choice: Option<&str>,
) -> Result<ScienceRuntimeIdentity, String> {
    select_science_runtime_for_paths_cached(
        data_dir,
        explicit_bin,
        None,
        app_bin,
        choice,
        &ScienceVersionCache::default(),
    )
}

#[cfg(test)]
fn select_science_runtime_for_paths_with_updated(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    official_updated_bin: Option<&Path>,
    app_bin: &Path,
    choice: Option<&str>,
) -> Result<ScienceRuntimeIdentity, String> {
    select_science_runtime_for_paths_cached(
        data_dir,
        explicit_bin,
        official_updated_bin,
        app_bin,
        choice,
        &ScienceVersionCache::default(),
    )
}

fn select_science_runtime_for_paths_cached(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    official_updated_bin: Option<&Path>,
    app_bin: &Path,
    choice: Option<&str>,
    version_cache: &ScienceVersionCache,
) -> Result<ScienceRuntimeIdentity, String> {
    if let Some(bin) = explicit_bin {
        if !is_explicit_executable_file(bin) {
            return Err("显式 SCIENCE_BIN 不是安全的绝对可执行文件；已拒绝回退".into());
        }
        return runtime_identity(
            bin.to_path_buf(),
            ScienceRuntimeSource::Explicit,
            version_cache,
        )
        .ok_or_else(|| "显式 SCIENCE_BIN 未通过版本预检；已拒绝回退".to_string());
    }
    if let Some(bin) = official_updated_bin {
        return runtime_identity(
            bin.to_path_buf(),
            ScienceRuntimeSource::OfficialUpdated,
            version_cache,
        )
        .ok_or_else(|| "updater Science snapshot 未通过版本预检；已拒绝回退旧 App".to_string());
    }
    if let Some(runtime) = runtime_identity(
        app_bin.to_path_buf(),
        ScienceRuntimeSource::InstalledApp,
        version_cache,
    ) {
        return Ok(runtime);
    }
    let cached = cached_science_bin(data_dir);
    let cached_version = version_cache.version(&cached);
    if choice == Some(CACHED_ONCE_CHOICE) {
        let _ = cached_version
            .ok_or("缓存 Science 版本无法确认；请安装或更新 Claude Science 后再试")?;
        return runtime_identity(cached, ScienceRuntimeSource::CachedOnce, version_cache)
            .ok_or("缓存 Science 文件在版本确认期间发生变化；已拒绝启动".into());
    }
    if cached_version.is_some() {
        return Err("SCIENCE_RUNTIME_CHOICE_REQUIRED：请明确选择仅本次使用缓存版本，或安装/更新 Claude Science".into());
    }
    Err("找不到可用的 Claude Science App；请先安装或更新 Claude Science".into())
}

pub(crate) fn select_science_runtime_cached(
    choice: Option<&str>,
    version_cache: &ScienceVersionCache,
) -> Result<ScienceRuntimeIdentity, String> {
    let data_dir = sandbox_data_dir();
    let explicit = explicit_science_bin()?;
    let official_updated = official_updated_science_bin()?;
    select_science_runtime_for_paths_cached(
        &data_dir,
        explicit.as_deref(),
        official_updated.as_deref(),
        Path::new(SCIENCE_BIN),
        choice,
        version_cache,
    )
}

fn runtime_probe_candidates(
    port: u16,
    version_cache: &ScienceVersionCache,
) -> Result<Vec<ScienceRuntimeIdentity>, String> {
    if let Some(explicit) = explicit_science_bin()? {
        return Ok(
            runtime_identity(explicit, ScienceRuntimeSource::Explicit, version_cache)
                .into_iter()
                .collect(),
        );
    }
    let mut candidates = Vec::new();
    let snapshot_root = config::default_dir().join(OFFICIAL_UPDATED_SNAPSHOT_DIR);
    if let Some(snapshot) = official_updated_snapshot_for_listener(port, &snapshot_root, true)? {
        if let Some(runtime) = runtime_identity(
            snapshot,
            ScienceRuntimeSource::OfficialUpdated,
            version_cache,
        ) {
            candidates.push(runtime);
        }
    }
    let app = PathBuf::from(SCIENCE_BIN);
    if let Some(app) = runtime_identity(app, ScienceRuntimeSource::InstalledApp, version_cache) {
        candidates.push(app);
    }
    let cached = cached_science_bin(&sandbox_data_dir());
    if let Some(cached) = runtime_identity(cached, ScienceRuntimeSource::CachedOnce, version_cache)
    {
        candidates.push(cached);
    }
    Ok(candidates)
}

#[cfg(test)]
fn science_status_running(out: &Output) -> bool {
    out.status.success() && science_status_value(out) == Some(true)
}

fn science_status_value(out: &Output) -> Option<bool> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (idx, ch) in stdout.char_indices() {
        if ch != '{' {
            continue;
        }
        let mut stream =
            serde_json::Deserializer::from_str(&stdout[idx..]).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            if let Some(running) = value.get("running").and_then(|running| running.as_bool()) {
                return Some(running);
            }
        }
    }
    None
}

#[cfg(test)]
fn trusted_science_status(out: &Output) -> Option<bool> {
    match science_status_value(out) {
        Some(false) => Some(false),
        Some(true) if out.status.success() => Some(true),
        _ => None,
    }
}

fn runtime_status_value(out: &Output) -> Option<bool> {
    match science_status_value(out) {
        Some(false) => Some(false),
        Some(true) if out.status.success() => Some(true),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxScienceState {
    RunningHealthy,
    Stopped,
    Unknown,
}

#[cfg(test)]
fn classify_sandbox_state(
    status: Option<bool>,
    health_ready: bool,
    port_accepts_tcp: bool,
) -> SandboxScienceState {
    match status {
        Some(true) if health_ready => SandboxScienceState::RunningHealthy,
        Some(false) if !port_accepts_tcp => SandboxScienceState::Stopped,
        _ => SandboxScienceState::Unknown,
    }
}

fn classify_known_runtime_state(
    status: Option<bool>,
    health_ready: bool,
    port_accepts_tcp: bool,
    listener_matches_runtime: bool,
) -> SandboxScienceState {
    match status {
        Some(true) if health_ready && listener_matches_runtime => {
            SandboxScienceState::RunningHealthy
        }
        Some(false) if !port_accepts_tcp => SandboxScienceState::Stopped,
        _ => SandboxScienceState::Unknown,
    }
}

fn loopback_port_accepts_tcp(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

fn parse_unique_listener_pid(stdout: &str) -> Option<u32> {
    let mut pids = stdout
        .lines()
        .map(str::trim)
        .filter(|pid| !pid.is_empty())
        .map(str::parse::<u32>);
    let pid = pids.next()?.ok()?;
    if pid <= 1 || pids.any(|other| other.ok() != Some(pid)) {
        return None;
    }
    Some(pid)
}

fn unique_listener_pid(port: u16) -> Option<u32> {
    let listener = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .ok()?;
    if !listener.status.success() {
        return None;
    }
    parse_unique_listener_pid(&String::from_utf8(listener.stdout).ok()?)
}

fn process_text_paths(pid: u32) -> Option<Vec<PathBuf>> {
    let pid_text = pid.to_string();
    let text_files = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &pid_text, "-d", "txt", "-Fn"])
        .output()
        .ok()?;
    if !text_files.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&text_files.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('n'))
            .filter_map(|path| Path::new(path).canonicalize().ok())
            .collect(),
    )
}

fn listener_runtime_pid(port: u16, runtime: &ScienceRuntimeIdentity) -> Option<u32> {
    if !runtime.is_current() {
        return None;
    }
    let pid = unique_listener_pid(port)?;
    #[cfg(test)]
    {
        let pid_text = pid.to_string();
        if test_listener_marker_matches(&pid_text, runtime) {
            return Some(pid);
        }
    }
    let expected = runtime.path.canonicalize().ok()?;
    process_text_paths(pid)?
        .into_iter()
        .any(|path| path == expected)
        .then_some(pid)
}

fn listener_uses_runtime(port: u16, runtime: &ScienceRuntimeIdentity) -> bool {
    listener_runtime_pid(port, runtime).is_some()
}

#[cfg(test)]
fn test_listener_marker_matches(pid: &str, runtime: &ScienceRuntimeIdentity) -> bool {
    if std::env::var("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY")
        .ok()
        .as_deref()
        != Some("1")
    {
        return false;
    }
    let Some(configured) = std::env::var_os("SCIENCE_BIN").map(PathBuf::from) else {
        return false;
    };
    if configured.canonicalize().ok() != runtime.path.canonicalize().ok() {
        return false;
    }
    std::fs::read_to_string(sandbox_data_dir().join("fake-science/pid"))
        .ok()
        .is_some_and(|recorded| recorded.trim() == pid)
}

/// Return the sandbox UI URL, falling back to the plain localhost port.
pub(crate) fn sandbox_url(port: u16, runtime: &ScienceRuntimeIdentity) -> String {
    let home = sandbox_home();
    let data_dir = sandbox_data_dir();
    if runtime.is_current() {
        if let Ok(out) = Command::new(&runtime.path)
            .arg("url")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(url) = first_http_url(&s) {
                return url;
            }
        }
    }
    format!("http://127.0.0.1:{port}")
}

fn runtime_status(runtime: &ScienceRuntimeIdentity) -> Option<bool> {
    if !runtime.is_current() {
        return None;
    }
    let out = Command::new(&runtime.path)
        .arg("status")
        .arg("--data-dir")
        .arg(sandbox_data_dir())
        .env("HOME", sandbox_home())
        .output()
        .ok()?;
    // Some Science builds use a non-zero exit to mean "not running" while
    // still returning a valid {"running":false} payload. Accept only that
    // negative result; a non-zero positive or malformed response stays unknown.
    runtime_status_value(&out)
}

pub(crate) fn probe_known_runtime(
    port: u16,
    runtime: &ScienceRuntimeIdentity,
) -> SandboxScienceState {
    let status = runtime_status(runtime);
    let health_ready = proc::http_health(port, None, 400);
    let port_accepts_tcp = health_ready || loopback_port_accepts_tcp(port);
    let listener_matches_runtime =
        status == Some(true) && health_ready && listener_uses_runtime(port, runtime);
    classify_known_runtime_state(
        status,
        health_ready,
        port_accepts_tcp,
        listener_matches_runtime,
    )
}

pub(crate) fn probe_sandbox_runtime(
    port: u16,
) -> Result<(SandboxScienceState, Option<ScienceRuntimeIdentity>), String> {
    probe_sandbox_runtime_cached(port, &ScienceVersionCache::default())
}

pub(crate) fn probe_sandbox_runtime_cached(
    port: u16,
    version_cache: &ScienceVersionCache,
) -> Result<(SandboxScienceState, Option<ScienceRuntimeIdentity>), String> {
    let health_ready = proc::http_health(port, None, 400);
    let port_accepts_tcp = health_ready || loopback_port_accepts_tcp(port);
    let candidates = runtime_probe_candidates(port, version_cache)?;
    let no_candidates = candidates.is_empty();
    let mut saw_stopped = false;
    let mut saw_running_unconfirmed = false;
    for runtime in candidates {
        match runtime_status(&runtime) {
            Some(true) if health_ready && listener_uses_runtime(port, &runtime) => {
                return Ok((SandboxScienceState::RunningHealthy, Some(runtime)))
            }
            Some(true) => saw_running_unconfirmed = true,
            Some(false) => saw_stopped = true,
            None => {}
        }
    }
    if saw_running_unconfirmed {
        return Ok((SandboxScienceState::Unknown, None));
    }
    if !port_accepts_tcp && (saw_stopped || !sandbox_data_dir().exists()) {
        return Ok((SandboxScienceState::Stopped, None));
    }
    if !port_accepts_tcp && no_candidates {
        return Ok((SandboxScienceState::Stopped, None));
    }
    Ok((SandboxScienceState::Unknown, None))
}

fn stop_runtime_from_probe(
    state: SandboxScienceState,
    runtime: Option<ScienceRuntimeIdentity>,
) -> Result<Option<ScienceRuntimeIdentity>, String> {
    match (state, runtime) {
        (SandboxScienceState::Stopped, _) => Ok(None),
        (SandboxScienceState::RunningHealthy, Some(runtime)) => Ok(Some(runtime)),
        (SandboxScienceState::RunningHealthy, None) => {
            Err("Science 状态为运行中，但无法确认其 binary 身份；已拒绝按端口停止".into())
        }
        (SandboxScienceState::Unknown, _) => {
            Err("无法确认当前 Science daemon 使用的 binary；已拒绝按端口停止".into())
        }
    }
}

/// Check that the sandbox Science associated with our data-dir is running.
/// A naked `/health` response is not sufficient identity proof.
#[cfg(test)]
pub(crate) fn sandbox_running_ours(port: u16, runtime: &ScienceRuntimeIdentity) -> bool {
    probe_known_runtime(port, runtime) == SandboxScienceState::RunningHealthy
}

/// The caller has just observed a healthy response and only needs to prove the
/// listener executable identity; avoid repeating status and health CLI work.
pub(crate) fn sandbox_listener_matches_runtime(
    port: u16,
    runtime: &ScienceRuntimeIdentity,
) -> bool {
    listener_uses_runtime(port, runtime)
}

/// Stop the sandbox Science process and clear the in-memory sandbox URL.
///
/// Returns `Err` when the stop script is missing or exits non-zero, so callers
/// can report that Science may not have stopped cleanly.
pub(crate) fn stop_sandbox<R: Runtime>(
    app: &tauri::AppHandle<R>,
    sandbox: &mut Option<Child>,
    sandbox_url: &mut Option<String>,
    runtime: Option<&ScienceRuntimeIdentity>,
) -> Result<(), String> {
    if !sandbox_data_dir().exists() {
        kill_child(sandbox);
        *sandbox_url = None;
        return Ok(());
    }
    let recovered;
    let runtime = match runtime {
        Some(runtime) => runtime,
        None => {
            let port = config::load_from(&config::default_dir())
                .map_err(|e| format!("读取 Science 端口配置失败：{e}"))?
                .sandbox_port;
            let (state, runtime) = probe_sandbox_runtime(port)?;
            let Some(runtime) = stop_runtime_from_probe(state, runtime)? else {
                kill_child(sandbox);
                *sandbox_url = None;
                return Ok(());
            };
            recovered = runtime;
            &recovered
        }
    };
    if !runtime.is_current() {
        return Err("Science binary 在选择后发生变化；已拒绝用不同文件控制现有 daemon".into());
    }
    let sandbox_port = config::load_from(&config::default_dir())
        .map_err(|error| format!("读取 Science 端口配置失败：{error}"))?
        .sandbox_port;
    let listener_before_stop = listener_runtime_pid(sandbox_port, runtime);
    let mut err = None;
    match asset_root(app) {
        Some(root) => {
            let stop = root.join("scripts/stop-science-sandbox.sh");
            if stop.is_file() {
                match Command::new("zsh")
                    .arg(&stop)
                    .env("SANDBOX_HOME", sandbox_home())
                    .env("SCIENCE_BIN", &runtime.path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => {}
                    Ok(s) => err = Some(format!("停止沙箱脚本非零退出（{:?}）。", s.code())),
                    Err(e) => err = Some(format!("调用停止沙箱脚本失败：{e}")),
                }
            } else {
                err = Some(
                    "找不到打包的停止脚本，无法确认沙箱已停止（沙箱可能仍在运行）。".to_string(),
                );
            }
        }
        None => {
            err = Some(
                "定位不到资源根，取不到停止脚本，无法确认沙箱已停止（沙箱可能仍在运行）。"
                    .to_string(),
            );
        }
    }
    if err.is_none() && loopback_port_accepts_tcp(sandbox_port) {
        match listener_before_stop {
            Some(pid) if listener_runtime_pid(sandbox_port, runtime) == Some(pid) => {
                // Some upstream Science builds return success and remove their
                // lockfile without terminating the daemon. The user requested
                // stop, so signal only the exact PID whose listener and
                // canonical executable were proved both before and after CLI.
                // SAFETY: kill does not dereference pointers. PID > 1 and exact
                // listener identity were checked immediately above.
                if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
                    err = Some("Science stop 返回成功但精确 daemon 无法接收 TERM。".into());
                } else {
                    for _ in 0..50 {
                        if !loopback_port_accepts_tcp(sandbox_port) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if listener_runtime_pid(sandbox_port, runtime) == Some(pid) {
                        // SAFETY: the same exact listener/runtime identity is
                        // revalidated after the TERM wait.
                        let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        for _ in 0..20 {
                            if !loopback_port_accepts_tcp(sandbox_port) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                    if loopback_port_accepts_tcp(sandbox_port) {
                        err = Some(
                            "Science stop 返回成功，但端口仍被占用；已拒绝把未知监听者当作停止成功。"
                                .into(),
                        );
                    }
                }
            }
            _ => {
                err = Some(
                    "Science stop 返回成功，但停止后的监听身份与启动记录不一致；未发送信号。"
                        .into(),
                );
            }
        }
    }
    kill_child(sandbox);
    *sandbox_url = None;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    use super::{
        classify_known_runtime_state, classify_sandbox_state, first_http_url,
        official_updated_science_bin_for_home, official_updated_snapshot_for_home,
        official_updated_snapshot_from_process_paths, parse_unique_listener_pid,
        runtime_identity_is_current, runtime_status_value, safe_science_version_with_timeout,
        sandbox_home, sandbox_running_ours, sandbox_url, science_executable_fingerprint,
        science_runtime_preflight_for_paths, science_runtime_preflight_for_paths_cached,
        science_runtime_preflight_for_paths_with_updated, science_status_running,
        secure_runtime_snapshot_root, select_science_runtime_for_paths,
        select_science_runtime_for_paths_cached, select_science_runtime_for_paths_with_updated,
        settings_change_needs_teardown, stop_runtime_from_probe, trusted_science_status,
        SandboxScienceState, ScienceRuntimeIdentity, ScienceRuntimeSource, ScienceVersionCache,
        CACHED_ONCE_CHOICE,
    };

    // ---------- P1-c: 端口变更是否需拆链路（纯函数，4 组合） ----------
    #[test]
    fn settings_teardown_when_any_port_changes() {
        assert!(
            !settings_change_needs_teardown(18991, 18991, 8990, 8990),
            "端口未变 → 不拆链路"
        );
        assert!(
            settings_change_needs_teardown(18991, 19000, 8990, 8990),
            "代理端口变 → 拆（旧代理绑旧端口、沙箱烘旧 URL）"
        );
        assert!(
            settings_change_needs_teardown(18991, 18991, 8990, 9000),
            "沙箱端口变 → 拆（旧沙箱在旧端口成孤儿）"
        );
        assert!(
            settings_change_needs_teardown(18991, 19000, 8990, 9000),
            "都变 → 拆"
        );
    }

    #[test]
    fn science_version_probe_times_out_and_reaps_its_child(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-version-timeout")?;
        let binary = root.join("hanging-science");
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then exec /bin/sleep 60; fi\nexit 0\n",
        )?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let started = std::time::Instant::now();
        assert_eq!(
            safe_science_version_with_timeout(&binary, std::time::Duration::from_millis(100)),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn science_version_probe_kills_descendant_before_it_can_act_after_return(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-version-descendant")?;
        let binary = root.join("forking-science");
        let started_marker = root.join("descendant-started");
        let late_marker = root.join("descendant-late");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then\n  ( : > '{}'; /bin/sleep 1; : > '{}' ) &\n  while [ ! -f '{}' ]; do /bin/sleep 0.01; done\n  printf '%s\\n' 'descendant-safe-v1'\n  exit 0\nfi\nexit 0\n",
                started_marker.display(),
                late_marker.display(),
                started_marker.display()
            ),
        )?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let started = std::time::Instant::now();
        let version =
            safe_science_version_with_timeout(&binary, std::time::Duration::from_secs(15));
        assert_eq!(version.as_deref(), Some("descendant-safe-v1"));
        assert!(started.elapsed() < std::time::Duration::from_secs(17));
        assert!(
            started_marker.exists(),
            "the descendant must actually start"
        );
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        assert!(
            !late_marker.exists(),
            "the descendant must not remain executable after the probe returns"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn science_version_probe_rejects_oversize_output() -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-version-oversize")?;
        let binary = root.join("oversize-science");
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then printf '%s\\n' 'valid-first-line'; printf '%01030d' 0; exit 0; fi\nexit 0\n",
        )?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        assert_eq!(
            safe_science_version_with_timeout(&binary, std::time::Duration::from_secs(2)),
            None,
            "a legal first line must not hide oversized trailing output"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn first_http_url_takes_only_first_valid_url() {
        let multi = "http://127.0.0.1:8990/setup?nonce=abc123\n\
                     This is a single-use link, expires in 60 seconds.";
        assert_eq!(
            first_http_url(multi).as_deref(),
            Some("http://127.0.0.1:8990/setup?nonce=abc123"),
        );
        let inline = "https://x.example/y?z=1  (single-use)";
        assert_eq!(
            first_http_url(inline).as_deref(),
            Some("https://x.example/y?z=1")
        );
        let lead = "Open this link in your browser:\nhttp://127.0.0.1:8990/a";
        assert_eq!(
            first_http_url(lead).as_deref(),
            Some("http://127.0.0.1:8990/a")
        );
        assert_eq!(first_http_url("no url here\nnor here"), None);
        assert_eq!(
            first_http_url("http://127.0.0.1:8990").as_deref(),
            Some("http://127.0.0.1:8990")
        );
    }

    #[test]
    fn listener_pid_parser_requires_one_safe_identity() {
        assert_eq!(parse_unique_listener_pid("preamble\n"), None);
        assert_eq!(parse_unique_listener_pid("1\n"), None);
        assert_eq!(parse_unique_listener_pid("42\n42\n"), Some(42));
        assert_eq!(parse_unique_listener_pid("42\n43\n"), None);
        assert_eq!(parse_unique_listener_pid("42\ninvalid\n"), None);
    }

    #[test]
    fn version_cache_is_shared_and_invalidates_when_binary_changes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-version-cache")?;
        let app_bin = root.join("claude-science");
        let count = root.join("version-count");
        write_counted_version_bin(&app_bin, &count, "claude-science cache-v1")?;
        let data_dir = root.join("data");
        fs::create_dir_all(&data_dir)?;
        let cache = ScienceVersionCache::default();

        let preflight =
            science_runtime_preflight_for_paths_cached(&data_dir, None, None, &app_bin, &cache)?;
        assert_eq!(preflight["selected_version"], "claude-science cache-v1");
        let selected =
            select_science_runtime_for_paths_cached(&data_dir, None, None, &app_bin, None, &cache)?;
        assert_eq!(selected.version.as_deref(), Some("claude-science cache-v1"));
        assert_eq!(fs::read_to_string(&count)?, "1");

        write_counted_version_bin(&app_bin, &count, "claude-science cache-version-two")?;
        let selected =
            select_science_runtime_for_paths_cached(&data_dir, None, None, &app_bin, None, &cache)?;
        assert_eq!(
            selected.version.as_deref(),
            Some("claude-science cache-version-two")
        );
        assert_eq!(fs::read_to_string(&count)?, "2");

        assert_eq!(
            cache.force_refresh(&app_bin).as_deref(),
            Some("claude-science cache-version-two")
        );
        assert_eq!(fs::read_to_string(&count)?, "3");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn science_status_running_accepts_compact_and_spaced_json() {
        assert!(science_status_running(&status_output(
            0,
            r#"{"running":true}"#
        )));
        assert!(science_status_running(&status_output(
            0,
            r#"{"running": true}"#
        )));
        assert!(!science_status_running(&status_output(
            0,
            r#"{"running":false}"#
        )));
        assert!(!science_status_running(&status_output(0, "running")));
        assert!(!science_status_running(&status_output(
            1,
            r#"{"running": true}"#
        )));
    }

    #[test]
    fn science_status_running_accepts_json_with_cli_text() {
        assert!(science_status_running(&status_output(
            0,
            "Claude Science status:\n{\"running\": true, \"port\": 8990}\nready"
        )));
        assert!(science_status_running(&status_output(
            0,
            "warning: {not-json}\n{\"state\":\"ok\"}\n{\"running\": true}"
        )));
        assert!(!science_status_running(&status_output(
            0,
            "warning\n{\"running\": false}\n{\"running\": true}"
        )));
    }

    #[test]
    fn sandbox_state_classification_fails_closed_on_probe_disagreement() {
        assert_eq!(
            classify_sandbox_state(Some(true), true, true),
            SandboxScienceState::RunningHealthy
        );
        assert_eq!(
            classify_sandbox_state(Some(false), false, false),
            SandboxScienceState::Stopped
        );
        for state in [
            classify_sandbox_state(None, false, false),
            classify_sandbox_state(Some(true), false, true),
            classify_sandbox_state(Some(true), false, false),
            classify_sandbox_state(Some(false), true, true),
            classify_sandbox_state(Some(false), false, true),
        ] {
            assert_eq!(state, SandboxScienceState::Unknown);
        }
        assert_eq!(
            trusted_science_status(&status_output(1, r#"{"running":false}"#)),
            Some(false),
            "a stopped daemon may be reported with a non-zero CLI exit"
        );
    }

    #[test]
    fn stop_probe_is_idempotent_only_for_confirmed_stopped_state() {
        assert_eq!(
            stop_runtime_from_probe(SandboxScienceState::Stopped, None).unwrap(),
            None
        );
        assert!(stop_runtime_from_probe(SandboxScienceState::Unknown, None).is_err());
        assert!(stop_runtime_from_probe(SandboxScienceState::RunningHealthy, None).is_err());
    }

    #[test]
    fn known_runtime_state_requires_listener_binary_match() {
        assert_eq!(
            classify_known_runtime_state(Some(true), true, true, true),
            SandboxScienceState::RunningHealthy
        );
        assert_eq!(
            classify_known_runtime_state(Some(true), true, true, false),
            SandboxScienceState::Unknown
        );
        assert_eq!(
            runtime_status_value(&status_output(1, r#"{"running":false}"#)),
            Some(false),
            "a selected runtime may report stopped with a non-zero CLI exit"
        );
        assert_eq!(
            runtime_status_value(&status_output(1, r#"{"running":true}"#)),
            None,
            "a non-zero positive status is never trusted"
        );
    }

    #[test]
    fn known_runtime_classifier_requires_listener_binary_identity() {
        assert_eq!(
            classify_known_runtime_state(Some(true), true, true, true),
            SandboxScienceState::RunningHealthy
        );
        assert_eq!(
            classify_known_runtime_state(Some(true), true, true, false),
            SandboxScienceState::Unknown
        );
    }

    #[test]
    fn runtime_selection_requires_explicit_one_shot_cache_choice(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-bin-selection")?;
        let data_dir = root.join("home").join(".claude-science");
        let explicit_bin = root.join("explicit-claude-science");
        let cached_bin = data_dir.join("bin").join("claude-science");
        let app_bin = root.join("app-claude-science");

        write_fake_version_bin(&explicit_bin, 0o755, "fake-explicit-1")?;
        write_fake_version_bin(&cached_bin, 0o755, "fake-cache-1")?;
        write_fake_version_bin(&app_bin, 0o755, "fake-app-1")?;
        let preflight =
            science_runtime_preflight_for_paths(&data_dir, Some(&explicit_bin), &app_bin)?;
        assert_eq!(preflight["status"], "installed_ready");
        assert_eq!(preflight["selected_source"], "explicit");
        assert_eq!(preflight["selected_version"], "fake-explicit-1");
        assert_eq!(
            select_science_runtime_for_paths(
                &data_dir,
                Some(&explicit_bin),
                &app_bin,
                Some(CACHED_ONCE_CHOICE),
            )?
            .path,
            explicit_bin,
            "a valid explicit development override wins even if cache was authorized"
        );

        fs::set_permissions(&explicit_bin, fs::Permissions::from_mode(0o644))?;
        assert!(
            select_science_runtime_for_paths(&data_dir, Some(&explicit_bin), &app_bin, None)
                .is_err(),
            "an invalid explicit override must not fall through to sandbox or system Science"
        );

        let app =
            select_science_runtime_for_paths(&data_dir, None, &app_bin, Some(CACHED_ONCE_CHOICE))?;
        assert_eq!(
            app.path, app_bin,
            "the installed Science app always wins over an old cache"
        );
        assert_eq!(app.source, ScienceRuntimeSource::InstalledApp);
        assert_eq!(app.version.as_deref(), Some("fake-app-1"));

        let explicit_link = root.join("explicit-link");
        symlink(&app_bin, &explicit_link)?;
        assert!(
            select_science_runtime_for_paths(&data_dir, Some(&explicit_link), &app_bin, None)
                .is_err(),
            "an explicit symlink must fail closed"
        );

        let real_parent = root.join("real-parent");
        let linked_parent = root.join("linked-parent");
        let parent_bin = real_parent.join("claude-science");
        write_fake_version_bin(&parent_bin, 0o755, "fake-parent-1")?;
        symlink(&real_parent, &linked_parent)?;
        assert!(
            select_science_runtime_for_paths(
                &data_dir,
                Some(&linked_parent.join("claude-science")),
                &app_bin,
                None,
            )
            .is_err(),
            "an explicit path with a symlinked parent must fail closed"
        );

        write_fake_bin(&app_bin, 0o755)?;
        let failed_app_preflight = science_runtime_preflight_for_paths(&data_dir, None, &app_bin)?;
        assert_eq!(failed_app_preflight["status"], "cached_choice_required");
        assert!(
            select_science_runtime_for_paths(&data_dir, None, &app_bin, None)
                .expect_err("failed App preflight must offer, not implicitly use, cache")
                .contains("SCIENCE_RUNTIME_CHOICE_REQUIRED")
        );

        fs::set_permissions(&app_bin, fs::Permissions::from_mode(0o644))?;
        let preflight = science_runtime_preflight_for_paths(&data_dir, None, &app_bin)?;
        assert_eq!(preflight["status"], "cached_choice_required");
        assert_eq!(preflight["cached_version"], "fake-cache-1");
        let no_choice = select_science_runtime_for_paths(&data_dir, None, &app_bin, None)
            .expect_err("cache must not launch without one-shot authorization");
        assert!(no_choice.contains("SCIENCE_RUNTIME_CHOICE_REQUIRED"));
        let cached =
            select_science_runtime_for_paths(&data_dir, None, &app_bin, Some(CACHED_ONCE_CHOICE))?;
        assert_eq!(cached.path, cached_bin);
        assert_eq!(cached.source, ScienceRuntimeSource::CachedOnce);
        assert_eq!(cached.version.as_deref(), Some("fake-cache-1"));

        write_fake_bin(&cached_bin, 0o755)?;
        let preflight = science_runtime_preflight_for_paths(&data_dir, None, &app_bin)?;
        assert_eq!(preflight["status"], "missing");
        assert!(select_science_runtime_for_paths(
            &data_dir,
            None,
            &app_bin,
            Some(CACHED_ONCE_CHOICE),
        )
        .is_err());

        fs::set_permissions(&cached_bin, fs::Permissions::from_mode(0o644))?;
        assert_eq!(
            science_runtime_preflight_for_paths(&data_dir, None, &app_bin)?["status"],
            "missing"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn locally_validated_updated_runtime_wins_over_app_and_is_fingerprint_bound(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-official-updated")?;
        let home = root.join("home");
        let data_dir = home.join(".csswitch/sandbox/home/.claude-science");
        let updated_bin = home.join(".claude-science/bin/claude-science");
        let app_bin = root.join("app-claude-science");
        let explicit_bin = root.join("explicit-claude-science");
        write_fake_version_bin(&updated_bin, 0o755, "fake-official-updated-v2")?;
        {
            let mut file = fs::OpenOptions::new().append(true).open(&updated_bin)?;
            file.write_all(b"\n#")?;
            file.write_all(&vec![b' '; super::MIN_SCIENCE_BINARY_SIZE as usize])?;
        }
        write_fake_version_bin(&app_bin, 0o755, "fake-app-v1")?;
        write_fake_version_bin(&explicit_bin, 0o755, "fake-explicit-v3")?;

        assert_eq!(
            official_updated_science_bin_for_home(&home, false).as_deref(),
            Some(updated_bin.as_path())
        );
        assert!(official_updated_science_bin_for_home(&home, true).is_none());

        let snapshot = official_updated_snapshot_for_home(
            &home,
            &root.join("runtime-snapshots/science"),
            false,
        )?
        .expect("updated snapshot");
        assert_ne!(snapshot, updated_bin);

        let preflight = science_runtime_preflight_for_paths_with_updated(
            &data_dir,
            None,
            Some(&snapshot),
            &app_bin,
        )
        .expect("the locally validated updater snapshot must pass preflight");
        assert_eq!(preflight["selected_source"], "official_updated");
        assert_eq!(preflight["selected_version"], "fake-official-updated-v2");

        let selected = select_science_runtime_for_paths_with_updated(
            &data_dir,
            None,
            Some(&snapshot),
            &app_bin,
            None,
        )
        .expect("the locally validated updater snapshot must be selected");
        assert_eq!(selected.path, snapshot);
        assert_eq!(selected.source, ScienceRuntimeSource::OfficialUpdated);
        assert!(runtime_identity_is_current(&selected));

        let explicit = select_science_runtime_for_paths_with_updated(
            &data_dir,
            Some(&explicit_bin),
            Some(&selected.path),
            &app_bin,
            None,
        )
        .expect("a valid explicit override must still take priority");
        assert_eq!(explicit.source, ScienceRuntimeSource::Explicit);

        write_fake_version_bin(&updated_bin, 0o755, "fake-official-updated-v3")?;
        assert!(
            runtime_identity_is_current(&selected),
            "an updater replacement must not change the running snapshot identity"
        );
        fs::set_permissions(&selected.path, fs::Permissions::from_mode(0o700))?;
        write_fake_version_bin(&selected.path, 0o755, "fake-snapshot-tampered")?;
        assert!(!runtime_identity_is_current(&selected));

        write_fake_bin(&selected.path, 0o755)?;
        let preflight_error = science_runtime_preflight_for_paths_with_updated(
            &data_dir,
            None,
            Some(&selected.path),
            &app_bin,
        )
        .expect_err("an invalid updater snapshot must not fall through to the App seed");
        assert!(preflight_error.contains("已拒绝回退旧 App"));
        let selection_error = select_science_runtime_for_paths_with_updated(
            &data_dir,
            None,
            Some(&selected.path),
            &app_bin,
            None,
        )
        .expect_err("an invalid updater snapshot must not select the App seed");
        assert!(selection_error.contains("已拒绝回退旧 App"));

        fs::set_permissions(
            home.join(".claude-science/bin"),
            fs::Permissions::from_mode(0o775),
        )?;
        assert!(official_updated_science_bin_for_home(&home, false).is_none());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn historical_updated_snapshots_remain_recoverable_after_source_replacement(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-updated-snapshot-recovery")?;
        let home = root.join("home");
        let candidate = home.join(".claude-science/bin/claude-science");
        let snapshots = root.join("runtime-snapshots/science");
        write_padded_fake_version_bin(&candidate, "fake-updater-a")?;
        let snapshot_a =
            official_updated_snapshot_for_home(&home, &snapshots, false)?.expect("snapshot A");

        write_padded_fake_version_bin(&candidate, "fake-updater-b")?;
        let snapshot_b =
            official_updated_snapshot_for_home(&home, &snapshots, false)?.expect("snapshot B");
        assert_ne!(snapshot_a, snapshot_b);

        let unrelated = root.join("not-a-runtime");
        write_padded_fake_version_bin(&unrelated, "unrelated")?;
        assert_eq!(
            official_updated_snapshot_from_process_paths(
                &snapshots,
                &[unrelated, snapshot_a.clone()],
                false,
            )?,
            Some(snapshot_a),
            "recovery validates only the executable reported for the live listener"
        );
        assert_eq!(
            official_updated_snapshot_from_process_paths(
                &snapshots,
                std::slice::from_ref(&snapshot_b),
                false,
            )?,
            Some(snapshot_b)
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn snapshot_root_symlink_is_rejected_without_mutating_target(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-snapshot-root-symlink")?;
        let target = root.join("target");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        let link = root.join("snapshot-root");
        symlink(&target, &link)?;

        assert!(secure_runtime_snapshot_root(&link).is_err());
        assert_eq!(
            fs::symlink_metadata(&target)?.permissions().mode() & 0o777,
            0o755
        );
        assert!(target.read_dir()?.next().is_none());

        let ancestor_target = root.join("ancestor-target");
        fs::create_dir(&ancestor_target)?;
        fs::set_permissions(&ancestor_target, fs::Permissions::from_mode(0o755))?;
        let linked_ancestor = root.join("linked-ancestor");
        symlink(&ancestor_target, &linked_ancestor)?;
        assert!(secure_runtime_snapshot_root(&linked_ancestor.join("science")).is_err());
        assert_eq!(
            fs::symlink_metadata(&ancestor_target)?.permissions().mode() & 0o777,
            0o755
        );
        assert!(ancestor_target.read_dir()?.next().is_none());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires CSSWITCH_REAL_SCIENCE_BIN pointing to an installed updater executable"]
    fn real_updated_runtime_candidate_is_eligible_without_reading_real_science_data(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let source = std::env::var_os("CSSWITCH_REAL_SCIENCE_BIN")
            .map(std::path::PathBuf::from)
            .ok_or("CSSWITCH_REAL_SCIENCE_BIN is required")?;
        let root = unique_temp_dir("science-real-updated")?;
        let home = root.join("home");
        let candidate = home.join(".claude-science/bin/claude-science");
        fs::create_dir_all(candidate.parent().expect("candidate parent"))?;
        fs::copy(&source, &candidate)?;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))?;

        assert_eq!(
            official_updated_science_bin_for_home(&home, true).as_deref(),
            Some(candidate.as_path()),
            "the fixed updater executable should pass the same local identity guards used in production"
        );
        let snapshot = official_updated_snapshot_for_home(
            &home,
            &root.join("runtime-snapshots/science"),
            true,
        )?
        .expect("real updater snapshot");
        assert_ne!(snapshot, candidate);
        assert_eq!(fs::read(&snapshot)?, fs::read(&candidate)?);
        let isolated_home = root.join("isolated-home");
        fs::create_dir_all(&isolated_home)?;
        let output = std::process::Command::new(&snapshot)
            .arg("--version")
            .env("HOME", &isolated_home)
            .output()?;
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)?.starts_with("claude-science "));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn cached_runtime_symlink_is_never_offered_or_executed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-cache-symlink")?;
        let data_dir = root.join("home").join(".claude-science");
        let cached_bin = data_dir.join("bin").join("claude-science");
        let target = root.join("target-claude-science");
        let missing_app = root.join("missing-app-claude-science");
        write_fake_version_bin(&target, 0o755, "fake-target-1")?;
        fs::create_dir_all(cached_bin.parent().expect("cached parent"))?;
        symlink(&target, &cached_bin)?;

        let preflight = science_runtime_preflight_for_paths(&data_dir, None, &missing_app)?;
        assert_eq!(preflight["status"], "missing");
        assert!(select_science_runtime_for_paths(
            &data_dir,
            None,
            &missing_app,
            Some(CACHED_ONCE_CHOICE),
        )
        .is_err());

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn replacing_installed_app_uses_new_version_without_mutating_cache_or_data_dir(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("science-app-upgrade")?;
        let data_dir = root.join("home").join(".claude-science");
        let cached_bin = data_dir.join("bin").join("claude-science");
        let app_bin = root.join("app-claude-science");
        let state_marker = data_dir.join("persistent-state.txt");
        write_fake_version_bin(&cached_bin, 0o755, "fake-cache-old")?;
        fs::write(&state_marker, "keep-me")?;
        let cached_before = fs::read(&cached_bin)?;

        write_fake_version_bin(&app_bin, 0o755, "fake-app-v1")?;
        let first = select_science_runtime_for_paths(&data_dir, None, &app_bin, None)?;
        assert_eq!(first.version.as_deref(), Some("fake-app-v1"));

        write_fake_version_bin(&app_bin, 0o755, "fake-app-v2")?;
        let second = select_science_runtime_for_paths(&data_dir, None, &app_bin, None)?;
        assert_eq!(second.version.as_deref(), Some("fake-app-v2"));
        assert_eq!(second.path, app_bin);
        assert_eq!(fs::read_to_string(&state_marker)?, "keep-me");
        assert_eq!(fs::read(&cached_bin)?, cached_before);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn sandbox_home_is_writable_under_config_dir() {
        let h = sandbox_home();
        assert!(h.ends_with("sandbox/home"), "应以 sandbox/home 结尾：{h:?}");
        assert!(
            h.to_string_lossy().contains(".csswitch"),
            "应在 .csswitch 下：{h:?}"
        );
    }

    #[test]
    fn sandbox_url_falls_back_to_localhost_when_cli_absent() {
        let root = unique_temp_dir("science-url-fallback").unwrap();
        let bin = root.join("claude-science");
        write_fake_bin(&bin, 0o755).unwrap();
        let runtime = ScienceRuntimeIdentity {
            path: bin,
            source: ScienceRuntimeSource::InstalledApp,
            version: None,
            fingerprint: science_executable_fingerprint(&root.join("claude-science")).unwrap(),
        };
        assert_eq!(sandbox_url(8990, &runtime), "http://127.0.0.1:8990");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sandbox_identity_does_not_trust_health_when_cli_absent() {
        let root = unique_temp_dir("science-identity-fallback").unwrap();
        let bin = root.join("claude-science");
        write_fake_bin(&bin, 0o755).unwrap();
        let runtime = ScienceRuntimeIdentity {
            path: bin,
            source: ScienceRuntimeSource::InstalledApp,
            version: None,
            fingerprint: science_executable_fingerprint(&root.join("claude-science")).unwrap(),
        };
        assert!(!sandbox_running_ours(9, &runtime));
        fs::remove_dir_all(root).unwrap();
    }

    fn status_output(code: i32, stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn unique_temp_dir(name: &str) -> std::io::Result<std::path::PathBuf> {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "csswitch-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p)?;
        p.canonicalize()
    }

    fn write_fake_bin(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    fn write_padded_fake_version_bin(path: &std::path::Path, version: &str) -> std::io::Result<()> {
        write_fake_version_bin(path, 0o755, version)?;
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(b"\n#")?;
        file.write_all(&vec![b' '; super::MIN_SCIENCE_BINARY_SIZE as usize])?;
        Ok(())
    }

    fn write_fake_version_bin(
        path: &std::path::Path,
        mode: u32,
        version: &str,
    ) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then printf '%s\\n' '{}'; exit 0; fi\nexit 0\n",
                version
            ),
        )?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    fn write_counted_version_bin(
        path: &std::path::Path,
        count: &std::path::Path,
        version: &str,
    ) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then count=$(cat '{}' 2>/dev/null || echo 0); count=$((count + 1)); printf '%s' \"$count\" > '{}'; printf '%s\\n' '{}'; exit 0; fi\nexit 0\n",
                count.display(),
                count.display(),
                version
            ),
        )?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
    }
}
