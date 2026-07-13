use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tauri::Runtime;

use crate::runtime::external_skill_route::{ensure_route_skill, inspect_route_skill, SKILL_NAME};
use crate::runtime::proxy_lifecycle::gateway_bin_path;

const INSTALL_SERVER_NAME: &str = "csswitch-skill-installer";
const UNINSTALL_SERVER_NAME: &str = "csswitch-skill-uninstaller";
const MANAGED_MARKER: &str = "[managed-by:csswitch]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RegistrationStatus {
    Registered,
    AlreadyRegistered,
    RestartRequired,
    Warning(String),
}

impl RegistrationStatus {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Registered => "REGISTERED",
            Self::AlreadyRegistered => "AVAILABLE",
            Self::RestartRequired => "RESTART_REQUIRED",
            Self::Warning(_) => "WARNING",
        }
    }

    pub(crate) fn user_note(&self) -> Option<String> {
        match self {
            Self::Registered => Some("外部 Skill 本地安装与卸载工具已注册。".into()),
            Self::AlreadyRegistered => None,
            Self::RestartRequired => {
                Some("外部 Skill 本地安装与卸载工具需要重启 Science 后加载。".into())
            }
            Self::Warning(message) => Some(format!(
                "外部 Skill 本地工具未就绪：{message}；Science 仍会正常启动。"
            )),
        }
    }
}

pub(crate) fn register_before_science_start<R: Runtime>(
    app: &tauri::AppHandle<R>,
    data_dir: &Path,
    bridge_dir: &Path,
    bridge_key_file: &Path,
) -> RegistrationStatus {
    let result = (|| -> Result<bool, String> {
        let (config, expected) = registration_inputs(app, data_dir, bridge_dir, bridge_key_file)?;
        let mcp_changed = merge_runtime_registration(&config, expected)?;
        let route_changed = ensure_route_skill(data_dir)?;
        Ok(mcp_changed || route_changed)
    })();
    match result {
        Ok(true) => RegistrationStatus::Registered,
        Ok(false) => RegistrationStatus::AlreadyRegistered,
        Err(error) => RegistrationStatus::Warning(error),
    }
}

pub(crate) fn inspect_while_science_running<R: Runtime>(
    app: &tauri::AppHandle<R>,
    data_dir: &Path,
    bridge_dir: &Path,
    bridge_key_file: &Path,
) -> RegistrationStatus {
    let result = (|| -> Result<bool, String> {
        let (config, expected) = registration_inputs(app, data_dir, bridge_dir, bridge_key_file)?;
        Ok(registration_matches(&config, &expected)? && inspect_route_skill(data_dir)?)
    })();
    match result {
        Ok(true) => RegistrationStatus::AlreadyRegistered,
        Ok(false) => RegistrationStatus::RestartRequired,
        Err(error) => RegistrationStatus::Warning(error),
    }
}

/// Attach the fixed routing Skill through Science's own local control plane.
///
/// The one-time URL is passed via the child environment (never argv), and the
/// gateway command accepts only the fixed route name and loopback origins.
pub(crate) fn attach_route_after_science_start<R: Runtime>(
    app: &tauri::AppHandle<R>,
    control_url: &str,
) -> Result<(), String> {
    let gateway = gateway_bin_path(app).ok_or("找不到 csswitch-gateway sidecar")?;
    let output = Command::new(gateway)
        .arg("science-control")
        .arg("attach-route")
        .env("CSSWITCH_SCIENCE_CONTROL_URL", control_url)
        .output()
        .map_err(|_| "启动本地 Science 路由绑定命令失败")?;
    if !output.status.success() {
        return Err("Science 未接受 CSSwitch 路由 Skill 绑定".into());
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "本地 Science 路由绑定响应非法")?;
    if value.get("status").and_then(Value::as_str) != Some("ATTACHED")
        || value.get("skill_name").and_then(Value::as_str) != Some(SKILL_NAME)
    {
        return Err("本地 Science 路由绑定结果不完整".into());
    }
    Ok(())
}

fn registration_inputs<R: Runtime>(
    app: &tauri::AppHandle<R>,
    data_dir: &Path,
    bridge_dir: &Path,
    bridge_key_file: &Path,
) -> Result<(PathBuf, Vec<Value>), String> {
    reject_symlink_path(data_dir)?;
    reject_symlink_path(bridge_key_file)?;
    if !bridge_key_file.is_absolute() || !bridge_key_file.is_file() {
        return Err("CSSwitch 私有 Skill bridge key file 不可用".into());
    }
    let gateway = gateway_bin_path(app).ok_or("找不到 csswitch-gateway sidecar")?;
    // Science's local_mcp_root is instance-scoped (<data-dir>/mcp), while the
    // tool resolves active-org.json again at call time before installing files.
    let config = data_dir.join("mcp").join("local-mcp.json");
    let command = gateway.to_string_lossy();
    let bridge = bridge_dir.to_string_lossy();
    let bridge_key_file = bridge_key_file.to_string_lossy();
    let expected = vec![
        json!({
            "name": INSTALL_SERVER_NAME,
            "command": command,
            "args": ["skill-install-mcp", "--bridge-dir", bridge, "--tool-mode", "install"],
            "env": {"CSSWITCH_SKILL_BRIDGE_KEY_FILE": bridge_key_file},
            "description": format!("安装、导入、添加外部 Skill；install/import/add an external public GitHub Skill. Use install_external_skill instead of host.skills.edit/publish. {MANAGED_MARKER}")
        }),
        json!({
            "name": UNINSTALL_SERVER_NAME,
            "command": command,
            "args": ["skill-install-mcp", "--bridge-dir", bridge, "--tool-mode", "uninstall"],
            "env": {"CSSWITCH_SKILL_BRIDGE_KEY_FILE": bridge_key_file},
            "description": format!("卸载、删除、移除 CSSwitch 导入的外部 Skill；uninstall/delete/remove a CSSwitch-imported external Skill. Use uninstall_external_skill instead of host.skills.delete or skills.deleteDraft. {MANAGED_MARKER}")
        }),
    ];
    Ok((config, expected))
}

fn registration_matches(config: &Path, expected: &[Value]) -> Result<bool, String> {
    if !config.exists() {
        return Ok(false);
    }
    reject_symlink_path(config)?;
    let root = read_config(config)?;
    let servers = root
        .get("servers")
        .and_then(Value::as_array)
        .ok_or("local-mcp.json 缺少 servers 数组")?;
    let expected_present = expected
        .iter()
        .all(|item| servers.iter().any(|server| server_matches(server, item)));
    Ok(expected_present)
}

fn server_matches(server: &Value, expected: &Value) -> bool {
    ["name", "command", "args", "env", "description"]
        .iter()
        .all(|key| server.get(*key) == expected.get(*key))
        && server
            .get("description")
            .and_then(Value::as_str)
            .map(|description| description.contains(MANAGED_MARKER))
            .unwrap_or(false)
}

#[cfg(test)]
fn merge_registration(config: &Path, expected: Value) -> Result<bool, String> {
    merge_registrations(config, vec![expected])
}

#[cfg(test)]
fn merge_registrations(config: &Path, expected: Vec<Value>) -> Result<bool, String> {
    merge_registrations_and_remove(config, expected, &[])
}

fn merge_runtime_registration(config: &Path, expected: Vec<Value>) -> Result<bool, String> {
    merge_registrations_and_remove(config, expected, &[])
}

fn merge_registrations_and_remove(
    config: &Path,
    expected: Vec<Value>,
    obsolete_managed_names: &[&str],
) -> Result<bool, String> {
    if let Some(parent) = config.parent() {
        reject_symlink_path(parent)?;
        fs::create_dir_all(parent).map_err(|e| format!("创建本地 MCP 目录失败：{e}"))?;
        reject_symlink_path(parent)?;
    }
    reject_symlink_path(config)?;
    let mut root = if config.exists() {
        read_config(config)?
    } else {
        json!({"servers": []})
    };
    let object = root
        .as_object_mut()
        .ok_or("local-mcp.json 顶层必须是对象")?;
    let servers = object.entry("servers").or_insert_with(|| json!([]));
    let servers = servers
        .as_array_mut()
        .ok_or("local-mcp.json 的 servers 必须是数组")?;
    let mut changed = false;
    for item in expected {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or("CSSwitch MCP 配置缺少名称")?;
        let existing = servers
            .iter()
            .position(|server| server.get("name").and_then(Value::as_str) == Some(name));
        if let Some(index) = existing {
            if server_matches(&servers[index], &item) {
                continue;
            }
            let managed = servers[index]
                .get("description")
                .and_then(Value::as_str)
                .map(|description| description.contains(MANAGED_MARKER))
                .unwrap_or(false);
            if !managed {
                return Err(format!(
                    "本地 MCP 已存在同名非 CSSwitch 配置 '{name}'，已拒绝覆盖"
                ));
            }
            servers[index] = item;
        } else {
            servers.push(item);
        }
        changed = true;
    }
    let original_len = servers.len();
    servers.retain(|server| {
        let obsolete = server
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| obsolete_managed_names.contains(&name));
        let managed = server
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|description| description.contains(MANAGED_MARKER));
        !(obsolete && managed)
    });
    changed |= servers.len() != original_len;
    if !changed {
        return Ok(false);
    }
    write_config_atomic(config, &root)?;
    Ok(true)
}

fn read_config(path: &Path) -> Result<Value, String> {
    let body = fs::read(path).map_err(|e| format!("读取 local-mcp.json 失败：{e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("local-mcp.json 非法：{e}"))
}

fn write_config_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("local-mcp.json 缺少父目录")?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".local-mcp.json.csswitch-{}-{suffix}",
        std::process::id()
    ));
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|e| format!("创建 MCP 临时配置失败：{e}"))?;
        serde_json::to_writer_pretty(&mut file, value)
            .map_err(|e| format!("编码 MCP 配置失败：{e}"))?;
        file.write_all(b"\n")
            .map_err(|e| format!("写 MCP 配置失败：{e}"))?;
        file.sync_all()
            .map_err(|e| format!("同步 MCP 配置失败：{e}"))?;
        fs::rename(&temp, path).map_err(|e| format!("提交 MCP 配置失败：{e}"))?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| format!("同步 MCP 目录失败：{e}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn reject_symlink_path(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("MCP 配置路径包含符号链接".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查 MCP 配置路径失败：{error}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from("/private/tmp").join(format!(
            "csswitch-mcp-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn expected(command: &str, data: &Path) -> Value {
        expected_named(INSTALL_SERVER_NAME, command, data)
    }

    fn expected_named(name: &str, command: &str, data: &Path) -> Value {
        let _ = data;
        let bridge = "/tmp/CSSwitch-Skill-Bridge-test";
        json!({
            "name": name,
            "command": command,
            "args": ["skill-install-mcp", "--bridge-dir", bridge],
            "env": {},
            "description": format!("installer {MANAGED_MARKER}")
        })
    }

    #[test]
    fn batch_registration_adds_distinct_scoped_connectors_atomically() {
        let root = temp_dir("two-connectors");
        let config = root.join("local-mcp.json");
        let installer = expected_named(INSTALL_SERVER_NAME, "/app/gateway", &root);
        let mut uninstaller = expected_named(UNINSTALL_SERVER_NAME, "/app/gateway", &root);
        uninstaller["args"] = json!([
            "skill-install-mcp",
            "--bridge-dir",
            "/tmp/CSSwitch-Skill-Bridge-test",
            "--tool-mode",
            "uninstall"
        ]);
        assert!(
            merge_registrations(&config, vec![installer.clone(), uninstaller.clone()]).unwrap()
        );
        assert!(
            !merge_registrations(&config, vec![installer.clone(), uninstaller.clone()]).unwrap()
        );
        let saved: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(saved["servers"].as_array().unwrap().len(), 2);
        assert_eq!(saved["servers"][0], installer);
        assert_eq!(saved["servers"][1], uninstaller);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_registration_migrates_combined_connector_to_scoped_pair() {
        let root = temp_dir("split-connectors");
        let config = root.join("local-mcp.json");
        let old_installer = expected_named(INSTALL_SERVER_NAME, "/old/gateway", &root);
        fs::write(
            &config,
            serde_json::to_vec(&json!({
                "servers": [old_installer, {"name":"other","command":"other"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut installer = expected_named(INSTALL_SERVER_NAME, "/new/gateway", &root);
        installer["args"] = json!([
            "skill-install-mcp",
            "--bridge-dir",
            "/tmp/CSSwitch-Skill-Bridge-test",
            "--tool-mode",
            "install"
        ]);
        let mut uninstaller = expected_named(UNINSTALL_SERVER_NAME, "/new/gateway", &root);
        uninstaller["args"] = json!([
            "skill-install-mcp",
            "--bridge-dir",
            "/tmp/CSSwitch-Skill-Bridge-test",
            "--tool-mode",
            "uninstall"
        ]);
        assert!(
            merge_runtime_registration(&config, vec![installer.clone(), uninstaller.clone()])
                .unwrap()
        );
        let saved: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        let servers = saved["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0], installer);
        assert_eq!(servers[1]["name"], "other");
        assert_eq!(servers[2], uninstaller);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_preserves_other_servers_and_unknown_top_level_fields() {
        let root = temp_dir("preserve");
        let config = root.join("local-mcp.json");
        fs::write(
            &config,
            br#"{"future":7,"servers":[{"name":"other","command":"other"}]}"#,
        )
        .unwrap();
        let item = expected("/app/csswitch-gateway", &root);
        assert!(merge_registration(&config, item.clone()).unwrap());
        let saved: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(saved["future"], 7);
        assert_eq!(saved["servers"][0]["name"], "other");
        assert_eq!(saved["servers"][1], item);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_is_idempotent_and_updates_only_managed_entry() {
        let root = temp_dir("update");
        let config = root.join("local-mcp.json");
        let old = expected("/old/csswitch-gateway", &root);
        merge_registration(&config, old).unwrap();
        let new = expected("/new/csswitch-gateway", &root);
        assert!(merge_registration(&config, new.clone()).unwrap());
        assert!(!merge_registration(&config, new.clone()).unwrap());
        let saved: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(saved["servers"].as_array().unwrap().len(), 1);
        assert_eq!(saved["servers"][0], new);
        let mut compatible = new.clone();
        compatible["futureScienceField"] = json!(true);
        assert!(server_matches(&compatible, &new));
        let mut updated_description = new.clone();
        updated_description["description"] =
            json!(format!("installer and uninstaller {MANAGED_MARKER}"));
        assert!(merge_registration(&config, updated_description.clone()).unwrap());
        let saved: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(saved["servers"][0], updated_description);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_refuses_same_name_unmanaged_entry_and_malformed_config() {
        let root = temp_dir("conflict");
        let config = root.join("local-mcp.json");
        fs::write(
            &config,
            format!(r#"{{"servers":[{{"name":"{INSTALL_SERVER_NAME}","command":"user-tool"}}]}}"#),
        )
        .unwrap();
        assert!(merge_registration(&config, expected("/app/gateway", &root)).is_err());
        assert!(fs::read_to_string(&config).unwrap().contains("user-tool"));
        fs::write(&config, b"{broken").unwrap();
        assert!(merge_registration(&config, expected("/app/gateway", &root)).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "skill_install_bridge_e2e.rs"]
mod real_science_e2e;
