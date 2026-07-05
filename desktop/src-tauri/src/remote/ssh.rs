//! SSH 连接与远程 Helper 命令执行。
//!
//! 通过命令行 `ssh` 与远程服务器通信，执行 `csswitch-helper` 的 JSON 命令。
//! 支持 KeyFile（私钥文件）和 SshAgent（ssh-agent）两种认证方式。
//! MVP 阶段不支持密码认证。
//!
//! 设计参考 cc-switch-remote 的 `remote/ssh.rs`，按 CSSwitch 实际需求简化：
//! - 一次 SSH 调用执行一个命令（无持久会话模式，CSSwitch 操作频率低）
//! - 超时 + 重试（指数退避：2s/4s/8s）
//! - 解析 helper 的 JSON 响应

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(feature = "desktop")]
use super::askpass::{self, AskpassBroker};
use super::auth::{AuthRuntimeOptions, SshAuthPlan};
use super::types::{RemoteAuthMethod, RemoteError, RemoteHostProfile};

/// Windows: 禁止弹出命令行窗口（CREATE_NO_WINDOW）
#[cfg(windows)]
const NO_WINDOW: u32 = 0x08000000;

/// 创建隐藏窗口的 Command（Windows 上不弹 cmd 窗口）
fn hide_cmd(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(NO_WINDOW);
    }
    cmd
}

/// SSH 超时秒数（ConnectTimeout）。
const SSH_TIMEOUT_SECS: u64 = 10;
/// Helper 命令执行超时（适用于大多数操作）。
const DEFAULT_CMD_TIMEOUT_SECS: u64 = 30;
/// 安装等慢速操作的超时。
const SLOW_CMD_TIMEOUT_SECS: u64 = 120;
/// 默认重试次数。
const DEFAULT_RETRIES: u32 = 3;
/// Helper 发布的 GitHub 仓库（可通过环境变量覆盖）。
const HELPER_RELEASE_REPO: &str = "SuperJJ007/CSswitch";
const HELPER_RELEASE_REPO_ENV: &str = "CSSWITCH_HELPER_RELEASE_REPO";

/// 校验 GitHub owner/repo 格式，防止命令注入。
/// 只允许字母、数字、连字符、下划线、点。
fn validate_repo_format(repo: &str) -> Option<&str> {
    let repo = repo.trim();
    if repo.is_empty() {
        return None;
    }
    // 格式: owner/name，两部分都是 [a-zA-Z0-9._-]+
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    let valid = |s: &str| -> bool {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    if valid(owner) && valid(name) {
        Some(repo)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct SshCommandSpec {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

// ============================================================================
// SSH 参数构建
// ============================================================================

/// 构建 SSH 基础参数（通用部分）。
/// 参数说明：
/// - `ConnectTimeout`：连接超时 10 秒，避免网络不通时无限等待。
/// - `ServerAliveInterval`：每 15 秒发送 keepalive，防止 NAT/防火墙断开空闲连接。
/// - `StrictHostKeyChecking=accept-new`：首次自动接受主机密钥（后续连接验证指纹）。
/// - `BatchMode`：KeyFile/Agent 时设为 yes（禁止交互），密码时不设。
fn build_ssh_base_args_with_plan(
    profile: &RemoteHostProfile,
    auth_plan: &SshAuthPlan,
) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        profile.port.to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={SSH_TIMEOUT_SECS}"),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
    ];

    args.extend(auth_plan.args.clone());

    args.push("--".to_string());
    args.push(format!("{}@{}", profile.username, profile.host));
    args
}

/// 构建执行一次 helper 命令的完整 SSH 参数。
/// 远程执行：`<helper_path> --json <helper_args...>`
pub fn build_ssh_args(profile: &RemoteHostProfile, helper_args: &[String]) -> Vec<String> {
    build_ssh_command_spec(
        profile,
        helper_args,
        AuthRuntimeOptions::default_for(profile),
    )
    .args
}

fn build_ssh_command_spec(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    runtime: AuthRuntimeOptions,
) -> SshCommandSpec {
    let auth_plan = SshAuthPlan::from_profile(profile, runtime);
    let mut args = build_ssh_base_args_with_plan(profile, &auth_plan);
    // 构建 helper 命令行：`<path> --json <args...>`
    let cmd = format!(
        "{} --json {}",
        shell_quote(&profile.helper_path),
        helper_args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    );
    args.push(cmd);
    SshCommandSpec {
        args,
        env: auth_plan.env,
    }
}

/// 构建安装 helper 的 SSH 命令。
/// 在远程执行 shell 脚本：下载 release 资产 → 校验 → 安装。
///
/// P0-2 修复：对平台信息进行白名单校验，防止命令注入。
pub fn build_helper_install_args(profile: &RemoteHostProfile) -> Vec<String> {
    build_helper_install_command_spec(profile, AuthRuntimeOptions::default_for(profile)).args
}

pub fn build_helper_install_command_spec(
    profile: &RemoteHostProfile,
    runtime: AuthRuntimeOptions,
) -> SshCommandSpec {
    let auth_plan = SshAuthPlan::from_profile(profile, runtime);
    let mut args = build_ssh_base_args_with_plan(profile, &auth_plan);
    let helper_path = shell_quote(&profile.helper_path);
    let repo = std::env::var(HELPER_RELEASE_REPO_ENV)
        .ok()
        .and_then(|v| validate_repo_format(&v).map(String::from))
        .unwrap_or_else(|| HELPER_RELEASE_REPO.to_string());

    // P0-2: 安装脚本中加入架构白名单校验，防止注入
    // 即使攻击者控制了 uname 输出，也只能匹配预定义的安全值
    let script = format!(
        r#"set -e
HELPER_PATH={helper_path}
HELPER_DIR=$(dirname "$HELPER_PATH")
mkdir -p "$HELPER_DIR"

download() {{
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "远程服务器需要 curl 或 wget 来下载 helper。请手动安装。" >&2
    exit 1
  fi
}}

# P0-2 修复：对架构和 OS 进行严格白名单校验
ARCH_RAW=$(uname -m)
case "$ARCH_RAW" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *)
    echo "不支持的架构: $ARCH_RAW（仅支持 x86_64/aarch64）" >&2
    exit 1
    ;;
esac

OS_RAW=$(uname -s)
case "$OS_RAW" in
  Linux) OS=linux ;;
  *)
    echo "不支持的操作系统: $OS_RAW（仅支持 Linux）" >&2
    exit 1
    ;;
esac

# 尝试从 GitHub API 获取最新 release 的下载 URL
# 使用硬编码的文件名模式，防止通配符注入
API_URL="https://api.github.com/repos/{repo}/releases/latest"
BINARY_NAME="csswitch-helper-${{OS}}-${{ARCH}}"

# 从 API 响应中提取匹配的下载 URL
# 优先 jq（JSON 专用工具最可靠），其次 python3（Linux 标配），最后 awk（兜底）
API_JSON=$(mktemp)
download "$API_URL" "$API_JSON"

if command -v jq >/dev/null 2>&1; then
  DOWNLOAD_URL=$(jq -r ".assets[] | select(.name==\"$BINARY_NAME\") | .browser_download_url" "$API_JSON" 2>/dev/null || true)
elif command -v python3 >/dev/null 2>&1; then
  DOWNLOAD_URL=$(python3 -c "
import json,sys
data=json.load(open('$API_JSON'))
for a in data.get('assets',[]):
    if a.get('name')=='$BINARY_NAME':
        print(a['browser_download_url'])
        break
" 2>/dev/null || true)
else
  DOWNLOAD_URL=$(awk -v name="\"$BINARY_NAME\"" '
    $0 ~ name {{ found=1 }}
    found && /browser_download_url/ {{
      if (match($0, /https:[^"]+/)) {{
        print substr($0, RSTART, RLENGTH)
        exit
      }}
    }}
  ' "$API_JSON" || true)
fi
rm -f "$API_JSON"

if [ -z "$DOWNLOAD_URL" ]; then
  echo "无法从 GitHub Releases 获取 $BINARY_NAME 下载链接。" >&2
  echo "手动安装: wget <url> -O $HELPER_PATH && chmod +x $HELPER_PATH" >&2
  exit 1
fi

TMP=$(mktemp)
download "$DOWNLOAD_URL" "$TMP"
chmod +x "$TMP"
mv "$TMP" "$HELPER_PATH"
"$HELPER_PATH" --json status
"#,
        helper_path = helper_path,
        repo = repo,
    );
    args.push(script);
    SshCommandSpec {
        args,
        env: auth_plan.env,
    }
}

// ============================================================================
// 命令执行
// ============================================================================

/// 在远程服务器上执行一次 helper 命令，解析 JSON 响应。
///
/// 参数：
/// - `profile`：SSH 连接配置
/// - `helper_args`：helper 子命令，如 `["proxy", "status"]`
/// - `timeout_secs`：超时秒数（含 SSH 连接和命令执行）
/// - `retries`：重试次数（0=不重试）
///
/// 返回：反序列化后的命令结果（T 类型）。
///
/// 错误：返回结构化的 `RemoteError`，包含可重试标记和修复建议。
pub fn detect_remote_platform(
    profile: &RemoteHostProfile,
) -> Result<(String, String), RemoteError> {
    let out = run_ssh_script(
        profile,
        "printf '%s\\n%s\\n' \"$(uname -s)\" \"$(uname -m)\"",
        DEFAULT_CMD_TIMEOUT_SECS,
    )?;
    let mut lines = out.lines().map(str::trim).filter(|line| !line.is_empty());
    let os_raw = lines.next().unwrap_or_default();
    let arch_raw = lines.next().unwrap_or_default();

    // P0-2 修复：对 OS 名称做严格白名单校验，防止非预期平台字符串污染 UI/日志
    let os = match os_raw {
        "Linux" => "linux",
        "Darwin" => "macos",
        other => {
            return Err(RemoteError {
                code: "unsupported_platform".to_string(),
                message: format!("远程服务器平台不支持：{other}（仅支持 Linux）"),
                details: None,
                recoverable: false,
                suggestion: Some(
                    "远程 Helper 目前仅支持 Linux 服务器。请在 Linux 上部署。".to_string(),
                ),
            });
        }
    }
    .to_string();

    let arch = match arch_raw {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => arch_raw,
    }
    .to_string();

    Ok((os, arch))
}

pub fn build_helper_stdin_install_args(profile: &RemoteHostProfile) -> Vec<String> {
    build_helper_stdin_install_command_spec(profile, AuthRuntimeOptions::default_for(profile)).args
}

fn build_helper_stdin_install_command_spec(
    profile: &RemoteHostProfile,
    runtime: AuthRuntimeOptions,
) -> SshCommandSpec {
    let auth_plan = SshAuthPlan::from_profile(profile, runtime);
    let mut args = build_ssh_base_args_with_plan(profile, &auth_plan);
    let helper_path = shell_quote(&profile.helper_path);
    let script = format!(
        r#"set -e
HELPER_PATH={helper_path}
HELPER_DIR=$(dirname "$HELPER_PATH")
mkdir -p "$HELPER_DIR"
TMP=$(mktemp "$HELPER_DIR/.csswitch-helper.XXXXXX")
cat > "$TMP"
chmod +x "$TMP"
mv "$TMP" "$HELPER_PATH"
"$HELPER_PATH" --json status
"#,
        helper_path = helper_path,
    );
    args.push(script);
    SshCommandSpec {
        args,
        env: auth_plan.env,
    }
}

pub fn install_helper_from_stdin(
    profile: &RemoteHostProfile,
    helper_bytes: &[u8],
) -> Result<String, RemoteError> {
    let (runtime, _broker) = auth_runtime_options(profile)?;
    let spec = build_helper_stdin_install_command_spec(profile, runtime);
    let mut command = hide_cmd(Command::new("ssh"));
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RemoteError {
            code: "ssh_spawn_failed".to_string(),
            message: format!("无法启动 SSH 客户端：{e}"),
            details: Some(format!("请确认 OpenSSH 客户端已安装并在 PATH 中：{e}")),
            recoverable: false,
            suggestion: Some(
                "Windows 10+ 自带 OpenSSH。请在系统可选功能中确认已安装。".to_string(),
            ),
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(helper_bytes).map_err(|e| RemoteError {
            code: "helper_upload_failed".to_string(),
            message: format!("上传 Helper 二进制失败：{e}"),
            details: None,
            recoverable: true,
            suggestion: Some("请检查 SSH 连接是否稳定，并重试保存服务器。".to_string()),
        })?;
    }

    let output = match wait_with_timeout_legacy(child, Duration::from_secs(SLOW_CMD_TIMEOUT_SECS)) {
        Ok(result) => result.map_err(|e| RemoteError {
            code: "ssh_io_error".to_string(),
            message: format!("SSH 进程 I/O 错误：{e}"),
            details: None,
            recoverable: true,
            suggestion: Some("请重试。如持续出现，请检查系统资源。".to_string()),
        })?,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            return Err(RemoteError {
                code: "ssh_timeout".to_string(),
                message: format!("SSH 上传 Helper 超时（{}秒）", SLOW_CMD_TIMEOUT_SECS),
                details: Some(format!("目标：{}@{}", profile.username, profile.host)),
                recoverable: true,
                suggestion: Some("网络慢或远程命令卡住。请检查 SSH 连接后重试。".to_string()),
            });
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(RemoteError {
                code: "ssh_thread_panic".to_string(),
                message: "SSH 执行线程异常退出".to_string(),
                details: None,
                recoverable: false,
                suggestion: Some("这可能是程序错误。请报告此问题。".to_string()),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(map_ssh_error(profile, &stderr, output.status.code()));
    }

    const MAX_UPLOAD_OUTPUT_SIZE: usize = 1024 * 1024;
    if output.stdout.len() > MAX_UPLOAD_OUTPUT_SIZE {
        return Err(RemoteError {
            code: "output_too_large".to_string(),
            message: format!("Helper 输出过大（{} 字节）", output.stdout.len()),
            details: None,
            recoverable: false,
            suggestion: Some("请在远程服务器上查看 Helper 日志排查问题。".to_string()),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| RemoteError {
        code: "invalid_utf8".to_string(),
        message: "Helper 返回了无效的 UTF-8 数据".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("这可能表示 Helper 二进制损坏。请尝试重新安装 Helper。".to_string()),
    })
}

pub fn run_helper_json<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
    retries: u32,
) -> Result<T, RemoteError> {
    let mut last_error: Option<RemoteError> = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            // 指数退避：2s / 4s / 8s
            let delay = Duration::from_secs(2u64.saturating_mul(1 << (attempt - 1)));
            std::thread::sleep(delay);
        }

        match try_run_ssh(profile, helper_args, timeout_secs) {
            Ok(stdout) => match parse_helper_response::<T>(&stdout) {
                Ok(data) => return Ok(data),
                Err(e) => {
                    last_error = Some(e);
                    // JSON 解析失败不重试（不是网络问题）
                    break;
                }
            },
            Err(e) => {
                let recoverable = is_recoverable_error(&e);
                last_error = Some(e);
                if !recoverable {
                    break;
                }
                // 可恢复错误继续重试
            }
        }
    }

    Err(last_error.unwrap_or_else(|| RemoteError {
        code: "unknown".to_string(),
        message: "未知远程错误".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("请查看日志或联系支持".to_string()),
    }))
}

/// 便捷方法：使用默认超时和不重试。
/// 注意：内部 `auth_runtime_options` 和 `build_ssh_command_spec` 各计算一次 SshAuthPlan
/// （开销极低，仅为几个字符串分配，可忽略不计）。
pub fn run_helper_json_simple<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json(profile, helper_args, DEFAULT_CMD_TIMEOUT_SECS, 0)
}

/// 便捷方法：使用默认超时和默认重试。
pub fn run_helper_json_with_retry<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json(
        profile,
        helper_args,
        DEFAULT_CMD_TIMEOUT_SECS,
        DEFAULT_RETRIES,
    )
}

/// 用于慢速操作（如安装 helper、验证 key）。
pub fn run_helper_json_slow<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json(profile, helper_args, SLOW_CMD_TIMEOUT_SECS, DEFAULT_RETRIES)
}

pub fn run_helper_install(profile: &RemoteHostProfile) -> Result<String, RemoteError> {
    let (runtime, _broker) = auth_runtime_options(profile)?;
    let spec = build_helper_install_command_spec(profile, runtime);
    run_ssh_command(
        profile,
        spec,
        SLOW_CMD_TIMEOUT_SECS,
        "install-helper".to_string(),
    )
}

// ============================================================================
// 内部实现
// ============================================================================

/// 执行 `ssh ... <cmd>` 并返回 stdout 字符串。

pub fn run_ssh_script(
    profile: &RemoteHostProfile,
    remote_cmd: &str,
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let (runtime, _broker) = auth_runtime_options(profile)?;
    let auth_plan = SshAuthPlan::from_profile(profile, runtime);
    let mut args = build_ssh_base_args_with_plan(profile, &auth_plan);
    args.push(remote_cmd.to_string());
    let mut command = hide_cmd(Command::new("ssh"));
    command.args(&args);
    for (key, value) in &auth_plan.env {
        command.env(key, value);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RemoteError {
            code: "ssh_spawn_failed".to_string(),
            message: format!("无法启动 SSH 客户端：{e}"),
            details: Some(format!("请确认 OpenSSH 客户端已安装并在 PATH 中：{e}")),
            recoverable: false,
            suggestion: Some(
                "Windows 10+ 自带 OpenSSH。请在系统可选功能中确认已安装。".to_string(),
            ),
        })?;

    let output = match wait_with_timeout_legacy(child, Duration::from_secs(timeout_secs)) {
        Ok(result) => result.map_err(|e| RemoteError {
            code: "ssh_io_error".to_string(),
            message: format!("SSH 进程 I/O 错误：{e}"),
            details: None,
            recoverable: true,
            suggestion: Some("请重试。如持续出现，请检查系统资源。".to_string()),
        })?,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            return Err(RemoteError {
                code: "ssh_timeout".to_string(),
                message: format!("SSH 命令执行超时（{}秒）", timeout_secs),
                details: Some(format!(
                    "命令：{} {} {}",
                    profile.host, profile.username, remote_cmd
                )),
                recoverable: true,
                suggestion: Some("网络慢或远程命令卡住。请检查 SSH 连接后重试。".to_string()),
            });
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(RemoteError {
                code: "ssh_thread_panic".to_string(),
                message: "SSH 执行线程异常退出".to_string(),
                details: None,
                recoverable: false,
                suggestion: Some("这可能是程序错误。请报告此问题。".to_string()),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(map_ssh_error(profile, &stderr, output.status.code()));
    }

    const MAX_SCRIPT_OUTPUT_SIZE: usize = 1024 * 1024;
    if output.stdout.len() > MAX_SCRIPT_OUTPUT_SIZE {
        return Err(RemoteError {
            code: "output_too_large".to_string(),
            message: format!("SSH 输出过大（{} 字节）", output.stdout.len()),
            details: None,
            recoverable: false,
            suggestion: Some("请在远程服务器上查看命令输出排查问题。".to_string()),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| RemoteError {
        code: "invalid_utf8".to_string(),
        message: "SSH 返回了无效的 UTF-8 数据".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("请检查远程 Shell 输出。".to_string()),
    })
}

#[cfg(feature = "desktop")]
fn auth_runtime_options(
    profile: &RemoteHostProfile,
) -> Result<(AuthRuntimeOptions, Option<AskpassBroker>), RemoteError> {
    let mut runtime = AuthRuntimeOptions::default_for(profile);
    let session_dir = askpass_session_dir();
    runtime.askpass_path = Some(askpass_executable()?.to_string_lossy().into_owned());
    runtime.askpass_session_dir = Some(session_dir.to_string_lossy().into_owned());
    runtime
        .askpass_env
        .push(("CSSWITCH_ASKPASS_MODE".to_string(), "1".to_string()));

    let plan = SshAuthPlan::from_profile(profile, runtime.clone());
    if !plan.interactive {
        return Ok((runtime, None));
    }

    let app = askpass::app_handle().ok_or_else(|| RemoteError {
        code: "ssh_auth_prompt_unavailable".to_string(),
        message: "需要输入登录信息，但当前窗口还没有准备好".to_string(),
        details: None,
        recoverable: true,
        suggestion: Some("请稍后重试连接。".to_string()),
    })?;
    let broker = AskpassBroker::start(app, session_dir).map_err(|e| RemoteError {
        code: "ssh_auth_prompt_failed".to_string(),
        message: "无法打开 SSH 登录输入窗口".to_string(),
        details: Some(e),
        recoverable: true,
        suggestion: Some("请重试连接；如果仍失败，请检查应用日志。".to_string()),
    })?;
    Ok((runtime, Some(broker)))
}

#[cfg(not(feature = "desktop"))]
fn auth_runtime_options(
    profile: &RemoteHostProfile,
) -> Result<(AuthRuntimeOptions, ()), RemoteError> {
    Ok((AuthRuntimeOptions::default_for(profile), ()))
}

#[cfg(feature = "desktop")]
fn askpass_executable() -> Result<PathBuf, RemoteError> {
    std::env::current_exe().map_err(|e| RemoteError {
        code: "ssh_askpass_path_failed".to_string(),
        message: "无法定位 SSH 登录辅助程序".to_string(),
        details: Some(e.to_string()),
        recoverable: false,
        suggestion: Some("请重新安装或重新启动 CSSwitch。".to_string()),
    })
}

#[cfg(feature = "desktop")]
fn askpass_session_dir() -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    crate::config::default_dir()
        .join("ssh-askpass")
        .join(format!("{}-{now}", std::process::id()))
}

fn try_run_ssh(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let (runtime, _broker) = auth_runtime_options(profile)?;
    let spec = build_ssh_command_spec(profile, helper_args, runtime);
    run_ssh_command(profile, spec, timeout_secs, helper_args.join(" "))
}

fn run_ssh_command(
    profile: &RemoteHostProfile,
    spec: SshCommandSpec,
    timeout_secs: u64,
    command_details: String,
) -> Result<String, RemoteError> {
    let mut command = hide_cmd(Command::new("ssh"));
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RemoteError {
            code: "ssh_spawn_failed".to_string(),
            message: format!("无法启动 SSH 客户端：{e}"),
            details: Some(format!("请确认 OpenSSH 客户端已安装并在 PATH 中：{e}")),
            recoverable: false,
            suggestion: Some(
                "Windows 10+ 自带 OpenSSH。请在「设置→应用→可选功能」中确认已安装。".to_string(),
            ),
        })?;

    let output = match wait_with_timeout(output, Duration::from_secs(timeout_secs)) {
        Ok(Some(output)) => output,
        Ok(None) => {
            return Err(RemoteError {
                code: "ssh_timeout".to_string(),
                message: format!("SSH 命令执行超时（{}秒）", timeout_secs),
                details: Some(format!(
                    "命令：{} {} {}",
                    profile.host, profile.username, command_details
                )),
                recoverable: true,
                suggestion: Some(
                    "网络慢或远程命令卡住。请检查网络连接，或在 SSH 配置中设置超时参数。"
                        .to_string(),
                ),
            });
        }
        Err(e) => {
            return Err(RemoteError {
                code: "ssh_io_error".to_string(),
                message: format!("SSH 进程 I/O 错误：{e}"),
                details: None,
                recoverable: true,
                suggestion: Some("请重试。如持续出现，请检查系统资源。".to_string()),
            })
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(map_ssh_error(profile, &stderr, output.status.code()));
    }

    // P1-11 修复：限制 Helper 输出大小，防止 OOM
    const MAX_OUTPUT_SIZE: usize = 1024 * 1024; // 1MB
    if output.stdout.len() > MAX_OUTPUT_SIZE {
        return Err(RemoteError {
            code: "output_too_large".to_string(),
            message: format!(
                "Helper 输出过大（{} 字节，限制 {} 字节）",
                output.stdout.len(),
                MAX_OUTPUT_SIZE
            ),
            details: Some("输出被截断以防止内存溢出".to_string()),
            recoverable: false,
            suggestion: Some(
                "请在远程服务器上查看 Helper 日志文件排查问题（csswitch-helper logs proxy）。"
                    .to_string(),
            ),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| RemoteError {
        code: "invalid_utf8".to_string(),
        message: "Helper 返回了无效的 UTF-8 数据".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("这可能表示 Helper 二进制损坏。请尝试重新安装 Helper。".to_string()),
    })
}

fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stdout) = stdout.take() {
            let _ = stdout.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr.take() {
            let _ = stderr.read_to_end(&mut buf);
        }
        buf
    });

    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(status.map(|status| Output {
        status,
        stdout,
        stderr,
    }))
}

fn wait_with_timeout_legacy(
    child: std::process::Child,
    timeout: Duration,
) -> Result<std::io::Result<Output>, std::sync::mpsc::RecvTimeoutError> {
    match wait_with_timeout(child, timeout) {
        Ok(Some(output)) => Ok(Ok(output)),
        Ok(None) => Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        Err(e) => Ok(Err(e)),
    }
}

/// 解析 helper 的 `{"ok":true,"data":...}` JSON 响应。
fn parse_helper_response<T: DeserializeOwned>(stdout: &str) -> Result<T, RemoteError> {
    // 取最后一行非空内容（忽略 shell 登录 banner 等噪声）
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(stdout)
        .trim();

    let envelope: serde_json::Value = serde_json::from_str(json_line).map_err(|e| RemoteError {
        code: "invalid_json".to_string(),
        message: format!("Helper 返回了无效的 JSON：{e}"),
        details: Some(format!(
            "原始输出（截断）：{}",
            &json_line[..json_line.len().min(200)]
        )),
        recoverable: false,
        suggestion: Some("Helper 版本可能不兼容。请尝试重新安装 Helper。".to_string()),
    })?;

    let ok = envelope
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if ok {
        let data = envelope
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::from_value(data).map_err(|e| RemoteError {
            code: "data_parse_error".to_string(),
            message: format!("Helper 返回数据格式不匹配：{e}"),
            details: None,
            recoverable: false,
            suggestion: Some("Helper 版本可能不兼容。请尝试升级 Helper。".to_string()),
        })
    } else {
        let error = envelope.get("error");
        Err(RemoteError {
            code: error
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .unwrap_or("helper_error")
                .to_string(),
            message: error
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Helper 命令执行失败")
                .to_string(),
            details: error
                .and_then(|e| e.get("details"))
                .and_then(|d| d.as_str())
                .map(|s| s.to_string()),
            recoverable: false,
            suggestion: error
                .and_then(|e| e.get("suggestion"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// 将 SSH 错误输出映射为结构化的 `RemoteError`。
fn map_ssh_error(profile: &RemoteHostProfile, stderr: &str, exit_code: Option<i32>) -> RemoteError {
    let stderr_lower = stderr.to_lowercase();

    // 认证失败（不可重试）
    // 注意：只包含 "permission denied" 但没有 "publickey"/"authentication failed"
    // 的是文件权限错误，由后面 permission_denied 分支处理。
    if stderr_lower.contains("publickey") || stderr_lower.contains("authentication failed") {
        return RemoteError {
            code: "ssh_auth_failed".to_string(),
            message: "SSH 认证失败，请检查用户名和密钥配置".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                match &profile.auth_method {
                    RemoteAuthMethod::KeyFile { .. } => {
                        "请确认私钥文件路径正确且已添加到远程服务器的 authorized_keys。"
                    }
                    RemoteAuthMethod::SshAgent => {
                        "请确认 ssh-agent 已运行且已添加对应密钥（ssh-add -l 查看）。"
                    }
                    RemoteAuthMethod::Recommended { .. } => {
                        "请检查服务器地址、用户名、密码或密钥文件是否正确。"
                    }
                    RemoteAuthMethod::Password { .. } => "请确认服务器密码正确。",
                }
                .to_string(),
            ),
        };
    }

    // 连接超时/拒绝（可重试）
    if stderr_lower.contains("connection timed out")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("no route to host")
        || stderr_lower.contains("network is unreachable")
    {
        return RemoteError {
            code: "ssh_connection_failed".to_string(),
            message: format!(
                "无法连接到 {}:{}，请检查网络和服务器地址",
                profile.host, profile.port
            ),
            details: Some(stderr.to_string()),
            recoverable: true,
            suggestion: Some(
                "请确认：1) 服务器地址和端口正确  2) 防火墙允许 SSH  3) 服务器 SSH 服务正在运行"
                    .to_string(),
            ),
        };
    }

    // Helper 未找到
    if stderr_lower.contains("no such file")
        || stderr_lower.contains("not found")
        || stderr.contains("没有那个文件或目录")
    {
        return RemoteError {
            code: "helper_not_found".to_string(),
            message: format!(
                "远程 Helper 未安装或路径不正确（当前：{}）",
                profile.helper_path
            ),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "请点击「安装 Helper」按钮自动安装，或手动部署 Helper 到服务器。".to_string(),
            ),
        };
    }

    // P1-6 修复：增强 SSH 错误映射，覆盖更多常见错误场景

    // 磁盘空间不足
    if stderr_lower.contains("no space left")
        || stderr_lower.contains("disk full")
        || stderr_lower.contains("write failed")
    {
        return RemoteError {
            code: "disk_full".to_string(),
            message: "远程服务器磁盘空间不足".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "请清理远程服务器磁盘空间，或使用 df -h 检查磁盘使用情况。".to_string(),
            ),
        };
    }

    // 权限不足（写入/执行权限）—— 非认证类的 permission denied
    if stderr_lower.contains("permission denied") {
        return RemoteError {
            code: "permission_denied".to_string(),
            message: "远程服务器权限不足".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "请确认远程用户对 Helper 路径和日志目录有读写权限（chmod +x helper_path）。"
                    .to_string(),
            ),
        };
    }

    // Shell 配置错误（bashrc/profile 报错）
    if stderr_lower.contains("command not found")
        || stderr_lower.contains("syntax error")
        || stderr_lower.contains(".bashrc")
        || stderr_lower.contains(".profile")
    {
        return RemoteError {
            code: "shell_config_error".to_string(),
            message: "远程 Shell 配置异常".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "请检查远程用户的 .bashrc 或 .profile 文件是否有错误（临时解决：ssh -t user@host /bin/bash --noprofile）。".to_string()
            ),
        };
    }

    // 端口被占用
    if stderr_lower.contains("address already in use")
        || stderr_lower.contains("port is already allocated")
        || stderr_lower.contains("bind: address already in use")
    {
        return RemoteError {
            code: "port_in_use".to_string(),
            message: "远程服务器端口已被占用".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "请停止占用该端口的进程（lsof -i :端口 或 netstat -tulpn | grep 端口），或更换端口。".to_string()
            ),
        };
    }

    // 主机密钥变更（中间人攻击警告）
    if stderr_lower.contains("remote host identification has changed")
        || stderr_lower.contains("host key verification failed")
    {
        return RemoteError {
            code: "host_key_changed".to_string(),
            message: "远程服务器主机密钥已变更".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "服务器可能被重装或存在安全风险。请确认服务器身份后，手动删除 ~/.ssh/known_hosts 中的旧密钥。".to_string()
            ),
        };
    }

    // 未知错误（兜底）
    RemoteError {
        code: format!("ssh_exit_{}", exit_code.unwrap_or(-1)),
        message: format!(
            "SSH 命令执行失败（退出码 {}）",
            exit_code.map_or("未知".to_string(), |c| c.to_string())
        ),
        details: Some(stderr.to_string()),
        recoverable: exit_code.map_or(false, |c| c == 255), // 255 通常为连接错误，可重试
        suggestion: Some(
            "请在终端手动执行 SSH 命令排查问题：ssh -vvv user@host（-vvv 开启详细日志）。"
                .to_string(),
        ),
    }
}

/// 判断错误是否可重试（网络类错误可重试，认证/配置类不可重试）。
fn is_recoverable_error(error: &RemoteError) -> bool {
    error.recoverable
        && matches!(
            error.code.as_str(),
            "ssh_io_error" | "ssh_connection_failed" | "ssh_exit_255" | "ssh_spawn_failed"
        )
}

// ============================================================================
// 工具函数
// ============================================================================

/// 安全的 shell 引号转义。
/// 如果参数只包含安全字符（字母数字 + `-_./:`），不添加引号；
/// 否则用单引号包裹并转义内部单引号。
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:~".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::super::types::RemoteSshAdvancedOptions;
    use super::*;

    fn sample_profile() -> RemoteHostProfile {
        RemoteHostProfile {
            id: "test".to_string(),
            name: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "testuser".to_string(),
            auth_method: RemoteAuthMethod::SshAgent,
            helper_path: "/usr/local/bin/csswitch-helper".to_string(),
            last_connected: None,
            ssh_options: RemoteSshAdvancedOptions::default(),
        }
    }

    #[test]
    fn ssh_args_include_connect_timeout() {
        let args = build_ssh_args(&sample_profile(), &["status".to_string()]);
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"ConnectTimeout=10".to_string()));
    }

    #[test]
    fn ssh_args_include_batch_mode_for_sshagent() {
        let args = build_ssh_args(&sample_profile(), &["status".to_string()]);
        assert!(args.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn ssh_args_include_keyfile_for_key_auth() {
        let mut p = sample_profile();
        p.auth_method = RemoteAuthMethod::KeyFile {
            path: "~/.ssh/id_ed25519".to_string(),
            save_key_password: true,
            allow_password_fallback: true,
            allow_verification_code: true,
            remember_connection: true,
        };
        let args = build_ssh_args(&p, &["status".to_string()]);
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"~/.ssh/id_ed25519".to_string()));
    }

    #[test]
    fn password_command_spec_uses_askpass_environment() {
        let mut p = sample_profile();
        p.auth_method = RemoteAuthMethod::Password {
            save_password: true,
            allow_verification_code: true,
            remember_connection: true,
        };

        let spec = build_ssh_command_spec(&p, &["status".to_string()], AuthRuntimeOptions::test());

        assert!(spec.args.contains(&"BatchMode=no".to_string()));
        assert!(spec.args.contains(&"NumberOfPasswordPrompts=3".to_string()));
        assert_env(&spec.env, "SSH_ASKPASS", "csswitch-ssh-askpass");
        assert_env(&spec.env, "SSH_ASKPASS_REQUIRE", "force");
        assert_env(&spec.env, "DISPLAY", "csswitch");
        assert_env(&spec.env, "CSSWITCH_ASKPASS_PROFILE", "test");
        assert_env(
            &spec.env,
            "CSSWITCH_ASKPASS_DIR",
            "csswitch-askpass-session",
        );
    }

    #[test]
    fn key_password_command_spec_passes_key_path_to_askpass() {
        let mut p = sample_profile();
        p.auth_method = RemoteAuthMethod::KeyFile {
            path: "~/.ssh/id_ed25519".to_string(),
            save_key_password: true,
            allow_password_fallback: false,
            allow_verification_code: false,
            remember_connection: false,
        };

        let spec = build_ssh_command_spec(&p, &["status".to_string()], AuthRuntimeOptions::test());

        assert!(spec.args.contains(&"BatchMode=no".to_string()));
        assert_env(&spec.env, "CSSWITCH_ASKPASS_KEY_PATH", "~/.ssh/id_ed25519");
    }

    #[test]
    fn ssh_agent_command_spec_stays_noninteractive() {
        let spec = build_ssh_command_spec(
            &sample_profile(),
            &["status".to_string()],
            AuthRuntimeOptions::test(),
        );

        assert!(spec.args.contains(&"BatchMode=yes".to_string()));
        assert!(spec.args.contains(&"NumberOfPasswordPrompts=0".to_string()));
        assert!(spec.env.is_empty());
    }

    fn assert_env(env: &[(String, String)], key: &str, value: &str) {
        assert_eq!(
            env.iter()
                .find(|(env_key, _)| env_key == key)
                .map(|(_, env_value)| env_value.as_str()),
            Some(value)
        );
    }

    #[test]
    fn shell_quote_leaves_safe_strings_unchanged() {
        assert_eq!(shell_quote("hello-world"), "hello-world");
        assert_eq!(
            shell_quote("/usr/local/bin/helper"),
            "/usr/local/bin/helper"
        );
    }

    #[test]
    fn shell_quote_quotes_unsafe_strings() {
        let quoted = shell_quote("hello world");
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
    }

    #[test]
    fn parse_response_handles_ok() {
        let json = r#"{"ok":true,"data":{"status":"running"}}"#;
        let result: serde_json::Value = parse_helper_response(json).unwrap();
        assert_eq!(result["status"], "running");
    }

    #[test]
    fn parse_response_handles_error() {
        let json = r#"{"ok":false,"error":{"code":"test_error","message":"something went wrong"}}"#;
        let result: Result<serde_json::Value, RemoteError> = parse_helper_response(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, "test_error");
    }

    #[test]
    fn parse_response_takes_last_nonempty_line() {
        let multi = "Login banner\n\n{\"ok\":true,\"data\":42}";
        let result: i32 = parse_helper_response(multi).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn recoverable_errors_are_marked_as_such() {
        let err = map_ssh_error(&sample_profile(), "Connection timed out", Some(255));
        assert!(err.recoverable);
        assert_eq!(err.code, "ssh_connection_failed");
    }

    #[test]
    fn auth_errors_are_not_recoverable() {
        let err = map_ssh_error(
            &sample_profile(),
            "Permission denied (publickey)",
            Some(255),
        );
        assert!(!err.recoverable);
        assert_eq!(err.code, "ssh_auth_failed");
    }
}
