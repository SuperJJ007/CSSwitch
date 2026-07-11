//! 进程管家用到的纯 std 辅助：探活、一次性 secret 生成、上游可达性。
//! 无第三方依赖，便于单测；有状态的子进程编排放在 lib.rs（持 Child 句柄）。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildLiveness {
    Running,
    Exited(String),
    Unknown(String),
}

fn classify_child_try_wait(
    result: std::io::Result<Option<std::process::ExitStatus>>,
) -> ChildLiveness {
    match result {
        Ok(None) => ChildLiveness::Running,
        Ok(Some(status)) => ChildLiveness::Exited(status.to_string()),
        Err(error) => ChildLiveness::Unknown(error.to_string()),
    }
}

pub fn poll_child_liveness(child: &mut std::process::Child) -> ChildLiveness {
    classify_child_try_wait(child.try_wait())
}

pub fn tracked_child_is_running(child: &mut Option<std::process::Child>) -> bool {
    child
        .as_mut()
        .map(|child| matches!(poll_child_liveness(child), ChildLiveness::Running))
        .unwrap_or(false)
}

/// Check whether a loopback TCP port is already accepting connections without
/// sending any HTTP bytes or path secret to the listener.
pub fn loopback_port_in_use(port: u16, timeout_ms: u64) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).is_ok()
}

/// Final publish/probe guard used immediately before exposing a child as usable.
/// It is deliberately fail-closed for both an observed exit and an indeterminate poll.
pub fn require_child_running(child: &mut std::process::Child, context: &str) -> Result<(), String> {
    match poll_child_liveness(child) {
        ChildLiveness::Running => Ok(()),
        ChildLiveness::Exited(status) => Err(format!("{context}提前退出（{status}）")),
        ChildLiveness::Unknown(error) => Err(format!("{context}存活状态未知：{error}")),
    }
}

/// 对本地回环代理做 HTTP 探活：`GET /<secret>/health`，响应状态行含 200 即视为健康。
/// 代理带 path-secret 鉴权时必须带上 secret，否则会拿到 403。
pub fn http_health(port: u16, secret: Option<&str>, timeout_ms: u64) -> bool {
    http_health_response(port, secret, timeout_ms).is_some()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayHealth {
    pub gateway: String,
    pub provider: String,
    pub shim: String,
    pub launch_id: String,
}

/// 读取 Rust gateway 声明的运行身份。
pub fn http_gateway_health(
    port: u16,
    secret: Option<&str>,
    timeout_ms: u64,
) -> Option<GatewayHealth> {
    let response = http_health_response(port, secret, timeout_ms)?;
    gateway_health_from_response(&response)
}

/// CSSwitch proxy 强身份探活。Managed Rust launch 必须匹配
/// gateway/provider/shim/launch_id；launch_id 防止同端口同 secret、甚至同
/// provider/shim 的旧 gateway 冒充新 child。
pub fn http_health_gateway(
    port: u16,
    secret: Option<&str>,
    timeout_ms: u64,
    expected_gateway: &str,
    expected_provider: Option<&str>,
    expected_shim: Option<&str>,
    expected_launch_id: Option<&str>,
) -> bool {
    let Some(actual) = http_gateway_health(port, secret, timeout_ms) else {
        return false;
    };
    gateway_health_matches(
        &actual,
        expected_gateway,
        expected_provider,
        expected_shim,
        expected_launch_id,
    )
}

fn gateway_health_matches(
    actual: &GatewayHealth,
    expected_gateway: &str,
    expected_provider: Option<&str>,
    expected_shim: Option<&str>,
    expected_launch_id: Option<&str>,
) -> bool {
    actual.gateway == expected_gateway
        && expected_provider
            .map(|expected| actual.provider == expected)
            .unwrap_or(true)
        && expected_shim
            .map(|expected| actual.shim == expected)
            .unwrap_or(true)
        && expected_launch_id
            .map(|expected| !expected.is_empty() && actual.launch_id == expected)
            .unwrap_or(true)
}

fn http_health_response(port: u16, secret: Option<&str>, timeout_ms: u64) -> Option<String> {
    let addr = ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())?;
    let dur = Duration::from_millis(timeout_ms);
    let mut stream = match TcpStream::connect_timeout(&addr, dur) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let _ = stream.set_read_timeout(Some(dur));
    let _ = stream.set_write_timeout(Some(dur));
    let path = match secret {
        Some(s) if !s.is_empty() => format!("/{s}/health"),
        _ => "/health".to_string(),
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return None;
    }
    let mut buf = Vec::new();
    // 读完整个小 health 响应；读上限防呆，超时则用已读内容判定。
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if let Some(expected) = expected_http_response_len(&buf) {
            if buf.len() >= expected {
                break;
            }
        }
    }
    let resp = String::from_utf8_lossy(&buf).to_string();
    let status_line = resp.lines().next().unwrap_or("");
    // 形如 "HTTP/1.1 200 OK"：严格取第二段等于 200，避免 contains 误配 reason phrase。
    if status_line.split_whitespace().nth(1) == Some("200") {
        Some(resp)
    } else {
        None
    }
}

fn gateway_health_from_response(resp: &str) -> Option<GatewayHealth> {
    let (_, body) = resp.split_once("\r\n\r\n")?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let string_field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let gateway = string_field("gateway");
    if gateway.is_empty() {
        return None;
    }
    Some(GatewayHealth {
        gateway,
        provider: string_field("provider"),
        shim: string_field("shim"),
        launch_id: string_field("launch_id"),
    })
}

fn expected_http_response_len(buf: &[u8]) -> Option<usize> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..header_end]);
    for line in head.lines() {
        let (name, value) = match line.split_once(':') {
            Some(parts) => parts,
            None => continue,
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            if let Ok(len) = value.trim().parse::<usize>() {
                return Some(header_end + len);
            }
        }
    }
    Some(header_end)
}

/// 向本地回环代理 POST 一段 JSON（`POST /<secret><path>`），返回 HTTP 响应状态码；
/// 连不上 / 无响应返回 None。用于「存 key 后用最小请求真正验一次 key」——
/// 请求经代理打到上游，200=可用，401/403=key 被拒。回环明文，无需 TLS。
/// timeout_ms 要给足（代理要转发上游），建议调用方传 ~15000。
pub fn http_post_status(
    port: u16,
    secret: Option<&str>,
    path_suffix: &str,
    body: &[u8],
    timeout_ms: u64,
) -> Option<u16> {
    let addr = ("127.0.0.1", port).to_socket_addrs().ok()?.next()?;
    let dur = Duration::from_millis(timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, dur).ok()?;
    let _ = stream.set_read_timeout(Some(dur));
    let _ = stream.set_write_timeout(Some(dur));
    let path = match secret {
        Some(s) if !s.is_empty() => format!("/{s}{path_suffix}"),
        _ => path_suffix.to_string(),
    };
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(body).ok()?;
    // 只需读到状态行（非流式响应，状态行随首个头块到达）；读上限防呆。
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
}

/// 向本地回环代理 GET 一段路径（`GET /<secret><path>`），返回 (状态码, 响应体)。
/// 连不上 / 无响应返回 None。用于经代理回源拉中转站 `/v1/models`——代理有 TLS（urllib），
/// 这里只打回环明文，无需在 Rust 侧引 TLS。timeout_ms 要给足（代理要转发上游），建议 ~15000。
pub fn http_get_body(
    port: u16,
    secret: Option<&str>,
    path_suffix: &str,
    timeout_ms: u64,
) -> Option<(u16, String)> {
    let addr = ("127.0.0.1", port).to_socket_addrs().ok()?.next()?;
    let dur = Duration::from_millis(timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, dur).ok()?;
    let _ = stream.set_read_timeout(Some(dur));
    let _ = stream.set_write_timeout(Some(dur));
    let path = match secret {
        Some(s) if !s.is_empty() => format!("/{s}{path_suffix}"),
        _ => path_suffix.to_string(),
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    // 读完整响应（状态行 + 头 + 体）。上限防呆：模型列表通常 < 数十 KB，给 1 MiB。
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() > 1_048_576 {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())?;
    // 分割头与体：首个空行之后即响应体。
    let body = match text.split_once("\r\n\r\n") {
        Some((_, b)) => b.to_string(),
        None => String::new(),
    };
    Some((status, body))
}

/// 上游主机可达性（仅 TCP 连通，不校验 key）。绿灯=可达，黄灯=不可达。
pub fn tcp_reachable(host: &str, port: u16, timeout_ms: u64) -> bool {
    let dur = Duration::from_millis(timeout_ms);
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            for a in addrs {
                if TcpStream::connect_timeout(&a, dur).is_ok() {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// 生成一次性 path-secret：从 /dev/urandom 取 16 字节，hex 编码为 32 字符。
/// 失败关闭：urandom 不可用时返回 Err，绝不退回可猜的弱 secret（宁可起代理失败）。
pub fn gen_secret() -> std::io::Result<String> {
    use std::fs::File;
    let mut b = [0u8; 16];
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(&mut b)?;
    Ok(hex(&b))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        s.push(H[(byte >> 4) as usize] as char);
        s.push(H[(byte & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn loopback_port_occupancy_probe_detects_listener_without_http() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8765);
        assert!(loopback_port_in_use(port, 200));
        drop(listener);
        // Do not assert global vacancy after drop: the parallel test suite may
        // legitimately acquire this just-released ephemeral port immediately.
    }

    #[test]
    fn child_liveness_classifier_fails_closed_for_exit_and_poll_error() {
        assert_eq!(classify_child_try_wait(Ok(None)), ChildLiveness::Running);
        let status = Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .expect("test shell should exit");
        assert!(matches!(
            classify_child_try_wait(Ok(Some(status))),
            ChildLiveness::Exited(_)
        ));
        let unknown = classify_child_try_wait(Err(std::io::Error::other("poll failed")));
        assert_eq!(unknown, ChildLiveness::Unknown("poll failed".into()));
    }

    #[test]
    fn final_child_publish_guard_accepts_running_and_rejects_exited_child() {
        let mut running = Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .expect("test child should start");
        assert!(require_child_running(&mut running, "test child ").is_ok());
        let _ = running.kill();
        let _ = running.wait();

        let mut exited = Command::new("sh")
            .args(["-c", "exit 9"])
            .spawn()
            .expect("test child should start");
        let _ = exited.wait();
        let err = require_child_running(&mut exited, "publish child ").unwrap_err();
        assert!(err.contains("提前退出"));
    }

    #[test]
    fn tracked_child_presence_is_not_enough_for_running_status() {
        let mut absent = None;
        assert!(!tracked_child_is_running(&mut absent));

        let mut running = Some(
            Command::new("sh")
                .args(["-c", "sleep 5"])
                .spawn()
                .expect("test child should start"),
        );
        assert!(tracked_child_is_running(&mut running));
        if let Some(child) = running.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(!tracked_child_is_running(&mut running));
    }

    #[test]
    fn health_false_when_nothing_listening() {
        // 一个几乎肯定没人监听的高端口。
        assert!(!http_health(59999, None, 300));
    }

    #[test]
    fn gateway_health_parses_full_rust_identity() {
        let rust = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"status\":\"ok\",\"gateway\":\"rust\",\"provider\":\"deepseek\",\"shim\":\"rewrite\",\"launch_id\":\"launch-new\"}";
        assert_eq!(
            gateway_health_from_response(rust),
            Some(GatewayHealth {
                gateway: "rust".into(),
                provider: "deepseek".into(),
                shim: "rewrite".into(),
                launch_id: "launch-new".into(),
            })
        );
    }

    #[test]
    fn gateway_health_rejects_missing_or_malformed_identity() {
        assert!(
            gateway_health_from_response("HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ok\"}").is_none()
        );
        assert!(gateway_health_from_response("HTTP/1.1 200 OK\r\n\r\nnot-json").is_none());
    }

    #[test]
    fn managed_health_rejects_old_rust_launch() {
        let old = GatewayHealth {
            gateway: "rust".into(),
            provider: "deepseek".into(),
            shim: "off".into(),
            launch_id: "old-launch".into(),
        };
        assert!(!gateway_health_matches(
            &old,
            "rust",
            Some("deepseek"),
            Some("off"),
            Some("new-launch"),
        ));
        assert!(gateway_health_matches(
            &old,
            "rust",
            Some("deepseek"),
            Some("off"),
            Some("old-launch"),
        ));
        assert!(!gateway_health_matches(
            &old,
            "rust",
            Some("deepseek"),
            Some("off"),
            Some(""),
        ));
    }

    #[test]
    fn health_reader_waits_for_declared_body() {
        let partial = b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n{\"gateway\":\"r";
        assert_eq!(
            expected_http_response_len(partial),
            Some("HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n".len() + 17)
        );
        let complete = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(expected_http_response_len(complete), Some(complete.len()));
    }

    #[test]
    fn get_body_none_when_nothing_listening() {
        // 没人监听 → 连不上 → None（与 http_health 一致的失败关闭语义）。
        assert!(http_get_body(59998, Some("secret"), "/v1/models", 300).is_none());
    }

    #[test]
    fn gen_secret_is_32_hex_and_varies() {
        let a = gen_secret().unwrap();
        let b = gen_secret().unwrap();
        assert_eq!(a.len(), 32, "urandom 路径应是 32 hex 字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次生成应不同");
    }

    #[test]
    fn hex_encodes_known_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }
}
