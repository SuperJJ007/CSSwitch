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

use std::process::{Command, Stdio};
use std::time::Duration;

use serde::de::DeserializeOwned;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use super::types::{RemoteAuthMethod, RemoteError, RemoteHostProfile};

/// Windows: 禁止弹出命令行窗口（CREATE_NO_WINDOW）
#[cfg(windows)]
const NO_WINDOW: u32 = 0x08000000;

fn hide_cmd(mut cmd: Command) -> Command {
    #[cfg(windows)] { cmd.creation_flags(NO_WINDOW); }
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

// ============================================================================
// SSH 参数构建
// ============================================================================

/// 构建 SSH 基础参数（通用部分）。
/// 参数说明：
/// - `ConnectTimeout`：连接超时 10 秒，避免网络不通时无限等待。
/// - `ServerAliveInterval`：每 15 秒发送 keepalive，防止 NAT/防火墙断开空闲连接。
/// - `StrictHostKeyChecking=accept-new`：首次自动接受主机密钥（后续连接验证指纹）。
/// - `BatchMode`：KeyFile/Agent 时设为 yes（禁止交互），密码时不设。
fn build_ssh_base_args(profile: &RemoteHostProfile) -> Vec<String> {
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
        "-o".to_string(),
        "NumberOfPasswordPrompts=0".to_string(), // 禁止密码提示
    ];

    match &profile.auth_method {
        RemoteAuthMethod::KeyFile { path } => {
            args.push("-i".to_string());
            args.push(path.clone());
            args.push("-o".to_string());
            args.push("BatchMode=yes".to_string());
        }
        RemoteAuthMethod::SshAgent => {
            args.push("-o".to_string());
            args.push("BatchMode=yes".to_string());
        }
    }

    args.push("--".to_string());
    args.push(format!("{}@{}", profile.username, profile.host));
    args
}

/// 构建执行一次 helper 命令的完整 SSH 参数。
/// 远程执行：`<helper_path> --json <helper_args...>`
pub fn build_ssh_args(profile: &RemoteHostProfile, helper_args: &[String]) -> Vec<String> {
    let mut args = build_ssh_base_args(profile);
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
    args
}

/// 构建安装 helper 的 SSH 命令。
/// 在远程执行 shell 脚本：下载 release 资产 → 校验 → 安装。
pub fn build_helper_install_args(profile: &RemoteHostProfile) -> Vec<String> {
    let mut args = build_ssh_base_args(profile);
    let helper_path = shell_quote(&profile.helper_path);
    let repo = std::env::var(HELPER_RELEASE_REPO_ENV)
        .unwrap_or_else(|_| HELPER_RELEASE_REPO.to_string());

    // 安装脚本：从 GitHub Releases 下载 helper 二进制。
    // 使用 curl 或 wget 下载 → chmod +x → 验证。
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

ARCH=$(uname -m)
case "$ARCH" in x86_64|amd64) ARCH=x86_64 ;; aarch64|arm64) ARCH=aarch64 ;; *) echo "不支持的架构: $ARCH" >&2; exit 1 ;; esac
OS=$(uname -s)

# 尝试从 GitHub API 获取最新 release 的下载 URL
API_URL="https://api.github.com/repos/{repo}/releases/latest"
DOWNLOAD_URL=$(curl -sSL "$API_URL" 2>/dev/null | grep -o '"browser_download_url": *"[^"]*helper-linux-'"$ARCH"'"' | head -1 | grep -o 'https://[^"]*' || true)

if [ -z "$DOWNLOAD_URL" ]; then
  echo "无法从 GitHub Releases 获取 helper 下载链接。请手动安装。" >&2
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
    args
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
    run_helper_json(profile, helper_args, DEFAULT_CMD_TIMEOUT_SECS, DEFAULT_RETRIES)
}

/// 用于慢速操作（如安装 helper、验证 key）。
pub fn run_helper_json_slow<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json(profile, helper_args, SLOW_CMD_TIMEOUT_SECS, DEFAULT_RETRIES)
}

// ============================================================================
// 内部实现
// ============================================================================

/// 执行 `ssh ... <cmd>` 并返回 stdout 字符串。
fn try_run_ssh(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let args = build_ssh_args(profile, helper_args);
    let output = hide_cmd(Command::new("ssh"))
        .args(&args)
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

    // 使用 channel + recv_timeout 实现真正的命令超时
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = output.wait_with_output();
        let _ = tx.send(result);
    });

    let output = match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
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
                message: format!("SSH 命令执行超时（{timeout_secs}秒）"),
                details: None,
                recoverable: true,
                suggestion: Some("网络慢或远程命令卡住。请检查网络连接。".to_string()),
            });
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(RemoteError {
                code: "ssh_thread_panic".to_string(),
                message: "SSH 执行线程异常退出".to_string(),
                details: None,
                recoverable: false,
                suggestion: Some("请报告此问题。".to_string()),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(map_ssh_error(profile, &stderr, output.status.code()));
    }

    String::from_utf8(output.stdout).map_err(|_| RemoteError {
        code: "invalid_utf8".to_string(),
        message: "Helper 返回了无效的 UTF-8 数据".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("这可能表示 Helper 二进制损坏。请尝试重新安装 Helper。".to_string()),
    })
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

    let envelope: serde_json::Value =
        serde_json::from_str(json_line).map_err(|e| RemoteError {
            code: "invalid_json".to_string(),
            message: format!("Helper 返回了无效的 JSON：{e}"),
            details: Some(format!("原始输出（截断）：{}", &json_line[..json_line.len().min(200)])),
            recoverable: false,
            suggestion: Some("Helper 版本可能不兼容。请尝试重新安装 Helper。".to_string()),
        })?;

    let ok = envelope
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if ok {
        let data = envelope.get("data").cloned().unwrap_or(serde_json::Value::Null);
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
    if stderr_lower.contains("permission denied")
        || stderr_lower.contains("publickey")
        || stderr_lower.contains("authentication failed")
    {
        return RemoteError {
            code: "ssh_auth_failed".to_string(),
            message: "SSH 认证失败，请检查用户名和密钥配置".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(match &profile.auth_method {
                RemoteAuthMethod::KeyFile { .. } => {
                    "请确认私钥文件路径正确且已添加到远程服务器的 authorized_keys。"
                }
                RemoteAuthMethod::SshAgent => {
                    "请确认 ssh-agent 已运行且已添加对应密钥（ssh-add -l 查看）。"
                }
            }.to_string()),
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
            suggestion: Some("请点击「安装 Helper」按钮自动安装，或手动部署 Helper 到服务器。".to_string()),
        };
    }

    // 未知错误
    RemoteError {
        code: format!("ssh_exit_{}", exit_code.unwrap_or(-1)),
        message: format!(
            "SSH 命令执行失败（退出码 {}）",
            exit_code.map_or("未知".to_string(), |c| c.to_string())
        ),
        details: Some(stderr.to_string()),
        recoverable: exit_code.map_or(false, |c| c == 255), // 255 通常为连接错误，可重试
        suggestion: Some("请查看错误详情，或尝试在终端手动执行 SSH 命令排查。".to_string()),
    }
}

/// 判断错误是否可重试（网络类错误可重试，认证/配置类不可重试）。
fn is_recoverable_error(error: &RemoteError) -> bool {
    error.recoverable && matches!(
        error.code.as_str(),
        "ssh_io_error"
            | "ssh_connection_failed"
            | "ssh_exit_255"
            | "ssh_spawn_failed"
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
        };
        let args = build_ssh_args(&p, &["status".to_string()]);
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"~/.ssh/id_ed25519".to_string()));
    }

    #[test]
    fn shell_quote_leaves_safe_strings_unchanged() {
        assert_eq!(shell_quote("hello-world"), "hello-world");
        assert_eq!(shell_quote("/usr/local/bin/helper"), "/usr/local/bin/helper");
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
        let err = map_ssh_error(&sample_profile(), "Permission denied (publickey)", Some(255));
        assert!(!err.recoverable);
        assert_eq!(err.code, "ssh_auth_failed");
    }
}
