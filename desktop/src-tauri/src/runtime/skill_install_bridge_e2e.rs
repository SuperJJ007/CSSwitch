use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::{merge_registrations, INSTALL_SERVER_NAME, MANAGED_MARKER, UNINSTALL_SERVER_NAME};
use crate::runtime::external_skill_route::ensure_route_skill;

const INSTALL_URL: &str = "https://github.com/anthropics/skills/tree/main/skills/internal-comms";

fn temp_dir(label: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = PathBuf::from("/private/tmp")
        .join(format!("csswitch-{label}-{}-{suffix}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 8765);
    port
}

fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn prepare_safe_bin(root: &Path) -> PathBuf {
    let bin = root.join("safe-bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("security"),
        "#!/bin/sh\numask 077\nprintf 'invoked\\n' >> \"$CSSWITCH_E2E_SECURITY_MARKER\"\nexit 1\n",
    );
    write_executable(
        &bin.join("python3"),
        "#!/bin/sh\nexec /Applications/Xcode.app/Contents/Developer/Library/Frameworks/Python3.framework/Versions/3.9/bin/python3 \"$@\"\n",
    );
    bin
}

fn safe_command(program: &Path, sandbox_home: &Path, safe_bin: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("HOME", sandbox_home)
        .env(
            "PATH",
            format!(
                "{}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                safe_bin.display()
            ),
        )
        .env("TMPDIR", "/private/tmp")
        .env(
            "DEVELOPER_DIR",
            "/Applications/Xcode.app/Contents/Developer",
        )
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env(
            "CSSWITCH_E2E_SECURITY_MARKER",
            sandbox_home.join("security-stub-invoked.log"),
        );
    command
}

fn output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let root = temp_dir("e2e-command-output");
    let stdout_path = root.join("stdout");
    let stderr_path = root.join("stderr");
    let stdout = File::create(&stdout_path).map_err(|e| e.to_string())?;
    let stderr = File::create(&stderr_path).map_err(|e| e.to_string())?;
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&root);
            return Err("子命令超时并已终止".into());
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = fs::read(&stdout_path).map_err(|e| e.to_string())?;
    let stderr = fs::read(&stderr_path).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(root);
    if stdout.len() > 8 * 1024 * 1024 || stderr.len() > 8 * 1024 * 1024 {
        return Err("子命令输出超限".into());
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[derive(Clone, Debug, Default)]
struct Observation {
    connector_skill_discovered: bool,
    connector_skill_loaded: bool,
    repl_description_ok: bool,
    repl_calls: usize,
    tool_result_count: usize,
    name_only_status_seen: bool,
    url_status_seen: bool,
    url_failure_seen: bool,
    attach_called: bool,
    skill_loaded_after_attach: bool,
    restart_validation: bool,
    skill_loaded_after_restart: bool,
    route_skill_discovered: bool,
    route_skill_loaded: bool,
    uninstaller_skill_loaded: bool,
    uninstall_repl_calls: usize,
    uninstall_status_seen: bool,
    detach_called: bool,
    skill_absent_after_detach: bool,
    invoked_tools: Vec<String>,
    streaming_count: usize,
    advertised_tool_names: Vec<String>,
    repl_description: Option<String>,
}

struct MockProvider {
    port: u16,
    observation: Arc<Mutex<Observation>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockProvider {
    fn start(bridge_dir: String) -> Self {
        use std::sync::atomic::Ordering;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let observation = Arc::new(Mutex::new(Observation::default()));
        let thread_observation = observation.clone();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let worker = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                        let Ok(body) = read_http_body(&mut stream) else {
                            continue;
                        };
                        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let stream_requested =
                            value.get("stream").and_then(Value::as_bool) == Some(true);
                        if stream_requested {
                            let mut locked = thread_observation
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            locked.streaming_count += 1;
                            if let Some(tools) = value.get("tools").and_then(Value::as_array) {
                                for tool in tools {
                                    if let Some(name) = tool.get("name").and_then(Value::as_str) {
                                        if !locked
                                            .advertised_tool_names
                                            .iter()
                                            .any(|seen| seen == name)
                                        {
                                            locked.advertised_tool_names.push(name.to_string());
                                        }
                                        if name == "repl" {
                                            locked.repl_description = tool
                                                .get("description")
                                                .and_then(Value::as_str)
                                                .map(str::to_string);
                                            locked.repl_description_ok =
                                                locked.repl_description.as_deref().is_some_and(
                                                    |description| description.contains("host.mcp"),
                                                );
                                        }
                                    }
                                }
                            }
                        }
                        let response = route_model_request(
                            &value,
                            stream_requested,
                            &thread_observation,
                            &bridge_dir,
                        );
                        write_http_response(&mut stream, response.0, &response.1);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            observation,
            shutdown,
            thread: Some(worker),
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn snapshot(&self) -> Observation {
        self.observation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn begin_restart_validation(&self) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        observation.restart_validation = true;
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

fn read_http_body(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if bytes.len() > 64 * 1024 {
            return Err("headers too large".into());
        }
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("request ended before headers".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).map_err(|e| e.to_string())?;
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if length > 8 * 1024 * 1024 {
        return Err("body too large".into());
    }
    while bytes.len() - header_end < length {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("body truncated".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes[header_end..header_end + length].to_vec())
}

fn write_http_response(stream: &mut TcpStream, content_type: &str, body: &[u8]) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body);
}

fn route_model_request(
    value: &Value,
    stream_requested: bool,
    observation: &Arc<Mutex<Observation>>,
    bridge_dir: &str,
) -> (&'static str, Vec<u8>) {
    if !stream_requested {
        return ("application/json", json_response());
    }
    if json_contains(value, "You are reviewing work an agent did in frame") {
        return ("text/event-stream", terminal_sse());
    }
    let last_content = value
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .and_then(|message| message.get("content"));
    let last_is_tool_result =
        last_content.is_some_and(|content| json_has_type(content, "tool_result"));
    let recent_tool_result = latest_tool_result(value);
    let restart_validation = observation
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .restart_validation;
    let restart_prompt = json_contains(
        value,
        "请加载并使用 internal-comms Skill 做一次无副作用检查",
    );
    // The conversation retains earlier prompts. A newer uninstall request must
    // take precedence over the persisted restart-validation message.
    if json_contains(value, "请卸载 internal-comms") {
        return route_uninstall_request(value, observation, bridge_dir);
    }
    if restart_validation && restart_prompt {
        if let Some(result) = recent_tool_result {
            let loaded = json_contains(result, "<skill-metadata name=\"internal-comms\"")
                && !json_contains(result, "Unknown skill")
                && !json_contains(result, "partially unavailable");
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.skill_loaded_after_restart = loaded;
            return ("text/event-stream", terminal_sse());
        }
        let mut locked = observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locked.invoked_tools.push("skill".into());
        return (
            "text/event-stream",
            tool_use_sse(
                "skill",
                &json!({
                    "skill": "internal-comms",
                    "human_description": "Verifying persisted Skill"
                }),
            ),
        );
    }
    let has_installer_docs = json_contains(value, "host.mcp(\"csswitch-skill-installer\"")
        && json_contains(value, "install_external_skill");
    let source_url = json_contains(value, INSTALL_URL);

    if last_is_tool_result {
        let last = last_content.expect("checked above");
        if json_contains(last, "HOST_ACCESS_REQUIRED") {
            let host_path = bridge_dir.to_string();
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("request_host_access".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "request_host_access",
                    &json!({"host_path": host_path, "mode": "rw"}),
                ),
            );
        }
        if json_contains(last, "guestPath") && json_contains(last, "granted") {
            let id = format!("{}-999", std::process::id());
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("edit_file".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "edit_file",
                    &json!({
                        "file_path": Path::new(bridge_dir).join(format!("{id}.request.json")),
                        "old_string": "",
                        "new_string": serde_json::to_string(&json!({
                            "operation": "install",
                            "arguments": {"source_url": INSTALL_URL}
                        })).unwrap()
                    }),
                ),
            );
        }
        if json_contains(last, "bytes_written")
            || json_contains(last, "File not found")
            || json_contains(last, "not accessible")
        {
            let id = format!("{}-999", std::process::id());
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("read_file".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "read_file",
                    &json!({"file_path": Path::new(bridge_dir).join(format!("{id}.response.json"))}),
                ),
            );
        }
        if json_contains(last, "FILES_COMMITTED_ATTACH_REQUIRED") {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.url_status_seen = true;
            locked.attach_called = true;
            locked.invoked_tools.push("repl".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "repl",
                    &json!({
                        "code": "result = host.agents.attach_skill(\"OPERON\", \"internal-comms\")\nprint(result)",
                        "human_description": "Attaching imported Skill"
                    }),
                ),
            );
        }
        if json_contains(value, "Verifying attached Skill") {
            let loaded = !json_contains(last, "Unknown skill")
                && !json_contains(last, "partially unavailable");
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.skill_loaded_after_attach = loaded;
            locked.tool_result_count += 1;
            collect_tool_use_names(value, &mut locked.invoked_tools);
            return ("text/event-stream", terminal_sse());
        }
        if json_contains(value, "Attaching imported Skill") {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("skill".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "skill",
                    &json!({
                        "skill": "internal-comms",
                        "human_description": "Verifying attached Skill"
                    }),
                ),
            );
        }
        if json_contains(last, "NEED_SOURCE_URL") || json_contains(last, "INSTALL_FAILED") {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.tool_result_count += 1;
            locked.name_only_status_seen |= json_contains(last, "NEED_SOURCE_URL");
            locked.url_failure_seen |= json_contains(last, "INSTALL_FAILED");
            collect_tool_use_names(value, &mut locked.invoked_tools);
            return ("text/event-stream", terminal_sse());
        }
        if has_installer_docs {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.connector_skill_loaded = true;
            locked.repl_calls += 1;
            locked.invoked_tools.push("repl".into());
            return ("text/event-stream", repl_tool_sse(source_url));
        }
    }

    if has_installer_docs {
        let mut locked = observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locked.connector_skill_loaded = true;
        locked.repl_calls += 1;
        locked.invoked_tools.push("repl".into());
        ("text/event-stream", repl_tool_sse(source_url))
    } else {
        let mut locked = observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locked.connector_skill_discovered |= json_contains(value, "mcp-csswitch-skill-installer");
        locked.invoked_tools.push("skill".into());
        (
            "text/event-stream",
            tool_use_sse(
                "skill",
                &json!({
                    "skill": "mcp-csswitch-skill-installer",
                    "human_description": "Loading CSSwitch installer"
                }),
            ),
        )
    }
}

fn route_uninstall_request(
    value: &Value,
    observation: &Arc<Mutex<Observation>>,
    bridge_dir: &str,
) -> (&'static str, Vec<u8>) {
    let last_content = value
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .and_then(|message| message.get("content"));
    let last_is_tool_result =
        last_content.is_some_and(|content| json_has_type(content, "tool_result"));
    let has_route_docs = json_contains(value, "This Skill only routes external Skill operations");
    let has_uninstaller_docs = json_contains(
        value,
        "<skill-metadata name=\"mcp-csswitch-skill-uninstaller\"",
    );

    if last_is_tool_result {
        let last = last_content.expect("checked above");
        if json_contains(value, "Verifying uninstalled Skill") {
            let absent = json_contains(last, "Unknown skill")
                || json_contains(last, "not found")
                || json_contains(last, "does not exist");
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.skill_absent_after_detach = absent;
            collect_tool_use_names(value, &mut locked.invoked_tools);
            return ("text/event-stream", terminal_sse());
        }
        if json_contains(value, "Detaching quarantined Skill") {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("skill".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "skill",
                    &json!({
                        "skill": "internal-comms",
                        "human_description": "Verifying uninstalled Skill"
                    }),
                ),
            );
        }
        if json_contains(last, "QUARANTINED_DETACH_REQUIRED") {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.uninstall_status_seen = true;
            locked.detach_called = true;
            locked.invoked_tools.push("repl".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "repl",
                    &json!({
                        "code": "result = host.agents.detach_skill(\"OPERON\", \"internal-comms\")\nprint(result)",
                        "human_description": "Detaching quarantined Skill"
                    }),
                ),
            );
        }
        if json_contains(last, "HOST_ACCESS_REQUIRED") {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("request_host_access".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "request_host_access",
                    &json!({"host_path": bridge_dir, "mode": "rw"}),
                ),
            );
        }
        if json_contains(last, "guestPath") && json_contains(last, "granted") {
            let id = format!("{}-998", std::process::id());
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("edit_file".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "edit_file",
                    &json!({
                        "file_path": Path::new(bridge_dir).join(format!("{id}.request.json")),
                        "old_string": "",
                        "new_string": serde_json::to_string(&json!({
                            "operation": "uninstall",
                            "arguments": {"skill_name": "internal-comms"}
                        })).unwrap()
                    }),
                ),
            );
        }
        if json_contains(last, "bytes_written")
            || json_contains(last, "File not found")
            || json_contains(last, "not accessible")
        {
            let id = format!("{}-998", std::process::id());
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.invoked_tools.push("read_file".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "read_file",
                    &json!({"file_path": Path::new(bridge_dir).join(format!("{id}.response.json"))}),
                ),
            );
        }
        if has_uninstaller_docs {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.uninstaller_skill_loaded = true;
            locked.uninstall_repl_calls += 1;
            locked.invoked_tools.push("repl".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "repl",
                    &json!({
                        "code": "result = host.mcp(\"csswitch-skill-uninstaller\", \"uninstall_external_skill\", skill_name=\"internal-comms\")\nprint(result)",
                        "human_description": "Uninstalling external Skill"
                    }),
                ),
            );
        }
        if has_route_docs {
            let mut locked = observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked.route_skill_loaded = true;
            locked.invoked_tools.push("skill".into());
            return (
                "text/event-stream",
                tool_use_sse(
                    "skill",
                    &json!({
                        "skill": "mcp-csswitch-skill-uninstaller",
                        "human_description": "Loading CSSwitch uninstaller"
                    }),
                ),
            );
        }
    }

    if has_uninstaller_docs {
        let mut locked = observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locked.uninstaller_skill_loaded = true;
        locked.uninstall_repl_calls += 1;
        locked.invoked_tools.push("repl".into());
        return (
            "text/event-stream",
            tool_use_sse(
                "repl",
                &json!({
                    "code": "result = host.mcp(\"csswitch-skill-uninstaller\", \"uninstall_external_skill\", skill_name=\"internal-comms\")\nprint(result)",
                    "human_description": "Uninstalling external Skill"
                }),
            ),
        );
    }
    if has_route_docs {
        let mut locked = observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locked.route_skill_loaded = true;
        locked.invoked_tools.push("skill".into());
        return (
            "text/event-stream",
            tool_use_sse(
                "skill",
                &json!({
                    "skill": "mcp-csswitch-skill-uninstaller",
                    "human_description": "Loading CSSwitch uninstaller"
                }),
            ),
        );
    }
    let mut locked = observation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    locked.route_skill_discovered |= json_contains(value, "csswitch-external-skill-tools");
    locked.invoked_tools.push("skill".into());
    (
        "text/event-stream",
        tool_use_sse(
            "skill",
            &json!({
                "skill": "csswitch-external-skill-tools",
                "human_description": "Loading CSSwitch external Skill tools"
            }),
        ),
    )
}

fn repl_tool_sse(source_url: bool) -> Vec<u8> {
    let code = if source_url {
        format!(
            "result = host.mcp(\"csswitch-skill-installer\", \"install_external_skill\", source_url={INSTALL_URL:?})\nprint(result)"
        )
    } else {
        "result = host.mcp(\"csswitch-skill-installer\", \"install_external_skill\", skill_name=\"internal-comms\")\nprint(result)".to_string()
    };
    tool_use_sse(
        "repl",
        &json!({
            "code": code,
            "human_description": "Installing external Skill"
        }),
    )
}

fn json_has_type(value: &Value, expected: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| json_has_type(value, expected)),
        Value::Object(values) => {
            values.get("type").and_then(Value::as_str) == Some(expected)
                || values.values().any(|value| json_has_type(value, expected))
        }
        _ => false,
    }
}

fn latest_tool_result(value: &Value) -> Option<&Value> {
    value
        .get("messages")?
        .as_array()?
        .iter()
        .rev()
        .filter_map(|message| message.get("content"))
        .find(|content| json_has_type(content, "tool_result"))
}

#[test]
fn finds_tool_result_before_science_third_party_safety_notice() {
    let transcript = json!({
        "messages": [
            {"role":"assistant","content":[{"type":"tool_use","name":"skill"}]},
            {"role":"user","content":[{"type":"tool_result","content":"<skill-metadata name=\"internal-comms\" />"}]},
            {"role":"user","content":[{"type":"text","text":"[System] third-party authored"}]}
        ]
    });
    let result = latest_tool_result(&transcript).expect("missing recent tool result");
    assert!(json_contains(
        result,
        "<skill-metadata name=\"internal-comms\""
    ));
}

fn json_contains(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(text) => text.contains(expected),
        Value::Array(values) => values.iter().any(|value| json_contains(value, expected)),
        Value::Object(values) => values.values().any(|value| json_contains(value, expected)),
        _ => false,
    }
}

fn collect_tool_use_names(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_tool_use_names(value, names);
            }
        }
        Value::Object(values) => {
            if values.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(name) = values.get("name").and_then(Value::as_str) {
                    names.push(name.to_string());
                }
            }
            for value in values.values() {
                collect_tool_use_names(value, names);
            }
        }
        _ => {}
    }
}

fn json_response() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "msg_csswitch_aux",
        "type": "message",
        "role": "assistant",
        "model": "csswitch-local-mock",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .unwrap()
}

fn terminal_sse() -> Vec<u8> {
    concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_csswitch_done\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"csswitch-local-mock\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    )
    .as_bytes()
    .to_vec()
}

fn tool_use_sse(tool_name: &str, input: &Value) -> Vec<u8> {
    let start = json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "tool_use", "id": "toolu_csswitch_install", "name": tool_name, "input": {}}
    });
    let delta = json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": serde_json::to_string(input).unwrap()}
    });
    format!(
        concat!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_csswitch_install\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"csswitch-local-mock\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n",
            "event: content_block_start\ndata: {start}\n\n",
            "event: content_block_delta\ndata: {delta}\n\n",
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        ),
        start = start,
        delta = delta
    )
    .into_bytes()
}

struct ScienceGuard {
    science_bin: PathBuf,
    sandbox_home: PathBuf,
    safe_bin: PathBuf,
    data_dir: PathBuf,
    child: Option<Child>,
    gateway_child: Option<Child>,
    install_bridge: PathBuf,
    playwright: PathBuf,
    session: String,
    workdir: PathBuf,
    browser_open: bool,
    port: u16,
}

impl ScienceGuard {
    fn stop_science(&mut self) {
        if self.browser_open {
            let _ = playwright_output(self, &["close"]);
            self.browser_open = false;
        }
        let mut stop = safe_command(&self.science_bin, &self.sandbox_home, &self.safe_bin);
        stop.arg("stop").arg("--data-dir").arg(&self.data_dir);
        let _ = output_with_timeout(&mut stop, Duration::from_secs(15));
        if let Some(mut child) = self.child.take() {
            for _ in 0..50 {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn restart_science(
        &mut self,
        sandbox_port: u16,
        proxy_url: &str,
        stdout: &Path,
        stderr: &Path,
    ) {
        assert!(
            self.child.is_none(),
            "Science must be stopped before restart"
        );
        self.child = Some(spawn_science(
            &self.science_bin,
            &self.sandbox_home,
            &self.safe_bin,
            &self.data_dir,
            self.port,
            sandbox_port,
            proxy_url,
            stdout,
            stderr,
        ));
    }

    fn stop(&mut self) {
        self.stop_science();
        if let Some(mut child) = self.gateway_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.install_bridge);
    }
}

impl Drop for ScienceGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn playwright_output(guard: &ScienceGuard, args: &[&str]) -> Result<String, String> {
    let mut command = safe_command(&guard.playwright, &guard.sandbox_home, &guard.safe_bin);
    command
        .args(args)
        .current_dir(&guard.workdir)
        .env("PLAYWRIGHT_CLI_SESSION", &guard.session);
    let output = output_with_timeout(&mut command, Duration::from_secs(120))
        .map_err(|error| format!("Playwright {args:?}：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Playwright 失败 exit={:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

fn snapshot(guard: &ScienceGuard) -> Result<String, String> {
    let value: Value = serde_json::from_str(&playwright_output(guard, &["--json", "snapshot"])?)
        .map_err(|e| format!("snapshot JSON 非法：{e}"))?;
    value
        .get("snapshot")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or("snapshot 缺少内容".into())
}

fn snapshot_ref(snapshot: &str, needle: &str) -> Option<String> {
    let line = snapshot.lines().find(|line| line.contains(needle))?;
    let start = line.find("[ref=")? + 5;
    let end = line[start..].find(']')? + start;
    Some(line[start..end].to_string())
}

fn wait_control(guard: &ScienceGuard, needle: &str, attempts: usize) -> Result<String, String> {
    for _ in 0..attempts {
        let current = snapshot(guard)?;
        if snapshot_ref(&current, needle).is_some() {
            return Ok(current);
        }
        thread::sleep(Duration::from_millis(300));
    }
    Err(format!("控件未出现：{needle}"))
}

fn wait_chat_idle(guard: &ScienceGuard, attempts: usize) -> Result<String, String> {
    for _ in 0..attempts {
        let current = snapshot(guard)?;
        let idle = current.contains("textbox \"Ask anything")
            && !current.contains("button \"Stop\"")
            && !current.contains("— working")
            && !current.contains("Waiting for your approval")
            && !current.contains("Reviewing");
        if idle {
            return Ok(current);
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err("Science 对话未在有界等待内进入 idle 状态".into())
}

fn click(guard: &ScienceGuard, current: &str, needle: &str) -> Result<(), String> {
    let element = snapshot_ref(current, needle).ok_or_else(|| format!("缺少控件：{needle}"))?;
    playwright_output(guard, &["click", &element]).map(|_| ())
}

fn science_url(guard: &ScienceGuard) -> Result<String, String> {
    for _ in 0..150 {
        let mut command = safe_command(&guard.science_bin, &guard.sandbox_home, &guard.safe_bin);
        command.arg("url").arg("--data-dir").arg(&guard.data_dir);
        let output = output_with_timeout(&mut command, Duration::from_secs(5))?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(url) = stdout.split_whitespace().find(|part| {
                part.starts_with("http://localhost:") || part.starts_with("http://127.0.0.1:")
            }) {
                if url.contains(&format!(":{}", guard.port)) {
                    return Ok(url.trim_matches(['\'', '"']).to_string());
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("Science URL 未就绪".into())
}

fn attach_route_via_control(guard: &ScienceGuard, gateway: &Path) -> Result<(), String> {
    let control_url = science_url(guard)?;
    let mut command = safe_command(gateway, &guard.sandbox_home, &guard.safe_bin);
    command
        .arg("science-control")
        .arg("attach-route")
        .env("CSSWITCH_SCIENCE_CONTROL_URL", control_url);
    let output = output_with_timeout(&mut command, Duration::from_secs(10))?;
    if !output.status.success() {
        return Err(format!(
            "路由 Skill 控制面绑定失败：{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("路由 Skill 控制面响应非法：{error}"))?;
    if value.get("status").and_then(Value::as_str) != Some("ATTACHED") {
        return Err("路由 Skill 未返回 ATTACHED".into());
    }
    Ok(())
}

fn open_chat(guard: &mut ScienceGuard) -> Result<String, String> {
    guard.browser_open = true;
    let url = science_url(guard)?;
    playwright_output(guard, &["open", &url])?;
    for _ in 0..30 {
        let current = snapshot(guard)?;
        if current.contains("button \"Sign in\"") {
            click(guard, &current, "button \"Sign in\"")?;
            break;
        }
        if current.contains("button \"Open project Example project\"") {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    for _ in 0..10 {
        let current = snapshot(guard)?;
        if current.contains("button \"Keep defaults\"") {
            click(guard, &current, "button \"Keep defaults\"")?;
        } else if current.contains("button \"Close\"") {
            click(guard, &current, "button \"Close\"")?;
        } else if current.contains("button \"Open project Example project\"") {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let home = wait_control(guard, "button \"Open project Example project\"", 30)?;
    click(guard, &home, "button \"Open project Example project\"")?;
    let project = wait_control(guard, "button \"New\"", 30)?;
    click(guard, &project, "button \"New\"")?;
    wait_chat_idle(guard, 80)
}

fn send_prompt(guard: &ScienceGuard, current: &str, prompt: &str) -> Result<(), String> {
    let textbox = snapshot_ref(current, "textbox \"Ask anything").ok_or("缺少聊天输入框")?;
    let verification = prompt
        .split("https://")
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(prompt);
    playwright_output(guard, &["fill", &textbox, prompt])?;
    let mut filled = snapshot(guard)?;
    for _ in 0..10 {
        if filled.contains("button \"Send\"") {
            click(guard, &filled, "button \"Send\"")?;
        } else {
            playwright_output(guard, &["press", "Enter"])?;
        }
        thread::sleep(Duration::from_millis(500));
        let current = snapshot(guard)?;
        if current.contains(verification) {
            return Ok(());
        }
        filled = current;
    }
    Err("点击 Send 后提示未进入 Science 对话".into())
}

fn wait_observation(
    provider: &MockProvider,
    predicate: impl Fn(&Observation) -> bool,
) -> Observation {
    for _ in 0..900 {
        let observation = provider.snapshot();
        if predicate(&observation) {
            return observation;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "mock provider 未在有界等待内看到完整工具回合：{:?}",
        provider.snapshot()
    );
}

fn wait_round(
    guard: &ScienceGuard,
    provider: &MockProvider,
    predicate: impl Fn(&Observation) -> bool,
) -> Observation {
    for attempt in 0..900 {
        let observation = provider.snapshot();
        if predicate(&observation) {
            return observation;
        }
        if attempt % 20 == 0 {
            if let Ok(current) = snapshot(guard) {
                if current.contains("button \"Allow for project\"") {
                    click(guard, &current, "button \"Allow for project\"").unwrap();
                } else if current.contains("button \"Allow\"") {
                    click(guard, &current, "button \"Allow\"").unwrap();
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "mock provider 未在有界等待内看到获批后的完整工具回合：{:?}",
        provider.snapshot()
    );
}

#[allow(clippy::too_many_arguments)]
fn spawn_science(
    science_bin: &Path,
    sandbox_home: &Path,
    safe_bin: &Path,
    data_dir: &Path,
    port: u16,
    sandbox_port: u16,
    proxy_url: &str,
    stdout: &Path,
    stderr: &Path,
) -> Child {
    let mut command = safe_command(science_bin, sandbox_home, safe_bin);
    command
        .arg("serve")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg(port.to_string())
        .arg("--sandbox-port")
        .arg(sandbox_port.to_string())
        .arg("--no-browser")
        .arg("--no-auto-update")
        .env("ANTHROPIC_BASE_URL", proxy_url)
        .stdout(Stdio::from(File::create(stdout).unwrap()))
        .stderr(Stdio::from(File::create(stderr).unwrap()));
    command.spawn().unwrap()
}

fn wait_port(port: u16) {
    for _ in 0..300 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("Science 端口未就绪：{port}");
}

fn assert_installed_runtime_identity(guard: &ScienceGuard, version_output: &str) {
    let listener = Command::new("/usr/sbin/lsof")
        .args([
            "-nP",
            &format!("-iTCP:{}", guard.port),
            "-sTCP:LISTEN",
            "-t",
        ])
        .output()
        .expect("inspect Science listener");
    assert!(listener.status.success());
    let pid = String::from_utf8(listener.stdout)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert!(!pid.is_empty());

    let text_files = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &pid, "-d", "txt", "-Fn"])
        .output()
        .expect("inspect Science executable and runtime files");
    assert!(text_files.status.success());
    let text_files = String::from_utf8(text_files.stdout).unwrap();
    let science_bin = guard.science_bin.canonicalize().unwrap();
    let data_dir = guard.data_dir.canonicalize().unwrap();
    assert!(
        text_files.contains(&format!("n{}", science_bin.display())),
        "listener PID {pid} is not executing {}: {text_files}",
        science_bin.display()
    );
    let version = version_output
        .split_whitespace()
        .nth(1)
        .expect("Science --version must contain a version token");
    let runtime_root = data_dir.join("runtime");
    let matching_runtime = fs::read_dir(&runtime_root)
        .unwrap_or_else(|error| panic!("missing {}: {error}", runtime_root.display()))
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry.file_name().to_string_lossy().starts_with(version)
        });
    assert!(
        matching_runtime.is_some(),
        "listener PID {pid} did not initialize a runtime matching {version} under {}",
        runtime_root.display()
    );
    let runtime_prefix = format!("n{}/runtime/{version}", data_dir.display());
    for line in text_files
        .lines()
        .filter(|line| line.starts_with(&format!("n{}/runtime/", data_dir.display())))
    {
        assert!(
            line.starts_with(&runtime_prefix),
            "listener PID {pid} loaded a mismatched version runtime: {line}"
        );
    }
}

fn wait_log_contains(path: &Path, needle: &str) {
    for _ in 0..1_200 {
        if fs::read_to_string(path)
            .ok()
            .is_some_and(|contents| contents.contains(needle))
        {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("Science 日志未在有界等待内出现：{needle}");
}

#[test]
#[ignore = "explicit installed Science local-MCP dialogue E2E; temp HOME/data-dir, public GitHub, local mock/browser only"]
fn isolated_science_installs_attaches_and_persists_external_skill() {
    assert_eq!(
        std::env::var("CSSWITCH_REAL_SCIENCE_SKILL_INSTALL_MCP_E2E").as_deref(),
        Ok("1"),
        "必须显式设置 CSSWITCH_REAL_SCIENCE_SKILL_INSTALL_MCP_E2E=1"
    );
    let root = temp_dir("real-skill-install-mcp");
    let outer_home = root.join("home");
    let sandbox_home = outer_home.join(".csswitch/sandbox/home");
    let data_dir = sandbox_home.join(".claude-science");
    let safe_bin = prepare_safe_bin(&sandbox_home);
    let workdir = root.join("browser-workdir");
    fs::create_dir_all(&workdir).unwrap();
    let (login, _) = crate::oauth_forge::ensure_virtual_login(
        &data_dir,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    assert!(ensure_route_skill(&data_dir).unwrap());
    assert!(!ensure_route_skill(&data_dir).unwrap());

    let gateway = std::env::var_os("CSSWITCH_E2E_GATEWAY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../gateway/target/debug/csswitch-gateway")
        });
    assert!(gateway.is_file(), "缺少本轮 gateway：{}", gateway.display());
    let gateway_port = free_port();
    let gateway_secret = "csswitch-e2e-secret";
    let install_bridge =
        outer_home.join(format!("CSSwitch-Skill-Bridge-e2e-{}", std::process::id()));
    let provider = MockProvider::start(install_bridge.to_string_lossy().into_owned());
    let gateway_child = Command::new(&gateway)
        .arg("--provider")
        .arg("deepseek")
        .arg("--port")
        .arg(gateway_port.to_string())
        .env("DEEPSEEK_API_KEY", "fake-e2e-key")
        .env("CSSWITCH_AUTH_TOKEN", gateway_secret)
        .env("CSSWITCH_UPSTREAM_URL", provider.endpoint())
        .env("CSSWITCH_SKILL_DATA_DIR", &data_dir)
        .env("CSSWITCH_SKILL_BRIDGE_DIR", &install_bridge)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(root.join("gateway.stderr.log")).unwrap(),
        ))
        .spawn()
        .unwrap();
    wait_port(gateway_port);
    for _ in 0..100 {
        if install_bridge.is_dir() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(install_bridge.is_dir());
    let config = data_dir.join("mcp/local-mcp.json");
    let installer = json!({
        "name": INSTALL_SERVER_NAME,
        "command": gateway.to_string_lossy(),
        "args": ["skill-install-mcp", "--bridge-dir", install_bridge.to_string_lossy(), "--tool-mode", "install"],
        "env": {},
        "description": format!("Install public GitHub Skills locally. {MANAGED_MARKER}")
    });
    let uninstaller = json!({
        "name": UNINSTALL_SERVER_NAME,
        "command": gateway.to_string_lossy(),
        "args": ["skill-install-mcp", "--bridge-dir", install_bridge.to_string_lossy(), "--tool-mode", "uninstall"],
        "env": {},
        "description": format!("Uninstall CSSwitch imported Skills locally. {MANAGED_MARKER}")
    });
    assert!(merge_registrations(&config, vec![installer, uninstaller]).unwrap());

    let science_bin = std::env::var_os("CSSWITCH_REAL_SCIENCE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Applications/Claude Science.app/Contents/Resources/bin/claude-science")
        })
        .canonicalize()
        .expect("canonical installed Science binary");
    let playwright = std::env::var_os("CSSWITCH_PLAYWRIGHT_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Users/superjj/.codex/skills/playwright/scripts/playwright_cli.sh")
        });
    assert!(science_bin.is_file());
    assert!(playwright.is_file());
    let mut version_command = safe_command(&science_bin, &sandbox_home, &safe_bin);
    version_command.arg("--version");
    let version_output = output_with_timeout(&mut version_command, Duration::from_secs(10))
        .expect("read installed Science version");
    assert!(version_output.status.success());
    let science_version = String::from_utf8(version_output.stdout).unwrap();
    assert!(science_version.starts_with("claude-science "));

    let port = free_port();
    let sandbox_port = free_port();
    assert_ne!(port, sandbox_port);
    let science_stderr = root.join("science.stderr.log");
    let child = spawn_science(
        &science_bin,
        &sandbox_home,
        &safe_bin,
        &data_dir,
        port,
        sandbox_port,
        &format!("http://127.0.0.1:{gateway_port}/{gateway_secret}"),
        &root.join("science.stdout.log"),
        &science_stderr,
    );
    let mut guard = ScienceGuard {
        science_bin,
        sandbox_home,
        safe_bin,
        data_dir: data_dir.clone(),
        child: Some(child),
        gateway_child: Some(gateway_child),
        install_bridge,
        playwright,
        session: format!("csswitch-skill-install-{}", std::process::id()),
        workdir,
        browser_open: false,
        port,
    };
    wait_port(port);
    assert_installed_runtime_identity(&guard, &science_version);
    wait_log_contains(&science_stderr, "MCP warmup complete");
    attach_route_via_control(&guard, &gateway).unwrap();

    let chat = open_chat(&mut guard).unwrap();
    send_prompt(
        &guard,
        &chat,
        &format!("请安装这个外部 Skill： {INSTALL_URL}"),
    )
    .unwrap();
    let advertised = wait_observation(&provider, |value| value.streaming_count >= 1);
    assert!(advertised.connector_skill_discovered);
    assert!(advertised.repl_description_ok);
    assert!(!advertised
        .advertised_tool_names
        .iter()
        .any(|name| name.contains("install_external_skill")));
    let url_round = wait_round(&guard, &provider, |value| {
        value.repl_calls >= 1 && (value.skill_loaded_after_attach || value.url_failure_seen)
    });
    assert!(
        url_round.url_status_seen,
        "URL install failed: {url_round:?}"
    );
    assert!(
        url_round.attach_called,
        "attach_skill was not called: {url_round:?}"
    );
    assert!(
        url_round.skill_loaded_after_attach,
        "attached Skill did not load: {url_round:?}"
    );
    assert_eq!(url_round.tool_result_count, 1);
    assert!(url_round.invoked_tools.iter().all(|name| matches!(
        name.as_str(),
        "skill" | "repl" | "request_host_access" | "edit_file" | "read_file"
    )));
    assert!(!url_round
        .invoked_tools
        .iter()
        .any(|name| name.contains("host.skills.edit") || name.contains("host.skills.publish")));

    let installed = data_dir
        .join("orgs")
        .join(&login.org_uuid)
        .join("skills/internal-comms");
    assert!(installed.join("SKILL.md").is_file());
    let import_origin: Value = serde_json::from_slice(
        &fs::read(installed.join(".import-origin")).expect("missing import origin marker"),
    )
    .expect("invalid import origin marker");
    assert_eq!(import_origin["version"], 1);
    assert_eq!(import_origin["repo"], "anthropics/skills");
    assert_eq!(import_origin["plugin"], "internal-comms");
    assert_eq!(import_origin["marketplace"], "csswitch-local-bridge");
    assert_eq!(import_origin["path"], "skills/internal-comms");
    assert_eq!(import_origin["license"], "NOASSERTION");
    assert!(!installed.join(".catalog_stamp").exists());

    guard.stop_science();
    provider.begin_restart_validation();
    let restart_stdout = root.join("science-restart.stdout.log");
    let restart_stderr = root.join("science-restart.stderr.log");
    guard.restart_science(
        free_port(),
        &format!("http://127.0.0.1:{gateway_port}/{gateway_secret}"),
        &restart_stdout,
        &restart_stderr,
    );
    wait_port(guard.port);
    assert_installed_runtime_identity(&guard, &science_version);
    wait_log_contains(&restart_stderr, "MCP warmup complete");
    let restarted_chat = open_chat(&mut guard).unwrap();
    send_prompt(
        &guard,
        &restarted_chat,
        "请加载并使用 internal-comms Skill 做一次无副作用检查",
    )
    .unwrap();
    let restarted = wait_round(&guard, &provider, |value| value.skill_loaded_after_restart);
    assert!(
        restarted.skill_loaded_after_restart,
        "attached Skill did not persist across Science restart: {restarted:?}"
    );

    let idle = wait_chat_idle(&guard, 120).unwrap();
    send_prompt(&guard, &idle, "请卸载 internal-comms").unwrap();
    let uninstalled = wait_round(&guard, &provider, |value| {
        value.skill_absent_after_detach || value.uninstall_status_seen
    });
    assert!(
        uninstalled.route_skill_discovered,
        "OPERON did not advertise the CSSwitch route Skill: {uninstalled:?}"
    );
    assert!(
        uninstalled.route_skill_loaded,
        "the CSSwitch route Skill was not loaded: {uninstalled:?}"
    );
    assert!(
        uninstalled.uninstaller_skill_loaded,
        "the scoped uninstaller connector Skill was not loaded: {uninstalled:?}"
    );
    assert_eq!(uninstalled.uninstall_repl_calls, 1);
    assert!(uninstalled.uninstall_status_seen);
    assert!(uninstalled.detach_called);
    let final_uninstall = wait_round(&guard, &provider, |value| value.skill_absent_after_detach);
    assert!(final_uninstall.skill_absent_after_detach);
    assert!(!installed.exists());
    let trash = data_dir
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("skill-trash");
    assert!(fs::read_dir(&trash).unwrap().any(|entry| {
        entry.ok().is_some_and(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("internal-comms-")
        })
    }));
    assert!(!final_uninstall
        .invoked_tools
        .iter()
        .any(|name| name.contains("host.skills.delete") || name == "bash"));

    guard.stop();
    drop(provider);
    let stderr = fs::read_to_string(science_stderr).unwrap_or_default();
    assert!(!stderr.contains("ANTHROPIC_API_KEY"));
    assert!(!stderr.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    let _ = fs::remove_dir_all(root);
}
