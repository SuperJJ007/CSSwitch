use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{active_org, ScienceExecutableFingerprint, ScienceHostContext, AGENT_NAME};

#[cfg(not(test))]
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const PROCESS_TIMEOUT: Duration = Duration::from_secs(2);
const PROCESS_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub uncertain: bool,
}

impl AttachError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            retryable: false,
            uncertain: false,
        }
    }

    fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }

    fn uncertain(mut self, value: bool) -> Self {
        self.uncertain = value;
        self
    }
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AttachError {}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttachResult {
    AlreadyAttached,
    Attached,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchSkillUpdate {
    pub attached: Vec<String>,
    pub detached: Vec<String>,
    pub missing_attach: Vec<String>,
    pub remaining_detach: Vec<String>,
    pub changed: bool,
}

pub fn update_agent_skills(
    context: &ScienceHostContext,
    attach: &[String],
    detach: &[String],
    expected_active_org: &str,
) -> Result<BatchSkillUpdate, AttachError> {
    validate_context(context)?;
    require_active_org(context, expected_active_org, false)?;
    let attach = attach.iter().cloned().collect::<BTreeSet<_>>();
    let detach = detach.iter().cloned().collect::<BTreeSet<_>>();
    if attach.iter().any(|name| detach.contains(name)) {
        return Err(AttachError::new(
            "SCIENCE_ATTACH_FAILED",
            "同一批次不能同时 attach 和 detach 同名 Skill",
        ));
    }
    let control_url = fresh_control_url(context)?;
    let (origin, nonce) = validate_control_url(&control_url, context.sandbox_port)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(8))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| {
            AttachError::new(
                "SCIENCE_CONTROL_FAILED",
                "初始化 Science batch control 客户端失败",
            )
        })?;
    let session = authenticate(&client, &origin, &nonce)?;
    let before = agent_skills(&client, &origin, &session.auth_cookie)?;
    let requested_attach = attach.difference(&before).cloned().collect::<Vec<_>>();
    let requested_detach = detach.intersection(&before).cloned().collect::<Vec<_>>();
    let changed = !requested_attach.is_empty() || !requested_detach.is_empty();
    if changed {
        require_active_org(context, expected_active_org, false)?;
        let body = serde_json::to_vec(&json!({
            "attach": requested_attach,
            "detach": requested_detach,
        }))
        .map_err(|_| {
            AttachError::new("SCIENCE_ATTACH_FAILED", "编码 Science batch Skill 请求失败")
        })?;
        let request = client
            .put(format!("{origin}/api/agents/{AGENT_NAME}/skills"))
            .header(ORIGIN, &origin)
            .header(
                COOKIE,
                format!(
                    "operon_auth={}; operon_csrf={}",
                    session.auth_cookie, session.csrf_cookie
                ),
            )
            .header("x-operon-csrf", &session.csrf_cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send();
        if request
            .as_ref()
            .is_ok_and(|response| !response.status().is_success())
        {
            // Always read back below. Science may have applied the mutation before
            // returning an error response.
        }
    }
    require_active_org(context, expected_active_org, changed)?;
    let after = agent_skills(&client, &origin, &session.auth_cookie)
        .map_err(|_| attach_state_uncertain("Science batch Skill 请求后无法回读 OPERON 状态"))?;
    let missing_attach = attach.difference(&after).cloned().collect::<Vec<_>>();
    let remaining_detach = detach.intersection(&after).cloned().collect::<Vec<_>>();
    let attached = attach.intersection(&after).cloned().collect::<Vec<_>>();
    let detached = detach.difference(&after).cloned().collect::<Vec<_>>();
    if !missing_attach.is_empty() || !remaining_detach.is_empty() {
        return Err(attach_state_uncertain(&format!(
            "Science batch Skill 状态不完整：缺少绑定 {} 个，仍绑定 {} 个",
            missing_attach.len(),
            remaining_detach.len()
        )));
    }
    Ok(BatchSkillUpdate {
        attached,
        detached,
        missing_attach,
        remaining_detach,
        changed,
    })
}

pub fn attach_skill(
    context: &ScienceHostContext,
    skill_name: &str,
    expected_active_org: &str,
) -> Result<AttachResult, AttachError> {
    validate_context(context)?;
    require_active_org(context, expected_active_org, false)?;
    let control_url = fresh_control_url(context)?;
    let (origin, nonce) = validate_control_url(&control_url, context.sandbox_port)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| {
            AttachError::new(
                "SCIENCE_CONTROL_FAILED",
                "初始化 Science control 客户端失败",
            )
        })?;
    let session = authenticate(&client, &origin, &nonce)?;
    if agent_has_skill(&client, &origin, &session.auth_cookie, skill_name)? {
        require_active_org(context, expected_active_org, false)?;
        return Ok(AttachResult::AlreadyAttached);
    }
    require_active_org(context, expected_active_org, false)?;
    let body = serde_json::to_vec(&json!({"skill_name": skill_name}))
        .map_err(|_| AttachError::new("SCIENCE_ATTACH_FAILED", "编码 Skill attach 请求失败"))?;
    let attach = client
        .post(format!("{origin}/api/agents/{AGENT_NAME}/skills"))
        .header(ORIGIN, &origin)
        .header(
            COOKIE,
            format!(
                "operon_auth={}; operon_csrf={}",
                session.auth_cookie, session.csrf_cookie
            ),
        )
        .header("x-operon-csrf", &session.csrf_cookie)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send();
    match attach {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            require_active_org(context, expected_active_org, true)?;
            match agent_has_skill(&client, &origin, &session.auth_cookie, skill_name) {
                Ok(true) => return Ok(AttachResult::Attached),
                Ok(false) => {}
                Err(_) => {
                    return Err(attach_state_uncertain(
                        "Science attach 返回失败状态，且回读绑定状态失败",
                    ))
                }
            }
            return Err(AttachError::new(
                "SCIENCE_ATTACH_FAILED",
                format!("Science attach 返回 HTTP {}", response.status()),
            ));
        }
        Err(_) => {
            if let Ok(true) = agent_has_skill(&client, &origin, &session.auth_cookie, skill_name) {
                require_active_org(context, expected_active_org, true)?;
                return Ok(AttachResult::Attached);
            }
            return Err(attach_state_uncertain("Science attach 请求结果无法确认"));
        }
    }
    require_active_org(context, expected_active_org, true)?;
    match agent_has_skill(&client, &origin, &session.auth_cookie, skill_name) {
        Ok(true) => {}
        Ok(false) => return Err(attach_state_uncertain("Science 未回读到 OPERON Skill 绑定")),
        Err(_) => return Err(attach_state_uncertain("Science attach 后回读绑定状态失败")),
    }
    Ok(AttachResult::Attached)
}

fn attach_state_uncertain(message: &str) -> AttachError {
    AttachError::new("ATTACH_STATE_UNCERTAIN", message)
        .retryable(true)
        .uncertain(true)
}

pub fn verify_attach_control_ready(context: &ScienceHostContext) -> Result<(), AttachError> {
    validate_context(context)?;
    let expected_org = active_org(&context.data_dir).map_err(|_| {
        AttachError::new("SCIENCE_NOT_READY", "无法确认 Science active org").retryable(true)
    })?;
    require_active_org(context, &expected_org, false)?;
    let control_url = fresh_control_url(context)?;
    let (origin, nonce) = validate_control_url(&control_url, context.sandbox_port)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| {
            AttachError::new(
                "SCIENCE_NOT_READY",
                "无法初始化 Science attach control 客户端",
            )
        })?;
    let session = authenticate(&client, &origin, &nonce)?;
    let _ = agent_has_skill(
        &client,
        &origin,
        &session.auth_cookie,
        "__csswitch_preflight__",
    )?;
    require_active_org(context, &expected_org, false)
}

/// Read the complete OPERON Skill attachment snapshot without changing it.
///
/// The same runtime identity, loopback URL, nonce authentication, response
/// bounds and active-org checks used by attach are retained here. Callers must
/// discard the whole result on any error; a failed readback is not evidence
/// that every Skill is detached.
pub fn read_agent_skill_names(
    context: &ScienceHostContext,
    expected_active_org: &str,
) -> Result<BTreeSet<String>, AttachError> {
    validate_context(context)?;
    require_active_org(context, expected_active_org, false)?;
    let control_url = fresh_control_url(context)?;
    let (origin, nonce) = validate_control_url(&control_url, context.sandbox_port)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| {
            AttachError::new(
                "SCIENCE_CONTROL_FAILED",
                "初始化 Science Skill 状态客户端失败",
            )
        })?;
    let session = authenticate(&client, &origin, &nonce)?;
    let names = agent_skills_required(&client, &origin, &session.auth_cookie)?;
    require_active_org(context, expected_active_org, false)?;
    Ok(names)
}

fn validate_context(context: &ScienceHostContext) -> Result<(), AttachError> {
    if !context.binary.is_absolute()
        || !context.home.is_absolute()
        || !context.data_dir.is_absolute()
        || context.data_dir != context.home.join(".claude-science")
        || context.sandbox_port == 0
        || context.version.trim().is_empty()
    {
        return Err(
            AttachError::new("SCIENCE_NOT_READY", "Science host context 不完整").retryable(true),
        );
    }
    let canonical = context.binary.canonicalize().map_err(|_| {
        AttachError::new("SCIENCE_RUNTIME_CHANGED", "Science binary 不可用").retryable(true)
    })?;
    if canonical != context.binary {
        return Err(AttachError::new(
            "SCIENCE_RUNTIME_CHANGED",
            "Science binary canonical path 已变化",
        )
        .retryable(true));
    }
    let actual = executable_fingerprint(&context.binary).ok_or_else(|| {
        AttachError::new("SCIENCE_RUNTIME_CHANGED", "Science binary 身份不可验证").retryable(true)
    })?;
    if actual != context.fingerprint {
        return Err(AttachError::new(
            "SCIENCE_RUNTIME_CHANGED",
            "Science binary 在 Gateway 启动后发生变化",
        )
        .retryable(true));
    }
    Ok(())
}

fn executable_fingerprint(path: &Path) -> Option<ScienceExecutableFingerprint> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
        Some(ScienceExecutableFingerprint {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            mode: metadata.mode(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn require_active_org(
    context: &ScienceHostContext,
    expected: &str,
    after_request: bool,
) -> Result<(), AttachError> {
    let current = active_org(&context.data_dir).map_err(|_| {
        AttachError::new("SCIENCE_NOT_READY", "无法确认 Science active org").retryable(true)
    })?;
    if current != expected {
        let error = AttachError::new(
            if after_request {
                "ATTACH_STATE_UNCERTAIN"
            } else {
                "ACTIVE_ORG_CHANGED"
            },
            "Science active org 在安装期间发生变化",
        )
        .retryable(true);
        return Err(if after_request {
            error.uncertain(true)
        } else {
            error
        });
    }
    Ok(())
}

fn fresh_control_url(context: &ScienceHostContext) -> Result<String, AttachError> {
    let temp = context.home.join(".csswitch-skill-tmp");
    match fs::symlink_metadata(&temp) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AttachError::new(
                "SCIENCE_CONTROL_FAILED",
                "受控 Science 临时路径不是安全目录",
            ));
        }
        Ok(_) => {}
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&temp).map_err(|_| {
                AttachError::new("SCIENCE_CONTROL_FAILED", "创建受控 Science 临时目录失败")
            })?;
        }
        Err(_) => {
            return Err(AttachError::new(
                "SCIENCE_CONTROL_FAILED",
                "检查受控 Science 临时目录失败",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let directory = options.open(&temp).map_err(|_| {
            AttachError::new("SCIENCE_CONTROL_FAILED", "受控 Science 临时目录校验失败")
        })?;
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|_| {
                AttachError::new("SCIENCE_CONTROL_FAILED", "收紧 Science 临时目录权限失败")
            })?;
    }
    let mut command = Command::new(&context.binary);
    command
        .arg("url")
        .arg("--data-dir")
        .arg(&context.data_dir)
        .env_clear()
        .env("HOME", &context.home)
        .env("TMPDIR", &temp)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let output = output_with_timeout(&mut command)?;
    if !output.success {
        return Err(
            AttachError::new("SCIENCE_CONTROL_FAILED", "claude-science url 非零退出")
                .retryable(true),
        );
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        AttachError::new(
            "SCIENCE_CONTROL_FAILED",
            "claude-science url 输出不是 UTF-8",
        )
    })?;
    let urls = stdout
        .split_whitespace()
        .filter(|value| {
            reqwest::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    if urls.len() != 1 {
        return Err(AttachError::new(
            "SCIENCE_CONTROL_FAILED",
            "claude-science url 必须只返回一个本机 HTTP URL",
        ));
    }
    validate_control_url(&urls[0], context.sandbox_port)?;
    Ok(urls[0].clone())
}

struct ProcessOutput {
    success: bool,
    stdout: Vec<u8>,
    #[allow(dead_code)]
    stderr: Vec<u8>,
}

fn output_with_timeout(command: &mut Command) -> Result<ProcessOutput, AttachError> {
    let mut child = command.spawn().map_err(|_| {
        AttachError::new("SCIENCE_CONTROL_FAILED", "无法启动 claude-science url").retryable(true)
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AttachError::new("SCIENCE_CONTROL_FAILED", "无法读取 Science stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AttachError::new("SCIENCE_CONTROL_FAILED", "无法读取 Science stderr"))?;
    let stdout_reader = thread::spawn(move || read_capped(stdout));
    let stderr_reader = thread::spawn(move || read_capped(stderr));
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                #[cfg(unix)]
                unsafe {
                    // The CLI is isolated in its own process group. Kill any descendant
                    // that inherited stdout/stderr before joining the reader threads.
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                break status;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(AttachError::new(
                    "SCIENCE_CONTROL_TIMEOUT",
                    "claude-science url 超过 10 秒",
                )
                .retryable(true));
            }
            Err(_) => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(AttachError::new(
                    "SCIENCE_CONTROL_FAILED",
                    "无法确认 claude-science url 进程状态",
                ));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| AttachError::new("SCIENCE_CONTROL_FAILED", "读取 Science stdout 失败"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| AttachError::new("SCIENCE_CONTROL_FAILED", "读取 Science stderr 失败"))??;
    Ok(ProcessOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn read_limited(
    mut reader: impl Read,
    read_error: &'static str,
    limit_error: &'static str,
) -> Result<Vec<u8>, AttachError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((PROCESS_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AttachError::new("SCIENCE_CONTROL_FAILED", read_error))?;
    if bytes.len() > PROCESS_OUTPUT_LIMIT {
        return Err(AttachError::new(
            "SCIENCE_CONTROL_OUTPUT_LIMIT",
            limit_error,
        ));
    }
    Ok(bytes)
}

fn read_capped(reader: impl Read) -> Result<Vec<u8>, AttachError> {
    read_limited(
        reader,
        "读取 Science 输出失败",
        "claude-science url 输出超过 64 KiB",
    )
}

fn read_response_capped(response: Response) -> Result<Vec<u8>, AttachError> {
    read_limited(
        response,
        "读取 OPERON 响应失败",
        "OPERON 状态响应超过 64 KiB",
    )
}

fn validate_control_url(raw: &str, expected_port: u16) -> Result<(String, String), AttachError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|_| AttachError::new("SCIENCE_CONTROL_FAILED", "Science control URL 非法"))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.port() != Some(expected_port)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AttachError::new(
            "SCIENCE_CONTROL_FAILED",
            "Science control URL 不是预期的本机端口",
        ));
    }
    let nonces = url
        .query_pairs()
        .filter(|(name, _)| name == "nonce")
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    if nonces.len() != 1
        || nonces[0].is_empty()
        || nonces[0].len() > 512
        || !nonces[0]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
    {
        return Err(AttachError::new(
            "SCIENCE_CONTROL_FAILED",
            "Science control URL 缺少唯一合法 nonce",
        ));
    }
    Ok((
        format!(
            "http://{}:{}",
            url.host_str().expect("validated host"),
            expected_port
        ),
        nonces[0].clone(),
    ))
}

struct ControlSession {
    auth_cookie: String,
    csrf_cookie: String,
}

fn authenticate(client: &Client, origin: &str, nonce: &str) -> Result<ControlSession, AttachError> {
    // Current Science consumes the nonce from the browser URL itself. Older
    // releases exposed POST /api/auth/nonce instead, so retain that endpoint
    // as an explicit compatibility path only when the browser exchange does
    // not yield an authenticated session.
    let browser_auth = client
        .get(format!("{origin}/?nonce={nonce}"))
        .header(ORIGIN, origin)
        .header(ACCEPT, "text/html")
        .send();
    let browser_cookies = browser_auth.as_ref().ok().and_then(|response| {
        (response.status().is_success() || response.status().is_redirection()).then(|| {
            (
                response_cookie(response, "operon_auth"),
                response_cookie(response, "operon_csrf"),
            )
        })
    });
    let (auth_cookie, mut csrf_cookie) = match browser_cookies {
        Some((Some(auth_cookie), csrf_cookie)) => (auth_cookie, csrf_cookie),
        _ => {
            let auth = client
                .post(format!("{origin}/api/auth/nonce"))
                .header(ORIGIN, origin)
                .form(&[("nonce", nonce), ("dest", "/")])
                .send()
                .map_err(|_| {
                    AttachError::new("SCIENCE_CONTROL_FAILED", "Science nonce 认证失败")
                        .retryable(true)
                })?;
            ensure_success(&auth, "legacy nonce 认证")?;
            let auth_cookie = response_cookie(&auth, "operon_auth").ok_or_else(|| {
                AttachError::new("SCIENCE_CONTROL_FAILED", "Science nonce 未返回会话 cookie")
            })?;
            (auth_cookie, response_cookie(&auth, "operon_csrf"))
        }
    };
    let csrf = client
        .get(format!("{origin}/api/csrf"))
        .header(ORIGIN, origin)
        .header(COOKIE, format!("operon_auth={auth_cookie}"))
        .send()
        .map_err(|_| AttachError::new("SCIENCE_CONTROL_FAILED", "Science CSRF 初始化失败"))?;
    ensure_success(&csrf, "CSRF 初始化")?;
    csrf_cookie = response_cookie(&csrf, "operon_csrf").or(csrf_cookie);
    let csrf_cookie = csrf_cookie
        .ok_or_else(|| AttachError::new("SCIENCE_CONTROL_FAILED", "Science CSRF 未返回 cookie"))?;
    Ok(ControlSession {
        auth_cookie,
        csrf_cookie,
    })
}

fn agent_has_skill(
    client: &Client,
    origin: &str,
    auth_cookie: &str,
    skill_name: &str,
) -> Result<bool, AttachError> {
    Ok(agent_skills(client, origin, auth_cookie)?.contains(skill_name))
}

fn agent_skills(
    client: &Client,
    origin: &str,
    auth_cookie: &str,
) -> Result<BTreeSet<String>, AttachError> {
    agent_skills_with_policy(client, origin, auth_cookie, false)
}

fn agent_skills_required(
    client: &Client,
    origin: &str,
    auth_cookie: &str,
) -> Result<BTreeSet<String>, AttachError> {
    agent_skills_with_policy(client, origin, auth_cookie, true)
}

fn agent_skills_with_policy(
    client: &Client,
    origin: &str,
    auth_cookie: &str,
    require_operon: bool,
) -> Result<BTreeSet<String>, AttachError> {
    let response = client
        .get(format!(
            "{origin}/api/agents?names={AGENT_NAME}&include_metadata=true"
        ))
        .header(ORIGIN, origin)
        .header(COOKIE, format!("operon_auth={auth_cookie}"))
        .send()
        .map_err(|_| {
            AttachError::new("SCIENCE_CONTROL_FAILED", "读取 OPERON Skill 状态失败").retryable(true)
        })?;
    ensure_success(&response, "OPERON Skill 回读")?;
    let bytes = read_response_capped(response)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AttachError::new("SCIENCE_CONTROL_FAILED", "OPERON 状态响应非法"))?;
    let agents = value
        .as_array()
        .or_else(|| value.get("agents").and_then(Value::as_array))
        .ok_or_else(|| AttachError::new("SCIENCE_CONTROL_FAILED", "OPERON 状态缺少列表"))?;
    let agent = agents.iter().find(|agent| {
        agent
            .get("name")
            .or_else(|| agent.get("id"))
            .and_then(Value::as_str)
            == Some(AGENT_NAME)
    });
    let Some(agent) = agent else {
        return if require_operon {
            Err(AttachError::new(
                "SCIENCE_CONTROL_FAILED",
                "OPERON 状态响应缺少目标 Agent",
            ))
        } else {
            Ok(BTreeSet::new())
        };
    };
    let skills = agent
        .get("skill_names")
        .or_else(|| agent.get("skillNames"))
        .and_then(Value::as_array)
        .ok_or_else(|| AttachError::new("SCIENCE_CONTROL_FAILED", "OPERON 状态缺少 skill_names"))?;
    let mut names = BTreeSet::new();
    for skill in skills {
        let name = skill.as_str().ok_or_else(|| {
            AttachError::new("SCIENCE_CONTROL_FAILED", "OPERON skill_names 包含非法成员")
        })?;
        names.insert(name.to_string());
    }
    Ok(names)
}

fn response_cookie(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .filter_map(|header| header.split(';').next())
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(cookie_name, value)| {
            (cookie_name.trim() == name
                && !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() && byte != b';'))
            .then(|| value.to_string())
        })
}

fn ensure_success(response: &Response, stage: &str) -> Result<(), AttachError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(AttachError::new(
            "SCIENCE_CONTROL_FAILED",
            format!("Science {stage}返回 HTTP {}", response.status()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_root(label: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::path::PathBuf::from("/private/tmp").join(format!(
            "csswitch-science-control-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn context_with_script(label: &str, body: &str, port: u16) -> ScienceHostContext {
        let root = test_root(label);
        let binary = root.join("claude-science");
        fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("home");
        let data_dir = home.join(".claude-science");
        fs::create_dir_all(&data_dir).unwrap();
        ScienceHostContext {
            fingerprint: executable_fingerprint(&binary).unwrap(),
            binary,
            version: "test-version".into(),
            home,
            data_dir,
            sandbox_port: port,
        }
    }

    fn bind_loopback() -> Option<TcpListener> {
        match TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => Some(listener),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
            Err(error) => panic!("bind loopback mock: {error}"),
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while bytes.len() - header_end < length {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8(bytes[..header_end + length].to_vec()).unwrap()
    }

    fn reply(stream: &mut TcpStream, cookie: Option<&str>, body: &str) {
        let cookies = cookie.into_iter().collect::<Vec<_>>();
        reply_with_cookies(stream, &cookies, body);
    }

    fn reply_with_cookies(stream: &mut TcpStream, cookies: &[&str], body: &str) {
        let cookies = cookies
            .iter()
            .map(|value| format!("Set-Cookie: {value}; Path=/; SameSite=Strict\r\n"))
            .collect::<String>();
        let response = format!(
            "HTTP/1.1 200 OK\r\n{cookies}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    fn attach_context(label: &str, port: u16) -> ScienceHostContext {
        let context = context_with_script(
            label,
            &format!("printf '%s\\n' 'http://127.0.0.1:{port}/?nonce=fresh-nonce'"),
            port,
        );
        fs::write(
            context.data_dir.join("active-org.json"),
            br#"{"org_uuid":"org-test"}"#,
        )
        .unwrap();
        context
    }

    #[test]
    fn strict_control_url_requires_expected_port_and_one_nonce() {
        assert!(validate_control_url("http://127.0.0.1:8990/?nonce=x", 8990).is_ok());
        assert!(validate_control_url("http://127.0.0.1:8991/?nonce=x", 8990).is_err());
        assert!(validate_control_url("http://127.0.0.1:8990/?nonce=x&nonce=y", 8990).is_err());
        assert!(validate_control_url("https://127.0.0.1:8990/?nonce=x", 8990).is_err());
    }

    #[test]
    fn fresh_url_scrubs_credentials_and_accepts_one_strict_url() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("GITHUB_TOKEN", "must-not-leak");
        std::env::set_var("ANTHROPIC_API_KEY", "must-not-leak");
        std::env::set_var("DEEPSEEK_API_KEY", "must-not-leak");
        let context = context_with_script(
            "env-clear",
            r#"if [ -n "$GITHUB_TOKEN$ANTHROPIC_API_KEY$DEEPSEEK_API_KEY" ]; then exit 91; fi
printf '%s\n' 'http://127.0.0.1:18990/?nonce=fresh-one'"#,
            18_990,
        );
        assert_eq!(
            fresh_control_url(&context).unwrap(),
            "http://127.0.0.1:18990/?nonce=fresh-one"
        );
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("DEEPSEEK_API_KEY");
        fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
    }

    #[test]
    fn fresh_url_rejects_symlink_temp_without_changing_target_permissions() {
        let context = context_with_script(
            "symlink-temp",
            "printf '%s\n' 'http://127.0.0.1:18998/?nonce=one'",
            18_998,
        );
        let target = context.home.join("permission-target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, context.home.join(".csswitch-skill-tmp")).unwrap();

        assert_eq!(
            fresh_control_url(&context).unwrap_err().code,
            "SCIENCE_CONTROL_FAILED"
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
    }

    #[test]
    fn limited_reader_rejects_before_buffering_past_the_contract() {
        let oversized = std::io::Cursor::new(vec![b'x'; PROCESS_OUTPUT_LIMIT + 2]);
        let error = read_limited(oversized, "read failed", "too large").unwrap_err();
        assert_eq!(error.code, "SCIENCE_CONTROL_OUTPUT_LIMIT");
        assert_eq!(error.message, "too large");
    }

    #[test]
    fn fresh_url_rejects_nonzero_multiple_urls_wrong_port_and_changed_binary() {
        let _guard = ENV_LOCK.lock().unwrap();
        let nonzero = context_with_script("nonzero", "exit 7", 18_991);
        assert_eq!(
            fresh_control_url(&nonzero).unwrap_err().code,
            "SCIENCE_CONTROL_FAILED"
        );

        let multiple = context_with_script(
            "multiple",
            "printf '%s %s\\n' 'http://127.0.0.1:18992/?nonce=one' 'https://127.0.0.1:18992/?nonce=two'",
            18_992,
        );
        assert_eq!(
            fresh_control_url(&multiple).unwrap_err().code,
            "SCIENCE_CONTROL_FAILED"
        );

        let wrong_port = context_with_script(
            "wrong-port",
            "printf '%s\\n' 'http://127.0.0.1:29999/?nonce=one'",
            18_993,
        );
        assert_eq!(
            fresh_control_url(&wrong_port).unwrap_err().code,
            "SCIENCE_CONTROL_FAILED"
        );

        let changed = context_with_script(
            "changed",
            "printf '%s\\n' 'http://127.0.0.1:18994/?nonce=one'",
            18_994,
        );
        fs::write(&changed.binary, "#!/bin/sh\nexit 0\n# changed\n").unwrap();
        fs::set_permissions(&changed.binary, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_context(&changed).unwrap_err().code,
            "SCIENCE_RUNTIME_CHANGED"
        );

        for context in [nonzero, multiple, wrong_port, changed] {
            fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn fresh_url_kills_timeout_and_rejects_oversized_output() {
        let _guard = ENV_LOCK.lock().unwrap();
        let timeout = context_with_script(
            "timeout",
            "sleep 5\nprintf '%s\\n' 'http://127.0.0.1:18995/?nonce=late'",
            18_995,
        );
        assert_eq!(
            fresh_control_url(&timeout).unwrap_err().code,
            "SCIENCE_CONTROL_TIMEOUT"
        );

        let oversized = context_with_script("oversized", "exec /usr/bin/yes x", 18_996);
        assert_eq!(
            fresh_control_url(&oversized).unwrap_err().code,
            "SCIENCE_CONTROL_OUTPUT_LIMIT"
        );

        let orphan = context_with_script(
            "orphan-output",
            "sleep 5 &\nprintf '%s\\n' 'http://127.0.0.1:18997/?nonce=one'",
            18_997,
        );
        let started = Instant::now();
        assert!(fresh_control_url(&orphan).is_ok());
        assert!(started.elapsed() < PROCESS_TIMEOUT);

        for context in [timeout, oversized, orphan] {
            fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn nonce_csrf_attach_and_readback_use_operon_control_plane() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let context = attach_context("attach-success", port);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            let replies = [
                (Some("operon_auth=auth-token"), "{}"),
                (Some("operon_csrf=csrf-token"), "{}"),
                (None, r#"[{"name":"OPERON","skill_names":[]}]"#),
                (None, "{}"),
                (None, r#"[{"name":"OPERON","skill_names":["demo"]}]"#),
            ];
            for (cookie, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                captured.lock().unwrap().push(read_request(&mut stream));
                reply(&mut stream, cookie, body);
            }
        });
        assert_eq!(
            attach_skill(&context, "demo", "org-test").unwrap(),
            AttachResult::Attached
        );
        worker.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests[0].starts_with("GET /?nonce=fresh-nonce "));
        assert!(requests[1].starts_with("GET /api/csrf "));
        assert!(requests[2].starts_with("GET /api/agents?names=OPERON&include_metadata=true "));
        assert!(requests[3].starts_with("POST /api/agents/OPERON/skills "));
        assert!(requests[3]
            .to_ascii_lowercase()
            .contains("x-operon-csrf: csrf-token"));
        assert!(requests[3].contains(r#"{"skill_name":"demo"}"#));
        assert!(requests[4].starts_with("GET /api/agents?names=OPERON&include_metadata=true "));
        fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
    }

    #[test]
    fn browser_nonce_may_supply_auth_and_csrf_cookies_together() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut auth, _) = listener.accept().unwrap();
            assert!(read_request(&mut auth).starts_with("GET /?nonce=fresh-nonce "));
            reply_with_cookies(
                &mut auth,
                &["operon_auth=auth-token", "operon_csrf=csrf-token"],
                "{}",
            );
            let (mut csrf, _) = listener.accept().unwrap();
            assert!(read_request(&mut csrf).starts_with("GET /api/csrf "));
            reply(&mut csrf, None, "{}");
        });
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .unwrap();
        let session =
            authenticate(&client, &format!("http://127.0.0.1:{port}"), "fresh-nonce").unwrap();
        assert_eq!(session.auth_cookie, "auth-token");
        assert_eq!(session.csrf_cookie, "csrf-token");
        worker.join().unwrap();
    }

    #[test]
    fn legacy_nonce_endpoint_remains_an_explicit_compatibility_path() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            let replies = [
                (None, "{}"),
                (Some("operon_auth=auth-token"), "{}"),
                (Some("operon_csrf=csrf-token"), "{}"),
            ];
            for (cookie, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                captured.lock().unwrap().push(read_request(&mut stream));
                reply(&mut stream, cookie, body);
            }
        });
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .unwrap();
        let session =
            authenticate(&client, &format!("http://127.0.0.1:{port}"), "fresh-nonce").unwrap();
        assert_eq!(session.auth_cookie, "auth-token");
        assert_eq!(session.csrf_cookie, "csrf-token");
        worker.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests[0].starts_with("GET /?nonce=fresh-nonce "));
        assert!(requests[1].starts_with("POST /api/auth/nonce "));
        assert!(requests[1].contains("nonce=fresh-nonce"));
        assert!(requests[2].starts_with("GET /api/csrf "));
    }

    #[test]
    fn strict_list_readback_rejects_missing_operon_and_malformed_skills() {
        for body in [
            r#"[{"name":"OTHER","skill_names":[]}]"#,
            r#"[{"name":"OPERON","skill_names":"demo"}]"#,
            r#"[{"name":"OPERON","skill_names":["demo",42]}]"#,
        ] {
            let Some(listener) = bind_loopback() else {
                return;
            };
            let port = listener.local_addr().unwrap().port();
            let worker = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&mut stream);
                reply(&mut stream, None, body);
            });
            let client = Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .redirect(Policy::none())
                .no_proxy()
                .build()
                .unwrap();
            let error =
                agent_skills_required(&client, &format!("http://127.0.0.1:{port}"), "auth-token")
                    .unwrap_err();
            assert_eq!(error.code, "SCIENCE_CONTROL_FAILED");
            worker.join().unwrap();
        }
    }

    #[test]
    fn attach_post_transport_failure_is_confirmed_by_readback() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let context = attach_context("attach-readback", port);
        let worker = thread::spawn(move || {
            let replies = [
                (Some("operon_auth=auth-token"), "{}"),
                (Some("operon_csrf=csrf-token"), "{}"),
                (None, r#"[{"name":"OPERON","skill_names":[]}]"#),
            ];
            for (cookie, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&mut stream);
                reply(&mut stream, cookie, body);
            }
            let (mut failed_post, _) = listener.accept().unwrap();
            let request = read_request(&mut failed_post);
            assert!(request.starts_with("POST /api/agents/OPERON/skills "));
            drop(failed_post);
            let (mut readback, _) = listener.accept().unwrap();
            let request = read_request(&mut readback);
            assert!(request.starts_with("GET /api/agents?names=OPERON"));
            reply(
                &mut readback,
                None,
                r#"[{"name":"OPERON","skill_names":["demo"]}]"#,
            );
        });
        assert_eq!(
            attach_skill(&context, "demo", "org-test").unwrap(),
            AttachResult::Attached
        );
        worker.join().unwrap();
        fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
    }

    #[test]
    fn batch_update_uses_one_put_and_confirms_attach_and_detach_together() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let context = attach_context("batch-update", port);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            let replies = [
                (Some("operon_auth=auth-token"), "{}"),
                (Some("operon_csrf=csrf-token"), "{}"),
                (None, r#"[{"name":"OPERON","skill_names":["old"]}]"#),
                (None, "{}"),
                (
                    None,
                    r#"[{"name":"OPERON","skill_names":["alpha","beta"]}]"#,
                ),
            ];
            for (cookie, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                captured.lock().unwrap().push(read_request(&mut stream));
                reply(&mut stream, cookie, body);
            }
        });
        let result = update_agent_skills(
            &context,
            &["beta".to_string(), "alpha".to_string()],
            &["old".to_string()],
            "org-test",
        )
        .unwrap();
        assert_eq!(result.attached, vec!["alpha", "beta"]);
        assert_eq!(result.detached, vec!["old"]);
        assert!(result.changed);
        worker.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests[3].starts_with("PUT /api/agents/OPERON/skills "));
        assert!(requests[3].contains(r#"{"attach":["alpha","beta"],"detach":["old"]}"#));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("PUT /api/agents/OPERON/skills "))
                .count(),
            1
        );
        fs::remove_dir_all(context.home.parent().unwrap()).unwrap();
    }
}
