use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::storage::StorageError;
use super::{
    production_status, run_production_login_async, run_production_logout,
    run_production_logout_local, AuthStatus, LoginControl, LoginProgress, OAuthErrorCode,
    OAuthFlowError,
};

const CLI_SCHEMA_VERSION: u32 = 3;
const EXPIRING_WINDOW_SECONDS: i64 = 5 * 60;
const MAX_NDJSON_LINE_BYTES: usize = 8 * 1024;
const MAX_NDJSON_TOTAL_BYTES: usize = 64 * 1024;
const OPERATION_ID_ENV: &str = "CSSWITCH_CODEX_AUTH_OPERATION_ID";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliRun {
    pub json: String,
    pub exit_code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Status,
    Logout,
}

impl Command {
    fn parse(args: &[String]) -> Option<Self> {
        match args {
            [command] if command == "status" => Some(Self::Status),
            [command] if command == "logout" => Some(Self::Logout),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Logout => "logout",
        }
    }
}

#[derive(Serialize)]
struct StatusView<'a> {
    authenticated: bool,
    reason: &'static str,
    account_hash: Option<&'a str>,
    expiry_state: &'static str,
    expires_at: Option<i64>,
    auth_epoch: Option<&'a str>,
    auth_generation: u64,
}

#[derive(Serialize)]
struct ErrorView<'a> {
    code: &'a str,
    message: &'a str,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    challenge_detected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_kind: Option<&'a str>,
}

#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    schema_version: u32,
    ok: bool,
    command: &'a str,
    status: StatusView<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<WarningView<'a>>,
}

#[derive(Serialize)]
struct WarningView<'a> {
    code: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema_version: u32,
    ok: bool,
    command: Option<&'a str>,
    error: ErrorView<'a>,
}

trait AuthCommands {
    fn status(&self) -> Result<AuthStatus, OAuthFlowError>;
    fn logout(&self) -> Result<AuthStatus, OAuthFlowError>;
}

struct ProductionCommands {
    state_root: PathBuf,
    logout_local_only: bool,
}

impl AuthCommands for ProductionCommands {
    fn status(&self) -> Result<AuthStatus, OAuthFlowError> {
        production_status(self.state_root.clone())
    }

    fn logout(&self) -> Result<AuthStatus, OAuthFlowError> {
        if self.logout_local_only {
            run_production_logout_local(self.state_root.clone())
        } else {
            run_production_logout(self.state_root.clone())
        }
    }
}

pub fn run_cli(args: &[String]) -> CliRun {
    let Some(command) = Command::parse(args) else {
        return error_run(
            None,
            "invalid_arguments",
            "Usage: csswitch-gateway codex-auth login-browser|status|logout",
            false,
            2,
        );
    };

    #[cfg(not(target_os = "macos"))]
    {
        return oauth_error_run(
            command,
            OAuthFlowError::from(StorageError::UnsupportedPlatform),
        );
    }

    #[cfg(target_os = "macos")]
    {
        let state_root = match production_state_root() {
            Ok(root) => root,
            Err(error) => return oauth_error_run(command, error.into()),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_secs()).ok())
            .unwrap_or(i64::MAX);
        let logout_local_only = command == Command::Logout
            && std::env::var("CSSWITCH_CODEX_LOGOUT_SKIP_REVOKE").as_deref()
                == Ok("proxy_config_invalid");
        run_cli_with(
            command,
            now,
            &ProductionCommands {
                state_root,
                logout_local_only,
            },
            logout_local_only.then_some(WarningView {
                code: "revoke_skipped",
                reason: "proxy_config_invalid",
            }),
        )
    }
}

#[cfg(target_os = "macos")]
fn production_state_root() -> Result<PathBuf, StorageError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| StorageError::InvalidState("HOME is unavailable or not absolute".into()))?;
    Ok(super::state_root_from_home(&home))
}

fn run_cli_with(
    command: Command,
    now: i64,
    commands: &dyn AuthCommands,
    warning: Option<WarningView<'_>>,
) -> CliRun {
    let result = match command {
        Command::Status => commands.status(),
        Command::Logout => commands.logout(),
    };
    match result {
        Ok(status) => success_run(command, now, &status, warning),
        Err(error) => oauth_error_run(command, error),
    }
}

fn success_run(
    command: Command,
    now: i64,
    status: &AuthStatus,
    warning: Option<WarningView<'_>>,
) -> CliRun {
    let expiry_state = if !status.authenticated {
        "missing"
    } else {
        match status.expires_at {
            None => "unknown",
            Some(expires_at) if expires_at <= now => "expired",
            Some(expires_at) if expires_at <= now.saturating_add(EXPIRING_WINDOW_SECONDS) => {
                "expiring"
            }
            Some(_) => "valid",
        }
    };
    let envelope = SuccessEnvelope {
        schema_version: CLI_SCHEMA_VERSION,
        ok: true,
        command: command.as_str(),
        status: StatusView {
            authenticated: status.authenticated,
            reason: status.reason.as_str(),
            account_hash: status.account_hash.as_deref(),
            expiry_state,
            expires_at: status.expires_at,
            auth_epoch: status.auth_epoch.as_deref(),
            auth_generation: status.auth_generation,
        },
        warning,
    };
    serialize_or_internal(&envelope)
}

fn oauth_error_run(command: Command, error: OAuthFlowError) -> CliRun {
    let envelope = ErrorEnvelope {
        schema_version: CLI_SCHEMA_VERSION,
        ok: false,
        command: Some(command.as_str()),
        error: ErrorView {
            code: error.code.as_str(),
            message: error.message,
            retryable: error.retryable,
            stage: Some(error.stage),
            upstream_status: error.upstream_status,
            response_kind: error.response_kind,
            challenge_detected: error.challenge_detected,
            transport_kind: error.transport_kind,
        },
    };
    match serde_json::to_string(&envelope) {
        Ok(json) => CliRun {
            json,
            exit_code: exit_code(error.code),
        },
        Err(_) => internal_serialization_error(),
    }
}

fn error_run(
    command: Option<&str>,
    code: &str,
    message: &str,
    retryable: bool,
    exit_code: i32,
) -> CliRun {
    let envelope = ErrorEnvelope {
        schema_version: CLI_SCHEMA_VERSION,
        ok: false,
        command,
        error: ErrorView {
            code,
            message,
            retryable,
            stage: None,
            upstream_status: None,
            response_kind: None,
            challenge_detected: None,
            transport_kind: None,
        },
    };
    match serde_json::to_string(&envelope) {
        Ok(json) => CliRun { json, exit_code },
        Err(_) => internal_serialization_error(),
    }
}

fn serialize_or_internal(value: &impl Serialize) -> CliRun {
    match serde_json::to_string(value) {
        Ok(json) => CliRun { json, exit_code: 0 },
        Err(_) => internal_serialization_error(),
    }
}

fn internal_serialization_error() -> CliRun {
    CliRun {
        json: "{\"schema_version\":3,\"ok\":false,\"command\":null,\"error\":{\"code\":\"internal_error\",\"message\":\"Codex auth output could not be encoded\",\"retryable\":false}}".into(),
        exit_code: 8,
    }
}

#[derive(Serialize)]
struct StreamingEvent<'a> {
    schema_version: u32,
    operation_id: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disposition: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<StatusView<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<StreamingError<'a>>,
}

#[derive(Serialize)]
struct StreamingError<'a> {
    code: &'a str,
    stage: &'a str,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    challenge_detected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_kind: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelInput {
    schema_version: u32,
    operation_id: String,
    command: String,
}

fn valid_cancel_input(line: &[u8], operation_id: &str) -> bool {
    serde_json::from_slice::<CancelInput>(line).is_ok_and(|cancel| {
        cancel.schema_version == CLI_SCHEMA_VERSION
            && cancel.operation_id == operation_id
            && cancel.command == "cancel"
    })
}

#[derive(Clone)]
struct NdjsonWriter {
    inner: Arc<Mutex<NdjsonWriterInner>>,
}

struct NdjsonWriterInner {
    output: std::io::Stdout,
    total: usize,
    failed: bool,
}

impl NdjsonWriter {
    fn stdout() -> Self {
        Self {
            inner: Arc::new(Mutex::new(NdjsonWriterInner {
                output: std::io::stdout(),
                total: 0,
                failed: false,
            })),
        }
    }

    fn emit(&self, value: &impl Serialize) -> Result<(), ()> {
        let mut line = serde_json::to_vec(value).map_err(|_| ())?;
        if line.len() > MAX_NDJSON_LINE_BYTES || line.contains(&b'\n') || line.contains(&b'\r') {
            return Err(());
        }
        line.push(b'\n');
        let mut inner = self.inner.lock().map_err(|_| ())?;
        if inner.failed || inner.total.saturating_add(line.len()) > MAX_NDJSON_TOTAL_BYTES {
            inner.failed = true;
            return Err(());
        }
        if inner.output.write_all(&line).is_err() || inner.output.flush().is_err() {
            inner.failed = true;
            return Err(());
        }
        inner.total += line.len();
        Ok(())
    }
}

pub fn run_streaming_cli(args: &[String]) -> Option<i32> {
    match args {
        [command] if command == "login-browser" => {}
        [command, ..] if command == "login-device" || command == "login-browser" => return Some(2),
        _ => return None,
    }
    let operation_id = match std::env::var(OPERATION_ID_ENV) {
        Ok(value) if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            value
        }
        _ => return Some(2),
    };
    #[cfg(not(target_os = "macos"))]
    {
        let _ = operation_id;
        return Some(6);
    }
    #[cfg(target_os = "macos")]
    {
        let state_root = match production_state_root() {
            Ok(root) => root,
            Err(_) => return Some(6),
        };
        let writer = NdjsonWriter::stdout();
        let control = LoginControl::default();
        spawn_cancel_reader(operation_id.clone(), control.clone(), writer.clone());
        let progress_writer = writer.clone();
        let progress_operation_id = operation_id.clone();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return Some(8),
        };
        let result = runtime.block_on(run_production_login_async(
            state_root,
            &control,
            move |progress| {
                let event = match &progress {
                    LoginProgress::Waiting => progress_event(&progress_operation_id, "waiting"),
                    LoginProgress::Exchanging => {
                        progress_event(&progress_operation_id, "exchanging")
                    }
                    LoginProgress::Committing => {
                        progress_event(&progress_operation_id, "committing")
                    }
                };
                let _ = progress_writer.emit(&event);
            },
        ));
        let (state, status, error, code) = match &result {
            Ok(status) => (
                "succeeded",
                Some(status_view(now_seconds(), status)),
                None,
                0,
            ),
            Err(error) if error.code == OAuthErrorCode::AuthCancelled => (
                "cancelled",
                None,
                Some(streaming_error(error)),
                exit_code(error.code),
            ),
            Err(error) => (
                "failed",
                None,
                Some(streaming_error(error)),
                exit_code(error.code),
            ),
        };
        let terminal = StreamingEvent {
            schema_version: CLI_SCHEMA_VERSION,
            operation_id: &operation_id,
            kind: "terminal",
            state: Some(state),
            disposition: None,
            status,
            error,
        };
        if writer.emit(&terminal).is_err() {
            return Some(8);
        }
        Some(code)
    }
}

fn progress_event<'a>(operation_id: &'a str, state: &'a str) -> StreamingEvent<'a> {
    StreamingEvent {
        schema_version: CLI_SCHEMA_VERSION,
        operation_id,
        kind: "progress",
        state: Some(state),
        disposition: None,
        status: None,
        error: None,
    }
}

fn streaming_error(error: &OAuthFlowError) -> StreamingError<'_> {
    StreamingError {
        code: error.code.as_str(),
        stage: error.stage,
        retryable: error.retryable,
        upstream_status: error.upstream_status,
        response_kind: error.response_kind,
        challenge_detected: error.challenge_detected,
        transport_kind: error.transport_kind,
    }
}

fn status_view(now: i64, status: &AuthStatus) -> StatusView<'_> {
    let expiry_state = if !status.authenticated {
        "missing"
    } else {
        match status.expires_at {
            None => "unknown",
            Some(expires_at) if expires_at <= now => "expired",
            Some(expires_at) if expires_at <= now.saturating_add(EXPIRING_WINDOW_SECONDS) => {
                "expiring"
            }
            Some(_) => "valid",
        }
    };
    StatusView {
        authenticated: status.authenticated,
        reason: status.reason.as_str(),
        account_hash: status.account_hash.as_deref(),
        expiry_state,
        expires_at: status.expires_at,
        auth_epoch: status.auth_epoch.as_deref(),
        auth_generation: status.auth_generation,
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

fn spawn_cancel_reader(operation_id: String, control: LoginControl, writer: NdjsonWriter) {
    std::thread::spawn(move || {
        let mut input =
            std::io::BufReader::new(std::io::stdin()).take((MAX_NDJSON_LINE_BYTES + 1) as u64);
        let mut line = Vec::new();
        if input.read_until(b'\n', &mut line).is_err()
            || line.len() > MAX_NDJSON_LINE_BYTES
            || !line.ends_with(b"\n")
        {
            return;
        }
        if !valid_cancel_input(&line, &operation_id) {
            return;
        }
        let disposition = control.cancel();
        let event = StreamingEvent {
            schema_version: CLI_SCHEMA_VERSION,
            operation_id: &operation_id,
            kind: "cancel_ack",
            state: None,
            disposition: Some(disposition.as_str()),
            status: None,
            error: None,
        };
        let _ = writer.emit(&event);
    });
}

fn exit_code(code: OAuthErrorCode) -> i32 {
    match code {
        OAuthErrorCode::NotAuthenticated => 3,
        OAuthErrorCode::BrowserOpenFailed | OAuthErrorCode::OAuthDenied => 4,
        OAuthErrorCode::CallbackTimeout => 5,
        OAuthErrorCode::AuthBusy
        | OAuthErrorCode::AuthChanged
        | OAuthErrorCode::AuthStateInvalid
        | OAuthErrorCode::CallbackUnavailable
        | OAuthErrorCode::Storage
        | OAuthErrorCode::UnsupportedPlatform => 6,
        OAuthErrorCode::OAuthNetwork
        | OAuthErrorCode::OAuthProtocol
        | OAuthErrorCode::OAuthUnexpectedContentType
        | OAuthErrorCode::OAuthChallengeResponse
        | OAuthErrorCode::ProxyConnectFailed
        | OAuthErrorCode::TlsFailed
        | OAuthErrorCode::AuthCancelled => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::super::AuthStatusReason;
    use super::*;
    use serde_json::Value;

    #[test]
    fn legacy_device_login_command_is_rejected_before_any_auth_work() {
        assert_eq!(run_streaming_cli(&["login-device".into()]), Some(2));
        assert_eq!(
            run_streaming_cli(&["login-device".into(), "extra".into()]),
            Some(2)
        );
    }

    #[test]
    fn cancel_input_requires_schema_v3_exact_operation_and_fields() {
        let operation_id = "ab".repeat(16);
        let v3 = format!(
            "{{\"schema_version\":3,\"operation_id\":\"{operation_id}\",\"command\":\"cancel\"}}\n"
        );
        assert!(valid_cancel_input(v3.as_bytes(), &operation_id));
        assert!(!valid_cancel_input(
            v3.replacen("\"schema_version\":3", "\"schema_version\":2", 1)
                .as_bytes(),
            &operation_id,
        ));
        assert!(!valid_cancel_input(
            v3.replacen(&operation_id, &"cd".repeat(16), 1).as_bytes(),
            &operation_id,
        ));
        assert!(!valid_cancel_input(
            v3.replacen("\"command\":\"cancel\"", "\"command\":\"status\"", 1)
                .as_bytes(),
            &operation_id,
        ));
        assert!(!valid_cancel_input(
            v3.replacen("\"command\":", "\"extra\":true,\"command\":", 1)
                .as_bytes(),
            &operation_id,
        ));
    }

    struct FakeCommands {
        result: Result<AuthStatus, OAuthFlowError>,
    }

    impl AuthCommands for FakeCommands {
        fn status(&self) -> Result<AuthStatus, OAuthFlowError> {
            self.result.clone()
        }

        fn logout(&self) -> Result<AuthStatus, OAuthFlowError> {
            self.result.clone()
        }
    }

    fn status(authenticated: bool, expires_at: Option<i64>) -> AuthStatus {
        AuthStatus {
            authenticated,
            reason: if authenticated {
                AuthStatusReason::Ready
            } else {
                AuthStatusReason::StateMissing
            },
            account_hash: authenticated.then(|| "account-hash".into()),
            expires_at,
            auth_epoch: Some("00112233445566778899aabbccddeeff".into()),
            auth_generation: 7,
        }
    }

    #[test]
    fn success_is_one_line_versioned_and_secret_free() {
        let run = run_cli_with(
            Command::Status,
            1_000,
            &FakeCommands {
                result: Ok(status(true, Some(2_000))),
            },
            None,
        );
        assert_eq!(run.exit_code, 0);
        assert!(!run.json.contains('\n'));
        let value: Value = serde_json::from_str(&run.json).unwrap();
        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "status");
        assert_eq!(value["status"]["reason"], "ready");
        assert_eq!(value["status"]["expiry_state"], "valid");
        assert!(value["status"].get("access_token").is_none());
        assert!(value["status"].get("refresh_token").is_none());
    }

    #[test]
    fn expiry_states_are_stable() {
        let cases = [
            (false, None, "missing"),
            (true, None, "unknown"),
            (true, Some(999), "expired"),
            (true, Some(1_300), "expiring"),
            (true, Some(1_301), "valid"),
        ];
        for (authenticated, expires_at, expected) in cases {
            let run = run_cli_with(
                Command::Status,
                1_000,
                &FakeCommands {
                    result: Ok(status(authenticated, expires_at)),
                },
                None,
            );
            let value: Value = serde_json::from_str(&run.json).unwrap();
            assert_eq!(value["status"]["expiry_state"], expected);
        }
    }

    #[test]
    fn errors_keep_stable_codes_exit_codes_and_redaction() {
        let private_detail = "private-token-detail";
        let error = OAuthFlowError::from(StorageError::InvalidState(private_detail.into()));
        let run = run_cli_with(
            Command::Status,
            0,
            &FakeCommands { result: Err(error) },
            None,
        );
        assert_eq!(run.exit_code, 6);
        assert!(!run.json.contains(private_detail));
        let value: Value = serde_json::from_str(&run.json).unwrap();
        assert_eq!(value["error"]["code"], "auth_state_invalid");
        assert_eq!(value["command"], "status");
    }

    #[test]
    fn local_logout_warning_is_bounded_and_contains_no_route_details() {
        let run = run_cli_with(
            Command::Logout,
            1_000,
            &FakeCommands {
                result: Ok(status(false, None)),
            },
            Some(WarningView {
                code: "revoke_skipped",
                reason: "proxy_config_invalid",
            }),
        );
        assert_eq!(run.exit_code, 0);
        let value: Value = serde_json::from_str(&run.json).unwrap();
        assert_eq!(value["warning"]["code"], "revoke_skipped");
        assert_eq!(value["warning"]["reason"], "proxy_config_invalid");
        for forbidden in [
            "proxy_url",
            "http://",
            "https://",
            "socks5://",
            "socks5h://",
        ] {
            assert!(!run.json.contains(forbidden));
        }
    }

    #[test]
    fn invalid_arguments_are_not_echoed() {
        let secret = "private-unknown-argument";
        let run = run_cli(&[secret.into(), "extra".into()]);
        assert_eq!(run.exit_code, 2);
        assert!(!run.json.contains(secret));
        let value: Value = serde_json::from_str(&run.json).unwrap();
        assert!(value["command"].is_null());
        assert_eq!(value["error"]["code"], "invalid_arguments");
    }
}
