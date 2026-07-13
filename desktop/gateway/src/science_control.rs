use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE};
use reqwest::redirect::Policy;
use serde_json::{json, Value};

pub const ROUTE_SKILL_NAME: &str = "csswitch-external-skill-tools";
const CONTROL_URL_ENV: &str = "CSSWITCH_SCIENCE_CONTROL_URL";

pub fn run_cli(args: &[String]) -> Result<Value, String> {
    if args != ["attach-route"] {
        return Err("用法：science-control attach-route".into());
    }
    let url = std::env::var(CONTROL_URL_ENV).map_err(|_| "缺少本地 Science control URL")?;
    attach_route(&url)
}

fn attach_route(raw_url: &str) -> Result<Value, String> {
    let (origin, nonce) = validate_control_url(raw_url)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| "初始化本地 Science 控制客户端失败")?;

    let auth = client
        .post(format!("{origin}/api/auth/nonce"))
        .header(ORIGIN, &origin)
        .form(&[("nonce", nonce.as_str()), ("dest", "/")])
        .send()
        .map_err(|_| "本地 Science nonce 认证请求失败")?;
    ensure_success(&auth, "nonce 认证")?;
    let auth_cookie =
        response_cookie(&auth, "operon_auth").ok_or("本地 Science nonce 认证未返回会话 cookie")?;

    let csrf_response = client
        .get(format!("{origin}/api/csrf"))
        .header(ORIGIN, &origin)
        .header(COOKIE, format!("operon_auth={auth_cookie}"))
        .send()
        .map_err(|_| "本地 Science CSRF 请求失败")?;
    ensure_success(&csrf_response, "CSRF 初始化")?;
    let csrf_cookie = response_cookie(&csrf_response, "operon_csrf")
        .ok_or("本地 Science CSRF 初始化未返回 cookie")?;

    let body = serde_json::to_vec(&json!({"skill_name": ROUTE_SKILL_NAME}))
        .map_err(|_| "编码路由 Skill 绑定请求失败")?;
    let attach = client
        .post(format!("{origin}/api/agents/OPERON/skills"))
        .header(ORIGIN, &origin)
        .header(
            COOKIE,
            format!("operon_auth={auth_cookie}; operon_csrf={csrf_cookie}"),
        )
        .header("x-operon-csrf", &csrf_cookie)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .map_err(|_| "绑定 CSSwitch 路由 Skill 的本地请求失败")?;
    ensure_success(&attach, "路由 Skill 绑定")?;

    Ok(json!({
        "status": "ATTACHED",
        "agent_name": "OPERON",
        "skill_name": ROUTE_SKILL_NAME
    }))
}

fn validate_control_url(raw_url: &str) -> Result<(String, String), String> {
    let url = reqwest::Url::parse(raw_url).map_err(|_| "本地 Science control URL 非法")?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("只允许带显式端口的本机 HTTP Science control URL".into());
    }
    let nonces: Vec<String> = url
        .query_pairs()
        .filter(|(name, _)| name == "nonce")
        .map(|(_, value)| value.into_owned())
        .collect();
    if nonces.len() != 1
        || nonces[0].is_empty()
        || nonces[0].len() > 512
        || !nonces[0]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
    {
        return Err("本地 Science control URL 缺少有效 nonce".into());
    }
    let host = url.host_str().expect("validated host");
    let port = url.port().expect("validated port");
    Ok((format!("http://{host}:{port}"), nonces[0].clone()))
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

fn ensure_success(response: &Response, stage: &str) -> Result<(), String> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "本地 Science {stage}失败（HTTP {}）",
            response.status().as_u16()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

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

    fn reply(stream: &mut TcpStream, cookie: Option<&str>) {
        let cookie = cookie
            .map(|value| format!("Set-Cookie: {value}; Path=/; SameSite=Strict\r\n"))
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 200 OK\r\n{cookie}Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    #[test]
    fn attaches_fixed_route_via_nonce_and_csrf_flow() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            for cookie in [
                Some("operon_auth=auth-token"),
                Some("operon_csrf=csrf-token"),
                None,
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                captured.lock().unwrap().push(read_request(&mut stream));
                reply(&mut stream, cookie);
            }
        });
        let result = attach_route(&format!("http://127.0.0.1:{port}/?nonce=test-nonce")).unwrap();
        worker.join().unwrap();
        assert_eq!(result["status"], "ATTACHED");
        let requests = requests.lock().unwrap();
        assert!(requests[0].starts_with("POST /api/auth/nonce HTTP/1.1"));
        assert!(requests[0].contains("nonce=test-nonce&dest=%2F"));
        assert!(requests[1].starts_with("GET /api/csrf HTTP/1.1"));
        assert!(requests[1].contains("operon_auth=auth-token"));
        assert!(requests[2].starts_with("POST /api/agents/OPERON/skills HTTP/1.1"));
        assert!(requests[2].contains("x-operon-csrf: csrf-token"));
        assert!(requests[2].contains("operon_auth=auth-token; operon_csrf=csrf-token"));
        assert!(requests[2].contains(&format!(r#"{{"skill_name":"{ROUTE_SKILL_NAME}"}}"#)));
    }

    #[test]
    fn rejects_non_loopback_or_missing_nonce_without_network_access() {
        assert!(validate_control_url("https://example.com:8990/?nonce=x").is_err());
        assert!(validate_control_url("http://127.0.0.1:8990/").is_err());
        assert!(validate_control_url("http://localhost/?nonce=x").is_err());
        assert!(validate_control_url("http://user@127.0.0.1:8990/?nonce=x").is_err());
    }
}
