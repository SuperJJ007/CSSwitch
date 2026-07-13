use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::{json, Value};
use sha2::Sha256;

const INSTALL_TOOL_NAME: &str = "install_external_skill";
const UNINSTALL_TOOL_NAME: &str = "uninstall_external_skill";
const IMPORT_ORIGIN_FILE: &str = ".import-origin";
const CSSWITCH_MARKETPLACE: &str = "csswitch-local-bridge";
const MAX_IMPORT_ORIGIN_BYTES: usize = 16 * 1024;
const MAX_FILES: usize = 512;
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const BRIDGE_KEY_FILE_ENV: &str = "CSSWITCH_SKILL_BRIDGE_KEY_FILE";
const BRIDGE_REQUEST_VERSION: u64 = 1;
const BRIDGE_REQUEST_TTL_SECONDS: u64 = 180;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubSource {
    owner: String,
    repo: String,
    tail: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedSource {
    owner: String,
    repo: String,
    commit: String,
    path: String,
    files: Vec<TreeFile>,
}

#[derive(Debug, Clone)]
struct TreeFile {
    relative_path: PathBuf,
    blob_sha: String,
    executable: bool,
    size: usize,
}

#[derive(Debug)]
struct InstallLock {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolMode {
    Install,
    Uninstall,
    All,
}

impl ToolMode {
    fn server_name(self) -> &'static str {
        match self {
            Self::Install => "csswitch-skill-installer",
            Self::Uninstall => "csswitch-skill-uninstaller",
            Self::All => "csswitch-external-skill-bridge",
        }
    }

    fn allows(self, tool_name: &str) -> bool {
        match self {
            Self::Install => tool_name == INSTALL_TOOL_NAME,
            Self::Uninstall => tool_name == UNINSTALL_TOOL_NAME,
            Self::All => matches!(tool_name, INSTALL_TOOL_NAME | UNINSTALL_TOOL_NAME),
        }
    }

    fn definitions(self) -> Vec<Value> {
        match self {
            Self::Install => vec![install_tool_definition()],
            Self::Uninstall => vec![uninstall_tool_definition()],
            Self::All => vec![install_tool_definition(), uninstall_tool_definition()],
        }
    }
}

pub fn run_mcp(args: &[String]) -> Result<(), String> {
    let (bridge_dir, tool_mode) = parse_mcp_args(args)?;
    let bridge_token = read_bridge_token_file()?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("读取 MCP 请求失败：{e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(response) = handle_mcp_request(&bridge_dir, &bridge_token, tool_mode, &request)
        {
            serde_json::to_writer(&mut stdout, &response)
                .map_err(|e| format!("编码 MCP 响应失败：{e}"))?;
            stdout
                .write_all(b"\n")
                .map_err(|e| format!("写 MCP 响应失败：{e}"))?;
            stdout
                .flush()
                .map_err(|e| format!("刷新 MCP 响应失败：{e}"))?;
        }
    }
    Ok(())
}

fn parse_mcp_args(args: &[String]) -> Result<(PathBuf, ToolMode), String> {
    if !matches!(args.len(), 2 | 4) || args[0] != "--bridge-dir" {
        return Err("用法：skill-install-mcp --bridge-dir <CSSwitch private bridge dir> [--tool-mode install|uninstall]".into());
    }
    let path = PathBuf::from(args[1].trim());
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !path.is_absolute() || !name.starts_with("CSSwitch-Skill-Bridge-") {
        return Err("安装宿主必须是 CSSwitch 生成的隔离 HOME bridge directory".into());
    }
    let tool_mode = if args.len() == 2 {
        ToolMode::All
    } else {
        if args[2] != "--tool-mode" {
            return Err("MCP tool mode 参数非法".into());
        }
        match args[3].as_str() {
            "install" => ToolMode::Install,
            "uninstall" => ToolMode::Uninstall,
            _ => return Err("MCP tool mode 只支持 install 或 uninstall".into()),
        }
    };
    Ok((path, tool_mode))
}

fn handle_mcp_request(
    bridge_dir: &Path,
    bridge_token: &str,
    tool_mode: ToolMode,
    request: &Value,
) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": tool_mode.server_name(), "version": "0.1.0"}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_mode.definitions()}),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if !tool_mode.allows(tool_name) {
                return Some(rpc_error(id, -32602, "该 connector 不提供此工具"));
            }
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let payload = match tool_name {
                INSTALL_TOOL_NAME => {
                    if arguments
                        .get("source_url")
                        .and_then(Value::as_str)
                        .is_none_or(|value| value.trim().is_empty())
                    {
                        install_from_arguments(Path::new("/"), &arguments)
                    } else {
                        host_access_request(bridge_dir, bridge_token, "install", &arguments)
                    }
                }
                UNINSTALL_TOOL_NAME => match requested_skill_name(&arguments) {
                    Ok(_) => host_access_request(bridge_dir, bridge_token, "uninstall", &arguments),
                    Err(message) => uninstall_failure(message),
                },
                _ => return Some(rpc_error(id, -32602, "未知工具")),
            };
            tool_result(payload)
        }
        _ => return Some(rpc_error(id, -32601, "未知 MCP 方法")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn host_access_request(
    bridge_dir: &Path,
    bridge_token: &str,
    operation: &str,
    arguments: &Value,
) -> Value {
    let id = random_request_id().unwrap_or_else(|_| format!("{:032x}", unique_suffix()));
    let host_path = bridge_dir.to_string_lossy().into_owned();
    let mut request = json!({
        "version": BRIDGE_REQUEST_VERSION,
        "id": id,
        "issued_at": unix_seconds(),
        "operation": operation,
        "arguments": arguments
    });
    let signature = sign_bridge_request(bridge_token, &request).unwrap_or_else(|_| "0".repeat(64));
    request["signature"] = Value::String(signature);
    json!({
        "status": "HOST_ACCESS_REQUIRED",
        "message": "调用 request_host_access，为 bridge_dir 请求 rw 权限；授权后调用 edit_file（old_string 为空）把 request.payload 写入 request.filename，再用 read_file 读取 response_filename；未生成时短暂等待后重试。把响应原样告知用户。安装不要改用 host.skills.edit/publish；卸载不要改用 host.skills.delete 或 skills.deleteDraft。",
        "bridge_dir": bridge_dir,
        "host_access": {"host_path": host_path, "mode": "rw"},
        "request": {
            "filename": format!("{id}.request.json"),
            "response_filename": format!("{id}.response.json"),
            "payload": request
        },
        "directory_commit": false,
        "restart_required": false
    })
}

fn read_bridge_token_file() -> Result<String, String> {
    let path = std::env::var_os(BRIDGE_KEY_FILE_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("缺少 CSSwitch 私有 Skill bridge key file")?;
    reject_symlink_path(&path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options
        .open(&path)
        .map_err(|_| "无法读取 CSSwitch 私有 Skill bridge key file")?;
    let metadata = file
        .metadata()
        .map_err(|_| "无法检查 CSSwitch 私有 Skill bridge key file")?;
    if !metadata.is_file() || metadata.len() > 128 {
        return Err("CSSwitch 私有 Skill bridge key file 类型非法".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("CSSwitch 私有 Skill bridge key file 权限非法".into());
        }
    }
    let mut token = String::new();
    file.take(129)
        .read_to_string(&mut token)
        .map_err(|_| "无法读取 CSSwitch 私有 Skill bridge key file")?;
    let token = token.trim().to_ascii_lowercase();
    validate_bridge_token(&token)?;
    Ok(token)
}

fn validate_bridge_token(token: &str) -> Result<(), String> {
    if token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("CSSwitch Skill bridge token 格式非法".into())
    }
}

fn random_request_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| "无法生成本地 Skill request id")?;
    Ok(hex_encode(&bytes))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut sorted = BTreeMap::new();
            for (key, value) in object {
                sorted.insert(key.clone(), canonical_json(value));
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn sign_bridge_request(token: &str, unsigned_request: &Value) -> Result<String, String> {
    validate_bridge_token(token)?;
    let canonical = canonical_json(unsigned_request);
    let body = serde_json::to_vec(&canonical).map_err(|_| "无法编码本地 Skill 请求")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes())
        .map_err(|_| "无法初始化本地 Skill 请求签名")?;
    mac.update(&body);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode_32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("本地 Skill 请求签名非法".into());
    }
    let mut bytes = [0_u8; 32];
    for (index, output) in bytes.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "本地 Skill 请求签名非法")?;
    }
    Ok(bytes)
}

pub(crate) fn validate_bridge_request(
    bridge_token: &str,
    filename_id: &str,
    request: &Value,
) -> Result<(), String> {
    validate_bridge_token(bridge_token)?;
    if filename_id.len() != 32
        || !filename_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("本地 Skill request id 非法".into());
    }
    let object = request.as_object().ok_or("本地 Skill 请求不是对象")?;
    let allowed = [
        "version",
        "id",
        "issued_at",
        "operation",
        "arguments",
        "signature",
    ];
    if object.len() != allowed.len() || object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("本地 Skill 请求字段非法".into());
    }
    if request.get("version").and_then(Value::as_u64) != Some(BRIDGE_REQUEST_VERSION)
        || request.get("id").and_then(Value::as_str) != Some(filename_id)
    {
        return Err("本地 Skill 请求身份非法".into());
    }
    let issued_at = request
        .get("issued_at")
        .and_then(Value::as_u64)
        .ok_or("本地 Skill 请求时间非法")?;
    let now = unix_seconds();
    if issued_at > now.saturating_add(5)
        || now.saturating_sub(issued_at) > BRIDGE_REQUEST_TTL_SECONDS
    {
        return Err("本地 Skill 请求已过期".into());
    }
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .ok_or("本地 Skill 操作非法")?;
    let arguments = request
        .get("arguments")
        .and_then(Value::as_object)
        .ok_or("本地 Skill 请求参数非法")?;
    match operation {
        "install" => {
            if arguments
                .keys()
                .any(|key| !matches!(key.as_str(), "source_url" | "skill_name"))
                || arguments
                    .get("source_url")
                    .and_then(Value::as_str)
                    .is_none_or(|value| value.trim().is_empty())
            {
                return Err("本地 Skill 安装参数非法".into());
            }
        }
        "uninstall" => {
            if arguments.len() != 1 {
                return Err("本地 Skill 卸载参数非法".into());
            }
            requested_skill_name(request.get("arguments").unwrap())?;
        }
        _ => return Err("未知的本地 Skill 操作".into()),
    }
    let signature = request
        .get("signature")
        .and_then(Value::as_str)
        .ok_or("本地 Skill 请求缺少签名")?;
    let signature = hex_decode_32(signature)?;
    let mut unsigned = request.clone();
    unsigned
        .as_object_mut()
        .expect("validated bridge request object")
        .remove("signature");
    let canonical = canonical_json(&unsigned);
    let body = serde_json::to_vec(&canonical).map_err(|_| "无法编码本地 Skill 请求")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(bridge_token.as_bytes())
        .map_err(|_| "无法初始化本地 Skill 请求签名")?;
    mac.update(&body);
    mac.verify_slice(&signature)
        .map_err(|_| "本地 Skill 请求签名不匹配".into())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn install_tool_definition() -> Value {
    json!({
        "name": INSTALL_TOOL_NAME,
        "description": "安装、导入或添加外部 Skill；install, import, or add an existing external public GitHub Skill. Use this instead of host.skills.edit, host.skills.publish, Add Skill ZIP, or marketplace.importSkills. With only a Skill name, search for candidates but never install an ambiguous guessed repository; confirm the exact URL when needed. After FILES_COMMITTED_ATTACH_REQUIRED, call host.agents.attach_skill('OPERON', skill_name), then skill(skill_name) before reporting it usable.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "source_url": {"type": "string", "description": "Public GitHub Skill directory URL in https://github.com/owner/repo/tree/ref/path form."},
                "skill_name": {"type": "string", "description": "The name supplied by the user when no source URL is available."}
            },
            "additionalProperties": false
        }
    })
}

fn uninstall_tool_definition() -> Value {
    json!({
        "name": UNINSTALL_TOOL_NAME,
        "description": "卸载、删除或移除 CSSwitch 导入的外部 Skill；uninstall, delete, or remove a CSSwitch-imported external Skill. Use this instead of host.skills.delete or skills.deleteDraft. It validates the CSSwitch import-origin marker and moves the directory to quarantine. After QUARANTINED_DETACH_REQUIRED, call host.agents.detach_skill('OPERON', skill_name), then verify skill(skill_name) no longer loads before reporting completion.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "skill_name": {"type": "string", "description": "Exact installed Skill directory name to uninstall."}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }
    })
}

fn tool_result(payload: Value) -> Value {
    let is_error = matches!(
        payload.get("status").and_then(Value::as_str),
        Some("INSTALL_FAILED" | "UNINSTALL_FAILED")
    );
    let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": is_error
    })
}

pub(crate) fn handle_bridge_request(data_dir: &Path, request: &Value) -> Value {
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("");
    let arguments = request.get("arguments").unwrap_or(&Value::Null);
    match operation {
        "install" => install_from_arguments(data_dir, arguments),
        "uninstall" => uninstall_from_arguments(data_dir, arguments),
        _ => json!({
            "status": "REQUEST_FAILED",
            "message": "未知的本地 Skill 操作",
            "directory_commit": false,
            "restart_required": false
        }),
    }
}

pub(crate) fn install_from_arguments(data_dir: &Path, arguments: &Value) -> Value {
    let source_url = arguments
        .get("source_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let skill_name = arguments
        .get("skill_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(source_url) = source_url else {
        return json!({
            "status": "NEED_SOURCE_URL",
            "skill_name": skill_name,
            "message": "请提供该 Skill 的公开 GitHub 目录链接（https://github.com/owner/repo/tree/ref/path）。CSSwitch 不会根据名称猜测来源。",
            "directory_commit": false,
            "restart_required": false
        });
    };
    match install_external_skill(data_dir, source_url) {
        Ok(value) => value,
        Err(message) => json!({
            "status": "INSTALL_FAILED",
            "message": message,
            "directory_commit": false,
            "restart_required": false
        }),
    }
}

fn install_external_skill(data_dir: &Path, source_url: &str) -> Result<Value, String> {
    let parsed = parse_github_source(source_url)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("初始化 GitHub 客户端失败：{e}"))?;
    let resolved = resolve_source(&client, parsed)?;
    let skill_name = skill_name_from_source_path(&resolved.path)?;
    let active_org = read_active_org(data_dir)?;
    let skills_root = data_dir.join("orgs").join(&active_org).join("skills");
    ensure_safe_root(data_dir, &skills_root)?;
    fs::create_dir_all(&skills_root).map_err(|e| format!("创建 Skills 目录失败：{e}"))?;
    reject_symlink_path(&skills_root)?;

    let target = skills_root.join(&skill_name);
    let lock_path = skills_root.join(format!(".csswitch-install-{skill_name}.lock"));
    let lock = acquire_lock(&lock_path)?;
    if target.exists() || fs::symlink_metadata(&target).is_ok() {
        return Err(format!("Skill '{skill_name}' 已存在；CSSwitch 拒绝覆盖"));
    }
    let temp = skills_root.join(format!(
        ".csswitch-install-{skill_name}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir(&temp).map_err(|e| format!("创建安装临时目录失败：{e}"))?;
    let install_result: Result<(), String> = (|| {
        download_tree(&client, &resolved, &temp)?;
        if !temp.join("SKILL.md").is_file() {
            return Err("来源目录顶层缺少 SKILL.md".into());
        }
        write_import_origin(&temp, &resolved, &skill_name)?;
        sync_tree(&temp)?;
        rename_no_replace(&temp, &target)?;
        sync_directory(&skills_root)?;
        Ok(())
    })();
    if install_result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    drop(lock);
    install_result?;
    Ok(json!({
        "status": "FILES_COMMITTED_ATTACH_REQUIRED",
        "skill_name": skill_name,
        "agent_name": "OPERON",
        "attach_required": true,
        "attach_method": "host.agents.attach_skill",
        "resolved_commit_sha": resolved.commit,
        "source_resolution": true,
        "content_fetch": true,
        "directory_commit": true,
        "science_discovery": "FILES_VISIBLE_NOT_ATTACHED",
        "skill_trigger": "NOT_VERIFIED",
        "function_run": "NOT_VERIFIED",
        "restart_required": false,
        "new_conversation_required": false,
        "import_origin_written": true,
        "message": "Skill 目录和本地导入来源标记已完整写入，但尚未成为可用 Skill。现在必须调用 host.agents.attach_skill('OPERON', skill_name)，随后调用 skill(skill_name) 验证加载；验证成功前不要向用户报告安装完成。"
    }))
}

fn write_import_origin(
    skill_dir: &Path,
    source: &ResolvedSource,
    skill_name: &str,
) -> Result<(), String> {
    let marker = json!({
        "version": 1,
        "repo": format!("{}/{}", source.owner, source.repo),
        "sha": source.commit,
        "plugin": skill_name,
        "marketplace": CSSWITCH_MARKETPLACE,
        "path": source.path,
        "importedAt": rfc3339_now(),
        "license": "NOASSERTION"
    });
    let mut body =
        serde_json::to_vec(&marker).map_err(|e| format!("编码 Skill 导入来源失败：{e}"))?;
    body.push(b'\n');
    if body.len() > MAX_IMPORT_ORIGIN_BYTES {
        return Err("Skill 导入来源标记过大".into());
    }
    write_new_file(&skill_dir.join(IMPORT_ORIGIN_FILE), &body, false)
}

fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    rfc3339_from_unix(seconds)
}

fn rfc3339_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let second_of_day = seconds % 86_400;
    // Howard Hinnant's civil-from-days conversion, with day zero at 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn requested_skill_name(arguments: &Value) -> Result<String, String> {
    let name = arguments
        .get("skill_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("请提供要卸载的准确 Skill 名称")?;
    validate_skill_name(name)?;
    Ok(name.to_string())
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    let valid = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$").expect("static regex");
    if !valid.is_match(name) || matches!(name, "." | "..") {
        return Err("Skill 名称非法".into());
    }
    Ok(())
}

fn uninstall_failure(message: String) -> Value {
    json!({
        "status": "UNINSTALL_FAILED",
        "message": message,
        "directory_removed": false,
        "quarantine_commit": false,
        "restart_required": false
    })
}

pub(crate) fn uninstall_from_arguments(data_dir: &Path, arguments: &Value) -> Value {
    let skill_name = match requested_skill_name(arguments) {
        Ok(name) => name,
        Err(message) => return uninstall_failure(message),
    };
    match uninstall_external_skill(data_dir, &skill_name) {
        Ok(value) => value,
        Err(message) => uninstall_failure(message),
    }
}

fn uninstall_external_skill(data_dir: &Path, skill_name: &str) -> Result<Value, String> {
    validate_skill_name(skill_name)?;
    let active_org = read_active_org(data_dir)?;
    let skills_root = data_dir.join("orgs").join(&active_org).join("skills");
    ensure_safe_root(data_dir, &skills_root)?;
    let target = skills_root.join(skill_name);
    reject_symlink_path(&target)?;
    let metadata = fs::metadata(&target).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => format!("Skill '{skill_name}' 不存在"),
        _ => format!("读取 Skill '{skill_name}' 失败：{error}"),
    })?;
    if !metadata.is_dir() {
        return Err(format!("Skill '{skill_name}' 不是目录，拒绝操作"));
    }
    verify_csswitch_import_origin(&target, skill_name)?;

    let lock_path = skills_root.join(format!(".csswitch-install-{skill_name}.lock"));
    let lock = acquire_lock(&lock_path)?;
    // Recheck the target and its marker while holding the same per-name lock used
    // by installation, so an install/uninstall pair cannot cross in flight.
    reject_symlink_path(&target)?;
    verify_csswitch_import_origin(&target, skill_name)?;

    let trash_root = skill_trash_root(data_dir)?;
    prepare_trash_root(data_dir, &trash_root)?;
    let quarantine_name = format!(
        "{skill_name}-{}-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        std::process::id(),
        unique_suffix()
    );
    let quarantine = trash_root.join(&quarantine_name);
    rename_no_replace(&target, &quarantine)?;
    drop(lock);
    let sync_warning = sync_directory(&skills_root)
        .and_then(|_| sync_directory(&trash_root))
        .err();
    Ok(json!({
        "status": "QUARANTINED_DETACH_REQUIRED",
        "skill_name": skill_name,
        "agent_name": "OPERON",
        "detach_required": true,
        "detach_method": "host.agents.detach_skill",
        "directory_removed": true,
        "quarantine_commit": true,
        "quarantine_name": quarantine_name,
        "durability_sync": sync_warning.is_none(),
        "warning": sync_warning,
        "restart_required": false,
        "new_conversation_recommended": false,
        "message": "Skill 目录已从当前组织移入 CSSwitch 本地隔离回收区，但 Agent 绑定尚未解除。现在必须调用 host.agents.detach_skill('OPERON', skill_name)，随后验证 skill(skill_name) 不再可加载；完成前不要向用户报告卸载成功。"
    }))
}

fn verify_csswitch_import_origin(skill_dir: &Path, skill_name: &str) -> Result<Value, String> {
    let marker_path = skill_dir.join(IMPORT_ORIGIN_FILE);
    reject_symlink_path(&marker_path)?;
    let metadata = fs::metadata(&marker_path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => format!(
            "Skill '{skill_name}' 没有 CSSwitch 导入来源标记；拒绝删除手工、内置或其他来源 Skill"
        ),
        _ => format!("读取 Skill 导入来源失败：{error}"),
    })?;
    if !metadata.is_file() || metadata.len() as usize > MAX_IMPORT_ORIGIN_BYTES {
        return Err("Skill 导入来源标记不是受支持的小型普通文件".into());
    }
    let body = fs::read(&marker_path).map_err(|e| format!("读取 Skill 导入来源失败：{e}"))?;
    let marker: Value = serde_json::from_slice(&body)
        .map_err(|_| "Skill 导入来源标记非法；拒绝删除".to_string())?;
    let repo = marker.get("repo").and_then(Value::as_str).unwrap_or("");
    let sha = marker.get("sha").and_then(Value::as_str).unwrap_or("");
    let plugin = marker.get("plugin").and_then(Value::as_str).unwrap_or("");
    let marketplace = marker
        .get("marketplace")
        .and_then(Value::as_str)
        .unwrap_or("");
    let path = marker.get("path").and_then(Value::as_str).unwrap_or("");
    let imported_at = marker
        .get("importedAt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let license = marker.get("license").and_then(Value::as_str).unwrap_or("");
    let repo_valid = repo.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && owner.len() <= 100
            && !matches!(owner, "." | "..")
            && !name.is_empty()
            && name.len() <= 100
            && !matches!(name, "." | "..")
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            && !name.contains('/')
    });
    let valid = marker.get("version").and_then(Value::as_u64) == Some(1)
        && repo_valid
        && sha.len() == 40
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && plugin == skill_name
        && marketplace == CSSWITCH_MARKETPLACE
        && !path.is_empty()
        && path.len() <= 500
        && path.split('/').all(safe_component)
        && !imported_at.is_empty()
        && imported_at.len() <= 100
        && !license.is_empty()
        && license.len() <= 100;
    if !valid {
        return Err(format!(
            "Skill '{skill_name}' 不是可验证的 CSSwitch 本地导入；拒绝删除"
        ));
    }
    Ok(marker)
}

fn skill_trash_root(data_dir: &Path) -> Result<PathBuf, String> {
    if data_dir.file_name().and_then(|part| part.to_str()) != Some(".claude-science") {
        return Err("Science data-dir 不是 CSSwitch 管理的标准路径；拒绝卸载".into());
    }
    let home = data_dir
        .parent()
        .ok_or("Science data-dir 缺少 HOME 父目录")?;
    if home.file_name().and_then(|part| part.to_str()) != Some("home") {
        return Err("Science data-dir 不在 CSSwitch sandbox/home 下；拒绝卸载".into());
    }
    let sandbox = home
        .parent()
        .ok_or("Science data-dir 缺少 sandbox 父目录")?;
    Ok(sandbox.join("skill-trash"))
}

fn prepare_trash_root(data_dir: &Path, trash_root: &Path) -> Result<(), String> {
    let sandbox = data_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("Science data-dir 缺少 sandbox 父目录")?;
    if trash_root.parent() != Some(sandbox) {
        return Err("Skill 隔离回收目录越界".into());
    }
    reject_symlink_path(sandbox)?;
    reject_symlink_path(trash_root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !trash_root.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(trash_root)
                .map_err(|e| format!("创建 Skill 隔离回收目录失败：{e}"))?;
        }
        fs::set_permissions(trash_root, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置 Skill 隔离回收目录权限失败：{e}"))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(trash_root).map_err(|e| format!("创建 Skill 隔离回收目录失败：{e}"))?;
    reject_symlink_path(trash_root)?;
    Ok(())
}

fn parse_github_source(raw: &str) -> Result<GithubSource, String> {
    if raw.contains('?') || raw.contains('#') {
        return Err("GitHub URL 不得包含查询参数或片段".into());
    }
    let prefix = "https://github.com/";
    let rest = raw
        .strip_prefix(prefix)
        .ok_or("只支持 https://github.com 的公开目录 URL")?;
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    if parts.len() < 5 || parts[2] != "tree" {
        return Err("URL 必须是 https://github.com/owner/repo/tree/ref/path".into());
    }
    let token = Regex::new(r"^[A-Za-z0-9_.-]+$").expect("static regex");
    if !token.is_match(parts[0]) || !token.is_match(parts[1]) {
        return Err("GitHub owner 或 repo 非法".into());
    }
    let tail = parts[3..]
        .iter()
        .map(|part| percent_decode_segment(part))
        .collect::<Result<Vec<_>, _>>()?;
    if tail.len() < 2 || tail.len() > 32 || tail.iter().any(|part| !safe_component(part)) {
        return Err("GitHub ref/path 非法或过长".into());
    }
    Ok(GithubSource {
        owner: parts[0].into(),
        repo: parts[1].trim_end_matches(".git").into(),
        tail,
    })
}

fn percent_decode_segment(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("URL 百分号编码非法".into());
            }
            let hex =
                std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| "URL 编码非法")?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| "URL 编码非法")?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "URL 必须是 UTF-8".into())
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

fn encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn github_json(client: &Client, url: &str) -> Result<Option<Value>, String> {
    let response = client
        .get(url)
        .header(USER_AGENT, "CSSwitch-Skill-Installer/0.1")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|e| format!("GitHub 请求失败：{e}"))?;
    if response.status().as_u16() == 404 || response.status().as_u16() == 422 {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("GitHub 返回 HTTP {}", response.status().as_u16()));
    }
    let mut body = Vec::new();
    response
        .take((MAX_TOTAL_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取 GitHub 响应失败：{e}"))?;
    if body.len() > MAX_TOTAL_BYTES {
        return Err("GitHub 元数据响应过大".into());
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("GitHub JSON 非法：{e}"))
}

fn resolve_source(client: &Client, source: GithubSource) -> Result<ResolvedSource, String> {
    let mut matches = Vec::new();
    for split in 1..source.tail.len() {
        let reference = source.tail[..split].join("/");
        let path = source.tail[split..].join("/");
        let commit_url = format!(
            "https://api.github.com/repos/{}/{}/commits/{}",
            source.owner,
            source.repo,
            encode_path(&reference)
        );
        let Some(commit_json) = github_json(client, &commit_url)? else {
            continue;
        };
        let Some(commit) = commit_json.get("sha").and_then(Value::as_str) else {
            continue;
        };
        let tree_url = format!(
            "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
            source.owner, source.repo, commit
        );
        let Some(tree_json) = github_json(client, &tree_url)? else {
            continue;
        };
        if tree_json.get("truncated").and_then(Value::as_bool) == Some(true) {
            return Err("GitHub 仓库树过大且响应被截断，拒绝安装".into());
        }
        let files = collect_tree_files(&tree_json, &path)?;
        if files
            .iter()
            .any(|file| file.relative_path == Path::new("SKILL.md"))
        {
            matches.push(ResolvedSource {
                owner: source.owner.clone(),
                repo: source.repo.clone(),
                commit: commit.to_string(),
                path,
                files,
            });
        }
    }
    match matches.len() {
        0 => Err("无法解析 GitHub ref/path，或目录顶层没有 SKILL.md".into()),
        1 => Ok(matches.remove(0)),
        _ => Err("GitHub URL 的 ref/path 存在歧义；请改用 commit SHA 形式的 URL".into()),
    }
}

fn collect_tree_files(tree_json: &Value, source_path: &str) -> Result<Vec<TreeFile>, String> {
    let prefix = format!("{}/", source_path.trim_end_matches('/'));
    let entries = tree_json
        .get("tree")
        .and_then(Value::as_array)
        .ok_or("GitHub tree 响应缺少 tree")?;
    let mut files = Vec::new();
    let mut total = 0usize;
    for entry in entries {
        let Some(repo_path) = entry.get("path").and_then(Value::as_str) else {
            continue;
        };
        if !repo_path.starts_with(&prefix) {
            continue;
        }
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "tree" {
            continue;
        }
        let mode = entry.get("mode").and_then(Value::as_str).unwrap_or("");
        if kind != "blob" || !matches!(mode, "100644" | "100755") {
            return Err(format!(
                "Skill 包含不支持的链接、子模块或特殊文件：{repo_path}"
            ));
        }
        let relative = &repo_path[prefix.len()..];
        let relative_path = validate_relative_path(relative)?;
        let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "Skill 文件超过 {} MiB 限制",
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        total = total.checked_add(size).ok_or("Skill 总大小溢出")?;
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "Skill 总大小超过 {} MiB 限制",
                MAX_TOTAL_BYTES / 1024 / 1024
            ));
        }
        files.push(TreeFile {
            relative_path,
            blob_sha: entry
                .get("sha")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            executable: mode == "100755",
            size,
        });
        if files.len() > MAX_FILES {
            return Err(format!("Skill 文件数超过 {MAX_FILES} 限制"));
        }
    }
    Ok(files)
}

fn validate_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("Skill 包含不安全路径".into());
    }
    if value.as_bytes().contains(&0) || value.split('/').any(|part| part.is_empty()) {
        return Err("Skill 包含不安全路径".into());
    }
    Ok(path.to_path_buf())
}

fn skill_name_from_source_path(path: &str) -> Result<String, String> {
    let name = path.rsplit('/').next().unwrap_or("");
    let valid = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$").expect("static regex");
    if !valid.is_match(name) || matches!(name, "." | "..") {
        return Err("Skill 目录名非法".into());
    }
    Ok(name.to_string())
}

fn download_tree(client: &Client, source: &ResolvedSource, temp: &Path) -> Result<(), String> {
    let mut seen_casefold = BTreeMap::<String, PathBuf>::new();
    let mut actual_total = 0usize;
    for file in &source.files {
        let folded = file.relative_path.to_string_lossy().to_lowercase();
        if let Some(prior) = seen_casefold.insert(folded, file.relative_path.clone()) {
            return Err(format!(
                "Skill 包含大小写冲突路径：{} / {}",
                prior.display(),
                file.relative_path.display()
            ));
        }
        if file.blob_sha.len() != 40 || !file.blob_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("GitHub tree 包含非法 blob SHA".into());
        }
        let url = format!(
            "https://api.github.com/repos/{}/{}/git/blobs/{}",
            source.owner, source.repo, file.blob_sha
        );
        let Some(blob) = github_json(client, &url)? else {
            return Err("GitHub blob 不存在".into());
        };
        if blob.get("encoding").and_then(Value::as_str) != Some("base64") {
            return Err("GitHub blob 编码不受支持".into());
        }
        let encoded = blob
            .get("content")
            .and_then(Value::as_str)
            .ok_or("GitHub blob 缺少内容")?;
        let content = base64::engine::general_purpose::STANDARD
            .decode(
                encoded
                    .bytes()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| "GitHub blob base64 非法")?;
        if content.len() > MAX_FILE_BYTES || (file.size != 0 && content.len() != file.size) {
            return Err("GitHub blob 大小与 tree 元数据不一致或超限".into());
        }
        actual_total = actual_total
            .checked_add(content.len())
            .ok_or("Skill 总大小溢出")?;
        if actual_total > MAX_TOTAL_BYTES {
            return Err("Skill 下载总大小超限".into());
        }
        let destination = temp.join(&file.relative_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建 Skill 子目录失败：{e}"))?;
        }
        write_new_file(&destination, &content, file.executable)?;
    }
    Ok(())
}

fn write_new_file(path: &Path, content: &[u8], executable: bool) -> Result<(), String> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(if executable { 0o700 } else { 0o600 });
    let mut file = options
        .open(path)
        .map_err(|e| format!("创建 Skill 文件失败：{e}"))?;
    file.write_all(content)
        .map_err(|e| format!("写 Skill 文件失败：{e}"))?;
    file.sync_all()
        .map_err(|e| format!("同步 Skill 文件失败：{e}"))?;
    #[cfg(unix)]
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
    )
    .map_err(|e| format!("设置 Skill 文件权限失败：{e}"))?;
    Ok(())
}

fn read_active_org(data_dir: &Path) -> Result<String, String> {
    if !data_dir.is_absolute() {
        return Err("Science data-dir 必须是绝对路径".into());
    }
    reject_symlink_path(data_dir)?;
    let active = data_dir.join("active-org.json");
    reject_symlink_path(&active)?;
    let body = fs::read(&active).map_err(|_| "读取 Science active-org.json 失败")?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| "Science active-org.json 非法")?;
    let org = value
        .get("org_uuid")
        .and_then(Value::as_str)
        .ok_or("active-org.json 缺少 org_uuid")?;
    let valid = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("static regex");
    if !valid.is_match(org) {
        return Err("active org 标识非法".into());
    }
    Ok(org.to_string())
}

fn ensure_safe_root(data_dir: &Path, skills_root: &Path) -> Result<(), String> {
    let orgs = data_dir.join("orgs");
    if skills_root.strip_prefix(&orgs).is_err() {
        return Err("Skills 目标目录越界".into());
    }
    reject_symlink_path(data_dir)?;
    if orgs.exists() {
        reject_symlink_path(&orgs)?;
    }
    // Check the full intended path before create_dir_all so an existing org/skills
    // symlink cannot cause even a temporary write outside this Science data-dir.
    reject_symlink_path(skills_root)?;
    Ok(())
}

fn reject_symlink_path(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("路径包含符号链接，拒绝操作".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查路径失败：{error}")),
        }
    }
    Ok(())
}

fn acquire_lock(path: &Path) -> Result<InstallLock, String> {
    reject_symlink_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| "同名 Skill 正在安装，或存在残留安装锁")?;
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|_| "无法收紧 Skill 安装锁权限")?;
    file.try_lock().map_err(|_| "同名 Skill 正在安装")?;
    Ok(InstallLock { _file: file })
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn sync_tree(root: &Path) -> Result<(), String> {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("读取临时目录失败：{e}"))? {
            let path = entry
                .map_err(|e| format!("读取临时目录项失败：{e}"))?
                .path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        sync_directory(&dir)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("同步目录失败：{e}"))
}

#[cfg(target_os = "macos")]
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn renameatx_np(fromfd: i32, from: *const i8, tofd: i32, to: *const i8, flags: u32) -> i32;
    }
    const AT_FDCWD: i32 = -2;
    const RENAME_EXCL: u32 = 0x0000_0004;
    let from = CString::new(source.as_os_str().as_bytes()).map_err(|_| "临时路径非法")?;
    let to = CString::new(target.as_os_str().as_bytes()).map_err(|_| "目标路径非法")?;
    let result =
        unsafe { renameatx_np(AT_FDCWD, from.as_ptr(), AT_FDCWD, to.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "原子提交 Skill 失败：{}",
            io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const i8,
            newdirfd: i32,
            newpath: *const i8,
            flags: u32,
        ) -> i32;
    }
    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;
    let from = CString::new(source.as_os_str().as_bytes()).map_err(|_| "临时路径非法")?;
    let to = CString::new(target.as_os_str().as_bytes()).map_err(|_| "目标路径非法")?;
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            from.as_ptr(),
            AT_FDCWD,
            to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "原子提交 Skill 失败：{}",
            io::Error::last_os_error()
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        return Err("Skill 已存在；拒绝覆盖".into());
    }
    fs::rename(source, target).map_err(|e| format!("提交 Skill 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BRIDGE_TOKEN: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn mcp_request(bridge: &Path, tool_mode: ToolMode, request: &Value) -> Option<Value> {
        handle_mcp_request(bridge, TEST_BRIDGE_TOKEN, tool_mode, request)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let path = PathBuf::from("/private/tmp").join(format!(
            "csswitch-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn standard_data_dir(label: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(label);
        let data = root.join("sandbox/home/.claude-science");
        fs::create_dir_all(data.join("orgs/org-test/skills")).unwrap();
        fs::write(data.join("active-org.json"), br#"{"org_uuid":"org-test"}"#).unwrap();
        (root, data)
    }

    fn imported_skill(data: &Path, name: &str) -> PathBuf {
        let skill = data.join("orgs/org-test/skills").join(name);
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), b"---\nname: test\n---\n").unwrap();
        let source = ResolvedSource {
            owner: "owner".into(),
            repo: "repo".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            path: format!("skills/{name}"),
            files: vec![],
        };
        write_import_origin(&skill, &source, name).unwrap();
        skill
    }

    #[test]
    fn name_only_requests_source_without_writing() {
        let data = temp_dir("name-only");
        let result = install_from_arguments(&data, &json!({"skill_name": "pdf"}));
        assert_eq!(result["status"], "NEED_SOURCE_URL");
        assert_eq!(result["directory_commit"], false);
        assert!(!data.join("orgs").exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn tool_description_routes_external_install_away_from_authoring() {
        let tool = install_tool_definition();
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("host.skills.edit"));
        assert!(description.contains("host.skills.publish"));
        assert!(description.contains("ambiguous guessed repository"));
        assert!(description.contains("host.agents.attach_skill"));
    }

    #[test]
    fn import_origin_matches_science_sidecar_shape() {
        let root = temp_dir("origin");
        let source = ResolvedSource {
            owner: "anthropics".into(),
            repo: "skills".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            path: "skills/internal-comms".into(),
            files: vec![],
        };
        write_import_origin(&root, &source, "internal-comms").unwrap();
        let marker: Value =
            serde_json::from_slice(&fs::read(root.join(IMPORT_ORIGIN_FILE)).unwrap()).unwrap();
        assert_eq!(marker["version"], 1);
        assert_eq!(marker["repo"], "anthropics/skills");
        assert_eq!(marker["sha"], source.commit);
        assert_eq!(marker["plugin"], "internal-comms");
        assert_eq!(marker["marketplace"], CSSWITCH_MARKETPLACE);
        assert_eq!(marker["path"], "skills/internal-comms");
        assert_eq!(marker["license"], "NOASSERTION");
        assert!(marker["importedAt"].as_str().unwrap().ends_with('Z'));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rfc3339_formatter_handles_epoch_and_known_date() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_704_067_199), "2023-12-31T23:59:59Z");
    }

    #[test]
    fn uninstall_moves_only_csswitch_import_to_quarantine() {
        let (root, data) = standard_data_dir("uninstall");
        let runtime_sentinel = data.join("runtime/fake-version/skills/do-not-touch.txt");
        fs::create_dir_all(runtime_sentinel.parent().unwrap()).unwrap();
        fs::write(&runtime_sentinel, b"science-owned-runtime").unwrap();
        let skill = imported_skill(&data, "internal-comms");
        let result = uninstall_from_arguments(&data, &json!({"skill_name":"internal-comms"}));
        assert_eq!(result["status"], "QUARANTINED_DETACH_REQUIRED", "{result}");
        assert_eq!(result["detach_required"], true);
        assert_eq!(result["detach_method"], "host.agents.detach_skill");
        assert_eq!(result["directory_removed"], true);
        assert_eq!(result["quarantine_commit"], true);
        assert!(!skill.exists());
        let quarantine = data
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("skill-trash")
            .join(result["quarantine_name"].as_str().unwrap());
        assert!(quarantine.join("SKILL.md").is_file());
        assert!(quarantine.join(IMPORT_ORIGIN_FILE).is_file());
        assert_eq!(
            fs::read(&runtime_sentinel).unwrap(),
            b"science-owned-runtime",
            "uninstall must never mutate a version-runtime directory"
        );
        let repeated = uninstall_from_arguments(&data, &json!({"skill_name":"internal-comms"}));
        assert_eq!(repeated["status"], "UNINSTALL_FAILED");
        assert!(repeated["message"].as_str().unwrap().contains("不存在"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_refuses_unmarked_foreign_and_invalid_names() {
        let (root, data) = standard_data_dir("uninstall-refuse");
        let skills = data.join("orgs/org-test/skills");
        let manual = skills.join("manual-skill");
        fs::create_dir(&manual).unwrap();
        fs::write(manual.join("SKILL.md"), b"manual").unwrap();
        let unmarked = uninstall_from_arguments(&data, &json!({"skill_name":"manual-skill"}));
        assert_eq!(unmarked["status"], "UNINSTALL_FAILED");
        assert!(manual.exists());

        let foreign = imported_skill(&data, "foreign-skill");
        let mut marker: Value =
            serde_json::from_slice(&fs::read(foreign.join(IMPORT_ORIGIN_FILE)).unwrap()).unwrap();
        marker["marketplace"] = json!("another-importer");
        fs::write(
            foreign.join(IMPORT_ORIGIN_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let foreign_result =
            uninstall_from_arguments(&data, &json!({"skill_name":"foreign-skill"}));
        assert_eq!(foreign_result["status"], "UNINSTALL_FAILED");
        assert!(foreign.exists());

        let invalid = uninstall_from_arguments(&data, &json!({"skill_name":"../escape"}));
        assert_eq!(invalid["status"], "UNINSTALL_FAILED");
        assert!(invalid["message"].as_str().unwrap().contains("非法"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn github_url_parser_keeps_ambiguous_ref_tail_for_resolution() {
        let source =
            parse_github_source("https://github.com/owner/repo/tree/feature/slash/skills/pdf")
                .unwrap();
        assert_eq!(source.owner, "owner");
        assert_eq!(source.repo, "repo");
        assert_eq!(source.tail, ["feature", "slash", "skills", "pdf"]);
        assert!(parse_github_source("http://github.com/owner/repo/tree/main/pdf").is_err());
        assert!(parse_github_source("https://github.com/owner/repo/tree/main/../pdf").is_err());
        assert!(parse_github_source("https://github.com/owner/repo/tree/main/pdf?x=1").is_err());
    }

    #[test]
    fn tree_collection_rejects_symlinks_and_traversal() {
        let symlink = json!({"tree": [{"path":"skills/pdf/SKILL.md","type":"blob","mode":"120000","sha":"0123456789012345678901234567890123456789","size":4}]});
        assert!(collect_tree_files(&symlink, "skills/pdf").is_err());
        assert!(validate_relative_path("../escape").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn existing_org_symlink_is_rejected_before_directory_creation() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("org-symlink");
        let data = root.join("data");
        let outside = root.join("outside");
        fs::create_dir_all(data.join("orgs")).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, data.join("orgs/org-test")).unwrap();
        let skills = data.join("orgs/org-test/skills");
        assert!(ensure_safe_root(&data, &skills).is_err());
        assert!(!outside.join("skills").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_no_replace_never_overwrites_existing_target() {
        let root = temp_dir("rename");
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("new"), b"new").unwrap();
        fs::write(target.join("old"), b"old").unwrap();
        assert!(rename_no_replace(&source, &target).is_err());
        assert_eq!(fs::read(target.join("old")).unwrap(), b"old");
        assert!(source.join("new").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mcp_list_and_name_only_call_have_stable_shapes() {
        let bridge = Path::new("/tmp/CSSwitch-Skill-Bridge-test");
        let listed = mcp_request(
            bridge,
            ToolMode::All,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .unwrap();
        let names = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, [INSTALL_TOOL_NAME, UNINSTALL_TOOL_NAME]);
        let called = mcp_request(bridge, ToolMode::All, &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":INSTALL_TOOL_NAME,"arguments":{"skill_name":"pdf"}}})).unwrap();
        assert_eq!(
            called["result"]["structuredContent"]["status"],
            "NEED_SOURCE_URL"
        );
        let uninstall = mcp_request(bridge, ToolMode::All, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":UNINSTALL_TOOL_NAME,"arguments":{"skill_name":"pdf"}}})).unwrap();
        assert_eq!(
            uninstall["result"]["structuredContent"]["status"],
            "HOST_ACCESS_REQUIRED"
        );
        assert_eq!(
            uninstall["result"]["structuredContent"]["request"]["payload"]["operation"],
            "uninstall"
        );
    }

    #[test]
    fn scoped_connectors_expose_only_their_intended_tool() {
        let bridge = Path::new("/tmp/CSSwitch-Skill-Bridge-test");
        let initialized = mcp_request(
            bridge,
            ToolMode::Uninstall,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        )
        .unwrap();
        assert_eq!(
            initialized["result"]["serverInfo"]["name"],
            "csswitch-skill-uninstaller"
        );
        let listed = mcp_request(
            bridge,
            ToolMode::Uninstall,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(listed["result"]["tools"][0]["name"], UNINSTALL_TOOL_NAME);
        let rejected = mcp_request(
            bridge,
            ToolMode::Uninstall,
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":INSTALL_TOOL_NAME,"arguments":{}}}),
        )
        .unwrap();
        assert_eq!(rejected["error"]["code"], -32602);
    }

    #[test]
    fn bridge_request_signature_rejects_tampering_expiry_and_wrong_filename() {
        let bridge = Path::new("/tmp/CSSwitch-Skill-Bridge-test");
        let result = host_access_request(
            bridge,
            TEST_BRIDGE_TOKEN,
            "uninstall",
            &json!({"skill_name":"pdf"}),
        );
        let filename = result["request"]["filename"].as_str().unwrap();
        let id = filename.strip_suffix(".request.json").unwrap();
        let request = result["request"]["payload"].clone();
        validate_bridge_request(TEST_BRIDGE_TOKEN, id, &request).unwrap();

        let mut tampered = request.clone();
        tampered["arguments"]["skill_name"] = json!("other");
        assert!(validate_bridge_request(TEST_BRIDGE_TOKEN, id, &tampered).is_err());
        assert!(validate_bridge_request(TEST_BRIDGE_TOKEN, &"f".repeat(32), &request).is_err());

        let mut expired = request;
        expired["issued_at"] = json!(unix_seconds().saturating_sub(BRIDGE_REQUEST_TTL_SECONDS + 1));
        expired.as_object_mut().unwrap().remove("signature");
        let expired_signature = sign_bridge_request(TEST_BRIDGE_TOKEN, &expired).unwrap();
        expired["signature"] = json!(expired_signature);
        assert!(validate_bridge_request(TEST_BRIDGE_TOKEN, id, &expired).is_err());
    }

    #[test]
    fn persistent_advisory_lock_recovers_stale_file_and_serializes_callers() {
        let root = temp_dir("advisory-lock");
        let lock_path = root.join(".csswitch-install-pdf.lock");
        fs::write(&lock_path, b"stale").unwrap();
        let first = acquire_lock(&lock_path).unwrap();
        assert!(acquire_lock(&lock_path).is_err());
        drop(first);
        let second = acquire_lock(&lock_path).unwrap();
        drop(second);
        assert!(lock_path.is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
