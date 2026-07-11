use std::io::Read;
use std::time::Duration;

use reqwest::blocking::{Client, Response};

use crate::config::{GatewayConfig, UPSTREAM_UA};

#[derive(Debug)]
pub struct UpstreamBody {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub upstream_status: Option<u16>,
    pub detail: String,
}

#[derive(Debug)]
pub struct UpstreamStream {
    pub response: Response,
    pub first: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InferenceTimeouts {
    connect: Duration,
    read_idle: Duration,
}

// Python's urllib transport uses timeout=300 for connection setup and each
// blocking socket operation. Keep the Rust inference path on that contract for
// every provider. In reqwest's blocking client, `timeout` is reapplied to each
// blocking send/read operation (including every `Response::read` call), so it
// is an idle/first-byte guard rather than a total response-body deadline.
const INFERENCE_TIMEOUTS: InferenceTimeouts = InferenceTimeouts {
    connect: Duration::from_secs(300),
    read_idle: Duration::from_secs(300),
};

fn models_timeout_secs(provider: &str) -> u64 {
    if provider == "qwen" || provider == "openai-custom" || provider == "openai-responses" {
        300
    } else {
        120
    }
}

fn inference_client(timeouts: InferenceTimeouts) -> Result<Client, UpstreamError> {
    Client::builder()
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.read_idle)
        .build()
        .map_err(|e| UpstreamError {
            status: 502,
            upstream_status: None,
            detail: e.to_string(),
        })
}

fn models_client(timeout_secs: u64) -> Result<Client, UpstreamError> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| UpstreamError {
            status: 502,
            upstream_status: None,
            detail: e.to_string(),
        })
}

fn post_with_timeouts(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    timeouts: InferenceTimeouts,
) -> Result<Response, UpstreamError> {
    let request = inference_client(timeouts)?
        .post(&cfg.upstream_url)
        .header("content-type", "application/json")
        .header("user-agent", UPSTREAM_UA);
    let request = if cfg.provider == "qwen"
        || cfg.provider == "openai-custom"
        || cfg.provider == "openai-responses"
    {
        request.header("authorization", format!("Bearer {}", cfg.api_key))
    } else if cfg.provider == "relay" {
        request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", &cfg.api_key)
            .header("authorization", format!("Bearer {}", cfg.api_key))
    } else {
        request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", &cfg.api_key)
    };
    request.body(body).send().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })
}

fn post(cfg: &GatewayConfig, body: Vec<u8>) -> Result<Response, UpstreamError> {
    post_with_timeouts(cfg, body, INFERENCE_TIMEOUTS)
}

fn get_once(cfg: &GatewayConfig, url: &str) -> Result<UpstreamBody, UpstreamError> {
    // Model discovery intentionally retains its existing provider-specific
    // client contract. Its timeout/error semantics are handled separately.
    let request = models_client(models_timeout_secs(&cfg.provider))?
        .get(url)
        .header("user-agent", UPSTREAM_UA);
    let request = if cfg.provider == "openai-custom" || cfg.provider == "openai-responses" {
        request.header("authorization", format!("Bearer {}", cfg.api_key))
    } else if cfg.provider == "relay" {
        request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", &cfg.api_key)
            .header("authorization", format!("Bearer {}", cfg.api_key))
    } else {
        request
    };
    let mut resp = request.send().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let mut body = Vec::new();
    resp.read_to_end(&mut body).map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })?;
    if !(200..300).contains(&status) {
        let detail = if body.is_empty() {
            format!("upstream {status}")
        } else {
            format!("upstream {status}: {}", String::from_utf8_lossy(&body))
        };
        return Err(UpstreamError {
            status,
            upstream_status: Some(status),
            detail,
        });
    }
    Ok(UpstreamBody {
        status,
        content_type,
        body,
    })
}

fn retry_delay(attempt: usize) {
    std::thread::sleep(Duration::from_millis(800 * attempt as u64));
}

pub fn get(cfg: &GatewayConfig, url: &str) -> Result<UpstreamBody, UpstreamError> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match get_once(cfg, url) {
            Ok(resp) => return Ok(resp),
            Err(e) if e.upstream_status.is_some() => return Err(e),
            Err(e) => {
                last_error = Some(e);
                if attempt < 3 {
                    retry_delay(attempt);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: "upstream models request failed".to_string(),
    }))
}

fn map_http_error(resp: Response) -> UpstreamError {
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    let mapped = if matches!(status, 401 | 403 | 429) {
        status
    } else {
        502
    };
    let detail = if body.is_empty() {
        format!("upstream {status}")
    } else {
        format!("upstream {status}: {body}")
    };
    UpstreamError {
        status: mapped,
        upstream_status: Some(status),
        detail,
    }
}

pub fn post_nonstream(cfg: &GatewayConfig, body: Vec<u8>) -> Result<UpstreamBody, UpstreamError> {
    let mut last_error = None;
    for attempt in 1..=4 {
        let mut resp = match post(cfg, body.clone()) {
            Ok(resp) => resp,
            Err(e) => {
                last_error = Some(e);
                if attempt < 4 {
                    retry_delay(attempt);
                    continue;
                }
                break;
            }
        };
        if !resp.status().is_success() {
            return Err(map_http_error(resp));
        }
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let mut body = Vec::new();
        match resp.read_to_end(&mut body) {
            Ok(_) => {
                return Ok(UpstreamBody {
                    status,
                    content_type,
                    body,
                });
            }
            Err(e) => {
                last_error = Some(UpstreamError {
                    status: 502,
                    upstream_status: None,
                    detail: e.to_string(),
                });
                if attempt < 4 {
                    retry_delay(attempt);
                    continue;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: "upstream request failed".to_string(),
    }))
}

fn read_first_line(resp: &mut Response) -> Result<Vec<u8>, UpstreamError> {
    let mut first = Vec::new();
    let mut byte = [0_u8; 1];
    while first.len() < 65_536 {
        match resp.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                first.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) => {
                return Err(UpstreamError {
                    status: 502,
                    upstream_status: None,
                    detail: e.to_string(),
                });
            }
        }
    }
    if first.is_empty() {
        return Err(UpstreamError {
            status: 502,
            upstream_status: Some(200),
            detail: "upstream 200 but empty body".to_string(),
        });
    }
    Ok(first)
}

pub fn open_stream(cfg: &GatewayConfig, body: Vec<u8>) -> Result<UpstreamStream, UpstreamError> {
    let mut last_error = None;
    for attempt in 1..=4 {
        let mut resp = match post(cfg, body.clone()) {
            Ok(resp) => resp,
            Err(e) => {
                last_error = Some(e);
                if attempt < 4 {
                    retry_delay(attempt);
                    continue;
                }
                break;
            }
        };
        if !resp.status().is_success() {
            return Err(map_http_error(resp));
        }
        match read_first_line(&mut resp) {
            Ok(first) => {
                return Ok(UpstreamStream {
                    response: resp,
                    first,
                });
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < 4 {
                    retry_delay(attempt);
                    continue;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: "upstream stream failed".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        models_timeout_secs, post_with_timeouts, read_first_line, InferenceTimeouts,
        INFERENCE_TIMEOUTS,
    };
    use crate::config::GatewayConfig;

    fn bind_loopback() -> TcpListener {
        loop {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock upstream");
            if listener.local_addr().expect("mock address").port() != 8765 {
                return listener;
            }
        }
    }

    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set mock read timeout");
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut buf).expect("read mock request");
            assert!(read > 0, "gateway closed before request body completed");
            request.extend_from_slice(&buf[..read]);
            if expected_len.is_none() {
                if let Some(head_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let body_len = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                        .unwrap_or(0);
                    expected_len = Some(head_end + 4 + body_len);
                }
            }
            if expected_len.is_some_and(|len| request.len() >= len) {
                return;
            }
        }
    }

    fn spawn_stream(chunks: Vec<(Duration, &'static [u8])>) -> (String, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let total_len = chunks.iter().map(|(_, chunk)| chunk.len()).sum::<usize>();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {total_len}\r\n\r\n"
            )
            .expect("write mock response head");
            stream.flush().expect("flush mock response head");
            for (delay, chunk) in chunks {
                thread::sleep(delay);
                if stream.write_all(chunk).is_err() {
                    return;
                }
                if stream.flush().is_err() {
                    return;
                }
            }
        });
        (format!("http://{address}/v1/messages"), handle)
    }

    struct ServerRelease(Option<mpsc::Sender<()>>);

    impl ServerRelease {
        fn release(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    impl Drop for ServerRelease {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn spawn_blocked_stream(
        first: Option<&'static [u8]>,
    ) -> (
        String,
        ServerRelease,
        mpsc::Receiver<()>,
        thread::JoinHandle<()>,
    ) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let (release_tx, release_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            read_request(&mut stream);
            let declared_len = first.map_or(1, |chunk| chunk.len() + 1);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_len}\r\n\r\n"
            )
            .expect("write blocked response head");
            if let Some(chunk) = first {
                stream.write_all(chunk).expect("write first stream line");
            }
            stream.flush().expect("flush blocked response");
            blocked_tx.send(()).expect("signal blocked response");
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        (
            format!("http://{address}/v1/messages"),
            ServerRelease(Some(release_tx)),
            blocked_rx,
            handle,
        )
    }

    fn test_config(upstream_url: String) -> GatewayConfig {
        GatewayConfig {
            provider: "deepseek".to_string(),
            port: 0,
            auth_secret: None,
            api_key: "fake-key".to_string(),
            upstream_url,
            models_url: None,
            forced_model: None,
            relay_thinking: None,
            shim_mode: "off".to_string(),
            launch_id: "timeout-test".to_string(),
        }
    }

    #[test]
    fn inference_and_models_timeout_contracts_are_separate() {
        assert_eq!(INFERENCE_TIMEOUTS.connect, Duration::from_secs(300));
        assert_eq!(INFERENCE_TIMEOUTS.read_idle, Duration::from_secs(300));

        assert_eq!(models_timeout_secs("qwen"), 300);
        assert_eq!(models_timeout_secs("openai-custom"), 300);
        assert_eq!(models_timeout_secs("openai-responses"), 300);
        assert_eq!(models_timeout_secs("deepseek"), 120);
        assert_eq!(models_timeout_secs("relay"), 120);
    }

    #[test]
    fn active_stream_can_outlive_read_idle_timeout() {
        let read_idle = Duration::from_millis(500);
        let (url, upstream) = spawn_stream(vec![
            (Duration::ZERO, b"event: message_start\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
        ]);
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                read_idle,
            },
        )
        .expect("open active stream");
        let first = read_first_line(&mut response).expect("read first stream line");
        let started = Instant::now();
        let mut remaining = Vec::new();
        response
            .read_to_end(&mut remaining)
            .expect("active stream must not hit a total deadline");
        let elapsed = started.elapsed();

        assert_eq!(first, b"event: message_start\n");
        assert_eq!(
            remaining,
            b"data: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\n"
        );
        assert!(
            elapsed > read_idle,
            "stream should run longer than one idle window: {elapsed:?}"
        );
        upstream.join().expect("join active mock upstream");
    }

    #[test]
    fn stalled_stream_exceeding_read_idle_timeout_fails() {
        let read_idle = Duration::from_millis(250);
        let (url, mut release, blocked, upstream) =
            spawn_blocked_stream(Some(b"event: message_start\n"));
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                read_idle,
            },
        )
        .expect("open stalled stream");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("mock upstream must enter the controlled stall");
        assert_eq!(
            read_first_line(&mut response).expect("read first stream line"),
            b"event: message_start\n"
        );

        let started = Instant::now();
        let error = response
            .read_to_end(&mut Vec::new())
            .expect_err("stalled stream must hit the read-idle timeout");
        let elapsed = started.elapsed();
        assert!(
            !error.to_string().is_empty(),
            "stalled-stream error detail must not be empty"
        );
        assert!(elapsed >= read_idle, "timeout fired too early: {elapsed:?}");
        release.release();
        upstream.join().expect("join stalled mock upstream");
    }

    #[test]
    fn first_byte_idle_timeout_keeps_upstream_error_contract() {
        let read_idle = Duration::from_millis(250);
        let (url, mut release, blocked, upstream) = spawn_blocked_stream(None);
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                read_idle,
            },
        )
        .expect("receive mock response headers");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("mock upstream must enter the controlled first-byte stall");

        let started = Instant::now();
        let error = read_first_line(&mut response)
            .expect_err("missing first byte must hit the read-idle timeout");
        let elapsed = started.elapsed();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, None);
        assert!(!error.detail.is_empty());
        assert!(elapsed >= read_idle, "timeout fired too early: {elapsed:?}");
        release.release();
        upstream.join().expect("join first-byte mock upstream");
    }
}
