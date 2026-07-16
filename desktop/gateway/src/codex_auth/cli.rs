use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::storage::StorageError;
use super::{
    production_status, run_production_login, run_production_logout, AuthStatus, OAuthErrorCode,
    OAuthFlowError,
};

const CLI_SCHEMA_VERSION: u32 = 1;
const EXPIRING_WINDOW_SECONDS: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliRun {
    pub json: String,
    pub exit_code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Login,
    Status,
    Logout,
}

impl Command {
    fn parse(args: &[String]) -> Option<Self> {
        match args {
            [command] if command == "login" => Some(Self::Login),
            [command] if command == "status" => Some(Self::Status),
            [command] if command == "logout" => Some(Self::Logout),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Status => "status",
            Self::Logout => "logout",
        }
    }
}

#[derive(Serialize)]
struct StatusView<'a> {
    authenticated: bool,
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
}

#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    schema_version: u32,
    ok: bool,
    command: &'a str,
    status: StatusView<'a>,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema_version: u32,
    ok: bool,
    command: Option<&'a str>,
    error: ErrorView<'a>,
}

trait AuthCommands {
    fn login(&self) -> Result<AuthStatus, OAuthFlowError>;
    fn status(&self) -> Result<AuthStatus, OAuthFlowError>;
    fn logout(&self) -> Result<AuthStatus, OAuthFlowError>;
}

struct ProductionCommands {
    state_root: PathBuf,
}

impl AuthCommands for ProductionCommands {
    fn login(&self) -> Result<AuthStatus, OAuthFlowError> {
        run_production_login(self.state_root.clone())
    }

    fn status(&self) -> Result<AuthStatus, OAuthFlowError> {
        production_status(self.state_root.clone())
    }

    fn logout(&self) -> Result<AuthStatus, OAuthFlowError> {
        run_production_logout(self.state_root.clone())
    }
}

pub fn run_cli(args: &[String]) -> CliRun {
    let Some(command) = Command::parse(args) else {
        return error_run(
            None,
            "invalid_arguments",
            "Usage: csswitch-gateway codex-auth login|status|logout",
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
        run_cli_with(command, now, &ProductionCommands { state_root })
    }
}

#[cfg(target_os = "macos")]
fn production_state_root() -> Result<PathBuf, StorageError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| StorageError::InvalidState("HOME is unavailable or not absolute".into()))?;
    Ok(home.join(".csswitch"))
}

fn run_cli_with(command: Command, now: i64, commands: &dyn AuthCommands) -> CliRun {
    let result = match command {
        Command::Login => commands.login(),
        Command::Status => commands.status(),
        Command::Logout => commands.logout(),
    };
    match result {
        Ok(status) => success_run(command, now, &status),
        Err(error) => oauth_error_run(command, error),
    }
}

fn success_run(command: Command, now: i64, status: &AuthStatus) -> CliRun {
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
            account_hash: status.account_hash.as_deref(),
            expiry_state,
            expires_at: status.expires_at,
            auth_epoch: status.auth_epoch.as_deref(),
            auth_generation: status.auth_generation,
        },
    };
    serialize_or_internal(&envelope)
}

fn oauth_error_run(command: Command, error: OAuthFlowError) -> CliRun {
    error_run(
        Some(command.as_str()),
        error.code.as_str(),
        error.message,
        error.retryable,
        exit_code(error.code),
    )
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
        json: "{\"schema_version\":1,\"ok\":false,\"command\":null,\"error\":{\"code\":\"internal_error\",\"message\":\"Codex auth output could not be encoded\",\"retryable\":false}}".into(),
        exit_code: 8,
    }
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
        | OAuthErrorCode::KeychainUnavailable
        | OAuthErrorCode::Storage
        | OAuthErrorCode::UnsupportedPlatform => 6,
        OAuthErrorCode::OAuthNetwork | OAuthErrorCode::OAuthProtocol => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct FakeCommands {
        result: Result<AuthStatus, OAuthFlowError>,
    }

    impl AuthCommands for FakeCommands {
        fn login(&self) -> Result<AuthStatus, OAuthFlowError> {
            self.result.clone()
        }

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
        );
        assert_eq!(run.exit_code, 0);
        assert!(!run.json.contains('\n'));
        let value: Value = serde_json::from_str(&run.json).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "status");
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
            );
            let value: Value = serde_json::from_str(&run.json).unwrap();
            assert_eq!(value["status"]["expiry_state"], expected);
        }
    }

    #[test]
    fn errors_keep_stable_codes_exit_codes_and_redaction() {
        let private_detail = "private-token-detail";
        let error = OAuthFlowError::from(StorageError::InvalidState(private_detail.into()));
        let run = run_cli_with(Command::Login, 0, &FakeCommands { result: Err(error) });
        assert_eq!(run.exit_code, 6);
        assert!(!run.json.contains(private_detail));
        let value: Value = serde_json::from_str(&run.json).unwrap();
        assert_eq!(value["error"]["code"], "auth_state_invalid");
        assert_eq!(value["command"], "login");
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
