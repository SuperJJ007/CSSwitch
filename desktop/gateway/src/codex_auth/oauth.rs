use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use super::storage::{
    AuthRepository, AuthStatus, NewOAuthTokens, RefreshUpdate, RevokeToken, RevokeTokenKind,
    SecretStore, StateStore, StorageError,
};

pub const CODEX_OAUTH_ISSUER: &str = "https://auth.openai.com";
pub const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_OAUTH_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const CODEX_OAUTH_ORIGINATOR: &str = "codex_cli_rs";

const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_PORTS: &[u16] = &[1455, 1457];
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const BROWSER_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const CALLBACK_IO_TIMEOUT: Duration = Duration::from_secs(2);
const CALLBACK_POLL: Duration = Duration::from_millis(20);
const MAX_CALLBACK_HEAD: usize = 64 * 1024;
const MAX_CALLBACK_REQUESTS: usize = 64;
const MAX_TOKEN_RESPONSE: u64 = 1024 * 1024;
const MAX_JWT_PAYLOAD: usize = 256 * 1024;
const MAX_TOKEN_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthErrorCode {
    AuthBusy,
    AuthChanged,
    AuthStateInvalid,
    BrowserOpenFailed,
    CallbackTimeout,
    CallbackUnavailable,
    KeychainUnavailable,
    NotAuthenticated,
    OAuthDenied,
    OAuthNetwork,
    OAuthProtocol,
    Storage,
    UnsupportedPlatform,
}

impl OAuthErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthBusy => "auth_busy",
            Self::AuthChanged => "auth_changed",
            Self::AuthStateInvalid => "auth_state_invalid",
            Self::BrowserOpenFailed => "browser_open_failed",
            Self::CallbackTimeout => "callback_timeout",
            Self::CallbackUnavailable => "callback_unavailable",
            Self::KeychainUnavailable => "keychain_unavailable",
            Self::NotAuthenticated => "not_authenticated",
            Self::OAuthDenied => "oauth_denied",
            Self::OAuthNetwork => "oauth_network_error",
            Self::OAuthProtocol => "oauth_protocol_error",
            Self::Storage => "auth_storage_error",
            Self::UnsupportedPlatform => "unsupported_platform",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthFlowError {
    pub code: OAuthErrorCode,
    pub retryable: bool,
    pub message: &'static str,
}

impl fmt::Debug for OAuthFlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlowError")
            .field("code", &self.code)
            .field("retryable", &self.retryable)
            .field("message", &self.message)
            .finish()
    }
}

impl fmt::Display for OAuthFlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for OAuthFlowError {}

impl From<StorageError> for OAuthFlowError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Busy => Self::new(
                OAuthErrorCode::AuthBusy,
                true,
                "Another Codex auth operation is in progress",
            ),
            StorageError::AuthChanged => Self::new(
                OAuthErrorCode::AuthChanged,
                true,
                "Codex authentication changed during the operation",
            ),
            StorageError::InvalidState(_) => Self::new(
                OAuthErrorCode::AuthStateInvalid,
                false,
                "Codex authentication state is invalid",
            ),
            StorageError::KeychainUnavailable(_) => Self::new(
                OAuthErrorCode::KeychainUnavailable,
                false,
                "The CSSwitch Codex Keychain item is unavailable",
            ),
            StorageError::NotAuthenticated => Self::new(
                OAuthErrorCode::NotAuthenticated,
                false,
                "Codex is not signed in through CSSwitch",
            ),
            StorageError::UnsupportedPlatform => Self::new(
                OAuthErrorCode::UnsupportedPlatform,
                false,
                "Codex Keychain authentication is supported only on macOS",
            ),
            StorageError::Unavailable(_) | StorageError::RollbackFailed => Self::new(
                OAuthErrorCode::Storage,
                false,
                "Codex credentials could not be stored safely",
            ),
        }
    }
}

impl OAuthFlowError {
    fn new(code: OAuthErrorCode, retryable: bool, message: &'static str) -> Self {
        Self {
            code,
            retryable,
            message,
        }
    }

    fn protocol(message: &'static str) -> Self {
        Self::new(OAuthErrorCode::OAuthProtocol, false, message)
    }
}

pub trait BrowserOpener: Send + Sync {
    fn open(&self, authorization_url: &str, login_deadline: Instant) -> Result<(), OAuthFlowError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBrowser;

impl BrowserOpener for SystemBrowser {
    fn open(&self, authorization_url: &str, login_deadline: Instant) -> Result<(), OAuthFlowError> {
        #[cfg(target_os = "macos")]
        {
            let mut child = Command::new("/usr/bin/open")
                .arg(authorization_url)
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
                })?;
            let deadline = login_deadline.min(Instant::now() + BROWSER_OPEN_TIMEOUT);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => return Ok(()),
                    Ok(Some(_)) | Err(_) => {
                        return Err(OAuthFlowError::new(
                            OAuthErrorCode::BrowserOpenFailed,
                            true,
                            "The system browser could not be opened",
                        ));
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(OAuthFlowError::new(
                            OAuthErrorCode::BrowserOpenFailed,
                            true,
                            "The system browser did not return promptly",
                        ));
                    }
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (authorization_url, login_deadline);
            Err(OAuthFlowError::new(
                OAuthErrorCode::BrowserOpenFailed,
                false,
                "Codex browser login is supported only on macOS",
            ))
        }
    }
}

pub trait OAuthTransport: Send + Sync {
    fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
        login_deadline: Instant,
    ) -> Result<NewOAuthTokens, OAuthFlowError>;
}

pub trait RefreshTransport: Send + Sync {
    fn refresh(&self, refresh_token: &str) -> Result<RefreshUpdate, OAuthFlowError>;
}

pub trait RevokeTransport: Send + Sync {
    fn revoke(&self, token: &RevokeToken) -> Result<(), OAuthFlowError>;
}

#[derive(Clone)]
pub struct HttpOAuthTransport {
    token_endpoint: String,
    revoke_endpoint: String,
    client_id: String,
    client: Client,
}

impl fmt::Debug for HttpOAuthTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpOAuthTransport")
            .field("token_endpoint", &self.token_endpoint)
            .field("revoke_endpoint", &self.revoke_endpoint)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl HttpOAuthTransport {
    pub fn production() -> Result<Self, OAuthFlowError> {
        Self::new(
            format!("{CODEX_OAUTH_ISSUER}/oauth/token"),
            CODEX_OAUTH_CLIENT_ID.to_string(),
        )
    }

    fn new(token_endpoint: String, client_id: String) -> Result<Self, OAuthFlowError> {
        let mut endpoint = url::Url::parse(&token_endpoint)
            .map_err(|_| OAuthFlowError::protocol("The OAuth token endpoint is invalid"))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(OAuthFlowError::protocol(
                "The OAuth token endpoint is invalid",
            ));
        }
        endpoint.set_path("/oauth/revoke");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        let revoke_endpoint: String = endpoint.into();
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .build()
            .map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth network client could not be created",
                )
            })?;
        Ok(Self {
            token_endpoint,
            revoke_endpoint,
            client_id,
            client,
        })
    }

    #[cfg(test)]
    fn with_endpoints(
        token_endpoint: String,
        revoke_endpoint: String,
        client_id: String,
    ) -> Result<Self, OAuthFlowError> {
        let mut transport = Self::new(token_endpoint, client_id)?;
        let endpoint = url::Url::parse(&revoke_endpoint)
            .map_err(|_| OAuthFlowError::protocol("The OAuth revoke endpoint is invalid"))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(OAuthFlowError::protocol(
                "The OAuth revoke endpoint is invalid",
            ));
        }
        transport.revoke_endpoint = revoke_endpoint;
        Ok(transport)
    }
}

impl OAuthTransport for HttpOAuthTransport {
    fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
        login_deadline: Instant,
    ) -> Result<NewOAuthTokens, OAuthFlowError> {
        let remaining = login_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(OAuthFlowError::new(
                OAuthErrorCode::CallbackTimeout,
                true,
                "Codex sign-in timed out",
            ));
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", &self.client_id)
            .append_pair("code_verifier", code_verifier)
            .finish();
        let response = self
            .client
            .post(&self.token_endpoint)
            .timeout(remaining.min(Duration::from_secs(30)))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("user-agent", crate::config::UPSTREAM_UA)
            .body(body)
            .send()
            .map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth token exchange could not connect",
                )
            })?;
        if !response.status().is_success() {
            return Err(OAuthFlowError::new(
                OAuthErrorCode::OAuthProtocol,
                false,
                "The OAuth token exchange was rejected",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_TOKEN_RESPONSE)
        {
            return Err(OAuthFlowError::protocol(
                "The OAuth token response is too large",
            ));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        response
            .take(MAX_TOKEN_RESPONSE + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth token response could not be read",
                )
            })?;
        if bytes.len() as u64 > MAX_TOKEN_RESPONSE {
            return Err(OAuthFlowError::protocol(
                "The OAuth token response is too large",
            ));
        }
        let response: TokenResponse = serde_json::from_slice(&bytes)
            .map_err(|_| OAuthFlowError::protocol("The OAuth token response is invalid"))?;
        response.into_tokens()
    }
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

impl Drop for RefreshResponse {
    fn drop(&mut self) {
        if let Some(value) = self.id_token.as_mut() {
            value.zeroize();
        }
        if let Some(value) = self.access_token.as_mut() {
            value.zeroize();
        }
        if let Some(value) = self.refresh_token.as_mut() {
            value.zeroize();
        }
    }
}

impl RefreshResponse {
    fn into_update(mut self) -> Result<RefreshUpdate, OAuthFlowError> {
        if self
            .id_token
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
            || self
                .access_token
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
        {
            return Err(OAuthFlowError::protocol(
                "The OAuth refresh response contains an empty token",
            ));
        }
        if self
            .id_token
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TOKEN_BYTES)
            || self
                .access_token
                .as_ref()
                .is_some_and(|value| value.len() > MAX_TOKEN_BYTES)
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.len() > MAX_TOKEN_BYTES)
        {
            return Err(OAuthFlowError::protocol(
                "The OAuth refresh response contains an oversized token",
            ));
        }
        let access_token = self.access_token.take();
        let mut id_account_id = None;
        let mut id_expiry = None;
        if let Some(id_token) = self.id_token.as_ref() {
            let mut claims = decode_jwt_claims(id_token)?;
            id_account_id = claims
                .take_account_id()
                .filter(|value| !value.trim().is_empty());
            if id_account_id.is_none() {
                return Err(OAuthFlowError::protocol(
                    "The OAuth refresh token is missing an account identifier",
                ));
            }
            id_expiry = claims.exp;
        }
        let mut access_account_id = None;
        let mut access_expiry = None;
        if let Some(access_token) = access_token.as_ref() {
            if let Ok(mut claims) = decode_jwt_claims(access_token) {
                access_account_id = claims
                    .take_account_id()
                    .filter(|value| !value.trim().is_empty());
                access_expiry = claims.exp;
            }
        }
        if id_account_id
            .as_deref()
            .zip(access_account_id.as_deref())
            .is_some_and(|(id_account, access_account)| id_account != access_account)
        {
            return Err(OAuthFlowError::protocol(
                "The OAuth refresh response contains conflicting accounts",
            ));
        }
        let account_id = id_account_id.or(access_account_id);
        let expires_at = access_expiry.or(id_expiry);
        Ok(RefreshUpdate {
            access_token,
            refresh_token: self
                .refresh_token
                .take()
                .filter(|value| !value.trim().is_empty()),
            id_token: self.id_token.take(),
            account_id,
            expires_at,
        })
    }
}

impl RefreshTransport for HttpOAuthTransport {
    fn refresh(&self, refresh_token: &str) -> Result<RefreshUpdate, OAuthFlowError> {
        let body = Zeroizing::new(
            serde_json::to_vec(&RefreshRequest {
                client_id: &self.client_id,
                grant_type: "refresh_token",
                refresh_token,
            })
            .map_err(|_| OAuthFlowError::protocol("The OAuth refresh request is invalid"))?,
        );
        let response = self
            .client
            .post(&self.token_endpoint)
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/json")
            .header("user-agent", crate::config::UPSTREAM_UA)
            .body(body.to_vec())
            .send()
            .map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth refresh request could not connect",
                )
            })?;
        if !response.status().is_success() {
            return Err(OAuthFlowError::new(
                OAuthErrorCode::OAuthProtocol,
                false,
                "The OAuth refresh request was rejected",
            ));
        }
        let response = read_bounded_json::<RefreshResponse>(response)?;
        response.into_update()
    }
}

#[derive(Serialize)]
struct RevokeRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'a str>,
}

impl RevokeTransport for HttpOAuthTransport {
    fn revoke(&self, token: &RevokeToken) -> Result<(), OAuthFlowError> {
        let (token_type_hint, client_id) = match token.kind {
            RevokeTokenKind::Refresh => ("refresh_token", Some(self.client_id.as_str())),
            RevokeTokenKind::Access => ("access_token", None),
        };
        let body = Zeroizing::new(
            serde_json::to_vec(&RevokeRequest {
                token: &token.token,
                token_type_hint,
                client_id,
            })
            .map_err(|_| OAuthFlowError::protocol("The OAuth revoke request is invalid"))?,
        );
        let response = self
            .client
            .post(&self.revoke_endpoint)
            .timeout(Duration::from_secs(10))
            .header("content-type", "application/json")
            .header("user-agent", crate::config::UPSTREAM_UA)
            .body(body.to_vec())
            .send()
            .map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::OAuthNetwork,
                    true,
                    "The OAuth revoke request could not connect",
                )
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(OAuthFlowError::new(
                OAuthErrorCode::OAuthProtocol,
                false,
                "The OAuth revoke request was rejected",
            ))
        }
    }
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::blocking::Response,
) -> Result<T, OAuthFlowError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE)
    {
        return Err(OAuthFlowError::protocol("The OAuth response is too large"));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    response
        .take(MAX_TOKEN_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            OAuthFlowError::new(
                OAuthErrorCode::OAuthNetwork,
                true,
                "The OAuth response could not be read",
            )
        })?;
    if bytes.len() as u64 > MAX_TOKEN_RESPONSE {
        return Err(OAuthFlowError::protocol("The OAuth response is too large"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| OAuthFlowError::protocol("The OAuth response is invalid"))
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

impl TokenResponse {
    fn into_tokens(mut self) -> Result<NewOAuthTokens, OAuthFlowError> {
        if self.id_token.len() > MAX_TOKEN_BYTES
            || self.access_token.len() > MAX_TOKEN_BYTES
            || self.refresh_token.len() > MAX_TOKEN_BYTES
        {
            return Err(OAuthFlowError::protocol(
                "The OAuth token response contains an oversized token",
            ));
        }
        let mut id_claims = decode_jwt_claims(&self.id_token)?;
        let account_id = id_claims
            .take_account_id()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                OAuthFlowError::protocol("The OAuth token is missing an account identifier")
            })?;
        let expires_at = decode_jwt_claims(&self.access_token)
            .ok()
            .and_then(|claims| claims.exp)
            .or(id_claims.exp);
        Ok(NewOAuthTokens {
            access_token: std::mem::take(&mut self.access_token),
            refresh_token: std::mem::take(&mut self.refresh_token),
            id_token: std::mem::take(&mut self.id_token),
            account_id,
            expires_at,
        })
    }
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.id_token.zeroize();
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

#[derive(Deserialize, Default)]
struct JwtClaims {
    #[serde(default)]
    exp: Option<i64>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<JwtAuthClaims>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

impl JwtClaims {
    fn take_account_id(&mut self) -> Option<String> {
        self.auth
            .as_mut()
            .and_then(|auth| auth.chatgpt_account_id.take())
            .or_else(|| self.chatgpt_account_id.take())
    }
}

impl Drop for JwtClaims {
    fn drop(&mut self) {
        if let Some(account_id) = self.chatgpt_account_id.as_mut() {
            account_id.zeroize();
        }
    }
}

#[derive(Deserialize, Default)]
struct JwtAuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

impl Drop for JwtAuthClaims {
    fn drop(&mut self) {
        if let Some(account_id) = self.chatgpt_account_id.as_mut() {
            account_id.zeroize();
        }
    }
}

fn decode_jwt_claims(jwt: &str) -> Result<JwtClaims, OAuthFlowError> {
    let mut parts = jwt.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(OAuthFlowError::protocol(
            "The OAuth token format is invalid",
        ));
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(OAuthFlowError::protocol(
            "The OAuth token format is invalid",
        ));
    }
    let decoded = Zeroizing::new(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| OAuthFlowError::protocol("The OAuth token format is invalid"))?,
    );
    if decoded.len() > MAX_JWT_PAYLOAD {
        return Err(OAuthFlowError::protocol(
            "The OAuth token payload is too large",
        ));
    }
    serde_json::from_slice(&decoded)
        .map_err(|_| OAuthFlowError::protocol("The OAuth token payload is invalid"))
}

#[derive(Clone, Debug)]
pub struct LoginOptions {
    issuer: String,
    client_id: String,
    ports: Vec<u16>,
    timeout: Duration,
}

impl LoginOptions {
    pub fn production() -> Self {
        Self {
            issuer: CODEX_OAUTH_ISSUER.to_string(),
            client_id: CODEX_OAUTH_CLIENT_ID.to_string(),
            ports: CALLBACK_PORTS.to_vec(),
            timeout: LOGIN_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn for_test(issuer: String, client_id: String, ports: Vec<u16>, timeout: Duration) -> Self {
        Self {
            issuer,
            client_id,
            ports,
            timeout,
        }
    }
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

pub fn run_login_flow<S, T, B, O>(
    repository: &AuthRepository<S, T>,
    browser: &B,
    transport: &O,
    options: &LoginOptions,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: SecretStore,
    T: StateStore,
    B: BrowserOpener,
    O: OAuthTransport,
{
    let guard = repository.begin_mutation()?;
    let deadline = Instant::now() + options.timeout;
    let (listener, port) = bind_callback(&options.ports)?;
    listener.set_nonblocking(true).map_err(|_| {
        OAuthFlowError::new(
            OAuthErrorCode::CallbackUnavailable,
            true,
            "The local OAuth callback could not be configured",
        )
    })?;
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
    browser.open(&authorization_url, deadline)?;

    let mut request_count = 0_usize;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                request_count += 1;
                if request_count > MAX_CALLBACK_REQUESTS {
                    return Err(OAuthFlowError::protocol(
                        "Too many invalid OAuth callback requests were received",
                    ));
                }
                let action = match parse_callback(&mut stream, port, &state, deadline) {
                    Ok(action) => action,
                    Err(_) => {
                        write_callback_response(&mut stream, 400, "Sign-in request rejected");
                        continue;
                    }
                };
                match action {
                    CallbackAction::Ignore(status) => {
                        write_callback_response(&mut stream, status, "Sign-in request rejected");
                    }
                    CallbackAction::Denied => {
                        write_callback_response(&mut stream, 400, "Sign-in was not completed");
                        return Err(OAuthFlowError::new(
                            OAuthErrorCode::OAuthDenied,
                            false,
                            "Codex sign-in was denied or cancelled",
                        ));
                    }
                    CallbackAction::Code(mut code) => {
                        let result = transport
                            .exchange_code(&code, &redirect_uri, &pkce.verifier, deadline)
                            .and_then(|tokens| {
                                repository
                                    .commit_login_guarded(&guard, tokens)
                                    .map_err(OAuthFlowError::from)
                            });
                        code.zeroize();
                        match result {
                            Ok(status) => {
                                write_callback_response(
                                    &mut stream,
                                    200,
                                    "Codex sign-in completed. You can close this window.",
                                );
                                return Ok(status);
                            }
                            Err(error) => {
                                write_callback_response(
                                    &mut stream,
                                    500,
                                    "Codex sign-in could not be completed safely.",
                                );
                                return Err(error);
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(
                    CALLBACK_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(_) => {
                return Err(OAuthFlowError::new(
                    OAuthErrorCode::CallbackUnavailable,
                    true,
                    "The local OAuth callback stopped unexpectedly",
                ));
            }
        }
    }
    Err(OAuthFlowError::new(
        OAuthErrorCode::CallbackTimeout,
        true,
        "Codex sign-in timed out",
    ))
}

fn bind_callback(ports: &[u16]) -> Result<(TcpListener, u16), OAuthFlowError> {
    for port in ports {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", *port)) {
            let actual = listener.local_addr().map_err(|_| {
                OAuthFlowError::new(
                    OAuthErrorCode::CallbackUnavailable,
                    true,
                    "The local OAuth callback address could not be read",
                )
            })?;
            return Ok((listener, actual.port()));
        }
    }
    Err(OAuthFlowError::new(
        OAuthErrorCode::CallbackUnavailable,
        true,
        "OAuth callback ports 1455 and 1457 are unavailable",
    ))
}

fn build_authorization_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
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
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", CODEX_OAUTH_ORIGINATOR);
    Ok(Zeroizing::new(url.into()))
}

enum CallbackAction {
    Ignore(u16),
    Denied,
    Code(Zeroizing<String>),
}

impl fmt::Debug for CallbackAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignore(status) => f.debug_tuple("Ignore").field(status).finish(),
            Self::Denied => f.write_str("Denied"),
            Self::Code(_) => f.write_str("Code(<redacted>)"),
        }
    }
}

fn parse_callback(
    stream: &mut TcpStream,
    port: u16,
    expected_state: &str,
    login_deadline: Instant,
) -> Result<CallbackAction, OAuthFlowError> {
    let now = Instant::now();
    if now >= login_deadline {
        return Err(OAuthFlowError::protocol(
            "The OAuth callback request timed out",
        ));
    }
    let request_deadline = login_deadline.min(now + CALLBACK_IO_TIMEOUT);
    let write_timeout = request_deadline.saturating_duration_since(now);
    stream.set_write_timeout(Some(write_timeout)).ok();
    let head = read_http_head(stream, request_deadline)?;
    let text = std::str::from_utf8(&head)
        .map_err(|_| OAuthFlowError::protocol("The OAuth callback request is invalid"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| OAuthFlowError::protocol("The OAuth callback request is invalid"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "GET" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(OAuthFlowError::protocol(
            "The OAuth callback request is invalid",
        ));
    }
    let mut host = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(OAuthFlowError::protocol(
                "The OAuth callback request is invalid",
            ));
        };
        if name.trim().eq_ignore_ascii_case("host")
            && host.replace(value.trim().to_ascii_lowercase()).is_some()
        {
            return Err(OAuthFlowError::protocol(
                "The OAuth callback request is invalid",
            ));
        }
    }
    let expected_localhost = format!("localhost:{port}");
    let expected_loopback = format!("127.0.0.1:{port}");
    if !matches!(host.as_deref(), Some(value) if value == expected_localhost || value == expected_loopback)
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
        let key_digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        if !seen.insert(key_digest) {
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
    let code = code
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OAuthFlowError::protocol("The OAuth callback code is missing"))?;
    Ok(CallbackAction::Code(code))
}

fn read_http_head(
    stream: &mut TcpStream,
    request_deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, OAuthFlowError> {
    let mut head = Zeroizing::new(Vec::with_capacity(2048));
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let remaining = request_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(OAuthFlowError::protocol(
                "The OAuth callback request timed out",
            ));
        }
        stream.set_read_timeout(Some(remaining)).map_err(|_| {
            OAuthFlowError::protocol("The OAuth callback timeout could not be configured")
        })?;
        let read = stream.read(&mut byte).map_err(|_| {
            OAuthFlowError::protocol("The OAuth callback request could not be read")
        })?;
        if read == 0 {
            return Err(OAuthFlowError::protocol(
                "The OAuth callback request ended early",
            ));
        }
        head.push(byte[0]);
        if head.len() > MAX_CALLBACK_HEAD {
            return Err(OAuthFlowError::protocol(
                "The OAuth callback request is too large",
            ));
        }
    }
    Ok(head)
}

fn write_callback_response(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>CSSwitch Codex sign-in</title><p>{message}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::super::storage::{AuthState, StateStore};
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    type SecretMap = HashMap<(String, String), Vec<u8>>;
    type BrowserAction = dyn Fn(&str) -> Result<(), OAuthFlowError> + Send + Sync;

    #[derive(Clone, Default)]
    struct MemorySecrets(Arc<Mutex<SecretMap>>);

    impl SecretStore for MemorySecrets {
        fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(service.to_string(), account.to_string()))
                .cloned())
        }

        fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), StorageError> {
            self.0
                .lock()
                .unwrap()
                .insert((service.to_string(), account.to_string()), value.to_vec());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), StorageError> {
            self.0
                .lock()
                .unwrap()
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct MemoryState(Arc<Mutex<Option<AuthState>>>);

    impl StateStore for MemoryState {
        fn load(&self) -> Result<Option<AuthState>, StorageError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn commit(&self, state: &AuthState) -> Result<(), StorageError> {
            *self.0.lock().unwrap() = Some(state.clone());
            Ok(())
        }
    }

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::getrandom(&mut random).unwrap();
            Self(std::env::temp_dir().join(format!(
                "csswitch-oauth-test-{}-{}",
                std::process::id(),
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
            )))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct FakeBrowser {
        action: Arc<BrowserAction>,
    }

    impl BrowserOpener for FakeBrowser {
        fn open(
            &self,
            authorization_url: &str,
            _login_deadline: Instant,
        ) -> Result<(), OAuthFlowError> {
            (self.action)(authorization_url)
        }
    }

    fn browser_action(callback: impl Fn(String, String) + Send + Sync + 'static) -> FakeBrowser {
        FakeBrowser {
            action: Arc::new(move |authorization_url| {
                let parsed = url::Url::parse(authorization_url).unwrap();
                let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
                assert_eq!(params.get("scope").unwrap(), CODEX_OAUTH_SCOPE);
                assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
                assert_eq!(params.get("originator").unwrap(), CODEX_OAUTH_ORIGINATOR);
                assert!(!params.contains_key("code_verifier"));
                let redirect = params.get("redirect_uri").unwrap().clone();
                let state = params.get("state").unwrap().clone();
                callback(redirect, state);
                Ok(())
            }),
        }
    }

    fn send_callback(redirect: &str, query: &str, host_override: Option<&str>) -> String {
        let url = url::Url::parse(redirect).unwrap();
        let port = url.port().unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let host = host_override
            .map(str::to_string)
            .unwrap_or_else(|| format!("localhost:{port}"));
        write!(
            stream,
            "GET {}?{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            url.path(),
            query,
            host
        )
        .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn jwt(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{payload}.signature")
    }

    struct FakeOAuthServer {
        endpoint: String,
        request: Arc<Mutex<Option<String>>>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl FakeOAuthServer {
        fn success() -> Self {
            Self::success_delayed(Duration::ZERO)
        }

        fn success_delayed(delay: Duration) -> Self {
            let id_token = jwt(serde_json::json!({
                "exp": 2_000_000_000_i64,
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"}
            }));
            let access_token = jwt(serde_json::json!({"exp": 1_900_000_000_i64}));
            let body = serde_json::json!({
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": "refresh-test"
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes();
            Self::raw(response, delay)
        }

        fn raw(response: Vec<u8>, delay: Duration) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let request = Arc::new(Mutex::new(None));
            let request_out = Arc::clone(&request);
            let worker = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let head_text = String::from_utf8(head).unwrap();
                let length = head_text
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap();
                let mut request_body = vec![0_u8; length];
                stream.read_exact(&mut request_body).unwrap();
                *request_out.lock().unwrap() = Some(String::from_utf8(request_body).unwrap());
                thread::sleep(delay);
                let _ = stream.write_all(&response);
            });
            Self {
                endpoint: format!("http://{address}/oauth/token"),
                request,
                worker: Some(worker),
            }
        }
    }

    impl Drop for FakeOAuthServer {
        fn drop(&mut self) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn test_repository(root: &TempRoot) -> AuthRepository<MemorySecrets, MemoryState> {
        AuthRepository::new(
            MemorySecrets::default(),
            MemoryState::default(),
            root.0.clone(),
        )
    }

    #[test]
    fn full_login_uses_pkce_local_oauth_and_commits_only_after_callback() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let oauth = FakeOAuthServer::success();
        let transport =
            HttpOAuthTransport::new(oauth.endpoint.clone(), "client-test".into()).unwrap();
        let browser = browser_action(|redirect, state| {
            thread::spawn(move || {
                let query = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("code", "authorization-code")
                    .append_pair("state", &state)
                    .finish();
                let response = send_callback(&redirect, &query, None);
                assert!(response.starts_with("HTTP/1.1 200"));
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_secs(2),
        );

        let status = run_login_flow(&repository, &browser, &transport, &options).unwrap();
        assert!(status.authenticated);
        assert_eq!(status.auth_generation, 1);
        let request = oauth.request.lock().unwrap().clone().unwrap();
        let fields: HashMap<_, _> = url::form_urlencoded::parse(request.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(fields.get("code").unwrap(), "authorization-code");
        assert_eq!(fields.get("client_id").unwrap(), "client-test");
        assert!(fields.get("code_verifier").unwrap().len() >= 43);
    }

    #[test]
    fn production_options_and_authorization_parameters_match_pinned_codex_contract() {
        let options = LoginOptions::production();
        assert_eq!(options.issuer, CODEX_OAUTH_ISSUER);
        assert_eq!(options.client_id, CODEX_OAUTH_CLIENT_ID);
        assert_eq!(options.ports, vec![1455, 1457]);
        assert_eq!(options.timeout, Duration::from_secs(5 * 60));

        let url = build_authorization_url(
            &options.issuer,
            &options.client_id,
            "http://localhost:1455/auth/callback",
            "challenge-test",
            "state-test",
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let fields: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            parsed.as_str().split('?').next().unwrap(),
            "https://auth.openai.com/oauth/authorize"
        );
        assert_eq!(fields.get("response_type").unwrap(), "code");
        assert_eq!(fields.get("client_id").unwrap(), CODEX_OAUTH_CLIENT_ID);
        assert_eq!(fields.get("scope").unwrap(), CODEX_OAUTH_SCOPE);
        assert_eq!(fields.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(fields.get("id_token_add_organizations").unwrap(), "true");
        assert_eq!(fields.get("codex_cli_simplified_flow").unwrap(), "true");
        assert_eq!(fields.get("originator").unwrap(), CODEX_OAUTH_ORIGINATOR);
    }

    #[test]
    fn generated_pkce_and_state_have_expected_entropy_and_s256_binding() {
        let pkce = generate_pkce().unwrap();
        let state = generate_state().unwrap();
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(pkce.verifier.as_bytes())
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(state.as_bytes())
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            pkce.challenge,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
    }

    #[test]
    fn token_response_requires_account_id_and_enforces_token_bound() {
        let private_value = "private-token-value";
        let missing_account = TokenResponse {
            id_token: jwt(serde_json::json!({"exp": 2_000_000_000_i64})),
            access_token: jwt(serde_json::json!({"exp": 1_900_000_000_i64})),
            refresh_token: private_value.to_string(),
        }
        .into_tokens()
        .unwrap_err();
        assert_eq!(missing_account.code, OAuthErrorCode::OAuthProtocol);
        assert!(!format!("{missing_account:?} {missing_account}").contains(private_value));

        let oversized = TokenResponse {
            id_token: jwt(serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"}
            })),
            access_token: "x".repeat(MAX_TOKEN_BYTES + 1),
            refresh_token: "refresh-test".into(),
        }
        .into_tokens()
        .unwrap_err();
        assert_eq!(oversized.code, OAuthErrorCode::OAuthProtocol);
    }

    #[test]
    fn storage_error_mapping_preserves_stable_public_codes() {
        let cases = [
            (StorageError::Busy, OAuthErrorCode::AuthBusy),
            (StorageError::AuthChanged, OAuthErrorCode::AuthChanged),
            (
                StorageError::InvalidState("private detail".into()),
                OAuthErrorCode::AuthStateInvalid,
            ),
            (
                StorageError::KeychainUnavailable("private detail".into()),
                OAuthErrorCode::KeychainUnavailable,
            ),
            (
                StorageError::UnsupportedPlatform,
                OAuthErrorCode::UnsupportedPlatform,
            ),
        ];
        for (storage, expected) in cases {
            let error = OAuthFlowError::from(storage);
            assert_eq!(error.code, expected);
            assert_eq!(error.code.as_str(), expected.as_str());
            assert!(!format!("{error:?} {error}").contains("private detail"));
        }
    }

    #[test]
    fn token_response_limits_cover_declared_and_streamed_oversize_bodies() {
        let declared = FakeOAuthServer::raw(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                MAX_TOKEN_RESPONSE + 1
            )
            .into_bytes(),
            Duration::ZERO,
        );
        let transport =
            HttpOAuthTransport::new(declared.endpoint.clone(), "client-test".into()).unwrap();
        let error = transport
            .exchange_code(
                "code-test",
                "http://localhost/callback",
                "verifier-test",
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::OAuthProtocol);
        drop(declared);

        let body = vec![b'x'; MAX_TOKEN_RESPONSE as usize + 1];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let streamed = FakeOAuthServer::raw(response, Duration::ZERO);
        let transport =
            HttpOAuthTransport::new(streamed.endpoint.clone(), "client-test".into()).unwrap();
        let error = transport
            .exchange_code(
                "code-test",
                "http://localhost/callback",
                "verifier-test",
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::OAuthProtocol);
    }

    #[test]
    fn token_exchange_does_not_follow_redirects() {
        let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        target.set_nonblocking(true).unwrap();
        let location = format!("http://{}/stolen", target.local_addr().unwrap());
        let redirect = FakeOAuthServer::raw(
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            )
            .into_bytes(),
            Duration::ZERO,
        );
        let transport =
            HttpOAuthTransport::new(redirect.endpoint.clone(), "client-test".into()).unwrap();
        let error = transport
            .exchange_code(
                "private-code",
                "http://localhost/callback",
                "private-verifier",
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::OAuthProtocol);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn token_exchange_is_bounded_by_remaining_login_deadline() {
        let root = TempRoot::new();
        let repository = test_repository(&root).with_lock_timeout(Duration::from_millis(20));
        let server = FakeOAuthServer::success_delayed(Duration::from_millis(300));
        let transport =
            HttpOAuthTransport::new(server.endpoint.clone(), "client-test".into()).unwrap();
        let browser = browser_action(|redirect, state| {
            thread::spawn(move || {
                let response = send_callback(
                    &redirect,
                    &format!("code=deadline-test&state={state}"),
                    None,
                );
                assert!(response.starts_with("HTTP/1.1 500"));
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_millis(100),
        );
        let started = Instant::now();
        let error = run_login_flow(&repository, &browser, &transport, &options).unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::OAuthNetwork);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(!repository.status().unwrap().authenticated);
        drop(repository.begin_mutation().unwrap());
    }

    #[test]
    fn refresh_uses_pinned_json_contract_and_accepts_token_rotation() {
        let id_token = jwt(serde_json::json!({
            "exp": 2_100_000_000_i64,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"}
        }));
        let access_token = jwt(serde_json::json!({"exp": 2_000_000_000_i64}));
        let body = serde_json::json!({
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "refresh-rotated"
        })
        .to_string();
        let server = FakeOAuthServer::raw(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes(),
            Duration::ZERO,
        );
        let transport = HttpOAuthTransport::with_endpoints(
            server.endpoint.clone(),
            server.endpoint.clone(),
            "client-test".into(),
        )
        .unwrap();
        let update = transport.refresh("refresh-old").unwrap();
        assert_eq!(update.refresh_token.as_deref(), Some("refresh-rotated"));
        assert_eq!(update.account_id.as_deref(), Some("acct-test"));
        assert_eq!(update.expires_at, Some(2_000_000_000));
        let request: serde_json::Value =
            serde_json::from_str(server.request.lock().unwrap().as_deref().unwrap()).unwrap();
        assert_eq!(request["client_id"], "client-test");
        assert_eq!(request["grant_type"], "refresh_token");
        assert_eq!(request["refresh_token"], "refresh-old");
    }

    #[test]
    fn refresh_response_allows_optional_fields_and_checks_both_account_claims() {
        let refresh_only = RefreshResponse {
            id_token: None,
            access_token: None,
            refresh_token: Some("refresh-only".into()),
        }
        .into_update()
        .unwrap();
        assert!(refresh_only.access_token.is_none());
        assert_eq!(refresh_only.refresh_token.as_deref(), Some("refresh-only"));
        assert!(refresh_only.id_token.is_none());

        let access_only = RefreshResponse {
            id_token: None,
            access_token: Some(jwt(serde_json::json!({
                "exp": 2_200_000_000_i64,
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-access"}
            }))),
            refresh_token: None,
        }
        .into_update()
        .unwrap();
        assert_eq!(access_only.account_id.as_deref(), Some("acct-access"));
        assert_eq!(access_only.expires_at, Some(2_200_000_000));

        let conflict = RefreshResponse {
            id_token: Some(jwt(serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-id"}
            }))),
            access_token: Some(jwt(serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-access"}
            }))),
            refresh_token: None,
        }
        .into_update()
        .unwrap_err();
        assert_eq!(conflict.code, OAuthErrorCode::OAuthProtocol);
    }

    #[test]
    fn refresh_transport_accepts_success_without_access_token() {
        let body = r#"{"refresh_token":"refresh-only"}"#;
        let server = FakeOAuthServer::raw(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes(),
            Duration::ZERO,
        );
        let transport = HttpOAuthTransport::with_endpoints(
            server.endpoint.clone(),
            server.endpoint.clone(),
            "client-test".into(),
        )
        .unwrap();
        let update = transport.refresh("refresh-old").unwrap();
        assert!(update.access_token.is_none());
        assert_eq!(update.refresh_token.as_deref(), Some("refresh-only"));
    }

    #[test]
    fn revoke_prefers_official_refresh_token_json_shape() {
        let server = FakeOAuthServer::raw(
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
            Duration::ZERO,
        );
        let transport = HttpOAuthTransport::with_endpoints(
            server.endpoint.clone(),
            server.endpoint.clone(),
            "client-test".into(),
        )
        .unwrap();
        let token = RevokeToken {
            kind: RevokeTokenKind::Refresh,
            token: Zeroizing::new("refresh-private".into()),
        };
        transport.revoke(&token).unwrap();
        let request: serde_json::Value =
            serde_json::from_str(server.request.lock().unwrap().as_deref().unwrap()).unwrap();
        assert_eq!(request["token"], "refresh-private");
        assert_eq!(request["token_type_hint"], "refresh_token");
        assert_eq!(request["client_id"], "client-test");
        assert!(!format!("{token:?}").contains("refresh-private"));
    }

    #[test]
    fn callback_code_debug_is_redacted() {
        let sentinel = "private-authorization-code";
        let action = CallbackAction::Code(Zeroizing::new(sentinel.to_string()));
        assert!(!format!("{action:?}").contains(sentinel));
    }

    #[test]
    fn wrong_state_and_wrong_host_are_rejected_before_valid_callback() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let oauth = FakeOAuthServer::success();
        let transport =
            HttpOAuthTransport::new(oauth.endpoint.clone(), "client-test".into()).unwrap();
        let browser = browser_action(|redirect, state| {
            thread::spawn(move || {
                let bad_state = send_callback(&redirect, "code=bad&state=wrong", None);
                assert!(bad_state.starts_with("HTTP/1.1 400"));
                let bad_host = send_callback(
                    &redirect,
                    &format!("code=bad&state={state}"),
                    Some("example.com"),
                );
                assert!(bad_host.starts_with("HTTP/1.1 400"));
                let good = send_callback(&redirect, &format!("code=good&state={state}"), None);
                assert!(good.starts_with("HTTP/1.1 200"));
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_secs(2),
        );

        assert!(
            run_login_flow(&repository, &browser, &transport, &options)
                .unwrap()
                .authenticated
        );
    }

    struct CountingTransport {
        calls: Arc<AtomicUsize>,
    }

    impl OAuthTransport for CountingTransport {
        fn exchange_code(
            &self,
            _code: &str,
            _redirect_uri: &str,
            _code_verifier: &str,
            _login_deadline: Instant,
        ) -> Result<NewOAuthTokens, OAuthFlowError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(50));
            Ok(NewOAuthTokens {
                access_token: "access-test".into(),
                refresh_token: "refresh-test".into(),
                id_token: "id-test".into(),
                account_id: "acct-test".into(),
                expires_at: Some(2_000_000_000),
            })
        }
    }

    #[test]
    fn first_valid_callback_is_the_only_terminal_exchange() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let calls = Arc::new(AtomicUsize::new(0));
        let transport = CountingTransport {
            calls: Arc::clone(&calls),
        };
        let (done_tx, done_rx) = mpsc::channel();
        let browser = browser_action(move |redirect, state| {
            let second_redirect = redirect.clone();
            let second_state = state.clone();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(5));
                let url = url::Url::parse(&second_redirect).unwrap();
                if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", url.port().unwrap())) {
                    let query = format!("code=second&state={second_state}");
                    let _ = write!(
                        stream,
                        "GET {}?{} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
                        url.path(),
                        query,
                        url.port().unwrap()
                    );
                }
                let _ = done_tx.send(());
            });
            thread::spawn(move || {
                let response = send_callback(&redirect, &format!("code=first&state={state}"), None);
                assert!(response.starts_with("HTTP/1.1 200"));
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_secs(2),
        );

        let status = run_login_flow(&repository, &browser, &transport, &options).unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(status.authenticated);
        assert_eq!(status.auth_generation, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(repository.status().unwrap().auth_generation, 1);
    }

    struct NeverTransport;

    impl OAuthTransport for NeverTransport {
        fn exchange_code(
            &self,
            _code: &str,
            _redirect_uri: &str,
            _code_verifier: &str,
            _login_deadline: Instant,
        ) -> Result<NewOAuthTokens, OAuthFlowError> {
            panic!("transport must not be called")
        }
    }

    #[test]
    fn browser_failure_and_timeout_never_commit_credentials() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let failing_browser = FakeBrowser {
            action: Arc::new(|_| {
                Err(OAuthFlowError::new(
                    OAuthErrorCode::BrowserOpenFailed,
                    true,
                    "browser failed",
                ))
            }),
        };
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_millis(40),
        );
        assert_eq!(
            run_login_flow(&repository, &failing_browser, &NeverTransport, &options)
                .unwrap_err()
                .code,
            OAuthErrorCode::BrowserOpenFailed
        );
        assert!(!repository.status().unwrap().authenticated);

        let idle_browser = FakeBrowser {
            action: Arc::new(|_| Ok(())),
        };
        assert_eq!(
            run_login_flow(&repository, &idle_browser, &NeverTransport, &options)
                .unwrap_err()
                .code,
            OAuthErrorCode::CallbackTimeout
        );
        assert!(!repository.status().unwrap().authenticated);
    }

    #[test]
    fn matching_oauth_error_is_terminal_and_does_not_exchange() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let browser = browser_action(|redirect, state| {
            thread::spawn(move || {
                let query = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("error", "access_denied")
                    .append_pair("error_description", "private-upstream-description")
                    .append_pair("state", &state)
                    .finish();
                let response = send_callback(&redirect, &query, None);
                assert!(response.starts_with("HTTP/1.1 400"));
                assert!(!response.contains("private-upstream-description"));
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_secs(2),
        );
        let error = run_login_flow(&repository, &browser, &NeverTransport, &options).unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::OAuthDenied);
        assert!(!format!("{error:?} {error}").contains("private-upstream-description"));
        assert!(!repository.status().unwrap().authenticated);
    }

    #[test]
    fn callback_uses_second_candidate_when_first_is_occupied() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let first = occupied.local_addr().unwrap().port();
        let second_probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let second = second_probe.local_addr().unwrap().port();
        drop(second_probe);

        let (listener, actual) = bind_callback(&[first, second]).unwrap();
        assert_eq!(actual, second);
        drop(listener);
        drop(occupied);
    }

    struct DeadlineBrowser {
        entered: mpsc::Sender<()>,
    }

    impl BrowserOpener for DeadlineBrowser {
        fn open(
            &self,
            _authorization_url: &str,
            login_deadline: Instant,
        ) -> Result<(), OAuthFlowError> {
            let _ = self.entered.send(());
            while Instant::now() < login_deadline {
                thread::sleep(Duration::from_millis(5));
            }
            Err(OAuthFlowError::new(
                OAuthErrorCode::BrowserOpenFailed,
                true,
                "browser deadline reached",
            ))
        }
    }

    #[test]
    fn browser_wait_is_inside_global_deadline_and_holds_then_releases_lock() {
        let root = TempRoot::new();
        let repository = test_repository(&root).with_lock_timeout(Duration::from_millis(20));
        let repository_in_flow = repository.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let browser = DeadlineBrowser {
            entered: entered_tx,
        };
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_millis(100),
        );
        let started = Instant::now();
        let worker = thread::spawn(move || {
            run_login_flow(&repository_in_flow, &browser, &NeverTransport, &options).unwrap_err()
        });

        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(repository.begin_mutation().unwrap_err(), StorageError::Busy);
        let error = worker.join().unwrap();
        assert_eq!(error.code, OAuthErrorCode::BrowserOpenFailed);
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(repository.begin_mutation().unwrap());
    }

    #[test]
    fn callback_head_over_64_kib_is_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            parse_callback(
                &mut stream,
                port,
                "expected",
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err()
        });
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = format!(
            "GET {CALLBACK_PATH}?state=expected&code=value HTTP/1.1\r\nHost: localhost:{port}\r\nX-Fill: "
        )
        .into_bytes();
        request.extend(std::iter::repeat_n(b'x', MAX_CALLBACK_HEAD + 1));
        request.extend_from_slice(b"\r\n\r\n");
        let _ = stream.write_all(&request);
        drop(stream);
        assert_eq!(worker.join().unwrap().code, OAuthErrorCode::OAuthProtocol);
    }

    #[test]
    fn more_than_64_invalid_callbacks_terminate_without_exchange() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let (done_tx, done_rx) = mpsc::channel();
        let browser = browser_action(move |redirect, _state| {
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                for _ in 0..=MAX_CALLBACK_REQUESTS {
                    let response = send_callback(&redirect, "state=wrong&code=bad", None);
                    assert!(response.is_empty() || response.starts_with("HTTP/1.1 400"));
                }
                let _ = done_tx.send(());
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_secs(2),
        );

        let error = run_login_flow(&repository, &browser, &NeverTransport, &options).unwrap_err();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(error.code, OAuthErrorCode::OAuthProtocol);
        assert!(!repository.status().unwrap().authenticated);
    }

    #[test]
    fn duplicate_callback_fields_are_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            parse_callback(
                &mut stream,
                port,
                "expected",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err()
        });
        let response = send_callback(
            &format!("http://localhost:{port}{CALLBACK_PATH}"),
            "state=expected&state=expected&code=value",
            None,
        );
        assert!(response.is_empty());
        assert_eq!(worker.join().unwrap().code, OAuthErrorCode::OAuthProtocol);
    }

    #[test]
    fn slowloris_callback_cannot_extend_global_login_deadline() {
        let root = TempRoot::new();
        let repository = test_repository(&root);
        let browser = browser_action(|redirect, _state| {
            thread::spawn(move || {
                let url = url::Url::parse(&redirect).unwrap();
                let Ok(mut stream) = TcpStream::connect(("127.0.0.1", url.port().unwrap())) else {
                    return;
                };
                for byte in b"GET /auth/callback HTTP/1.1\r\nHost: localhost" {
                    if stream.write_all(&[*byte]).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        });
        let options = LoginOptions::for_test(
            "http://127.0.0.1:9".into(),
            "client-test".into(),
            vec![0],
            Duration::from_millis(100),
        );

        let started = Instant::now();
        let error = run_login_flow(&repository, &browser, &NeverTransport, &options).unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::CallbackTimeout);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!repository.status().unwrap().authenticated);
    }
}
