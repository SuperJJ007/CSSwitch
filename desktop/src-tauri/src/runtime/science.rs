use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::Runtime;

use crate::{config, proc};

use super::system::{asset_root, kill_child};

pub(crate) const SCIENCE_BIN: &str =
    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science";
pub(crate) const SCIENCE_DOWNLOAD_URL: &str = "https://claude.com/download";
pub(crate) const CACHED_ONCE_CHOICE: &str = "cached_once";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScienceRuntimeSource {
    Explicit,
    InstalledApp,
    CachedOnce,
}

impl ScienceRuntimeSource {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
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
        let fingerprint = science_executable_fingerprint(&canonical)
            .ok_or("Science binary 指纹不可用，无法启用 Skill attach control")?;
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
            },
            home: sandbox_home(),
            data_dir: sandbox_data_dir(),
            sandbox_port,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScienceExecutableFingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    mode: u32,
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

fn science_executable_fingerprint(path: &Path) -> Option<ScienceExecutableFingerprint> {
    if !is_executable_file(path) {
        return None;
    }
    let metadata = path.metadata().ok()?;
    Some(ScienceExecutableFingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        mode: metadata.mode(),
    })
}

fn cached_science_bin(data_dir: &Path) -> PathBuf {
    data_dir.join("bin").join("claude-science")
}

fn safe_science_version(path: &Path) -> Option<String> {
    let output = Command::new(path)
        .arg("--version")
        .env("HOME", sandbox_home())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
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
}

fn runtime_identity(
    path: PathBuf,
    source: ScienceRuntimeSource,
    version_cache: &ScienceVersionCache,
) -> ScienceRuntimeIdentity {
    let version = version_cache.version(&path);
    ScienceRuntimeIdentity {
        path,
        source,
        version,
    }
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
        app_bin,
        &ScienceVersionCache::default(),
    )
}

fn science_runtime_preflight_for_paths_cached(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    app_bin: &Path,
    version_cache: &ScienceVersionCache,
) -> Result<Value, String> {
    if let Some(bin) = explicit_bin {
        if !is_explicit_executable_file(bin) {
            return Err("显式 SCIENCE_BIN 不是安全的绝对可执行文件；已拒绝回退".into());
        }
        let version = version_cache
            .version(bin)
            .ok_or("显式 SCIENCE_BIN 未通过版本预检；已拒绝回退")?;
        return Ok(json!({
            "status": "installed_ready",
            "selected_source": ScienceRuntimeSource::Explicit.code(),
            "selected_version": version,
            "cached_version": Value::Null,
            "download_url": SCIENCE_DOWNLOAD_URL,
        }));
    }
    if let Some(version) = version_cache.version(app_bin) {
        return Ok(json!({
            "status": "installed_ready",
            "selected_source": ScienceRuntimeSource::InstalledApp.code(),
            "selected_version": version,
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
    confirmed_stopped: Option<&ScienceRuntimeIdentity>,
) -> Result<Value, String> {
    if let Ok(cfg) = config::load_from(&config::default_dir()) {
        if let Some(runtime) = confirmed_stopped {
            if runtime.source != ScienceRuntimeSource::CachedOnce
                && !loopback_port_accepts_tcp(cfg.sandbox_port)
            {
                if let Some(version) = version_cache.version(&runtime.path) {
                    return Ok(json!({
                        "status": "installed_ready",
                        "selected_source": runtime.source.code(),
                        "selected_version": version,
                        "cached_version": Value::Null,
                        "download_url": SCIENCE_DOWNLOAD_URL,
                    }));
                }
            }
        }
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
    science_runtime_preflight_for_paths_cached(
        &data_dir,
        explicit.as_deref(),
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
        app_bin,
        choice,
        &ScienceVersionCache::default(),
    )
}

fn select_science_runtime_for_paths_cached(
    data_dir: &Path,
    explicit_bin: Option<&Path>,
    app_bin: &Path,
    choice: Option<&str>,
    version_cache: &ScienceVersionCache,
) -> Result<ScienceRuntimeIdentity, String> {
    if let Some(bin) = explicit_bin {
        if !is_explicit_executable_file(bin) {
            return Err("显式 SCIENCE_BIN 不是安全的绝对可执行文件；已拒绝回退".into());
        }
        let version = version_cache
            .version(bin)
            .ok_or("显式 SCIENCE_BIN 未通过版本预检；已拒绝回退")?;
        return Ok(ScienceRuntimeIdentity {
            path: bin.to_path_buf(),
            source: ScienceRuntimeSource::Explicit,
            version: Some(version),
        });
    }
    if let Some(version) = version_cache.version(app_bin) {
        return Ok(ScienceRuntimeIdentity {
            path: app_bin.to_path_buf(),
            source: ScienceRuntimeSource::InstalledApp,
            version: Some(version),
        });
    }
    let cached = cached_science_bin(data_dir);
    let cached_version = version_cache.version(&cached);
    if choice == Some(CACHED_ONCE_CHOICE) {
        let version = cached_version
            .ok_or("缓存 Science 版本无法确认；请安装或更新 Claude Science 后再试")?;
        return Ok(ScienceRuntimeIdentity {
            path: cached,
            source: ScienceRuntimeSource::CachedOnce,
            version: Some(version),
        });
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
    select_science_runtime_for_paths_cached(
        &data_dir,
        explicit.as_deref(),
        Path::new(SCIENCE_BIN),
        choice,
        version_cache,
    )
}

fn runtime_probe_candidates(
    version_cache: &ScienceVersionCache,
) -> Result<Vec<ScienceRuntimeIdentity>, String> {
    if let Some(explicit) = explicit_science_bin()? {
        return Ok(vec![runtime_identity(
            explicit,
            ScienceRuntimeSource::Explicit,
            version_cache,
        )]);
    }
    let mut candidates = Vec::new();
    let app = PathBuf::from(SCIENCE_BIN);
    if version_cache.version(&app).is_some() {
        candidates.push(runtime_identity(
            app,
            ScienceRuntimeSource::InstalledApp,
            version_cache,
        ));
    }
    let cached = cached_science_bin(&sandbox_data_dir());
    if version_cache.version(&cached).is_some() {
        candidates.push(runtime_identity(
            cached,
            ScienceRuntimeSource::CachedOnce,
            version_cache,
        ));
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

fn listener_uses_runtime(port: u16, runtime: &ScienceRuntimeIdentity) -> bool {
    let listener = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output();
    let Ok(listener) = listener else {
        return false;
    };
    if !listener.status.success() {
        return false;
    }
    let Ok(stdout) = String::from_utf8(listener.stdout) else {
        return false;
    };
    let mut pids = stdout.lines().map(str::trim).filter(|pid| !pid.is_empty());
    let Some(pid) = pids.next() else {
        return false;
    };
    if pids.any(|other| other != pid) {
        return false;
    }
    #[cfg(test)]
    if test_listener_marker_matches(pid, runtime) {
        return true;
    }
    let Ok(expected) = runtime.path.canonicalize() else {
        return false;
    };
    let text_files = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", pid, "-d", "txt", "-Fn"])
        .output();
    let Ok(text_files) = text_files else {
        return false;
    };
    if !text_files.status.success() {
        return false;
    }
    String::from_utf8_lossy(&text_files.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter_map(|path| Path::new(path).canonicalize().ok())
        .any(|path| path == expected)
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

/// Put the CSSwitch-managed browser session on a distinct cookie origin from
/// the official Science instance. Browser cookies are scoped by host, not by
/// port, so opening both instances as `localhost:<port>` lets either login
/// replace the other's session cookie even though their daemons and data dirs
/// are isolated.
fn isolate_sandbox_browser_origin(raw: &str, port: u16) -> Option<String> {
    let localhost = format!("http://localhost:{port}");
    let loopback = format!("http://127.0.0.1:{port}");
    let suffix = raw
        .strip_prefix(&localhost)
        .or_else(|| raw.strip_prefix(&loopback))?;
    if !suffix.is_empty() && !suffix.starts_with('/') && !suffix.starts_with('?') {
        return None;
    }
    Some(format!("{loopback}{suffix}"))
}

/// Return the sandbox UI URL on the dedicated `127.0.0.1` browser origin,
/// falling back to the plain loopback port when the CLI emits no usable URL.
pub(crate) fn sandbox_url(port: u16, runtime: &ScienceRuntimeIdentity) -> String {
    let home = sandbox_home();
    let data_dir = sandbox_data_dir();
    if let Ok(out) = Command::new(&runtime.path)
        .arg("url")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", &home)
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(url) = first_http_url(&s) {
            if let Some(isolated) = isolate_sandbox_browser_origin(&url, port) {
                return isolated;
            }
        }
    }
    format!("http://127.0.0.1:{port}")
}

fn runtime_status(runtime: &ScienceRuntimeIdentity) -> Option<bool> {
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
    let candidates = runtime_probe_candidates(version_cache)?;
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
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    use super::{
        classify_known_runtime_state, classify_sandbox_state, first_http_url,
        isolate_sandbox_browser_origin, runtime_status_value, sandbox_home, sandbox_running_ours,
        sandbox_url, science_runtime_preflight_for_paths,
        science_runtime_preflight_for_paths_cached, science_status_running,
        select_science_runtime_for_paths, select_science_runtime_for_paths_cached,
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
    fn sandbox_browser_origin_is_distinct_from_official_localhost_cookie_scope() {
        assert_eq!(
            isolate_sandbox_browser_origin("http://localhost:8990/?nonce=single-use-token", 8990)
                .as_deref(),
            Some("http://127.0.0.1:8990/?nonce=single-use-token")
        );
        assert_eq!(
            isolate_sandbox_browser_origin("http://127.0.0.1:8990/project/one?nonce=token", 8990)
                .as_deref(),
            Some("http://127.0.0.1:8990/project/one?nonce=token")
        );
        assert!(
            isolate_sandbox_browser_origin("http://localhost:8765/?nonce=official", 8990).is_none()
        );
        assert!(isolate_sandbox_browser_origin(
            "http://localhost:8990.evil.invalid/?nonce=bad",
            8990
        )
        .is_none());
        assert!(
            isolate_sandbox_browser_origin("https://localhost:8990/?nonce=wrong-scheme", 8990)
                .is_none()
        );
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
            science_runtime_preflight_for_paths_cached(&data_dir, None, &app_bin, &cache)?;
        assert_eq!(preflight["selected_version"], "claude-science cache-v1");
        let selected =
            select_science_runtime_for_paths_cached(&data_dir, None, &app_bin, None, &cache)?;
        assert_eq!(selected.version.as_deref(), Some("claude-science cache-v1"));
        assert_eq!(fs::read_to_string(&count)?, "1");

        write_counted_version_bin(&app_bin, &count, "claude-science cache-version-two")?;
        let selected =
            select_science_runtime_for_paths_cached(&data_dir, None, &app_bin, None, &cache)?;
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
    fn sandbox_url_falls_back_to_loopback_ip_when_cli_absent() {
        let root = unique_temp_dir("science-url-fallback").unwrap();
        let bin = root.join("claude-science");
        write_fake_bin(&bin, 0o755).unwrap();
        let runtime = ScienceRuntimeIdentity {
            path: bin,
            source: ScienceRuntimeSource::InstalledApp,
            version: None,
        };
        assert_eq!(sandbox_url(8990, &runtime), "http://127.0.0.1:8990");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sandbox_url_normalizes_cli_localhost_nonce_to_loopback_ip() {
        let root = unique_temp_dir("science-url-cookie-origin").unwrap();
        let bin = root.join("claude-science");
        fs::write(
            &bin,
            "#!/bin/sh\nprintf '%s\\n' 'http://localhost:8990/?nonce=one-time'\n",
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = ScienceRuntimeIdentity {
            path: bin,
            source: ScienceRuntimeSource::InstalledApp,
            version: None,
        };
        assert_eq!(
            sandbox_url(8990, &runtime),
            "http://127.0.0.1:8990/?nonce=one-time"
        );
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
