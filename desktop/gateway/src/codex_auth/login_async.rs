use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use reqwest::header::CONTENT_TYPE;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

use super::oauth::{
    declared_response_kind, parse_new_oauth_tokens, response_kind, OAuthErrorCode, OAuthFlowError,
    CODEX_OAUTH_CLIENT_ID, CODEX_OAUTH_ISSUER, CODEX_OAUTH_ORIGINATOR, CODEX_OAUTH_SCOPE,
};
use super::storage::{AuthRepository, AuthStatus, SecretStore, StateStore};
use crate::codex_network::CodexHttpClientFactory;

const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_PORTS: &[u16] = &[1455, 1457];
const BROWSER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CALLBACK_HEAD: usize = 64 * 1024;
const MAX_CALLBACK_REQUESTS: usize = 64;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const CONTROL_RUNNING: u8 = 0;
const CONTROL_CANCELLED: u8 = 1;
const CONTROL_COMMITTING: u8 = 2;
const CONTROL_FINISHED: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelDisposition {
    Accepted,
    CommitInProgress,
    AlreadyTerminal,
}

impl CancelDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::CommitInProgress => "commit_in_progress",
            Self::AlreadyTerminal => "already_terminal",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginProgress {
    Waiting,
    Exchanging,
    Committing,
}

#[derive(Clone, Default)]
pub struct LoginControl {
    state: Arc<AtomicU8>,
}

impl LoginControl {
    pub fn cancel(&self) -> CancelDisposition {
        loop {
            match self.state.load(Ordering::Acquire) {
                CONTROL_RUNNING => {
                    if self
                        .state
                        .compare_exchange(
                            CONTROL_RUNNING,
                            CONTROL_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return CancelDisposition::Accepted;
                    }
                }
                CONTROL_CANCELLED => return CancelDisposition::Accepted,
                CONTROL_COMMITTING => return CancelDisposition::CommitInProgress,
                _ => return CancelDisposition::AlreadyTerminal,
            }
        }
    }

    fn begin_commit(&self) -> Result<(), OAuthFlowError> {
        self.state
            .compare_exchange(
                CONTROL_RUNNING,
                CONTROL_COMMITTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| cancelled_error("cancelled"))
    }

    fn finish(&self) {
        self.state.store(CONTROL_FINISHED, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CONTROL_CANCELLED
    }

    async fn cancelled(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[derive(Clone)]
struct AsyncLoginOptions {
    issuer: String,
    client_id: String,
    callback_ports: Vec<u16>,
    browser_timeout: Duration,
}

impl AsyncLoginOptions {
    fn production() -> Self {
        Self {
            issuer: CODEX_OAUTH_ISSUER.to_string(),
            client_id: CODEX_OAUTH_CLIENT_ID.to_string(),
            callback_ports: CALLBACK_PORTS.to_vec(),
            browser_timeout: BROWSER_TIMEOUT,
        }
    }
}

pub async fn run_production_login<S, T, F>(
    repository: &AuthRepository<S, T>,
    control: &LoginControl,
    progress: F,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: SecretStore,
    T: StateStore,
    F: Fn(LoginProgress),
{
    let factory = CodexHttpClientFactory::from_environment().map_err(|_| {
        OAuthFlowError::new(
            OAuthErrorCode::OAuthNetwork,
            false,
            "The Codex network route is invalid",
        )
        .at_stage("proxy_config")
    })?;
    let client = factory
        .async_builder()
        .map_err(|_| {
            OAuthFlowError::new(
                OAuthErrorCode::OAuthNetwork,
                false,
                "The Codex network route is invalid",
            )
            .at_stage("proxy_config")
        })?
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| {
            OAuthFlowError::new(
                OAuthErrorCode::OAuthNetwork,
                true,
                "The OAuth network client could not be created",
            )
            .at_stage("proxy_config")
        })?;
    let options = AsyncLoginOptions::production();
    let result = run_browser_login(
        repository,
        &client,
        factory.has_proxy(),
        &options,
        control,
        &progress,
    )
    .await;
    control.finish();
    result
}

async fn run_browser_login<S, T, F>(
    repository: &AuthRepository<S, T>,
    client: &reqwest::Client,
    has_proxy: bool,
    options: &AsyncLoginOptions,
    control: &LoginControl,
    progress: &F,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: SecretStore,
    T: StateStore,
    F: Fn(LoginProgress),
{
    run_browser_login_with_launcher(
        repository,
        client,
        has_proxy,
        options,
        control,
        progress,
        &SystemBrowserLauncher,
    )
    .await
}

trait BrowserLauncher {
    fn open<'a>(
        &'a self,
        url: &'a str,
        control: &'a LoginControl,
    ) -> Pin<Box<dyn Future<Output = Result<(), OAuthFlowError>> + 'a>>;
}

struct SystemBrowserLauncher;

impl BrowserLauncher for SystemBrowserLauncher {
    fn open<'a>(
        &'a self,
        url: &'a str,
        control: &'a LoginControl,
    ) -> Pin<Box<dyn Future<Output = Result<(), OAuthFlowError>> + 'a>> {
        Box::pin(open_browser(url, control))
    }
}

async fn run_browser_login_with_launcher<S, T, F, B>(
    repository: &AuthRepository<S, T>,
    client: &reqwest::Client,
    has_proxy: bool,
    options: &AsyncLoginOptions,
    control: &LoginControl,
    progress: &F,
    launcher: &B,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: SecretStore,
    T: StateStore,
    F: Fn(LoginProgress),
    B: BrowserLauncher,
{
    let guard = repository.begin_mutation().map_err(OAuthFlowError::from)?;
    let deadline = tokio::time::Instant::now() + options.browser_timeout;
    let (listener, port) = bind_callback(&options.callback_ports)?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let pkce = generate_pkce()?;
    let state = generate_state()?;
    let authorization_url = build_authorization_url(
        &options.issuer,
        &options.client_id,
        &redirect_uri,
        &pkce.challenge,
        &state,
    )?;
    launcher.open(&authorization_url, control).await?;
    progress(LoginProgress::Waiting);

    let mut request_count = 0_usize;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(OAuthFlowError::new(
                OAuthErrorCode::CallbackTimeout,
                true,
                "Codex sign-in timed out",
            )
            .at_stage("callback_wait"));
        }
        let accepted = tokio::select! {
            _ = control.cancelled() => return Err(cancelled_error("callback_wait")),
            result = tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                listener.accept(),
            ) => result,
        };
        let (mut stream, _) = match accepted {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => {
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::CallbackUnavailable,
                    true,
                    "The local OAuth callback stopped unexpectedly",
                )
                .at_stage("callback_wait"))
            }
            Err(_) => {
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::CallbackTimeout,
                    true,
                    "Codex sign-in timed out",
                )
                .at_stage("callback_wait"))
            }
        };
        request_count += 1;
        if request_count > MAX_CALLBACK_REQUESTS {
            return Err(
                OAuthFlowError::protocol("Too many OAuth callbacks").at_stage("callback_wait")
            );
        }
        let action = match read_and_parse_callback(&mut stream, port, &state, control).await {
            Ok(action) => action,
            Err(error) if error.code == OAuthErrorCode::AuthCancelled => return Err(error),
            Err(_) => {
                write_callback(&mut stream, 400, "Sign-in request rejected").await;
                continue;
            }
        };
        match action {
            CallbackAction::Ignore(status) => {
                write_callback(&mut stream, status, "Sign-in request rejected").await;
            }
            CallbackAction::Denied => {
                write_callback(&mut stream, 400, "Sign-in was not completed").await;
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::OAuthDenied,
                    false,
                    "Codex sign-in was denied or cancelled",
                )
                .at_stage("callback_wait"));
            }
            CallbackAction::Code(mut code) => {
                progress(LoginProgress::Exchanging);
                let tokens = exchange_code(
                    client,
                    has_proxy,
                    options,
                    &redirect_uri,
                    &code,
                    &pkce.verifier,
                    control,
                )
                .await;
                code.zeroize();
                match tokens {
                    Ok(tokens) => {
                        let result = commit_login(repository, &guard, tokens, control, progress);
                        if result.is_ok() {
                            write_callback(
                                &mut stream,
                                200,
                                "Codex sign-in completed. You can close this window.",
                            )
                            .await;
                        } else {
                            write_callback(
                                &mut stream,
                                500,
                                "Codex sign-in could not be completed safely.",
                            )
                            .await;
                        }
                        return result;
                    }
                    Err(error) => {
                        write_callback(
                            &mut stream,
                            500,
                            "Codex sign-in could not be completed safely.",
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
        }
    }
}

fn commit_login<S, T, F>(
    repository: &AuthRepository<S, T>,
    guard: &super::storage::AuthMutationGuard,
    tokens: super::storage::NewOAuthTokens,
    control: &LoginControl,
    progress: &F,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: SecretStore,
    T: StateStore,
    F: Fn(LoginProgress),
{
    control.begin_commit()?;
    progress(LoginProgress::Committing);
    repository
        .commit_login_guarded(guard, tokens)
        .map_err(|error| OAuthFlowError::from(error).at_stage("credential_commit"))
}

async fn exchange_code(
    client: &reqwest::Client,
    has_proxy: bool,
    options: &AsyncLoginOptions,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
    control: &LoginControl,
) -> Result<super::storage::NewOAuthTokens, OAuthFlowError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", &options.client_id)
        .append_pair("code_verifier", verifier)
        .finish();
    let endpoint = format!("{}/oauth/token", options.issuer.trim_end_matches('/'));
    let response = send_request(
        client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("user-agent", crate::config::UPSTREAM_UA)
            .body(body),
        control,
        has_proxy,
        "token_exchange",
    )
    .await?;
    let response = read_response(response, control, "token_exchange").await?;
    reject_challenge(&response, "token_exchange")?;
    require_json_success(&response, "token_exchange")?;
    parse_new_oauth_tokens(&response.body).map_err(|error| {
        error.at_stage("token_exchange").with_http(
            Some(response.status),
            Some(response.kind),
            Some(false),
        )
    })
}

struct ResponseData {
    status: u16,
    kind: &'static str,
    challenge: bool,
    body: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for ResponseData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseData")
            .field("status", &self.status)
            .field("kind", &self.kind)
            .field("challenge", &self.challenge)
            .field("body_len", &self.body.len())
            .finish()
    }
}

async fn send_request(
    request: reqwest::RequestBuilder,
    control: &LoginControl,
    has_proxy: bool,
    stage: &'static str,
) -> Result<reqwest::Response, OAuthFlowError> {
    let result = tokio::select! {
        _ = control.cancelled() => return Err(cancelled_error(stage)),
        result = tokio::time::timeout(REQUEST_TIMEOUT, request.send()) => result,
    };
    match result {
        Ok(Ok(response)) => Ok(response),
        Err(_) => Err(OAuthFlowError::new(
            OAuthErrorCode::OAuthNetwork,
            true,
            "The OAuth request timed out",
        )
        .at_stage(stage)
        .with_transport("timeout")),
        Ok(Err(error)) => {
            let (code, transport) = classify_request_error(&error, has_proxy);
            Err(
                OAuthFlowError::new(code, true, "The OAuth request could not connect")
                    .at_stage(stage)
                    .with_transport(transport),
            )
        }
    }
}

pub(super) fn classify_request_error(
    error: &reqwest::Error,
    has_proxy: bool,
) -> (OAuthErrorCode, &'static str) {
    if error.is_timeout() {
        (OAuthErrorCode::OAuthNetwork, "timeout")
    } else if has_tls_source(error) {
        (OAuthErrorCode::TlsFailed, "tls")
    } else if error.is_connect() && has_proxy {
        (OAuthErrorCode::ProxyConnectFailed, "proxy_connect")
    } else {
        (OAuthErrorCode::OAuthNetwork, "unknown")
    }
}

fn has_tls_source(error: &reqwest::Error) -> bool {
    fn visit(error: &(dyn std::error::Error + 'static), depth: usize) -> bool {
        if depth > 12 || error.downcast_ref::<rustls::Error>().is_some() {
            return depth <= 12;
        }
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            if visit(inner, depth + 1) {
                return true;
            }
        }
        error
            .source()
            .is_some_and(|source| visit(source, depth + 1))
    }
    visit(error, 0)
}

async fn read_response(
    mut response: reqwest::Response,
    control: &LoginControl,
    stage: &'static str,
) -> Result<ResponseData, OAuthFlowError> {
    let status = response.status().as_u16();
    let challenge = response
        .headers()
        .get("cf-mitigated")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("challenge"));
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    let declared_kind = declared_response_kind(content_type);
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    let mut body = Zeroizing::new(Vec::new());
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(OAuthFlowError::new(
                OAuthErrorCode::OAuthNetwork,
                true,
                "The OAuth response timed out",
            )
            .at_stage(stage)
            .with_transport("timeout"));
        }
        let chunk = tokio::select! {
            _ = control.cancelled() => return Err(cancelled_error(stage)),
            result = tokio::time::timeout(remaining, response.chunk()) => result,
        };
        match chunk {
            Ok(Ok(Some(chunk))) => {
                if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(OAuthFlowError::protocol("The OAuth response is too large")
                        .at_stage(stage)
                        .with_http(Some(status), Some(declared_kind), Some(challenge)));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) => {
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth response could not be read",
                )
                .at_stage(stage)
                .with_http(Some(status), Some(declared_kind), Some(challenge))
                .with_transport("http"))
            }
            Err(_) => {
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth response timed out",
                )
                .at_stage(stage)
                .with_http(Some(status), Some(declared_kind), Some(challenge))
                .with_transport("timeout"))
            }
        }
    }
    let kind = response_kind(declared_kind, body.is_empty());
    Ok(ResponseData {
        status,
        kind,
        challenge,
        body,
    })
}

fn reject_challenge(response: &ResponseData, stage: &'static str) -> Result<(), OAuthFlowError> {
    if response.challenge {
        Err(OAuthFlowError::new(
            OAuthErrorCode::OAuthChallengeResponse,
            true,
            "The OAuth endpoint returned a challenge response",
        )
        .at_stage(stage)
        .with_http(Some(response.status), Some(response.kind), Some(true)))
    } else {
        Ok(())
    }
}

fn require_json_success(
    response: &ResponseData,
    stage: &'static str,
) -> Result<(), OAuthFlowError> {
    if matches!(response.kind, "html" | "other" | "unknown") {
        return Err(unexpected_content_type(response, stage));
    }
    if !(200..300).contains(&response.status) {
        return Err(OAuthFlowError::protocol("The OAuth request was rejected")
            .at_stage(stage)
            .with_http(Some(response.status), Some(response.kind), Some(false)));
    }
    if response.kind != "json" {
        return Err(unexpected_content_type(response, stage));
    }
    Ok(())
}

fn unexpected_content_type(response: &ResponseData, stage: &'static str) -> OAuthFlowError {
    OAuthFlowError::new(
        OAuthErrorCode::OAuthUnexpectedContentType,
        true,
        "The OAuth endpoint returned an unexpected content type",
    )
    .at_stage(stage)
    .with_http(Some(response.status), Some(response.kind), Some(false))
}

fn cancelled_error(stage: &'static str) -> OAuthFlowError {
    OAuthFlowError::new(
        OAuthErrorCode::AuthCancelled,
        true,
        "Codex sign-in was cancelled",
    )
    .at_stage(stage)
}

struct PkceCodes {
    verifier: Zeroizing<String>,
    challenge: String,
}

fn generate_pkce() -> Result<PkceCodes, OAuthFlowError> {
    let mut random = [0_u8; 64];
    getrandom::getrandom(&mut random).map_err(|_| {
        OAuthFlowError::new(
            OAuthErrorCode::Storage,
            false,
            "Secure random generation failed",
        )
    })?;
    let verifier = Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random));
    random.zeroize();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Ok(PkceCodes {
        verifier,
        challenge,
    })
}

fn generate_state() -> Result<Zeroizing<String>, OAuthFlowError> {
    let mut random = [0_u8; 32];
    getrandom::getrandom(&mut random).map_err(|_| {
        OAuthFlowError::new(
            OAuthErrorCode::Storage,
            false,
            "Secure random generation failed",
        )
    })?;
    let state = Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random));
    random.zeroize();
    Ok(state)
}

fn build_authorization_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<Zeroizing<String>, OAuthFlowError> {
    let mut url = url::Url::parse(issuer)
        .map_err(|_| OAuthFlowError::protocol("The OAuth issuer is invalid"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(OAuthFlowError::protocol("The OAuth issuer is invalid"));
    }
    url.set_path("/oauth/authorize");
    url.set_query(None);
    url.set_fragment(None);
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", CODEX_OAUTH_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", CODEX_OAUTH_ORIGINATOR);
    Ok(Zeroizing::new(url.into()))
}

async fn open_browser(url: &str, control: &LoginControl) -> Result<(), OAuthFlowError> {
    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("/usr/bin/open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::BrowserOpenFailed,
                    true,
                    "The system browser could not be opened",
                )
                .at_stage("browser_open")
            })?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) | Err(_) => {
                    return Err(OAuthFlowError::new(
                        OAuthErrorCode::BrowserOpenFailed,
                        true,
                        "The system browser could not be opened",
                    )
                    .at_stage("browser_open"))
                }
                Ok(None) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::BrowserOpenFailed,
                    true,
                    "The system browser did not return promptly",
                )
                .at_stage("browser_open"));
            }
            tokio::select! {
                _ = control.cancelled() => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(cancelled_error("browser_open"));
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (url, control);
        Err(OAuthFlowError::new(
            OAuthErrorCode::BrowserOpenFailed,
            false,
            "Codex browser login is supported only on macOS",
        )
        .at_stage("browser_open"))
    }
}

fn bind_callback(ports: &[u16]) -> Result<(tokio::net::TcpListener, u16), OAuthFlowError> {
    for port in ports {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", *port)) {
            listener.set_nonblocking(true).map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::CallbackUnavailable,
                    true,
                    "The local OAuth callback could not be configured",
                )
                .at_stage("callback_wait")
            })?;
            let actual = listener.local_addr().map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::CallbackUnavailable,
                    true,
                    "The local OAuth callback address could not be read",
                )
                .at_stage("callback_wait")
            })?;
            return tokio::net::TcpListener::from_std(listener)
                .map(|listener| (listener, actual.port()))
                .map_err(|_| {
                    OAuthFlowError::new(
                        OAuthErrorCode::CallbackUnavailable,
                        true,
                        "The local OAuth callback could not be configured",
                    )
                    .at_stage("callback_wait")
                });
        }
    }
    Err(OAuthFlowError::new(
        OAuthErrorCode::CallbackUnavailable,
        true,
        "OAuth callback ports 1455 and 1457 are unavailable",
    )
    .at_stage("callback_wait"))
}

enum CallbackAction {
    Ignore(u16),
    Denied,
    Code(Zeroizing<String>),
}

async fn read_and_parse_callback(
    stream: &mut tokio::net::TcpStream,
    port: u16,
    expected_state: &str,
    control: &LoginControl,
) -> Result<CallbackAction, OAuthFlowError> {
    let deadline = tokio::time::Instant::now() + CALLBACK_IO_TIMEOUT;
    let mut head = Zeroizing::new(Vec::with_capacity(2048));
    let mut chunk = [0_u8; 1024];
    while !head.ends_with(b"\r\n\r\n") {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(
                OAuthFlowError::protocol("The OAuth callback timed out").at_stage("callback_wait")
            );
        }
        let read = tokio::select! {
            _ = control.cancelled() => return Err(cancelled_error("callback_wait")),
            result = tokio::time::timeout(remaining, stream.read(&mut chunk)) => result,
        };
        let read = read
            .map_err(|_| OAuthFlowError::protocol("The OAuth callback timed out"))?
            .map_err(|_| OAuthFlowError::protocol("The OAuth callback could not be read"))?;
        if read == 0 || head.len().saturating_add(read) > MAX_CALLBACK_HEAD {
            return Err(
                OAuthFlowError::protocol("The OAuth callback is invalid").at_stage("callback_wait")
            );
        }
        head.extend_from_slice(&chunk[..read]);
    }
    parse_callback_head(&head, port, expected_state)
}

fn parse_callback_head(
    head: &[u8],
    port: u16,
    expected_state: &str,
) -> Result<CallbackAction, OAuthFlowError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| OAuthFlowError::protocol("The OAuth callback is invalid"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| OAuthFlowError::protocol("The OAuth callback is invalid"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "GET" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(OAuthFlowError::protocol("The OAuth callback is invalid"));
    }
    let mut host = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(OAuthFlowError::protocol("The OAuth callback is invalid"));
        };
        if name.trim().eq_ignore_ascii_case("host")
            && host.replace(value.trim().to_ascii_lowercase()).is_some()
        {
            return Err(OAuthFlowError::protocol("The OAuth callback is invalid"));
        }
    }
    if !matches!(host.as_deref(), Some(value) if value == format!("localhost:{port}") || value == format!("127.0.0.1:{port}"))
    {
        return Err(OAuthFlowError::protocol(
            "The OAuth callback host is invalid",
        ));
    }
    if !target.starts_with('/') || target.starts_with("//") || target.contains('#') {
        return Err(OAuthFlowError::protocol(
            "The OAuth callback URL is invalid",
        ));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return Ok(CallbackAction::Ignore(404));
    }
    let mut seen = HashSet::new();
    let mut callback_state = None;
    let mut code = None;
    let mut denied = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key = Zeroizing::new(key.into_owned());
        let digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        if !seen.insert(digest) {
            return Err(OAuthFlowError::protocol(
                "The OAuth callback contains duplicate fields",
            ));
        }
        let value = Zeroizing::new(value.into_owned());
        match key.as_str() {
            "state" => callback_state = Some(value),
            "code" => code = Some(value),
            "error" => denied = !value.is_empty(),
            _ => {}
        }
    }
    if callback_state.as_deref().map(String::as_str) != Some(expected_state) {
        return Err(OAuthFlowError::protocol(
            "The OAuth callback state is invalid",
        ));
    }
    if denied {
        return Ok(CallbackAction::Denied);
    }
    Ok(CallbackAction::Code(
        code.filter(|value| !value.is_empty())
            .ok_or_else(|| OAuthFlowError::protocol("The OAuth callback code is missing"))?,
    ))
}

async fn write_callback(stream: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Bad Request",
    };
    let body = format!("<html><body>{message}</body></html>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::thread;

    type SecretMap = HashMap<(String, String), Vec<u8>>;

    #[derive(Clone, Default)]
    struct MemorySecrets(Arc<Mutex<SecretMap>>);

    impl SecretStore for MemorySecrets {
        fn load(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<Vec<u8>>, super::super::storage::StorageError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(service.to_string(), account.to_string()))
                .cloned())
        }

        fn save(
            &self,
            service: &str,
            account: &str,
            value: &[u8],
        ) -> Result<(), super::super::storage::StorageError> {
            self.0
                .lock()
                .unwrap()
                .insert((service.to_string(), account.to_string()), value.to_vec());
            Ok(())
        }

        fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), super::super::storage::StorageError> {
            self.0
                .lock()
                .unwrap()
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct MemoryState(Arc<Mutex<Option<super::super::storage::AuthState>>>);

    impl StateStore for MemoryState {
        fn load(
            &self,
        ) -> Result<Option<super::super::storage::AuthState>, super::super::storage::StorageError>
        {
            Ok(self.0.lock().unwrap().clone())
        }

        fn commit(
            &self,
            state: &super::super::storage::AuthState,
        ) -> Result<(), super::super::storage::StorageError> {
            *self.0.lock().unwrap() = Some(state.clone());
            Ok(())
        }
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::getrandom(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "csswitch-browser-login-test-{}-{}",
                std::process::id(),
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TestBrowserLauncher<F>(F);

    impl<F> BrowserLauncher for TestBrowserLauncher<F>
    where
        F: Fn(&str) -> Result<(), OAuthFlowError>,
    {
        fn open<'a>(
            &'a self,
            url: &'a str,
            _control: &'a LoginControl,
        ) -> Pin<Box<dyn Future<Output = Result<(), OAuthFlowError>> + 'a>> {
            let result = (self.0)(url);
            Box::pin(async move { result })
        }
    }

    fn accept_browser_open(_: &str) -> Result<(), OAuthFlowError> {
        Ok(())
    }

    fn repository(root: &TempRoot) -> AuthRepository<MemorySecrets, MemoryState> {
        AuthRepository::new(
            MemorySecrets::default(),
            MemoryState::default(),
            root.0.clone(),
        )
    }

    fn test_options(issuer: String, browser_timeout: Duration) -> AsyncLoginOptions {
        AsyncLoginOptions {
            issuer,
            client_id: "client-test".into(),
            callback_ports: vec![0],
            browser_timeout,
        }
    }

    fn jwt(claims: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
            let Some(head_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let head_end = head_end + 4;
            let head = String::from_utf8_lossy(&request[..head_end]);
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            if request.len() >= head_end + content_length {
                return request;
            }
        }
    }

    fn spawn_token_server(body: Vec<u8>) -> (String, Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let request = Arc::new(Mutex::new(Vec::new()));
        let request_out = request.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            *request_out.lock().unwrap() = read_http_request(&mut stream);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });
        (format!("http://{address}"), request, handle)
    }

    fn send_callback(redirect_uri: &str, query: &str, host: Option<&str>) -> Vec<u8> {
        let redirect = url::Url::parse(redirect_uri).unwrap();
        let port = redirect.port().unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(
                format!(
                    "GET {}?{query} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    redirect.path(),
                    host.map(str::to_string)
                        .unwrap_or_else(|| format!("localhost:{port}"))
                )
                .as_bytes(),
            )
            .unwrap();
        stream.flush().unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    fn direct_client() -> reqwest::Client {
        CodexHttpClientFactory::direct_for_test()
            .async_builder()
            .unwrap()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[test]
    fn cancel_and_commit_use_one_atomic_barrier() {
        let cancel_first = LoginControl::default();
        assert_eq!(cancel_first.cancel(), CancelDisposition::Accepted);
        assert_eq!(
            cancel_first.begin_commit().unwrap_err().code,
            OAuthErrorCode::AuthCancelled
        );

        let commit_first = LoginControl::default();
        commit_first.begin_commit().unwrap();
        assert_eq!(commit_first.cancel(), CancelDisposition::CommitInProgress);
    }

    #[test]
    fn browser_authorization_contract_keeps_pkce_state_and_scope() {
        let url = build_authorization_url(
            CODEX_OAUTH_ISSUER,
            CODEX_OAUTH_CLIENT_ID,
            "http://localhost:1455/auth/callback",
            "challenge",
            "state",
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let fields = parsed
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            fields.get("scope").map(String::as_str),
            Some(CODEX_OAUTH_SCOPE)
        );
        assert_eq!(fields.get("state").map(String::as_str), Some("state"));
        assert_eq!(
            fields.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_browser_flow_rejects_bad_host_and_state_then_exchanges_and_commits() {
        let id_token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"},
            "exp": 2_000_000_000_i64
        }));
        let access_token = jwt(serde_json::json!({
            "chatgpt_account_id": "acct-test",
            "exp": 2_000_000_000_i64
        }));
        let token_body = serde_json::to_vec(&serde_json::json!({
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "refresh-test"
        }))
        .unwrap();
        let (issuer, token_request, token_server) = spawn_token_server(token_body);
        let callback = Arc::new(Mutex::new(None));
        let callback_out = callback.clone();
        let launcher = TestBrowserLauncher(move |authorization_url: &str| {
            let parsed = url::Url::parse(authorization_url).unwrap();
            let fields = parsed.query_pairs().into_owned().collect::<HashMap<_, _>>();
            let redirect_uri = fields.get("redirect_uri").unwrap().clone();
            let state = fields.get("state").unwrap().clone();
            assert_eq!(
                fields.get("code_challenge_method").map(String::as_str),
                Some("S256")
            );
            assert!(fields
                .get("code_challenge")
                .is_some_and(|challenge| challenge.len() >= 43));
            let handle = thread::spawn(move || {
                let bad_host = send_callback(
                    &redirect_uri,
                    &format!("code=authorization-code&state={state}"),
                    Some("attacker.invalid"),
                );
                assert!(bad_host.starts_with(b"HTTP/1.1 400"));
                let bad_state = send_callback(
                    &redirect_uri,
                    "code=authorization-code&state=wrong-state",
                    None,
                );
                assert!(bad_state.starts_with(b"HTTP/1.1 400"));
                let success = send_callback(
                    &redirect_uri,
                    &format!("code=authorization-code&state={state}"),
                    None,
                );
                assert!(success.starts_with(b"HTTP/1.1 200"));
            });
            *callback_out.lock().unwrap() = Some(handle);
            Ok(())
        });
        let root = TempRoot::new();
        let repository = repository(&root);
        let progress = Arc::new(Mutex::new(Vec::new()));
        let progress_out = progress.clone();
        let status = run_browser_login_with_launcher(
            &repository,
            &direct_client(),
            false,
            &test_options(issuer, Duration::from_secs(2)),
            &LoginControl::default(),
            &move |event| progress_out.lock().unwrap().push(event),
            &launcher,
        )
        .await
        .unwrap();

        callback.lock().unwrap().take().unwrap().join().unwrap();
        token_server.join().unwrap();
        assert!(status.authenticated);
        assert_eq!(status.auth_generation, 1);
        assert_eq!(
            *progress.lock().unwrap(),
            [
                LoginProgress::Waiting,
                LoginProgress::Exchanging,
                LoginProgress::Committing,
            ]
        );
        let request = String::from_utf8_lossy(&token_request.lock().unwrap()).to_string();
        assert!(request.starts_with("POST /oauth/token HTTP/1.1"));
        assert!(request.contains("code=authorization-code"));
        assert!(request.contains("code_verifier="));
        assert!(repository.status().unwrap().authenticated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_browser_denial_and_timeout_never_commit() {
        let denied_callback = Arc::new(Mutex::new(None));
        let denied_callback_out = denied_callback.clone();
        let denied_launcher = TestBrowserLauncher(move |authorization_url: &str| {
            let parsed = url::Url::parse(authorization_url).unwrap();
            let fields = parsed.query_pairs().into_owned().collect::<HashMap<_, _>>();
            let redirect_uri = fields.get("redirect_uri").unwrap().clone();
            let state = fields.get("state").unwrap().clone();
            let handle = thread::spawn(move || {
                let response = send_callback(
                    &redirect_uri,
                    &format!("error=access_denied&state={state}"),
                    None,
                );
                assert!(response.starts_with(b"HTTP/1.1 400"));
            });
            *denied_callback_out.lock().unwrap() = Some(handle);
            Ok(())
        });
        let denied_root = TempRoot::new();
        let denied_repository = repository(&denied_root);
        let denied = run_browser_login_with_launcher(
            &denied_repository,
            &direct_client(),
            false,
            &test_options("https://auth.openai.com".into(), Duration::from_secs(2)),
            &LoginControl::default(),
            &|_| {},
            &denied_launcher,
        )
        .await
        .unwrap_err();
        denied_callback
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(denied.code, OAuthErrorCode::OAuthDenied);
        assert!(!denied_repository.status().unwrap().authenticated);
        assert_eq!(denied_repository.status().unwrap().auth_generation, 0);

        let timeout_root = TempRoot::new();
        let timeout_repository = repository(&timeout_root);
        let timeout = run_browser_login_with_launcher(
            &timeout_repository,
            &direct_client(),
            false,
            &test_options("https://auth.openai.com".into(), Duration::from_millis(50)),
            &LoginControl::default(),
            &|_| {},
            &TestBrowserLauncher(accept_browser_open),
        )
        .await
        .unwrap_err();
        assert_eq!(timeout.code, OAuthErrorCode::CallbackTimeout);
        assert!(!timeout_repository.status().unwrap().authenticated);
        assert_eq!(timeout_repository.status().unwrap().auth_generation, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hung_header_and_slow_body_are_cancellable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(500));
        });
        let control = LoginControl::default();
        let cancel = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let error = send_request(
            direct_client().get(format!("http://{address}/hung")),
            &control,
            false,
            "token_exchange",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::AuthCancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 20\r\nconnection: close\r\n\r\n{",
            );
            let _ = stream.flush();
            thread::sleep(Duration::from_secs(2));
        });
        let control = LoginControl::default();
        let response = send_request(
            direct_client().get(format!("http://{address}/slow")),
            &control,
            false,
            "token_exchange",
        )
        .await
        .unwrap();
        let cancel = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let error = read_response(response, &control, "token_exchange")
            .await
            .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::AuthCancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stable_rustls_source_is_classified_as_tls_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut hello = [0_u8; 1024];
            let _ = stream.read(&mut hello);
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            let _ = stream.flush();
        });
        let error = send_request(
            direct_client().get(format!("https://{address}/tls")),
            &LoginControl::default(),
            false,
            "token_exchange",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::TlsFailed);
        assert_eq!(error.transport_kind, Some("tls"));
        server.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unresponsive_connect_proxy_is_cancellable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_secs(2));
        });
        let route = csswitch_codex_network::resolve(
            &csswitch_codex_network::CodexNetworkSettings {
                mode: csswitch_codex_network::CodexNetworkMode::Custom,
                proxy_url: format!("http://{proxy}"),
            },
            &csswitch_codex_network::EnvironmentSnapshot::default(),
        )
        .unwrap();
        let client = CodexHttpClientFactory::for_test_route(route)
            .async_builder()
            .unwrap()
            .build()
            .unwrap();
        let control = LoginControl::default();
        let cancel = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let error = send_request(
            client.get("https://unresolvable.test:443/oauth/token"),
            &control,
            true,
            "token_exchange",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::AuthCancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refused_proxy_is_classified_without_exposing_url() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy = listener.local_addr().unwrap();
        drop(listener);
        let route = csswitch_codex_network::resolve(
            &csswitch_codex_network::CodexNetworkSettings {
                mode: csswitch_codex_network::CodexNetworkMode::Custom,
                proxy_url: format!("http://{proxy}"),
            },
            &csswitch_codex_network::EnvironmentSnapshot::default(),
        )
        .unwrap();
        let client = CodexHttpClientFactory::for_test_route(route)
            .async_builder()
            .unwrap()
            .build()
            .unwrap();
        let error = send_request(
            client.get("https://unresolvable.test:443/oauth/token"),
            &LoginControl::default(),
            true,
            "token_exchange",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::ProxyConnectFailed);
        assert_eq!(error.transport_kind, Some("proxy_connect"));
        assert!(!format!("{error:?} {error}").contains(&proxy.to_string()));
    }
}
