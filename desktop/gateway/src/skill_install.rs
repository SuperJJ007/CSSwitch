use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use regex::Regex;
use serde_json::{json, Value};
use sha2::Sha256;

use csswitch_skill_install_core::{
    attach_skill, find_bundle_for_skill, install_github_package_with_progress, quarantine_bundle,
    update_agent_skills, verify_attach_control_ready, AttachResult, BundleCommit, InstallCommit,
    InstallError, InstalledPackage, ScienceHostContext, GITHUB_BUNDLE_OPERATION_TIMEOUT_SECONDS,
    SCHEMA_VERSION,
};

const INSTALL_TOOL_NAME: &str = "install_external_skill";
const UNINSTALL_TOOL_NAME: &str = "uninstall_external_skill";
const POLL_TOOL_NAME: &str = "poll_external_skill_request";
const IMPORT_ORIGIN_FILE: &str = ".import-origin";
const CSSWITCH_MARKETPLACE: &str = "csswitch-local-bridge";
const MAX_IMPORT_ORIGIN_BYTES: usize = 16 * 1024;
const BRIDGE_KEY_FILE_ENV: &str = "CSSWITCH_SKILL_BRIDGE_KEY_FILE";
const BRIDGE_REQUEST_VERSION: u64 = 1;
const BRIDGE_REQUEST_TTL_SECONDS: u64 = 180;
pub(crate) const BRIDGE_INSTALL_RESPONSE_TIMEOUT_SECONDS: u64 =
    GITHUB_BUNDLE_OPERATION_TIMEOUT_SECONDS + 60;

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
            Self::Install => matches!(tool_name, INSTALL_TOOL_NAME | POLL_TOOL_NAME),
            Self::Uninstall => matches!(tool_name, UNINSTALL_TOOL_NAME | POLL_TOOL_NAME),
            Self::All => matches!(
                tool_name,
                INSTALL_TOOL_NAME | UNINSTALL_TOOL_NAME | POLL_TOOL_NAME
            ),
        }
    }

    fn definitions(self) -> Vec<Value> {
        match self {
            Self::Install => vec![install_tool_definition(), poll_tool_definition()],
            Self::Uninstall => vec![uninstall_tool_definition(), poll_tool_definition()],
            Self::All => vec![
                install_tool_definition(),
                uninstall_tool_definition(),
                poll_tool_definition(),
            ],
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
                UNINSTALL_TOOL_NAME => match validate_uninstall_arguments(&arguments) {
                    Ok(_) => host_access_request(bridge_dir, bridge_token, "uninstall", &arguments),
                    Err(message) => uninstall_failure(message),
                },
                POLL_TOOL_NAME => poll_bridge_request(bridge_dir, &arguments),
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
        "request_id": id,
        "message": "调用 request_host_access，为 host_access.host_path 请求 rw 权限。授权成功后必须使用返回的 guestPath：只调用一次 edit_file（old_string 为空）把 request.payload 写入 guestPath/request.filename。随后只调用 poll_external_skill_request 查询同一 request_id；首次不传 last_sequence，后续传回上次 sequence，让 gateway 长轮询阶段变化或最终响应。不要直接反复读取文件，不要运行 sleep、shell/Python 轮询，也绝不能再次写 request_filename、再次调用安装/卸载工具或创建新请求。宿主会把成功、失败、超时或中断恢复写成最终响应并清理 .processing；poll 工具超过 deadline_at + terminal_grace_seconds 仍无最终响应时会返回 HOST_RESPONSE_TIMEOUT。任何最终响应（包括 retryable 错误）都必须原样告知用户并结束本次请求；没有用户新的明确指令，绝不能自动重试或生成新 request_id。安装不要改用 host.skills.edit/publish；卸载不要改用 host.skills.delete 或 skills.deleteDraft。",
        "bridge_dir": bridge_dir,
        "host_access": {
            "host_path": host_path,
            "mode": "rw",
            "use_returned_guest_path": true
        },
        "request": {
            "filename": format!("{id}.request.json"),
            "status_filename": format!("{id}.status.json"),
            "response_filename": format!("{id}.response.json"),
            "poll_tool": POLL_TOOL_NAME,
            "poll_after_seconds": 0,
            "timeout_seconds": BRIDGE_INSTALL_RESPONSE_TIMEOUT_SECONDS,
            "terminal_grace_seconds": 5,
            "payload": request
        },
        "directory_commit": false,
        "restart_required": false
    })
}

fn poll_bridge_request(bridge_dir: &Path, arguments: &Value) -> Value {
    let request_id = arguments
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if request_id.len() != 32
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return json!({
            "status": "REQUEST_STATUS_INVALID",
            "request_id": request_id,
            "poll_again": false,
            "message": "request_id 必须是 install/uninstall 工具原样返回的 32 位小写十六进制 ID。"
        });
    }
    let last_sequence = arguments.get("last_sequence").and_then(Value::as_u64);
    let response_path = bridge_dir.join(format!("{request_id}.response.json"));
    let status_path = bridge_dir.join(format!("{request_id}.status.json"));
    let wait_deadline = Instant::now() + Duration::from_secs(10);
    let mut latest_status = None;
    loop {
        match read_bridge_poll_json(&response_path) {
            Ok(Some(mut response)) => {
                if let Some(object) = response.as_object_mut() {
                    object
                        .entry("request_id")
                        .or_insert_with(|| json!(request_id));
                    object.insert("poll_complete".into(), Value::Bool(true));
                    object.insert("poll_again".into(), Value::Bool(false));
                }
                return response;
            }
            Ok(None) => {}
            Err(message) => return poll_read_failure(request_id, &message),
        }
        match read_bridge_poll_json(&status_path) {
            Ok(Some(mut status)) => {
                let sequence = status.get("sequence").and_then(Value::as_u64);
                let deadline_at = status.get("deadline_at").and_then(Value::as_u64);
                let terminal_grace = status
                    .get("terminal_grace_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(5);
                if deadline_at.is_some_and(|deadline| {
                    unix_seconds() > deadline.saturating_add(terminal_grace)
                }) {
                    return json!({
                        "status": "HOST_RESPONSE_TIMEOUT",
                        "request_id": request_id,
                        "poll_again": false,
                        "deadline_at": deadline_at,
                        "message": "宿主最终响应超过固定 deadline 与 grace；停止轮询，禁止重复提交同一请求。"
                    });
                }
                if let Some(object) = status.as_object_mut() {
                    object.insert("poll_complete".into(), Value::Bool(false));
                    object.insert("poll_again".into(), Value::Bool(true));
                    object.insert("poll_tool".into(), Value::String(POLL_TOOL_NAME.into()));
                }
                latest_status = Some(status);
                if last_sequence.is_none() || sequence != last_sequence {
                    return latest_status.expect("status was just stored");
                }
            }
            Ok(None) => {}
            Err(message) => return poll_read_failure(request_id, &message),
        }
        if Instant::now() >= wait_deadline {
            return latest_status.unwrap_or_else(|| {
                json!({
                    "status": "REQUEST_NOT_READY",
                    "request_id": request_id,
                    "poll_complete": false,
                    "poll_again": true,
                    "poll_tool": POLL_TOOL_NAME,
                    "message": "宿主尚未接收该请求；保持同一 request_id 继续查询，禁止重新提交。"
                })
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn poll_read_failure(request_id: &str, message: &str) -> Value {
    json!({
        "status": "REQUEST_STATUS_UNAVAILABLE",
        "request_id": request_id,
        "poll_again": false,
        "message": format!("无法安全读取宿主请求状态：{message}")
    })
}

fn read_bridge_poll_json(path: &Path) -> Result<Option<Value>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err("状态文件类型或大小非法".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("状态文件属主或权限非法".into());
        }
    }
    serde_json::from_reader(file)
        .map(Some)
        .map_err(|_| "状态文件 JSON 非法".into())
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
            validate_uninstall_arguments(request.get("arguments").unwrap())?;
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
        "description": "安装、导入或添加公开 GitHub 中的单个 Skill 或 Nature-like Skill bundle。Agent 只提交准确 URL，不下载文件、不使用 shell、catalog、Skill Manager、host.skills.edit 或 host.skills.publish。CSSwitch 宿主完成 archive 下载、验证、原子提交并绑定 OPERON；不要手工调用 host.agents.attach_skill。单 Skill返回 INSTALLED_ATTACHED_VERIFY_REQUIRED 后必须调用 skill(skill_name)；bundle 返回 BUNDLE_INSTALLED_ATTACHED 后可直接报告已安装并绑定全部成员。任何最终错误都结束本次请求；即使 retryable=true，也必须先报告用户，禁止自动再次调用本工具。重试 FILES_COMMITTED_ATTACH_REQUIRED 时才可在同一用户请求内再次调用本工具。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "source_url": {"type": "string", "description": "Public GitHub repository, plugin/collection, or exact Skill directory URL."},
                "skill_name": {"type": "string", "description": "The name supplied by the user when no source URL is available."}
            },
            "additionalProperties": false
        }
    })
}

fn uninstall_tool_definition() -> Value {
    json!({
        "name": UNINSTALL_TOOL_NAME,
        "description": "卸载 CSSwitch 导入的外部 Skill。单 Skill 保持原隔离和手工 detach 流程。bundle 成员首次调用只返回 BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED、整包信息和受影响 Skill 列表，不改文件或绑定；必须向用户展示完整列表并等待明确确认。用户确认后才可再次调用，并把响应中的 bundle_id 原样作为 confirm_bundle_id；取消时不得再次调用。确认调用会重新校验 bundle、批量解除 OPERON 绑定并整包隔离，不要逐成员调用 host.agents.detach_skill，也不支持部分物理删除。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "skill_name": {"type": "string", "description": "Exact installed Skill directory name to uninstall."},
                "confirm_bundle_id": {"type": "string", "description": "Only after explicit user confirmation of BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED, pass that response's exact bundle_id. Omit on the first call and for single-Skill uninstall."}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }
    })
}

fn poll_tool_definition() -> Value {
    json!({
        "name": POLL_TOOL_NAME,
        "description": "只读查询一次已提交的 CSSwitch 外部 Skill 请求。首次只传 request_id；若返回 PROCESSING，下一次把其 sequence 原样作为 last_sequence，gateway 会在内部等待最多 10 秒直到阶段变化。不得用本工具创建新请求，也不得再次调用安装/卸载工具。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "request_id": {"type": "string", "description": "HOST_ACCESS_REQUIRED 原样返回的 request payload id。"},
                "last_sequence": {"type": "integer", "minimum": 0, "description": "上一次 PROCESSING 返回的 sequence；首次查询省略。"}
            },
            "required": ["request_id"],
            "additionalProperties": false
        }
    })
}

fn tool_result(payload: Value) -> Value {
    let payload = with_schema(payload);
    let status = payload.get("status").and_then(Value::as_str);
    let is_error = status.is_some_and(|status| {
        status.starts_with("GITHUB_")
            || matches!(
                status,
                "HOST_RESPONSE_TIMEOUT"
                    | "REQUEST_STATUS_INVALID"
                    | "REQUEST_STATUS_UNAVAILABLE"
                    | "SOURCE_REF_REQUIRES_COMMIT_SHA"
                    | "LEGACY_INTEGRITY_UNVERIFIED"
                    | "INSTALL_FAILED"
                    | "UNINSTALL_FAILED"
                    | "SKILL_NAME_CONFLICT"
                    | "INSTALLED_CONTENT_CHANGED"
                    | "UNSUPPORTED_SHARED_DEPENDENCY"
                    | "MULTIPLE_BUNDLE_CANDIDATES"
                    | "BUNDLE_STRUCTURE_UNSUPPORTED"
                    | "BUNDLE_LIMIT_EXCEEDED"
                    | "BUNDLE_PATH_CONFLICT"
                    | "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY"
            )
    });
    let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": is_error
    })
}

fn with_schema(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
    }
    payload
}

pub(crate) fn handle_bridge_request_with_progress(
    data_dir: &Path,
    science_context: Option<&ScienceHostContext>,
    request: &Value,
    progress: &mut dyn FnMut(&str, &str),
) -> Value {
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("");
    let arguments = request.get("arguments").unwrap_or(&Value::Null);
    with_schema(match operation {
        "install" => install_from_arguments_with_context_and_progress(
            data_dir,
            science_context,
            arguments,
            progress,
        ),
        "uninstall" => uninstall_from_arguments_with_context(data_dir, science_context, arguments),
        _ => json!({
            "status": "REQUEST_FAILED",
            "message": "未知的本地 Skill 操作",
            "directory_commit": false,
            "restart_required": false
        }),
    })
}

pub(crate) fn install_from_arguments(data_dir: &Path, arguments: &Value) -> Value {
    install_from_arguments_with_context(data_dir, None, arguments)
}

fn install_from_arguments_with_context(
    data_dir: &Path,
    science_context: Option<&ScienceHostContext>,
    arguments: &Value,
) -> Value {
    let mut progress = |_: &str, _: &str| {};
    install_from_arguments_with_context_and_progress(
        data_dir,
        science_context,
        arguments,
        &mut progress,
    )
}

fn install_from_arguments_with_context_and_progress(
    data_dir: &Path,
    science_context: Option<&ScienceHostContext>,
    arguments: &Value,
    progress: &mut dyn FnMut(&str, &str),
) -> Value {
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
            "source_kind": "github",
            "directory_commit": false,
            "attach_attempted": false,
            "attach_required": false,
            "attach_verified": false,
            "load_verification_required": false,
            "content_sha256": null,
            "resolved_commit_sha": null,
            "message": "请提供公开 GitHub 仓库、Skill 集合或准确 Skill 目录链接。CSSwitch 不会根据名称猜测来源。",
            "restart_required": false
        });
    };
    let Some(science_context) = science_context else {
        return install_not_ready(skill_name, "CSSwitch 尚未确认可用的 Science runtime");
    };
    progress("preflight", "正在确认 Science runtime 与 OPERON 控制面");
    if let Err(error) = verify_attach_control_ready(science_context) {
        return install_not_ready(skill_name, &error.message);
    }
    match install_external_skill(data_dir, source_url, science_context, progress) {
        Ok(value) => value,
        Err(error) => install_error_payload(skill_name, error),
    }
}

fn install_external_skill(
    data_dir: &Path,
    source_url: &str,
    science_context: &ScienceHostContext,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<Value, InstallError> {
    match install_github_package_with_progress(data_dir, source_url, progress)? {
        InstalledPackage::Skill(commit) => {
            progress("attach", "文件已提交，正在绑定 OPERON 并回读确认");
            Ok(attach_install_commit(science_context, commit))
        }
        InstalledPackage::Bundle(commit) => {
            progress("attach", "bundle 已提交，正在批量绑定 OPERON 并回读确认");
            Ok(attach_bundle_commit(science_context, commit))
        }
    }
}

fn attach_bundle_commit(context: &ScienceHostContext, commit: BundleCommit) -> Value {
    let attach = update_agent_skills(context, &commit.skill_names, &[], &commit.active_org);
    let (status, message, attach_required, attach_verified) = match attach.as_ref() {
        Ok(_) => (
            "BUNDLE_INSTALLED_ATTACHED",
            format!(
                "bundle 文件已安装，OPERON 已回读确认绑定 {} 个 Skill。",
                commit.skill_names.len()
            ),
            false,
            true,
        ),
        Err(error) if error.uncertain => (
            "ATTACH_STATE_UNCERTAIN",
            format!("bundle 文件已保留，但批量绑定结果不确定：{}", error.message),
            true,
            false,
        ),
        Err(error) => (
            "FILES_COMMITTED_ATTACH_REQUIRED",
            format!("bundle 文件已保留，但自动批量绑定未完成：{}", error.message),
            true,
            false,
        ),
    };
    let attach_error = attach.as_ref().err().cloned();
    let attached_names = attach
        .as_ref()
        .map(|result| result.attached.clone())
        .unwrap_or_default();
    let missing_names = if attach_verified {
        Vec::new()
    } else {
        commit.skill_names.clone()
    };
    let skills = commit
        .members
        .iter()
        .map(|member| {
            json!({
                "skill_name": member.skill_name,
                "content_sha256": member.content_sha256,
                "install_action": member.install_action,
                "attach_verified": attach_verified,
            })
        })
        .collect::<Vec<_>>();
    let content_fetch = !matches!(
        commit.action,
        csswitch_skill_install_core::InstallAction::ReusedVerified
    );
    json!({
        "status": status,
        "package_kind": "bundle",
        "bundle_id": commit.bundle_id,
        "bundle_name": commit.bundle_name,
        "skill_name": commit.skill_names.first(),
        "skill_names": commit.skill_names,
        "support_paths": commit.support_paths,
        "skills": skills,
        "source_kind": commit.source_kind.as_str(),
        "directory_commit": commit.directory_commit,
        "install_action": commit.action.as_str(),
        "attach_attempted": true,
        "attach_required": attach_required,
        "attach_verified": attach_verified,
        "attached_skill_names": attached_names,
        "missing_skill_names": missing_names,
        "load_verification_required": false,
        "content_sha256": commit.bundle_content_sha256,
        "resolved_commit_sha": commit.resolved_commit_sha,
        "source_digest_sha256": commit.source_digest_sha256,
        "dependency_scan": "BEST_EFFORT",
        "agent_name": "OPERON",
        "attach_method": "csswitch_batch_auto_attach",
        "source_resolution": true,
        "content_fetch": content_fetch,
        "science_discovery": if attach_verified { "BATCH_ATTACHED" } else { "FILES_VISIBLE_NOT_ATTACHED" },
        "skill_trigger": "NOT_REQUIRED_FOR_BUNDLE_ACCEPTANCE",
        "function_run": "NOT_VERIFIED",
        "restart_required": false,
        "new_conversation_required": false,
        "import_origin_written": true,
        "attach_error": attach_error,
        "message": message
    })
}

fn attach_install_commit(context: &ScienceHostContext, commit: InstallCommit) -> Value {
    let attach = attach_skill(context, &commit.skill_name, &commit.active_org);
    let (status, message, attach_required, attach_verified) = match attach.as_ref() {
        Ok(AttachResult::Attached | AttachResult::AlreadyAttached) => (
            "INSTALLED_ATTACHED_VERIFY_REQUIRED",
            "Skill 文件已验证并绑定 OPERON。Agent 现在必须调用 skill(skill_name) 验证当前会话加载；验证前不得报告可用。".to_string(),
            false,
            true,
        ),
        Err(error) if error.uncertain => (
            "ATTACH_STATE_UNCERTAIN",
            format!("Skill 文件已保留，但 OPERON 绑定结果不确定：{}", error.message),
            true,
            false,
        ),
        Err(error) => (
            "FILES_COMMITTED_ATTACH_REQUIRED",
            format!("Skill 文件已保留，但自动绑定未完成：{}。请重新调用同一安装工具重试。", error.message),
            true,
            false,
        ),
    };
    let attach_error = attach.err();
    let content_fetch = !matches!(
        commit.action,
        csswitch_skill_install_core::InstallAction::ReusedVerified
    );
    json!({
        "status": status,
        "skill_name": commit.skill_name,
        "source_kind": commit.source_kind.as_str(),
        "directory_commit": commit.directory_commit,
        "install_action": commit.action.as_str(),
        "attach_attempted": true,
        "attach_required": attach_required,
        "attach_verified": attach_verified,
        "load_verification_required": attach_verified,
        "content_sha256": commit.content_sha256,
        "resolved_commit_sha": commit.resolved_commit_sha,
        "source_digest_sha256": commit.source_digest_sha256,
        "dependency_scan": commit.dependency_scan,
        "agent_name": "OPERON",
        "attach_method": "csswitch_auto_attach",
        "source_resolution": true,
        "content_fetch": content_fetch,
        "science_discovery": if attach_verified { "ATTACHED" } else { "FILES_VISIBLE_NOT_ATTACHED" },
        "skill_trigger": "NOT_VERIFIED",
        "function_run": "NOT_VERIFIED",
        "restart_required": false,
        "new_conversation_required": false,
        "import_origin_written": true,
        "attach_error": attach_error,
        "message": message
    })
}

fn install_not_ready(skill_name: Option<&str>, message: &str) -> Value {
    json!({
        "status": "SCIENCE_NOT_READY",
        "skill_name": skill_name,
        "source_kind": "github",
        "directory_commit": false,
        "attach_attempted": false,
        "attach_required": false,
        "attach_verified": false,
        "load_verification_required": false,
        "content_sha256": null,
        "resolved_commit_sha": null,
        "restart_required": false,
        "message": message
    })
}

fn install_error_payload(skill_name: Option<&str>, error: InstallError) -> Value {
    let status = match error.code.as_str() {
        "SKILL_NAME_CONFLICT"
        | "INSTALLED_CONTENT_CHANGED"
        | "UNSUPPORTED_SHARED_DEPENDENCY"
        | "MULTIPLE_BUNDLE_CANDIDATES"
        | "BUNDLE_STRUCTURE_UNSUPPORTED"
        | "BUNDLE_LIMIT_EXCEEDED"
        | "BUNDLE_PATH_CONFLICT"
        | "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY"
        | "SOURCE_REF_REQUIRES_COMMIT_SHA"
        | "LEGACY_INTEGRITY_UNVERIFIED" => error.code.as_str(),
        code if code.starts_with("GITHUB_") => error.code.as_str(),
        "SCIENCE_NOT_READY" => "SCIENCE_NOT_READY",
        _ => "INSTALL_FAILED",
    };
    let user_retry_available = error.retryable;
    let message = format!(
        "{}。本次请求已结束且 bridge 状态已清理；不得自动重试。{}",
        error.message,
        if user_retry_available {
            "如需重试，必须先向用户报告并等待新的明确指令。"
        } else {
            "请向用户报告该最终错误。"
        }
    );
    json!({
        "status": status,
        "skill_name": skill_name,
        "source_kind": "github",
        "directory_commit": error.directory_commit,
        "attach_attempted": false,
        "attach_required": false,
        "attach_verified": false,
        "load_verification_required": false,
        "content_sha256": null,
        "resolved_commit_sha": null,
        "restart_required": false,
        "request_terminal": true,
        "automatic_retry_allowed": false,
        "user_retry_available": user_retry_available,
        "error": error,
        "message": message
    })
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

fn requested_bundle_confirmation(arguments: &Value) -> Result<Option<String>, String> {
    let Some(value) = arguments.get("confirm_bundle_id") else {
        return Ok(None);
    };
    let id = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("bundle 确认 ID 非法")?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("bundle 确认 ID 非法".into());
    }
    Ok(Some(id.to_string()))
}

fn validate_uninstall_arguments(arguments: &Value) -> Result<(String, Option<String>), String> {
    let object = arguments.as_object().ok_or("本地 Skill 卸载参数非法")?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "skill_name" | "confirm_bundle_id"))
    {
        return Err("本地 Skill 卸载参数非法".into());
    }
    let skill_name = requested_skill_name(arguments)?;
    let confirm_bundle_id = requested_bundle_confirmation(arguments)?;
    Ok((skill_name, confirm_bundle_id))
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

#[cfg(test)]
pub(crate) fn uninstall_from_arguments(data_dir: &Path, arguments: &Value) -> Value {
    uninstall_from_arguments_with_context(data_dir, None, arguments)
}

fn uninstall_from_arguments_with_context(
    data_dir: &Path,
    science_context: Option<&ScienceHostContext>,
    arguments: &Value,
) -> Value {
    let (skill_name, confirm_bundle_id) = match validate_uninstall_arguments(arguments) {
        Ok(values) => values,
        Err(message) => return uninstall_failure(message),
    };
    match uninstall_external_skill(
        data_dir,
        science_context,
        &skill_name,
        confirm_bundle_id.as_deref(),
    ) {
        Ok(value) => value,
        Err(message) => uninstall_failure(message),
    }
}

fn bundle_uninstall_confirmation(
    bundle: &csswitch_skill_install_core::BundleUninstall,
    skill_name: &str,
    confirmation_changed: bool,
) -> Value {
    let message = if confirmation_changed {
        "bundle 归属或确认 ID 已变化；文件和绑定尚未改动。请向用户重新展示当前整包成员，并等待新的明确确认。"
    } else {
        "该 Skill 属于 bundle；文件和绑定尚未改动。请向用户展示完整受影响 Skill 列表，并确认是否整包卸载。取消时不要再次调用卸载工具。"
    };
    json!({
        "status": "BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED",
        "package_kind": "bundle",
        "bundle_id": bundle.bundle_id,
        "confirm_bundle_id": bundle.bundle_id,
        "bundle_name": bundle.bundle_name,
        "skill_name": skill_name,
        "skill_names": bundle.skill_names,
        "affected_skill_names": bundle.skill_names,
        "confirmation_required": true,
        "confirmation_scope": "whole_bundle",
        "partial_uninstall_supported": false,
        "detach_required": false,
        "detach_attempted": false,
        "detach_verified": false,
        "directory_removed": false,
        "quarantine_commit": false,
        "restart_required": false,
        "request_terminal": true,
        "automatic_retry_allowed": false,
        "message": message
    })
}

fn uninstall_external_skill(
    data_dir: &Path,
    science_context: Option<&ScienceHostContext>,
    skill_name: &str,
    confirm_bundle_id: Option<&str>,
) -> Result<Value, String> {
    validate_skill_name(skill_name)?;
    if let Some(bundle) = find_bundle_for_skill(data_dir, skill_name).map_err(|e| e.to_string())? {
        if confirm_bundle_id != Some(bundle.bundle_id.as_str()) {
            return Ok(bundle_uninstall_confirmation(
                &bundle,
                skill_name,
                confirm_bundle_id.is_some(),
            ));
        }
        let context = science_context
            .ok_or("bundle 卸载需要 CSSwitch 已确认的 Science runtime；文件尚未改动")?;
        update_agent_skills(context, &[], &bundle.skill_names, &bundle.active_org).map_err(
            |error| format!("bundle 批量 detach 未确认，文件尚未改动：{}", error.message),
        )?;
        let commit = match quarantine_bundle(data_dir, &bundle) {
            Ok(commit) => commit,
            Err(error) => {
                return match update_agent_skills(
                    context,
                    &bundle.skill_names,
                    &[],
                    &bundle.active_org,
                ) {
                    Ok(_) => Err(format!(
                        "bundle 整包隔离失败，已恢复 OPERON 绑定：{}",
                        error.message
                    )),
                    Err(restore_error) => Err(format!(
                        "bundle 整包隔离失败，且 OPERON 绑定恢复未确认：{}；{}",
                        error.message, restore_error.message
                    )),
                };
            }
        };
        return Ok(json!({
            "status": "BUNDLE_UNINSTALLED_DETACHED",
            "package_kind": "bundle",
            "bundle_id": commit.bundle_id,
            "bundle_name": commit.bundle_name,
            "skill_name": skill_name,
            "skill_names": commit.skill_names,
            "agent_name": "OPERON",
            "detach_required": false,
            "detach_verified": true,
            "detach_method": "csswitch_batch_auto_detach",
            "directory_removed": true,
            "quarantine_commit": true,
            "quarantine_path": commit.quarantined_path,
            "restart_required": false,
            "message": "bundle 已整包解除 OPERON 绑定并移入 CSSwitch 隔离回收区。"
        }));
    }
    if confirm_bundle_id.is_some() {
        return Err("确认的 bundle 已不存在，或该 Skill 的 bundle 归属已改变；文件尚未改动".into());
    }
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

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
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
        let path = crate::test_temp_root().join(format!(
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

    fn write_poll_json(path: &Path, value: &Value) {
        let temp = path.with_extension("tmp");
        fs::write(&temp, serde_json::to_vec(value).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::rename(temp, path).unwrap();
    }

    fn imported_skill(data: &Path, name: &str) -> PathBuf {
        let skill = data.join("orgs/org-test/skills").join(name);
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), b"---\nname: test\n---\n").unwrap();
        let marker = json!({
            "version": 1,
            "repo": "owner/repo",
            "sha": "0123456789abcdef0123456789abcdef01234567",
            "plugin": name,
            "marketplace": CSSWITCH_MARKETPLACE,
            "path": format!("skills/{name}"),
            "importedAt": "2026-07-15T00:00:00Z",
            "license": "NOASSERTION"
        });
        fs::write(
            skill.join(IMPORT_ORIGIN_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
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
    fn tool_description_routes_download_and_attach_to_csswitch() {
        let tool = install_tool_definition();
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("host.skills.edit"));
        assert!(description.contains("host.skills.publish"));
        assert!(description.contains("Agent 只提交准确 URL"));
        assert!(description.contains("不下载文件"));
        assert!(description.contains("不要手工调用 host.agents.attach_skill"));
        assert!(description.contains("skill(skill_name)"));

        let uninstall = uninstall_tool_definition();
        let uninstall_description = uninstall["description"].as_str().unwrap();
        assert!(uninstall_description.contains("BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED"));
        assert!(uninstall_description.contains("confirm_bundle_id"));
        assert!(uninstall_description.contains("不支持部分物理删除"));
        assert!(uninstall["inputSchema"]["properties"]["confirm_bundle_id"].is_object());
    }

    #[test]
    fn bundle_uninstall_confirmation_is_structured_and_non_mutating() {
        let bundle = csswitch_skill_install_core::BundleUninstall {
            bundle_id: "a".repeat(64),
            bundle_name: "nature-skills".into(),
            active_org: "org-test".into(),
            skill_names: vec!["nature-reader".into(), "nature-writing".into()],
            top_level_paths: vec![
                "_shared".into(),
                "nature-reader".into(),
                "nature-writing".into(),
            ],
            manifest_path: crate::test_temp_root().join("bundle.json"),
        };
        let result = bundle_uninstall_confirmation(&bundle, "nature-reader", false);
        assert_eq!(result["status"], "BUNDLE_UNINSTALL_CONFIRMATION_REQUIRED");
        assert_eq!(result["bundle_id"], "a".repeat(64));
        assert_eq!(result["confirm_bundle_id"], "a".repeat(64));
        assert_eq!(result["bundle_name"], "nature-skills");
        assert_eq!(result["skill_name"], "nature-reader");
        assert_eq!(result["skill_names"], result["affected_skill_names"]);
        assert_eq!(result["skill_names"].as_array().unwrap().len(), 2);
        assert_eq!(result["confirmation_required"], true);
        assert_eq!(result["confirmation_scope"], "whole_bundle");
        assert_eq!(result["partial_uninstall_supported"], false);
        assert_eq!(result["detach_attempted"], false);
        assert_eq!(result["directory_removed"], false);
        assert_eq!(result["quarantine_commit"], false);

        let changed = bundle_uninstall_confirmation(&bundle, "nature-reader", true);
        assert!(changed["message"].as_str().unwrap().contains("重新展示"));
    }

    #[test]
    fn bundle_confirmation_argument_is_strictly_bound() {
        let id = "b".repeat(64);
        let parsed = validate_uninstall_arguments(
            &json!({"skill_name":"nature-reader","confirm_bundle_id":id}),
        )
        .unwrap();
        assert_eq!(parsed.0, "nature-reader");
        assert_eq!(
            parsed.1.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert!(validate_uninstall_arguments(
            &json!({"skill_name":"nature-reader","confirm_bundle_id":"B".repeat(64)})
        )
        .is_err());
        assert!(validate_uninstall_arguments(
            &json!({"skill_name":"nature-reader","confirm_bundle_id":"b".repeat(64),"confirm":true})
        )
        .is_err());
    }

    #[test]
    fn source_url_without_science_context_never_writes() {
        let data = temp_dir("science-not-ready");
        let result = install_from_arguments(
            &data,
            &json!({"source_url": "https://github.com/a/b/tree/main/skill"}),
        );
        assert_eq!(result["status"], "SCIENCE_NOT_READY");
        assert_eq!(result["directory_commit"], false);
        assert!(!data.join("orgs").exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn github_failures_keep_structured_status_and_mcp_error_semantics() {
        for code in [
            "GITHUB_RATE_LIMITED",
            "GITHUB_PERMISSION_DENIED",
            "GITHUB_NOT_FOUND",
            "GITHUB_TIMEOUT",
            "GITHUB_REDIRECT_INVALID",
            "SOURCE_REF_REQUIRES_COMMIT_SHA",
            "LEGACY_INTEGRITY_UNVERIFIED",
        ] {
            let payload = install_error_payload(
                Some("demo"),
                InstallError::new(code, "expected failure", "test").retryable(true),
            );
            assert_eq!(payload["status"], code);
            assert_eq!(payload["request_terminal"], true);
            assert_eq!(payload["automatic_retry_allowed"], false);
            assert_eq!(payload["user_retry_available"], true);
            assert!(payload["message"]
                .as_str()
                .unwrap()
                .contains("不得自动重试"));
            let result = tool_result(payload);
            assert_eq!(result["isError"], true);
            assert_eq!(result["structuredContent"]["status"], code);
        }
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

        let single = imported_skill(&data, "single-skill");
        let stale_confirmation = uninstall_from_arguments(
            &data,
            &json!({"skill_name":"single-skill","confirm_bundle_id":"c".repeat(64)}),
        );
        assert_eq!(stale_confirmation["status"], "UNINSTALL_FAILED");
        assert!(stale_confirmation["message"]
            .as_str()
            .unwrap()
            .contains("bundle"));
        assert!(single.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_accepts_v1_compatible_local_zip_marker() {
        let (root, data) = standard_data_dir("uninstall-local-zip");
        let skill = data.join("orgs/org-test/skills/local-demo");
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), b"demo").unwrap();
        fs::write(
            skill.join(IMPORT_ORIGIN_FILE),
            serde_json::to_vec(&json!({
                "version": 1,
                "repo": "csswitch/local-archive",
                "sha": "a".repeat(40),
                "plugin": "local-demo",
                "marketplace": CSSWITCH_MARKETPLACE,
                "path": "local-demo",
                "importedAt": "2026-07-15T00:00:00Z",
                "license": "NOASSERTION",
                "csswitch_revision": 2,
                "source_kind": "local_zip",
                "content_sha256": "b".repeat(64),
                "archive_sha256": "a".repeat(64)
            }))
            .unwrap(),
        )
        .unwrap();
        let result = uninstall_from_arguments(&data, &json!({"skill_name":"local-demo"}));
        assert_eq!(result["status"], "QUARANTINED_DETACH_REQUIRED");
        assert!(!skill.exists());
        fs::remove_dir_all(root).unwrap();
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
        assert_eq!(
            names,
            [INSTALL_TOOL_NAME, UNINSTALL_TOOL_NAME, POLL_TOOL_NAME]
        );
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
        let confirmed = mcp_request(bridge, ToolMode::All, &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":UNINSTALL_TOOL_NAME,"arguments":{"skill_name":"pdf","confirm_bundle_id":"d".repeat(64)}}})).unwrap();
        assert_eq!(
            confirmed["result"]["structuredContent"]["request"]["payload"]["arguments"]
                ["confirm_bundle_id"],
            "d".repeat(64)
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
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 2);
        assert_eq!(listed["result"]["tools"][0]["name"], UNINSTALL_TOOL_NAME);
        assert_eq!(listed["result"]["tools"][1]["name"], POLL_TOOL_NAME);
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
            &json!({"skill_name":"pdf","confirm_bundle_id":"e".repeat(64)}),
        );
        let filename = result["request"]["filename"].as_str().unwrap();
        let id = filename.strip_suffix(".request.json").unwrap();
        assert_eq!(result["request_id"], id);
        assert_eq!(
            result["request"]["status_filename"],
            format!("{id}.status.json")
        );
        assert_eq!(result["request"]["poll_tool"], POLL_TOOL_NAME);
        assert_eq!(result["request"]["poll_after_seconds"], 0);
        assert_eq!(
            result["request"]["timeout_seconds"],
            BRIDGE_INSTALL_RESPONSE_TIMEOUT_SECONDS
        );
        assert_eq!(result["request"]["terminal_grace_seconds"], 5);
        assert!(result["message"]
            .as_str()
            .unwrap()
            .contains("绝不能再次写 request_filename"));
        assert!(result["message"]
            .as_str()
            .unwrap()
            .contains("不要运行 sleep"));
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
    fn poll_tool_reports_progress_long_polls_and_returns_final_response() {
        let bridge = temp_dir("poll-bridge");
        let id = "a".repeat(32);
        let status_path = bridge.join(format!("{id}.status.json"));
        let response_path = bridge.join(format!("{id}.response.json"));
        write_poll_json(
            &status_path,
            &json!({
                "status": "PROCESSING",
                "request_id": id,
                "phase": "download",
                "sequence": 7,
                "elapsed_seconds": 12,
                "deadline_at": unix_seconds() + 60,
                "terminal_grace_seconds": 5
            }),
        );
        let first = poll_bridge_request(&bridge, &json!({"request_id": id}));
        assert_eq!(first["status"], "PROCESSING");
        assert_eq!(first["phase"], "download");
        assert_eq!(first["sequence"], 7);
        assert_eq!(first["poll_again"], true);

        let response_for_thread = response_path.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            write_poll_json(
                &response_for_thread,
                &json!({"status":"BUNDLE_INSTALLED_ATTACHED","directory_commit":true}),
            );
        });
        let final_response =
            poll_bridge_request(&bridge, &json!({"request_id": id, "last_sequence": 7}));
        worker.join().unwrap();
        assert_eq!(final_response["status"], "BUNDLE_INSTALLED_ATTACHED");
        assert_eq!(final_response["request_id"], id);
        assert_eq!(final_response["poll_complete"], true);
        assert_eq!(final_response["poll_again"], false);

        let invalid = poll_bridge_request(&bridge, &json!({"request_id":"../escape"}));
        assert_eq!(invalid["status"], "REQUEST_STATUS_INVALID");
        fs::remove_dir_all(bridge).unwrap();
    }

    #[test]
    fn poll_tool_stops_after_host_deadline_without_resubmitting() {
        let bridge = temp_dir("poll-timeout");
        let id = "b".repeat(32);
        write_poll_json(
            &bridge.join(format!("{id}.status.json")),
            &json!({
                "status": "PROCESSING",
                "request_id": id,
                "phase": "download",
                "sequence": 4,
                "deadline_at": unix_seconds().saturating_sub(10),
                "terminal_grace_seconds": 5
            }),
        );
        let result = poll_bridge_request(&bridge, &json!({"request_id": id}));
        assert_eq!(result["status"], "HOST_RESPONSE_TIMEOUT");
        assert_eq!(result["poll_again"], false);
        fs::remove_dir_all(bridge).unwrap();
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
