use std::fmt;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::{Client, Response};
use zeroize::Zeroizing;

use crate::codex_auth::InferenceSecrets;
use crate::config::{DEFAULT_CODEX_UPSTREAM_URL, UPSTREAM_UA};
use crate::provider_contracts::CodexRuntimeContract;

const CODEX_ORIGINATOR: &str = "codex_cli_rs";
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(crate) struct CodexTransport {
    client: Client,
    endpoint: String,
    request_timeout: Duration,
    read_idle_timeout: Duration,
}

#[derive(Clone, Default)]
pub(crate) struct CodexCancellation {
    cancelled: Arc<AtomicBool>,
}

impl CodexCancellation {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl fmt::Debug for CodexTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexTransport")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

pub(crate) struct CodexUpstream {
    response: Response,
    runtime: tokio::runtime::Runtime,
    cancellation: CodexCancellation,
    pending: Vec<u8>,
    pending_offset: usize,
    read_idle_timeout: Duration,
}

impl Read for CodexUpstream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.pending_offset < self.pending.len() {
            let available = &self.pending[self.pending_offset..];
            let length = available.len().min(buffer.len());
            buffer[..length].copy_from_slice(&available[..length]);
            self.pending_offset += length;
            if self.pending_offset == self.pending.len() {
                self.pending.clear();
                self.pending_offset = 0;
            }
            return Ok(length);
        }
        let cancellation = self.cancellation.clone();
        let read_idle_timeout = self.read_idle_timeout;
        let response = &mut self.response;
        let next = self.runtime.block_on(async move {
            tokio::select! {
                _ = wait_for_cancel(cancellation) => Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "Codex request cancelled",
                )),
                result = tokio::time::timeout(read_idle_timeout, response.chunk()) => match result {
                    Ok(result) => result
                        .map_err(|_| std::io::Error::other("Codex upstream read failed")),
                    Err(_) => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Codex upstream read timed out",
                    )),
                },
            }
        })?;
        let Some(chunk) = next else {
            return Ok(0);
        };
        let length = chunk.len().min(buffer.len());
        buffer[..length].copy_from_slice(&chunk[..length]);
        if length < chunk.len() {
            self.pending = chunk.to_vec();
            self.pending_offset = length;
        }
        Ok(length)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CodexTransportError {
    pub status: u16,
    pub upstream_status: Option<u16>,
    pub detail: &'static str,
    pub cancelled: bool,
}

impl fmt::Display for CodexTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for CodexTransportError {}

impl CodexTransport {
    pub(crate) fn production(contract: &CodexRuntimeContract) -> Result<Self, CodexTransportError> {
        Self::new(
            DEFAULT_CODEX_UPSTREAM_URL.to_string(),
            contract.connect_timeout,
            contract.request_timeout,
            contract.read_idle_timeout,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test(endpoint: String) -> Result<Self, CodexTransportError> {
        Self::new(
            endpoint,
            CONNECT_TIMEOUT,
            READ_IDLE_TIMEOUT,
            READ_IDLE_TIMEOUT,
        )
    }

    fn new(
        endpoint: String,
        connect_timeout: Duration,
        request_timeout: Duration,
        read_idle_timeout: Duration,
    ) -> Result<Self, CodexTransportError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|_| CodexTransportError {
                status: 502,
                upstream_status: None,
                detail: "Codex transport initialization failed",
                cancelled: false,
            })?;
        Ok(Self {
            client,
            endpoint,
            request_timeout,
            read_idle_timeout,
        })
    }

    /// Sends exactly one inference POST. Callers must never retry this method
    /// for the same Anthropic request, including on 401 or an empty 200.
    pub(crate) fn open_responses(
        &self,
        secrets: &InferenceSecrets,
        body: Vec<u8>,
        cancellation: CodexCancellation,
    ) -> Result<CodexUpstream, CodexTransportError> {
        let authorization = Zeroizing::new(format!("Bearer {}", secrets.access_token()));
        let mut authorization_header =
            HeaderValue::from_str(&authorization).map_err(|_| CodexTransportError {
                status: 401,
                upstream_status: None,
                detail: "Codex authorization is invalid",
                cancelled: false,
            })?;
        authorization_header.set_sensitive(true);
        let mut account_header =
            HeaderValue::from_str(secrets.account_id()).map_err(|_| CodexTransportError {
                status: 401,
                upstream_status: None,
                detail: "Codex account authorization is invalid",
                cancelled: false,
            })?;
        account_header.set_sensitive(true);
        let request = self
            .client
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .header(USER_AGENT, UPSTREAM_UA)
            .header("originator", CODEX_ORIGINATOR)
            .header("ChatGPT-Account-ID", account_header)
            .header(AUTHORIZATION, authorization_header)
            .body(body);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|_| CodexTransportError {
                status: 502,
                upstream_status: None,
                detail: "Codex transport runtime failed",
                cancelled: false,
            })?;
        let response = runtime.block_on(async {
            tokio::select! {
                _ = wait_for_cancel(cancellation.clone()) => Err(CodexTransportError {
                    status: 499,
                    upstream_status: None,
                    detail: "Codex request was cancelled",
                    cancelled: true,
                }),
                result = tokio::time::timeout(self.request_timeout, request.send()) => match result {
                    Ok(result) => result.map_err(|_| CodexTransportError {
                        status: 502,
                        upstream_status: None,
                        detail: "Codex upstream request failed",
                        cancelled: false,
                    }),
                    Err(_) => Err(CodexTransportError {
                        status: 504,
                        upstream_status: None,
                        detail: "Codex upstream response timed out",
                        cancelled: false,
                    }),
                },
            }
        })?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(CodexTransportError {
                status: if matches!(status, 401 | 403 | 429) {
                    status
                } else {
                    502
                },
                upstream_status: Some(status),
                detail: "Codex upstream rejected the request",
                cancelled: false,
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(CodexTransportError {
                status: 502,
                upstream_status: Some(status),
                detail: "Codex upstream returned an invalid content type",
                cancelled: false,
            });
        }
        Ok(CodexUpstream {
            response,
            runtime,
            cancellation,
            pending: Vec::new(),
            pending_offset: 0,
            read_idle_timeout: self.read_idle_timeout,
        })
    }
}

async fn wait_for_cancel(cancellation: CodexCancellation) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{CodexCancellation, CodexTransport};
    use crate::codex_auth::InferenceSecrets;

    fn bind_loopback() -> TcpListener {
        loop {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            if listener.local_addr().unwrap().port() != 8765 {
                return listener;
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut expected = None;
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if expected.is_none() {
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    expected = Some(end + 4 + length);
                }
            }
            if expected.is_some_and(|length| request.len() >= length) {
                return request;
            }
        }
    }

    fn mock_server(response: Vec<u8>) -> (String, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            tx.send(request).unwrap();
            stream.write_all(&response).unwrap();
            stream.flush().unwrap();
        });
        (format!("http://{address}/responses"), rx, handle)
    }

    fn secrets() -> InferenceSecrets {
        InferenceSecrets::for_test("access-secret", "account-secret")
    }

    #[test]
    fn single_post_uses_codex_headers_and_preserves_body() {
        let body = b"{\"stream\":true}".to_vec();
        let event = b"data: {\"type\":\"response.created\"}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            event.len()
        )
        .into_bytes();
        let mut response = response;
        response.extend_from_slice(event);
        let (endpoint, request_rx, handle) = mock_server(response);
        let transport = CodexTransport::for_test(endpoint).unwrap();
        let mut upstream = transport
            .open_responses(&secrets(), body.clone(), CodexCancellation::default())
            .unwrap();
        let mut received_body = Vec::new();
        upstream.read_to_end(&mut received_body).unwrap();
        assert_eq!(received_body, event);

        let request = request_rx.recv().unwrap();
        let head_end = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        assert!(head.starts_with("post /responses http/1.1"));
        assert!(head.contains("authorization: bearer access-secret"));
        assert!(head.contains("chatgpt-account-id: account-secret"));
        assert!(head.contains("originator: codex_cli_rs"));
        assert!(head.contains("accept: text/event-stream"));
        assert_eq!(&request[head_end + 4..], body);
        handle.join().unwrap();
    }

    #[test]
    fn upstream_errors_and_content_type_fail_without_a_second_post() {
        for response in [
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_vec(),
        ] {
            let (endpoint, request_rx, handle) = mock_server(response);
            let transport = CodexTransport::for_test(endpoint).unwrap();
            let error = transport
                .open_responses(
                    &secrets(),
                    b"{}".to_vec(),
                    CodexCancellation::default(),
                )
                .err()
                .unwrap();
            let request = request_rx.recv().unwrap();
            assert!(request.starts_with(b"POST "));
            if error.upstream_status == Some(401) {
                assert_eq!(error.status, 401);
            } else {
                assert_eq!(error.status, 502);
            }
            handle.join().unwrap();
        }
    }

    #[test]
    fn cancellation_aborts_first_byte_wait_without_reposting() {
        let listener = bind_loopback();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let requests_for_server = Arc::clone(&requests);
        let captured_for_server = Arc::clone(&captured);
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            requests_for_server.fetch_add(1, Ordering::SeqCst);
            *captured_for_server.lock().unwrap() = request;
            request_seen_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        let transport = CodexTransport::for_test(format!("http://{address}/responses")).unwrap();
        let cancellation = CodexCancellation::default();
        let cancellation_for_thread = cancellation.clone();
        let canceller = thread::spawn(move || {
            request_seen_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            cancellation_for_thread.cancel();
        });

        let started = Instant::now();
        let error = transport
            .open_responses(&secrets(), b"{}".to_vec(), cancellation)
            .err()
            .unwrap();
        assert!(error.cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(captured.lock().unwrap().starts_with(b"POST "));

        release_tx.send(()).unwrap();
        canceller.join().unwrap();
        server.join().unwrap();
    }
}
