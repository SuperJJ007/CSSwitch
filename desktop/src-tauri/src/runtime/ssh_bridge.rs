use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use glob::glob;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{value, Array, DocumentMut};

const MAX_ALIAS_COUNT: usize = 256;
const MAX_CONFIG_FILES: usize = 128;
const MAX_INCLUDE_DEPTH: usize = 16;
const MAX_CONFIG_FILE_BYTES: u64 = 256 * 1024;
const MAX_CONFIG_TOTAL_BYTES: u64 = 1024 * 1024;
const MAX_SCIENCE_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_STATE_BYTES: u64 = 128 * 1024;
const STATE_FILE: &str = "csswitch-ssh-bridge.v1.json";

#[derive(Default)]
struct ParseState {
    visited: HashSet<PathBuf>,
    aliases: Vec<String>,
    alias_set: BTreeSet<String>,
    total_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct BridgeState {
    schema_version: u32,
    original_ssh_hosts: Option<Vec<String>>,
    effective_ssh_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_effective_ssh_hosts: Option<Vec<String>>,
    managed_hosts: Vec<String>,
}

fn ssh_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut started = false;
    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            started = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            started = true;
            continue;
        }
        if ch == '#' && !quoted {
            break;
        }
        if ch.is_whitespace() && !quoted {
            if started {
                words.push(std::mem::take(&mut current));
                started = false;
            }
            continue;
        }
        current.push(ch);
        started = true;
    }
    if escaped || quoted {
        return Err("SSH config 包含未闭合的转义或引号".into());
    }
    if started {
        words.push(current);
    }
    Ok(words)
}

pub(crate) fn is_concrete_alias(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('-')
        && !value.starts_with('!')
        && !value.bytes().any(|byte| byte.is_ascii_control())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'@' | b'%' | b'+' | b'-')
        })
}

fn expand_include(pattern: &str, home: &Path) -> Result<Vec<PathBuf>, String> {
    if pattern
        .as_bytes()
        .iter()
        .any(|byte| byte.is_ascii_control())
    {
        return Err("SSH Include 包含控制字符".into());
    }
    let home_text = home
        .to_str()
        .ok_or("系统 HOME 路径不是有效 UTF-8，不能解析 SSH Include")?;
    let expanded = if pattern == "~" {
        home_text.to_string()
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else if pattern.contains("%d") {
        pattern.replace("%d", home_text)
    } else if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        home.join(".ssh")
            .join(pattern)
            .to_string_lossy()
            .into_owned()
    };
    if expanded.contains('%') {
        return Err("SSH Include 使用了 CSSwitch 无法安全展开的 token".into());
    }
    let mut paths = glob(&expanded)
        .map_err(|_| "SSH Include glob 非法".to_string())?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn parse_config(
    path: &Path,
    home: &Path,
    depth: usize,
    state: &mut ParseState,
) -> Result<(), String> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err("SSH Include 深度超过安全上限".into());
    }
    let canonical =
        fs::canonicalize(path).map_err(|_| "无法读取已授权的 SSH config".to_string())?;
    if !state.visited.insert(canonical.clone()) {
        return Ok(());
    }
    if state.visited.len() > MAX_CONFIG_FILES {
        return Err("SSH Include 文件数超过安全上限".into());
    }
    let metadata =
        fs::metadata(&canonical).map_err(|_| "无法检查已授权的 SSH config".to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_FILE_BYTES {
        return Err("SSH config 不是普通文件或超过安全大小上限".into());
    }
    state.total_bytes = state
        .total_bytes
        .checked_add(metadata.len())
        .ok_or("SSH config 总大小溢出")?;
    if state.total_bytes > MAX_CONFIG_TOTAL_BYTES {
        return Err("SSH config 总大小超过安全上限".into());
    }
    let text =
        fs::read_to_string(&canonical).map_err(|_| "SSH config 不是有效 UTF-8".to_string())?;
    for line in text.lines() {
        let words = ssh_words(line)?;
        let Some(first) = words.first() else {
            continue;
        };
        let mut keyword = first.clone();
        let mut values = words.iter().skip(1).cloned().collect::<Vec<_>>();
        if let Some(index) = keyword.find('=') {
            let inline = keyword[index + 1..].to_string();
            keyword.truncate(index);
            if !inline.is_empty() {
                values.insert(0, inline);
            }
        } else if values.first().is_some_and(|value| value == "=") {
            values.remove(0);
        } else if let Some(inline) = values
            .first()
            .and_then(|value| value.strip_prefix('='))
            .map(str::to_string)
        {
            if inline.is_empty() {
                values.remove(0);
            } else {
                values[0] = inline;
            }
        }
        if keyword.eq_ignore_ascii_case("host") {
            for alias in values.iter().filter(|alias| is_concrete_alias(alias)) {
                if state.alias_set.insert(alias.to_string()) {
                    state.aliases.push(alias.to_string());
                    if state.aliases.len() > MAX_ALIAS_COUNT {
                        return Err("可枚举 SSH Host alias 超过安全上限".into());
                    }
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            for pattern in &values {
                for included in expand_include(pattern, home)? {
                    parse_config(&included, home, depth + 1, state)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn system_ssh_hosts_for_home(home: &Path) -> Result<Vec<String>, String> {
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能启用系统 SSH 配置。".into());
    }
    let mut state = ParseState::default();
    parse_config(&home.join(".ssh/config"), home, 0, &mut state)?;
    if state.aliases.is_empty() {
        return Err("已授权 SSH config 中没有可枚举的具体 Host alias；通配 Host 不能用于 Science 前置校验。".into());
    }
    Ok(state.aliases)
}

pub(crate) fn system_ssh_hosts() -> Result<Vec<String>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能启用系统 SSH 配置。")?;
    system_ssh_hosts_for_home(&home)
}

pub(crate) fn system_ssh_bridge_fingerprint(enabled: bool) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"csswitch-system-ssh-bridge-v2\0");
    if enabled {
        for alias in system_ssh_hosts()? {
            hasher.update(alias.as_bytes());
            hasher.update(b"\0");
        }
    } else {
        hasher.update(b"disabled");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn checked_file_bytes(path: &Path, limit: u64, label: &str) -> Result<Option<Vec<u8>>, String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("无法安全打开{label}")),
    };
    let metadata = file.metadata().map_err(|_| format!("无法检查{label}"))?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > limit
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!("{label}不是安全普通文件或超过大小上限"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| format!("无法读取{label}"))?;
    if bytes.len() as u64 != metadata.len() {
        return Err(format!("{label}在读取期间发生变化"));
    }
    Ok(Some(bytes))
}

fn reject_symlink_components(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("隔离 Science 状态路径不是绝对路径".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("隔离 Science 状态路径包含符号链接".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err("无法检查隔离 Science 状态路径".into()),
        }
    }
    Ok(())
}

fn read_document(path: &Path) -> Result<DocumentMut, String> {
    match checked_file_bytes(path, MAX_SCIENCE_CONFIG_BYTES, "隔离 Science config.toml")? {
        Some(bytes) => String::from_utf8(bytes)
            .map_err(|_| "隔离 Science config.toml 不是有效 UTF-8".to_string())?
            .parse::<DocumentMut>()
            .map_err(|_| "隔离 Science config.toml 不是有效 TOML".to_string()),
        None => Ok(DocumentMut::new()),
    }
}

fn read_ssh_hosts(document: &DocumentMut) -> Result<Option<Vec<String>>, String> {
    let Some(item) = document.get("ssh_hosts") else {
        return Ok(None);
    };
    let array = item
        .as_array()
        .ok_or("隔离 Science config.toml 的 ssh_hosts 不是字符串数组")?;
    let mut hosts = Vec::with_capacity(array.len());
    for item in array.iter() {
        let host = item
            .as_str()
            .ok_or("隔离 Science config.toml 的 ssh_hosts 不是字符串数组")?;
        if host.is_empty() || host.len() > 255 || host.bytes().any(|byte| byte.is_ascii_control()) {
            return Err("隔离 Science config.toml 的 ssh_hosts 包含非法值".into());
        }
        hosts.push(host.to_string());
    }
    Ok(Some(hosts))
}

fn set_ssh_hosts(document: &mut DocumentMut, hosts: Option<&[String]>) {
    match hosts {
        Some(hosts) => {
            let mut array = Array::new();
            for host in hosts {
                array.push(host.as_str());
            }
            document["ssh_hosts"] = value(array);
        }
        None => {
            document.remove("ssh_hosts");
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    reject_symlink_components(path)?;
    let parent = path.parent().ok_or("隔离状态路径没有父目录")?;
    fs::create_dir_all(parent).map_err(|_| "无法创建隔离 Science 状态目录".to_string())?;
    let parent_meta = fs::symlink_metadata(parent).map_err(|_| "无法检查隔离 Science 状态目录")?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if parent_meta.file_type().is_symlink()
        || !parent_meta.is_dir()
        || parent_meta.uid() != uid
        || parent_meta.mode() & 0o022 != 0
    {
        return Err("隔离 Science 状态目录不安全".into());
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err("拒绝覆盖非普通隔离状态文件".into());
        }
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("隔离状态文件名非法")?;
    let tmp = parent.join(format!(".{name}.{}.tmp", crate::config::new_id()));
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(&tmp).map_err(|_| "无法创建隔离状态临时文件")?;
    let result = (|| -> Result<(), String> {
        file.write_all(bytes)
            .map_err(|_| "无法写入隔离状态临时文件")?;
        file.sync_all().map_err(|_| "无法同步隔离状态临时文件")?;
        fs::rename(&tmp, path).map_err(|_| "无法原子提交隔离状态文件")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "无法收紧隔离状态文件权限")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn read_state(path: &Path) -> Result<Option<BridgeState>, String> {
    let Some(bytes) = checked_file_bytes(path, MAX_STATE_BYTES, "CSSwitch SSH bridge sidecar")?
    else {
        return Ok(None);
    };
    let state: BridgeState = serde_json::from_slice(&bytes)
        .map_err(|_| "CSSwitch SSH bridge sidecar 非法，拒绝猜测修改 ssh_hosts".to_string())?;
    if state.schema_version != 1 {
        return Err("CSSwitch SSH bridge sidecar 版本不受支持".into());
    }
    Ok(Some(state))
}

fn merge_unique(original: Option<&[String]>, managed: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut merged = Vec::new();
    for host in original.into_iter().flatten().chain(managed.iter()) {
        if seen.insert(host.clone()) {
            merged.push(host.clone());
        }
    }
    merged
}

fn current_matches_owned_state(current: Option<&[String]>, state: &BridgeState) -> bool {
    current == Some(state.effective_ssh_hosts.as_slice())
        || current == state.original_ssh_hosts.as_deref()
        || state
            .previous_effective_ssh_hosts
            .as_deref()
            .is_some_and(|previous| current == Some(previous))
}

pub(crate) fn prepare_science_ssh_bridge_for(
    sandbox_home: &Path,
    system_home: &Path,
) -> Result<Vec<String>, String> {
    reject_symlink_components(sandbox_home)?;
    let managed_hosts = system_ssh_hosts_for_home(system_home)?;
    let data_dir = sandbox_home.join(".claude-science");
    let config_path = data_dir.join("config.toml");
    let state_path = data_dir.join(STATE_FILE);
    reject_symlink_components(&config_path)?;
    reject_symlink_components(&state_path)?;
    let mut document = read_document(&config_path)?;
    let current = read_ssh_hosts(&document)?;
    let prior = read_state(&state_path)?;
    let original = match &prior {
        Some(state) if current_matches_owned_state(current.as_deref(), state) => {
            state.original_ssh_hosts.clone()
        }
        Some(_) => {
            return Err(
                "隔离 Science ssh_hosts 在授权期间被外部修改；已保持原样，请先人工确认冲突".into(),
            )
        }
        None => current.clone(),
    };
    let effective = merge_unique(original.as_deref(), &managed_hosts);
    let transitional = BridgeState {
        schema_version: 1,
        original_ssh_hosts: original.clone(),
        effective_ssh_hosts: effective.clone(),
        previous_effective_ssh_hosts: current.clone(),
        managed_hosts: managed_hosts.clone(),
    };
    atomic_write(
        &state_path,
        &serde_json::to_vec_pretty(&transitional).map_err(|_| "无法序列化 SSH bridge sidecar")?,
    )?;
    set_ssh_hosts(&mut document, Some(&effective));
    atomic_write(&config_path, document.to_string().as_bytes())?;
    let committed = BridgeState {
        previous_effective_ssh_hosts: None,
        ..transitional
    };
    atomic_write(
        &state_path,
        &serde_json::to_vec_pretty(&committed).map_err(|_| "无法序列化 SSH bridge sidecar")?,
    )?;
    Ok(managed_hosts)
}

pub(crate) fn prepare_science_ssh_bridge(sandbox_home: &Path) -> Result<Vec<String>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能启用系统 SSH 配置。")?;
    prepare_science_ssh_bridge_for(sandbox_home, &home)
}

pub(crate) fn revoke_science_ssh_bridge(sandbox_home: &Path) -> Result<(), String> {
    reject_symlink_components(sandbox_home)?;
    let data_dir = sandbox_home.join(".claude-science");
    let state_path = data_dir.join(STATE_FILE);
    let config_path = data_dir.join("config.toml");
    reject_symlink_components(&config_path)?;
    reject_symlink_components(&state_path)?;
    let Some(state) = read_state(&state_path)? else {
        return Ok(());
    };
    let mut document = read_document(&config_path)?;
    let current = read_ssh_hosts(&document)?;
    if !current_matches_owned_state(current.as_deref(), &state) {
        return Err("隔离 Science ssh_hosts 在授权期间被外部修改；已保持原样，拒绝猜测撤销".into());
    }
    set_ssh_hosts(&mut document, state.original_ssh_hosts.as_deref());
    atomic_write(&config_path, document.to_string().as_bytes())?;
    let metadata = fs::symlink_metadata(&state_path).map_err(|_| "无法检查 SSH bridge sidecar")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err("SSH bridge sidecar 不是安全普通文件".into());
    }
    fs::remove_file(&state_path).map_err(|_| "无法删除 SSH bridge sidecar")?;
    Ok(())
}

fn validate_science_ssh_bridge_for(
    sandbox_home: &Path,
    system_home: &Path,
) -> Result<Vec<String>, String> {
    reject_symlink_components(sandbox_home)?;
    let data_dir = sandbox_home.join(".claude-science");
    let config_path = data_dir.join("config.toml");
    let state_path = data_dir.join(STATE_FILE);
    reject_symlink_components(&config_path)?;
    reject_symlink_components(&state_path)?;
    let state = read_state(&state_path)?
        .ok_or("CSSwitch SSH bridge sidecar 缺失，拒绝复用运行中的 Science")?;
    let current = read_ssh_hosts(&read_document(&config_path)?)?;
    if current.as_deref() != Some(state.effective_ssh_hosts.as_slice()) {
        return Err("隔离 Science ssh_hosts 与 CSSwitch SSH bridge 状态不一致".into());
    }
    let expected = system_ssh_hosts_for_home(system_home)?;
    if state.managed_hosts != expected {
        return Err("系统 SSH Host alias 已变化，拒绝复用运行中的 Science".into());
    }
    Ok(expected)
}

pub(crate) fn validate_science_ssh_bridge(sandbox_home: &Path) -> Result<Vec<String>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能验证系统 SSH 配置。")?;
    validate_science_ssh_bridge_for(sandbox_home, &home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "csswitch-ssh-bridge-{label}-{}",
            crate::config::new_id()
        ));
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn write_private(path: &Path, value: impl AsRef<[u8]>) {
        fs::write(path, value).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn parses_concrete_hosts_from_includes_without_copying_host_options() {
        let home = tmpdir("parse");
        fs::create_dir_all(home.join(".ssh/conf.d")).unwrap();
        fs::write(
            home.join(".ssh/config"),
            "Host direct-a \"direct-b\" * !blocked -bad\n  HostName secret.example\nInclude=conf.d/*.conf\n",
        )
        .unwrap();
        fs::write(
            home.join(".ssh/conf.d/a.conf"),
            "host=included-a included-a\nInclude = ../config\nMatch host *\n",
        )
        .unwrap();
        let aliases = system_ssh_hosts_for_home(&home).unwrap();
        assert_eq!(aliases, ["direct-a", "direct-b", "included-a"]);
        assert!(!aliases.iter().any(|alias| alias.contains("secret")));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_wildcard_only_and_oversized_config() {
        let home = tmpdir("bounds");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/config"), "Host * ?foo [abc] !blocked\n").unwrap();
        assert!(system_ssh_hosts_for_home(&home).is_err());
        fs::write(
            home.join(".ssh/config"),
            vec![b'x'; (MAX_CONFIG_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(system_ssh_hosts_for_home(&home).is_err());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn toml_merge_is_idempotent_and_revoke_restores_only_ssh_hosts() {
        let root = tmpdir("toml");
        let home = root.join("outer");
        let sandbox = root.join("sandbox");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(sandbox.join(".claude-science")).unwrap();
        fs::write(home.join(".ssh/config"), "Host managed-a managed-b\n").unwrap();
        let original = "# keep this comment\nquiet_logs = true\nssh_hosts = [\"user-host\"]\n\n[conda]\nauto_install = false\n";
        let config = sandbox.join(".claude-science/config.toml");
        write_private(&config, original);

        let hosts = prepare_science_ssh_bridge_for(&sandbox, &home).unwrap();
        assert_eq!(hosts, ["managed-a", "managed-b"]);
        let once = fs::read_to_string(&config).unwrap();
        assert!(once.contains("# keep this comment"));
        assert!(once.contains("[conda]"));
        assert!(once.contains("user-host"));
        assert!(once.contains("managed-a"));
        prepare_science_ssh_bridge_for(&sandbox, &home).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), once);

        revoke_science_ssh_bridge(&sandbox).unwrap();
        let restored = fs::read_to_string(&config).unwrap();
        let document: DocumentMut = restored.parse().unwrap();
        assert_eq!(
            read_ssh_hosts(&document).unwrap(),
            Some(vec!["user-host".into()])
        );
        assert!(restored.contains("# keep this comment"));
        assert!(!sandbox.join(".claude-science").join(STATE_FILE).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn revoke_removes_field_that_was_absent_and_conflicts_fail_closed() {
        let root = tmpdir("conflict");
        let home = root.join("outer");
        let sandbox = root.join("sandbox");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(sandbox.join(".claude-science")).unwrap();
        fs::write(home.join(".ssh/config"), "Host managed\n").unwrap();
        let config = sandbox.join(".claude-science/config.toml");
        write_private(&config, "quiet_logs = true\n");
        prepare_science_ssh_bridge_for(&sandbox, &home).unwrap();
        write_private(
            &config,
            "quiet_logs = true\nssh_hosts = [\"foreign-edit\"]\n",
        );
        let before = fs::read(&config).unwrap();
        assert!(revoke_science_ssh_bridge(&sandbox).is_err());
        assert_eq!(fs::read(&config).unwrap(), before);

        fs::remove_file(sandbox.join(".claude-science").join(STATE_FILE)).unwrap();
        write_private(&config, "quiet_logs = true\n");
        prepare_science_ssh_bridge_for(&sandbox, &home).unwrap();
        revoke_science_ssh_bridge(&sandbox).unwrap();
        let document: DocumentMut = fs::read_to_string(&config).unwrap().parse().unwrap();
        assert!(document.get("ssh_hosts").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_and_revoke_reject_symlinked_science_data_before_reading() {
        let root = tmpdir("data-link");
        let home = root.join("outer");
        let sandbox = root.join("sandbox");
        let foreign = root.join("foreign");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&foreign).unwrap();
        fs::write(home.join(".ssh/config"), "Host managed\n").unwrap();
        write_private(
            &foreign.join("config.toml"),
            "ssh_hosts = [\"must-not-read\"]\n",
        );
        fs::set_permissions(
            foreign.join("config.toml"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        std::os::unix::fs::symlink(&foreign, sandbox.join(".claude-science")).unwrap();

        assert!(prepare_science_ssh_bridge_for(&sandbox, &home)
            .unwrap_err()
            .contains("包含符号链接"));
        assert!(revoke_science_ssh_bridge(&sandbox)
            .unwrap_err()
            .contains("包含符号链接"));
        fs::set_permissions(
            foreign.join("config.toml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(foreign.join("config.toml")).unwrap(),
            "ssh_hosts = [\"must-not-read\"]\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn running_validation_requires_owned_current_state() {
        let root = tmpdir("validate");
        let home = root.join("outer");
        let sandbox = root.join("sandbox");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(sandbox.join(".claude-science")).unwrap();
        fs::write(home.join(".ssh/config"), "Host managed\n").unwrap();
        write_private(
            &sandbox.join(".claude-science/config.toml"),
            "quiet_logs = true\n",
        );
        prepare_science_ssh_bridge_for(&sandbox, &home).unwrap();
        assert_eq!(
            validate_science_ssh_bridge_for(&sandbox, &home).unwrap(),
            ["managed"]
        );
        write_private(
            &sandbox.join(".claude-science/config.toml"),
            "ssh_hosts = [\"foreign\"]\n",
        );
        assert!(validate_science_ssh_bridge_for(&sandbox, &home).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
