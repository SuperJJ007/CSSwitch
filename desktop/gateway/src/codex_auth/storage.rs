use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

#[cfg(not(feature = "acceptance-keychain"))]
pub const OAUTH_KEYCHAIN_SERVICE: &str = "com.csswitch.codex.oauth.v1";
#[cfg(feature = "acceptance-keychain")]
pub const OAUTH_KEYCHAIN_SERVICE: &str = "com.csswitch.acceptance.codex.oauth.v1";
#[cfg(not(feature = "acceptance-keychain"))]
pub const THINKING_KEYCHAIN_SERVICE: &str = "com.csswitch.codex.thinking.v1";
#[cfg(feature = "acceptance-keychain")]
pub const THINKING_KEYCHAIN_SERVICE: &str = "com.csswitch.acceptance.codex.thinking.v1";
pub const KEYCHAIN_ACCOUNT: &str = "default";

const AUTH_STATE_FILE: &str = "codex-auth-state.v1.json";
const AUTH_LOCK_FILE: &str = "codex-auth.mutation.lock";
const AUTH_RECORD_VERSION: u32 = 1;
const AUTH_STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    Busy,
    KeychainUnavailable(String),
    NotAuthenticated,
    Unavailable(String),
    InvalidState(String),
    AuthChanged,
    RollbackFailed,
    UnsupportedPlatform,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "Codex auth mutation is already in progress"),
            Self::KeychainUnavailable(detail) => {
                write!(f, "Codex Keychain unavailable: {detail}")
            }
            Self::NotAuthenticated => write!(f, "Codex authentication is not available"),
            Self::Unavailable(detail) => write!(f, "Codex auth storage unavailable: {detail}"),
            Self::InvalidState(detail) => write!(f, "Codex auth state invalid: {detail}"),
            Self::AuthChanged => write!(f, "Codex auth changed during mutation"),
            Self::RollbackFailed => {
                write!(f, "Codex auth rollback failed; credentials are disabled")
            }
            Self::UnsupportedPlatform => {
                write!(f, "Codex Keychain auth is supported only on macOS")
            }
        }
    }
}

impl std::error::Error for StorageError {}

pub trait SecretStore: Send + Sync + 'static {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, StorageError>;
    fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), StorageError>;
    fn delete(&self, service: &str, account: &str) -> Result<(), StorageError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KeychainSecretStore;

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecretStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, StorageError> {
        use security_framework::passwords::get_generic_password;
        use security_framework_sys::base::errSecItemNotFound;

        match get_generic_password(service, account) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.code() == errSecItemNotFound => Ok(None),
            Err(error) => Err(StorageError::KeychainUnavailable(format!(
                "Keychain read failed with status {}",
                error.code()
            ))),
        }
    }

    fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), StorageError> {
        security_framework::passwords::set_generic_password(service, account, value).map_err(
            |error| {
                StorageError::KeychainUnavailable(format!(
                    "Keychain write failed with status {}",
                    error.code()
                ))
            },
        )
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StorageError> {
        use security_framework::passwords::delete_generic_password;
        use security_framework_sys::base::errSecItemNotFound;

        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == errSecItemNotFound => Ok(()),
            Err(error) => Err(StorageError::KeychainUnavailable(format!(
                "Keychain delete failed with status {}",
                error.code()
            ))),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl SecretStore for KeychainSecretStore {
    fn load(&self, _service: &str, _account: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::UnsupportedPlatform)
    }

    fn save(&self, _service: &str, _account: &str, _value: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedPlatform)
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedPlatform)
    }
}

pub trait StateStore: Send + Sync + 'static {
    fn load(&self) -> Result<Option<AuthState>, StorageError>;
    fn commit(&self, state: &AuthState) -> Result<(), StorageError>;
}

#[derive(Clone, Debug)]
pub struct FsStateStore {
    root: PathBuf,
}

impl FsStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(AUTH_STATE_FILE)
    }
}

impl StateStore for FsStateStore {
    fn load(&self) -> Result<Option<AuthState>, StorageError> {
        if !validate_private_root(&self.root)? {
            return Ok(None);
        }
        let path = self.state_path();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(StorageError::InvalidState(
                    "auth state is not a regular file".into(),
                ));
            }
        }
        let file = match open_read_nofollow(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(StorageError::InvalidState(
                    "auth state is not a regular file".into(),
                ));
            }
            Err(error) => return Err(io_error("open auth state", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| io_error("inspect opened auth state", error))?;
        if !metadata.is_file() {
            return Err(StorageError::InvalidState(
                "auth state is not a regular file".into(),
            ));
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(StorageError::InvalidState("auth state is too large".into()));
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(StorageError::InvalidState(
                "auth state permissions are not 0600".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("read auth state", error))?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(StorageError::InvalidState("auth state is too large".into()));
        }
        let state: AuthState = serde_json::from_slice(&bytes)
            .map_err(|_| StorageError::InvalidState("auth state JSON is invalid".into()))?;
        state.validate()?;
        Ok(Some(state))
    }

    fn commit(&self, state: &AuthState) -> Result<(), StorageError> {
        state.validate()?;
        ensure_private_dir(&self.root)?;
        reject_unsafe_target(&self.state_path())?;

        let bytes = serde_json::to_vec(state)
            .map_err(|_| StorageError::InvalidState("auth state serialization failed".into()))?;
        let mut random = [0_u8; 8];
        getrandom::getrandom(&mut random)
            .map_err(|error| StorageError::Unavailable(format!("random failed: {error}")))?;
        let suffix = hex(&random);
        let temp = self.root.join(format!(
            ".{AUTH_STATE_FILE}.tmp-{}-{suffix}",
            std::process::id()
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options
                .open(&temp)
                .map_err(|error| io_error("create auth state temp file", error))?;
            file.write_all(&bytes)
                .map_err(|error| io_error("write auth state", error))?;
            file.sync_all()
                .map_err(|error| io_error("sync auth state", error))?;
            fs::rename(&temp, self.state_path())
                .map_err(|error| io_error("replace auth state", error))?;
            File::open(&self.root)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| io_error("sync auth state directory", error))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthState {
    pub version: u32,
    pub auth_epoch: String,
    pub auth_generation: u64,
    pub committed: bool,
}

impl AuthState {
    fn fresh() -> Result<Self, StorageError> {
        let mut epoch = [0_u8; 16];
        getrandom::getrandom(&mut epoch)
            .map_err(|error| StorageError::Unavailable(format!("random failed: {error}")))?;
        Ok(Self {
            version: AUTH_STATE_VERSION,
            auth_epoch: hex(&epoch),
            auth_generation: 0,
            committed: false,
        })
    }

    fn next_with_marker(&self, committed: bool) -> Result<Self, StorageError> {
        Ok(Self {
            version: AUTH_STATE_VERSION,
            auth_epoch: self.auth_epoch.clone(),
            auth_generation: self
                .auth_generation
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidState("auth generation exhausted".into()))?,
            committed,
        })
    }

    fn next_committed(&self) -> Result<Self, StorageError> {
        self.next_with_marker(true)
    }

    fn next_logged_out(&self) -> Result<Self, StorageError> {
        self.next_with_marker(false)
    }

    fn validate(&self) -> Result<(), StorageError> {
        if self.version != AUTH_STATE_VERSION {
            return Err(StorageError::InvalidState(
                "unsupported auth state version".into(),
            ));
        }
        if self.auth_epoch.len() != 32
            || !self.auth_epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(StorageError::InvalidState("invalid auth epoch".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OAuthRecord {
    version: u32,
    auth_epoch: String,
    auth_generation: u64,
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: String,
    expires_at: Option<i64>,
}

impl fmt::Debug for OAuthRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthRecord")
            .field("version", &self.version)
            .field("auth_epoch", &self.auth_epoch)
            .field("auth_generation", &self.auth_generation)
            .field("has_access_token", &!self.access_token.is_empty())
            .field("has_refresh_token", &!self.refresh_token.is_empty())
            .field("has_id_token", &!self.id_token.is_empty())
            .field("has_account_id", &!self.account_id.is_empty())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Drop for OAuthRecord {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
        self.account_id.zeroize();
    }
}

impl OAuthRecord {
    fn validate(&self) -> Result<(), StorageError> {
        if self.version != AUTH_RECORD_VERSION
            || self.auth_epoch.len() != 32
            || self.access_token.is_empty()
            || self.refresh_token.is_empty()
            || self.id_token.is_empty()
            || self.account_id.is_empty()
        {
            return Err(StorageError::InvalidState("OAuth record is invalid".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ThinkingRecord {
    version: u32,
    auth_epoch: String,
    auth_generation: u64,
    key_b64: String,
}

impl fmt::Debug for ThinkingRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThinkingRecord")
            .field("version", &self.version)
            .field("auth_epoch", &self.auth_epoch)
            .field("auth_generation", &self.auth_generation)
            .field("has_key", &!self.key_b64.is_empty())
            .finish()
    }
}

impl Drop for ThinkingRecord {
    fn drop(&mut self) {
        self.key_b64.zeroize();
    }
}

impl ThinkingRecord {
    fn validate(&self) -> Result<(), StorageError> {
        let key = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(&self.key_b64)
                .map_err(|_| StorageError::InvalidState("thinking key is invalid".into()))?,
        );
        if self.version != AUTH_RECORD_VERSION || self.auth_epoch.len() != 32 || key.len() != 32 {
            return Err(StorageError::InvalidState(
                "thinking record is invalid".into(),
            ));
        }
        Ok(())
    }
}

pub struct NewOAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub account_id: String,
    pub expires_at: Option<i64>,
}

impl fmt::Debug for NewOAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewOAuthTokens")
            .field("has_access_token", &!self.access_token.is_empty())
            .field("has_refresh_token", &!self.refresh_token.is_empty())
            .field("has_id_token", &!self.id_token.is_empty())
            .field("has_account_id", &!self.account_id.is_empty())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Drop for NewOAuthTokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
        self.account_id.zeroize();
    }
}

pub struct RefreshSnapshot {
    pub auth_epoch: String,
    pub auth_generation: u64,
    pub refresh_token: Zeroizing<String>,
    refresh_digest: [u8; 32],
}

impl fmt::Debug for RefreshSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshSnapshot")
            .field("auth_epoch", &self.auth_epoch)
            .field("auth_generation", &self.auth_generation)
            .field("has_refresh_token", &!self.refresh_token.is_empty())
            .finish_non_exhaustive()
    }
}

pub struct RefreshUpdate {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub account_id: Option<String>,
    pub expires_at: Option<i64>,
}

impl fmt::Debug for RefreshUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshUpdate")
            .field("has_access_token", &self.access_token.is_some())
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("has_id_token", &self.id_token.is_some())
            .field("has_account_id", &self.account_id.is_some())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Drop for RefreshUpdate {
    fn drop(&mut self) {
        if let Some(value) = self.access_token.as_mut() {
            value.zeroize();
        }
        if let Some(value) = self.refresh_token.as_mut() {
            value.zeroize();
        }
        if let Some(value) = self.id_token.as_mut() {
            value.zeroize();
        }
        if let Some(value) = self.account_id.as_mut() {
            value.zeroize();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeTokenKind {
    Access,
    Refresh,
}

pub struct RevokeToken {
    pub kind: RevokeTokenKind,
    pub token: Zeroizing<String>,
}

impl fmt::Debug for RevokeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevokeToken")
            .field("kind", &self.kind)
            .field("has_token", &!self.token.is_empty())
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub account_hash: Option<String>,
    pub expires_at: Option<i64>,
    pub auth_epoch: Option<String>,
    pub auth_generation: u64,
}

/// Short-lived, in-memory view used only to authorize one Codex upstream
/// operation. It is intentionally neither serializable nor debug-printable.
pub(crate) struct InferenceSecrets {
    access_token: String,
    account_id: String,
    thinking_key: [u8; 32],
    auth_epoch: String,
    auth_generation: u64,
    account_hash: String,
    expires_at: Option<i64>,
}

impl InferenceSecrets {
    #[cfg(test)]
    pub(crate) fn for_test(access_token: &str, account_id: &str) -> Self {
        Self {
            access_token: access_token.to_string(),
            account_id: account_id.to_string(),
            thinking_key: [7_u8; 32],
            auth_epoch: "ab".repeat(16),
            auth_generation: 1,
            account_hash: account_hash(account_id),
            expires_at: Some(2_000_000_000),
        }
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }

    pub(crate) fn thinking_key(&self) -> &[u8; 32] {
        &self.thinking_key
    }

    pub(crate) fn auth_epoch(&self) -> &str {
        &self.auth_epoch
    }

    pub(crate) fn auth_generation(&self) -> u64 {
        self.auth_generation
    }

    pub(crate) fn account_hash(&self) -> &str {
        &self.account_hash
    }

    pub(crate) fn expires_at(&self) -> Option<i64> {
        self.expires_at
    }
}

impl Drop for InferenceSecrets {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.account_id.zeroize();
        self.thinking_key.zeroize();
    }
}

pub struct AuthRepository<S: SecretStore, T: StateStore> {
    secrets: Arc<S>,
    state: Arc<T>,
    lock_root: PathBuf,
    lock_timeout: Duration,
}

pub struct AuthMutationGuard {
    root: PathBuf,
    _lock: MutationLock,
}

impl fmt::Debug for AuthMutationGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthMutationGuard")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl<S: SecretStore, T: StateStore> Clone for AuthRepository<S, T> {
    fn clone(&self) -> Self {
        Self {
            secrets: Arc::clone(&self.secrets),
            state: Arc::clone(&self.state),
            lock_root: self.lock_root.clone(),
            lock_timeout: self.lock_timeout,
        }
    }
}

impl AuthRepository<KeychainSecretStore, FsStateStore> {
    pub fn production(root: PathBuf) -> Self {
        Self::new(KeychainSecretStore, FsStateStore::new(root.clone()), root)
    }
}

impl<S: SecretStore, T: StateStore> AuthRepository<S, T> {
    pub fn new(secrets: S, state: T, lock_root: PathBuf) -> Self {
        Self {
            secrets: Arc::new(secrets),
            state: Arc::new(state),
            lock_root,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub fn commit_login(&self, tokens: NewOAuthTokens) -> Result<AuthStatus, StorageError> {
        let guard = self.begin_mutation()?;
        self.commit_login_guarded(&guard, tokens)
    }

    pub fn begin_mutation(&self) -> Result<AuthMutationGuard, StorageError> {
        Ok(AuthMutationGuard {
            root: self.lock_root.clone(),
            _lock: MutationLock::acquire(&self.lock_root, self.lock_timeout)?,
        })
    }

    pub fn commit_login_guarded(
        &self,
        guard: &AuthMutationGuard,
        tokens: NewOAuthTokens,
    ) -> Result<AuthStatus, StorageError> {
        validate_new_tokens(&tokens)?;
        if guard.root != self.lock_root {
            return Err(StorageError::AuthChanged);
        }
        let current = self.load_or_initialize_state()?;
        let next = current.next_committed()?;

        let old_oauth = self
            .secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?
            .map(Zeroizing::new);
        let old_thinking = self
            .secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?
            .map(Zeroizing::new);

        let oauth = OAuthRecord {
            version: AUTH_RECORD_VERSION,
            auth_epoch: next.auth_epoch.clone(),
            auth_generation: next.auth_generation,
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            id_token: tokens.id_token.clone(),
            account_id: tokens.account_id.clone(),
            expires_at: tokens.expires_at,
        };
        let mut thinking_key = [0_u8; 32];
        getrandom::getrandom(&mut thinking_key)
            .map_err(|error| StorageError::Unavailable(format!("random failed: {error}")))?;
        let thinking = ThinkingRecord {
            version: AUTH_RECORD_VERSION,
            auth_epoch: next.auth_epoch.clone(),
            auth_generation: next.auth_generation,
            key_b64: base64::engine::general_purpose::STANDARD.encode(thinking_key),
        };
        thinking_key.zeroize();

        let oauth_bytes = Zeroizing::new(
            serde_json::to_vec(&oauth)
                .map_err(|_| StorageError::InvalidState("OAuth serialization failed".into()))?,
        );
        let thinking_bytes = Zeroizing::new(
            serde_json::to_vec(&thinking)
                .map_err(|_| StorageError::InvalidState("thinking serialization failed".into()))?,
        );

        if let Err(error) =
            self.secrets
                .save(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &oauth_bytes)
        {
            return self.rollback_or(
                error,
                old_oauth.as_ref().map(|value| value.as_slice()),
                old_thinking.as_ref().map(|value| value.as_slice()),
            );
        }
        if let Err(error) =
            self.secrets
                .save(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &thinking_bytes)
        {
            return self.rollback_or(
                error,
                old_oauth.as_ref().map(|value| value.as_slice()),
                old_thinking.as_ref().map(|value| value.as_slice()),
            );
        }
        if let Err(error) = self.state.commit(&next) {
            return self.rollback_or(
                error,
                old_oauth.as_ref().map(|value| value.as_slice()),
                old_thinking.as_ref().map(|value| value.as_slice()),
            );
        }
        Ok(AuthStatus {
            authenticated: true,
            account_hash: Some(account_hash(&oauth.account_id)),
            expires_at: oauth.expires_at,
            auth_epoch: Some(next.auth_epoch),
            auth_generation: next.auth_generation,
        })
    }

    pub fn status(&self) -> Result<AuthStatus, StorageError> {
        let state = self.state.load()?;
        let Some(state) = state else {
            return Ok(AuthStatus {
                authenticated: false,
                account_hash: None,
                expires_at: None,
                auth_epoch: None,
                auth_generation: 0,
            });
        };
        let generation = state.auth_generation;
        let epoch = Some(state.auth_epoch.clone());
        if !state.committed {
            return Ok(AuthStatus {
                authenticated: false,
                account_hash: None,
                expires_at: None,
                auth_epoch: epoch,
                auth_generation: generation,
            });
        }
        let oauth = self.load_oauth()?;
        let thinking = self.load_thinking()?;
        let matched = oauth
            .as_ref()
            .zip(thinking.as_ref())
            .filter(|(oauth, thinking)| {
                oauth.auth_epoch == state.auth_epoch
                    && thinking.auth_epoch == state.auth_epoch
                    && oauth.auth_generation == state.auth_generation
                    && thinking.auth_generation == state.auth_generation
            });
        let Some((oauth, _thinking)) = matched else {
            return Ok(AuthStatus {
                authenticated: false,
                account_hash: None,
                expires_at: None,
                auth_epoch: epoch,
                auth_generation: generation,
            });
        };
        Ok(AuthStatus {
            authenticated: true,
            account_hash: Some(account_hash(&oauth.account_id)),
            expires_at: oauth.expires_at,
            auth_epoch: epoch,
            auth_generation: generation,
        })
    }

    pub(crate) fn inference_snapshot(&self) -> Result<InferenceSecrets, StorageError> {
        let state = self
            .state
            .load()?
            .filter(|state| state.committed)
            .ok_or(StorageError::NotAuthenticated)?;
        let oauth = self.load_oauth()?.ok_or(StorageError::NotAuthenticated)?;
        let thinking = self
            .load_thinking()?
            .ok_or(StorageError::NotAuthenticated)?;
        if oauth.auth_epoch != state.auth_epoch
            || thinking.auth_epoch != state.auth_epoch
            || oauth.auth_generation != state.auth_generation
            || thinking.auth_generation != state.auth_generation
        {
            return Err(StorageError::NotAuthenticated);
        }
        let decoded = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(&thinking.key_b64)
                .map_err(|_| StorageError::InvalidState("thinking key is invalid".into()))?,
        );
        let thinking_key: [u8; 32] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| StorageError::InvalidState("thinking key is invalid".into()))?;
        Ok(InferenceSecrets {
            access_token: oauth.access_token.clone(),
            account_id: oauth.account_id.clone(),
            thinking_key,
            auth_epoch: state.auth_epoch,
            auth_generation: state.auth_generation,
            account_hash: account_hash(&oauth.account_id),
            expires_at: oauth.expires_at,
        })
    }

    pub fn refresh_snapshot_guarded(
        &self,
        guard: &AuthMutationGuard,
    ) -> Result<RefreshSnapshot, StorageError> {
        self.ensure_guard(guard)?;
        let state = self
            .state
            .load()?
            .filter(|state| state.committed)
            .ok_or(StorageError::NotAuthenticated)?;
        let oauth = self.load_oauth()?.ok_or(StorageError::NotAuthenticated)?;
        let thinking = self
            .load_thinking()?
            .ok_or(StorageError::NotAuthenticated)?;
        if oauth.auth_epoch != state.auth_epoch
            || thinking.auth_epoch != state.auth_epoch
            || oauth.auth_generation != state.auth_generation
            || thinking.auth_generation != state.auth_generation
        {
            return Err(StorageError::NotAuthenticated);
        }
        let refresh_digest = Sha256::digest(oauth.refresh_token.as_bytes()).into();
        Ok(RefreshSnapshot {
            auth_epoch: state.auth_epoch,
            auth_generation: state.auth_generation,
            refresh_token: Zeroizing::new(oauth.refresh_token.clone()),
            refresh_digest,
        })
    }

    pub fn commit_refresh_guarded(
        &self,
        guard: &AuthMutationGuard,
        expected: &RefreshSnapshot,
        update: RefreshUpdate,
    ) -> Result<AuthStatus, StorageError> {
        self.ensure_guard(guard)?;
        validate_refresh_update(&update)?;
        let current = self
            .state
            .load()?
            .filter(|state| state.committed)
            .ok_or(StorageError::AuthChanged)?;
        let old_oauth = self
            .secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?
            .map(Zeroizing::new)
            .ok_or(StorageError::AuthChanged)?;
        let old_thinking = self
            .secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?
            .map(Zeroizing::new)
            .ok_or(StorageError::AuthChanged)?;
        let oauth = parse_oauth_record(&old_oauth)?;
        let thinking = parse_thinking_record(&old_thinking)?;
        let current_digest: [u8; 32] = Sha256::digest(oauth.refresh_token.as_bytes()).into();
        if current.auth_epoch != expected.auth_epoch
            || current.auth_generation != expected.auth_generation
            || oauth.auth_epoch != current.auth_epoch
            || thinking.auth_epoch != current.auth_epoch
            || oauth.auth_generation != current.auth_generation
            || thinking.auth_generation != current.auth_generation
            || current_digest != expected.refresh_digest
        {
            return Err(StorageError::AuthChanged);
        }
        if update
            .account_id
            .as_deref()
            .is_some_and(|account_id| account_id != oauth.account_id)
        {
            return Err(StorageError::AuthChanged);
        }

        let next = current.next_committed()?;
        let access_changed = update.access_token.is_some();
        let refreshed = OAuthRecord {
            version: AUTH_RECORD_VERSION,
            auth_epoch: next.auth_epoch.clone(),
            auth_generation: next.auth_generation,
            access_token: update
                .access_token
                .as_ref()
                .cloned()
                .unwrap_or_else(|| oauth.access_token.clone()),
            refresh_token: update
                .refresh_token
                .as_ref()
                .cloned()
                .unwrap_or_else(|| oauth.refresh_token.clone()),
            id_token: update
                .id_token
                .as_ref()
                .cloned()
                .unwrap_or_else(|| oauth.id_token.clone()),
            account_id: oauth.account_id.clone(),
            expires_at: if access_changed {
                update.expires_at
            } else {
                oauth.expires_at
            },
        };
        let refreshed_thinking = ThinkingRecord {
            version: AUTH_RECORD_VERSION,
            auth_epoch: next.auth_epoch.clone(),
            auth_generation: next.auth_generation,
            key_b64: thinking.key_b64.clone(),
        };
        let oauth_bytes = Zeroizing::new(
            serde_json::to_vec(&refreshed)
                .map_err(|_| StorageError::InvalidState("OAuth serialization failed".into()))?,
        );
        let thinking_bytes = Zeroizing::new(
            serde_json::to_vec(&refreshed_thinking)
                .map_err(|_| StorageError::InvalidState("thinking serialization failed".into()))?,
        );

        if let Err(error) =
            self.secrets
                .save(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &oauth_bytes)
        {
            return self.rollback_or(
                error,
                Some(old_oauth.as_slice()),
                Some(old_thinking.as_slice()),
            );
        }
        if let Err(error) =
            self.secrets
                .save(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &thinking_bytes)
        {
            return self.rollback_or(
                error,
                Some(old_oauth.as_slice()),
                Some(old_thinking.as_slice()),
            );
        }
        if let Err(error) = self.state.commit(&next) {
            return self.rollback_or(
                error,
                Some(old_oauth.as_slice()),
                Some(old_thinking.as_slice()),
            );
        }
        Ok(AuthStatus {
            authenticated: true,
            account_hash: Some(account_hash(&refreshed.account_id)),
            expires_at: refreshed.expires_at,
            auth_epoch: Some(next.auth_epoch),
            auth_generation: next.auth_generation,
        })
    }

    pub fn revoke_token_guarded(
        &self,
        guard: &AuthMutationGuard,
    ) -> Result<Option<RevokeToken>, StorageError> {
        self.ensure_guard(guard)?;
        let Some(state) = self.state.load()?.filter(|state| state.committed) else {
            return Ok(None);
        };
        let Some(oauth) = self.load_oauth()? else {
            return Ok(None);
        };
        let Some(thinking) = self.load_thinking()? else {
            return Ok(None);
        };
        if oauth.auth_epoch != state.auth_epoch
            || thinking.auth_epoch != state.auth_epoch
            || oauth.auth_generation != state.auth_generation
            || thinking.auth_generation != state.auth_generation
        {
            return Ok(None);
        }
        let (kind, token) = if !oauth.refresh_token.is_empty() {
            (RevokeTokenKind::Refresh, oauth.refresh_token.clone())
        } else if !oauth.access_token.is_empty() {
            (RevokeTokenKind::Access, oauth.access_token.clone())
        } else {
            return Ok(None);
        };
        Ok(Some(RevokeToken {
            kind,
            token: Zeroizing::new(token),
        }))
    }

    pub fn commit_logout_guarded(
        &self,
        guard: &AuthMutationGuard,
    ) -> Result<AuthStatus, StorageError> {
        self.ensure_guard(guard)?;
        let current = self.load_or_initialize_state()?;
        let next = current.next_logged_out()?;
        self.state.commit(&next)?;

        let oauth_delete = self
            .secrets
            .delete(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        let thinking_delete = self
            .secrets
            .delete(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        oauth_delete?;
        thinking_delete?;
        Ok(AuthStatus {
            authenticated: false,
            account_hash: None,
            expires_at: None,
            auth_epoch: Some(next.auth_epoch),
            auth_generation: next.auth_generation,
        })
    }

    fn ensure_guard(&self, guard: &AuthMutationGuard) -> Result<(), StorageError> {
        if guard.root == self.lock_root {
            Ok(())
        } else {
            Err(StorageError::AuthChanged)
        }
    }

    fn load_or_initialize_state(&self) -> Result<AuthState, StorageError> {
        match self.state.load()? {
            Some(state) => Ok(state),
            None => {
                let state = AuthState::fresh()?;
                self.state.commit(&state)?;
                Ok(state)
            }
        }
    }

    fn load_oauth(&self) -> Result<Option<OAuthRecord>, StorageError> {
        let Some(bytes) = self
            .secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?
        else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        parse_oauth_record(&bytes).map(Some)
    }

    fn load_thinking(&self) -> Result<Option<ThinkingRecord>, StorageError> {
        let Some(bytes) = self
            .secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?
        else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        parse_thinking_record(&bytes).map(Some)
    }

    fn restore_secret(&self, service: &str, old: Option<&[u8]>) -> Result<(), StorageError> {
        match old {
            Some(value) => self.secrets.save(service, KEYCHAIN_ACCOUNT, value),
            None => self.secrets.delete(service, KEYCHAIN_ACCOUNT),
        }
    }

    fn rollback_or(
        &self,
        original: StorageError,
        old_oauth: Option<&[u8]>,
        old_thinking: Option<&[u8]>,
    ) -> Result<AuthStatus, StorageError> {
        let oauth_restore = self.restore_secret(OAUTH_KEYCHAIN_SERVICE, old_oauth);
        let thinking_restore = self.restore_secret(THINKING_KEYCHAIN_SERVICE, old_thinking);
        if oauth_restore.is_ok() && thinking_restore.is_ok() {
            Err(original)
        } else {
            Err(StorageError::RollbackFailed)
        }
    }
}

fn validate_new_tokens(tokens: &NewOAuthTokens) -> Result<(), StorageError> {
    if tokens.access_token.trim().is_empty()
        || tokens.refresh_token.trim().is_empty()
        || tokens.id_token.trim().is_empty()
        || tokens.account_id.trim().is_empty()
    {
        return Err(StorageError::InvalidState(
            "OAuth response is missing required fields".into(),
        ));
    }
    Ok(())
}

fn validate_refresh_update(update: &RefreshUpdate) -> Result<(), StorageError> {
    if update
        .access_token
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
        || update
            .refresh_token
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        || update
            .id_token
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        || update
            .account_id
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
    {
        return Err(StorageError::InvalidState(
            "OAuth refresh response is invalid".into(),
        ));
    }
    Ok(())
}

fn parse_oauth_record(bytes: &[u8]) -> Result<OAuthRecord, StorageError> {
    let record: OAuthRecord = serde_json::from_slice(bytes)
        .map_err(|_| StorageError::InvalidState("OAuth record JSON is invalid".into()))?;
    record.validate()?;
    Ok(record)
}

fn parse_thinking_record(bytes: &[u8]) -> Result<ThinkingRecord, StorageError> {
    let record: ThinkingRecord = serde_json::from_slice(bytes)
        .map_err(|_| StorageError::InvalidState("thinking record JSON is invalid".into()))?;
    record.validate()?;
    Ok(record)
}

#[derive(Debug)]
struct MutationLock {
    file: File,
}

impl MutationLock {
    fn acquire(root: &Path, timeout: Duration) -> Result<Self, StorageError> {
        ensure_private_dir(root)?;
        let path = root.join(AUTH_LOCK_FILE);
        reject_unsafe_target(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .map_err(|error| io_error("open auth mutation lock", error))?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error("inspect auth mutation lock", error))?;
        if !metadata.is_file() {
            return Err(StorageError::InvalidState(
                "auth mutation lock is not a regular file".into(),
            ));
        }
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("set auth mutation lock permissions", error))?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error("inspect opened auth mutation lock", error))?;
        if !metadata.is_file() {
            return Err(StorageError::InvalidState(
                "auth mutation lock is not a regular file".into(),
            ));
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(StorageError::InvalidState(
                "auth mutation lock permissions are not 0600".into(),
            ));
        }

        #[cfg(unix)]
        {
            let start = Instant::now();
            loop {
                let result =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if result == 0 {
                    return Ok(Self { file });
                }
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
                {
                    return Err(io_error("lock Codex auth mutation", error));
                }
                if start.elapsed() >= timeout {
                    return Err(StorageError::Busy);
                }
                thread::sleep(LOCK_RETRY.min(timeout.saturating_sub(start.elapsed())));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = timeout;
            Err(StorageError::UnsupportedPlatform)
        }
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn ensure_private_dir(root: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(StorageError::InvalidState(
                    "auth root is not a regular directory".into(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|error| io_error("create auth root", error))?;
        }
        Err(error) => return Err(io_error("inspect auth root", error)),
    }
    #[cfg(unix)]
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .map_err(|error| io_error("set auth root permissions", error))?;
    Ok(())
}

fn validate_private_root(root: &Path) -> Result<bool, StorageError> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error("inspect auth root", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StorageError::InvalidState(
            "auth root is not a regular directory".into(),
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(StorageError::InvalidState(
            "auth root permissions are not 0700".into(),
        ));
    }
    Ok(true)
}

fn reject_unsafe_target(path: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Err(StorageError::InvalidState(format!(
                "unsafe auth path: {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect auth path", error)),
    }
}

fn open_read_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options.open(path)
}

fn account_hash(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    hex(&digest[..16])
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn io_error(context: &str, error: std::io::Error) -> StorageError {
    StorageError::Unavailable(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_namespace_is_compile_time_fixed_for_the_build_variant() {
        #[cfg(feature = "acceptance-keychain")]
        {
            assert_eq!(
                OAUTH_KEYCHAIN_SERVICE,
                "com.csswitch.acceptance.codex.oauth.v1"
            );
            assert_eq!(
                THINKING_KEYCHAIN_SERVICE,
                "com.csswitch.acceptance.codex.thinking.v1"
            );
        }
        #[cfg(not(feature = "acceptance-keychain"))]
        {
            assert_eq!(OAUTH_KEYCHAIN_SERVICE, "com.csswitch.codex.oauth.v1");
            assert_eq!(THINKING_KEYCHAIN_SERVICE, "com.csswitch.codex.thinking.v1");
        }
    }
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::process::{Command, Stdio};
    use std::sync::Mutex;

    type SecretMap = HashMap<(String, String), Vec<u8>>;

    #[derive(Clone, Copy)]
    enum SaveBehavior {
        Succeed,
        FailBeforeWrite,
        MutateThenFail,
    }

    #[derive(Clone, Default)]
    struct MemorySecrets {
        values: Arc<Mutex<SecretMap>>,
        save_scripts: Arc<Mutex<HashMap<String, VecDeque<SaveBehavior>>>>,
        delete_failures: Arc<Mutex<HashSet<String>>>,
        delete_calls: Arc<Mutex<Vec<String>>>,
    }

    impl MemorySecrets {
        fn script_save(&self, service: &str, script: impl IntoIterator<Item = SaveBehavior>) {
            self.save_scripts
                .lock()
                .unwrap()
                .insert(service.to_string(), script.into_iter().collect());
        }

        fn fail_delete(&self, service: &str) {
            self.delete_failures
                .lock()
                .unwrap()
                .insert(service.to_string());
        }
    }

    impl SecretStore for MemorySecrets {
        fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(&(service.to_string(), account.to_string()))
                .cloned())
        }

        fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), StorageError> {
            let behavior = self
                .save_scripts
                .lock()
                .unwrap()
                .get_mut(service)
                .and_then(VecDeque::pop_front)
                .unwrap_or(SaveBehavior::Succeed);
            if matches!(behavior, SaveBehavior::FailBeforeWrite) {
                return Err(StorageError::Unavailable("injected save failure".into()));
            }
            self.values
                .lock()
                .unwrap()
                .insert((service.to_string(), account.to_string()), value.to_vec());
            if matches!(behavior, SaveBehavior::MutateThenFail) {
                Err(StorageError::Unavailable(
                    "injected post-write failure".into(),
                ))
            } else {
                Ok(())
            }
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), StorageError> {
            self.delete_calls.lock().unwrap().push(service.to_string());
            if self.delete_failures.lock().unwrap().contains(service) {
                return Err(StorageError::KeychainUnavailable(
                    "injected delete failure".into(),
                ));
            }
            self.values
                .lock()
                .unwrap()
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct MemoryState {
        value: Arc<Mutex<Option<AuthState>>>,
        fail_commit_generation: Arc<Mutex<Option<u64>>>,
    }

    impl MemoryState {
        fn fail_commit(&self, generation: u64) {
            *self.fail_commit_generation.lock().unwrap() = Some(generation);
        }
    }

    impl StateStore for MemoryState {
        fn load(&self) -> Result<Option<AuthState>, StorageError> {
            Ok(self.value.lock().unwrap().clone())
        }

        fn commit(&self, state: &AuthState) -> Result<(), StorageError> {
            if *self.fail_commit_generation.lock().unwrap() == Some(state.auth_generation) {
                return Err(StorageError::Unavailable("injected state failure".into()));
            }
            *self.value.lock().unwrap() = Some(state.clone());
            Ok(())
        }
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::getrandom(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "csswitch-codex-auth-test-{}-{}",
                std::process::id(),
                hex(&random)
            ));
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tokens(seed: &str) -> NewOAuthTokens {
        NewOAuthTokens {
            access_token: format!("access-{seed}"),
            refresh_token: format!("refresh-{seed}"),
            id_token: format!("id-{seed}"),
            account_id: format!("account-{seed}"),
            expires_at: Some(2_000_000_000),
        }
    }

    fn refresh_update(seed: &str, account_id: Option<&str>) -> RefreshUpdate {
        RefreshUpdate {
            access_token: Some(format!("access-{seed}")),
            refresh_token: Some(format!("refresh-{seed}")),
            id_token: Some(format!("id-{seed}")),
            account_id: account_id.map(str::to_string),
            expires_at: Some(2_100_000_000),
        }
    }

    fn repository(
        secrets: MemorySecrets,
        state: MemoryState,
        root: &TempRoot,
    ) -> AuthRepository<MemorySecrets, MemoryState> {
        AuthRepository::new(secrets, state, root.0.clone())
            .with_lock_timeout(Duration::from_millis(20))
    }

    #[test]
    fn successful_login_commits_matching_oauth_thinking_and_state() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state.clone(), &root);

        let status = repo.commit_login(tokens("one")).unwrap();
        assert!(status.authenticated);
        assert_eq!(status.auth_generation, 1);
        assert_eq!(status.account_hash.as_deref().map(str::len), Some(32));
        assert!(secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap()
            .is_some());
        assert!(secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap()
            .is_some());
        assert!(state.load().unwrap().unwrap().committed);
    }

    #[test]
    fn inference_snapshot_requires_exact_state_oauth_and_thinking_generation() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets, state.clone(), &root);

        repo.commit_login(tokens("snapshot")).unwrap();
        let snapshot = repo.inference_snapshot().unwrap();
        assert_eq!(snapshot.access_token(), "access-snapshot");
        assert_eq!(snapshot.account_id(), "account-snapshot");
        assert_eq!(snapshot.auth_generation(), 1);
        assert_eq!(snapshot.auth_epoch().len(), 32);
        assert_eq!(snapshot.account_hash().len(), 32);
        assert_eq!(snapshot.expires_at(), Some(2_000_000_000));
        assert!(snapshot.thinking_key().iter().any(|byte| *byte != 0));
        drop(snapshot);

        let mut mismatched = state.load().unwrap().unwrap();
        mismatched.auth_generation += 1;
        state.commit(&mismatched).unwrap();
        assert_eq!(
            repo.inference_snapshot().err(),
            Some(StorageError::NotAuthenticated)
        );
    }

    #[test]
    fn thinking_write_failure_restores_previous_oauth_record() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state, &root);
        repo.commit_login(tokens("old")).unwrap();
        let old_oauth = secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        let old_thinking = secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();

        secrets.script_save(
            THINKING_KEYCHAIN_SERVICE,
            [SaveBehavior::MutateThenFail, SaveBehavior::Succeed],
        );
        assert!(repo.commit_login(tokens("new")).is_err());
        assert_eq!(
            secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_oauth
        );
        assert_eq!(
            secrets
                .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_thinking
        );
        assert_eq!(repo.status().unwrap().auth_generation, 1);
        assert!(repo.status().unwrap().authenticated);
    }

    #[test]
    fn state_commit_failure_restores_both_previous_records() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state.clone(), &root);
        repo.commit_login(tokens("old")).unwrap();
        let old_oauth = secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        let old_thinking = secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();

        state.fail_commit(2);
        assert!(repo.commit_login(tokens("new")).is_err());
        assert_eq!(
            secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_oauth
        );
        assert_eq!(
            secrets
                .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_thinking
        );
        assert_eq!(repo.status().unwrap().auth_generation, 1);
        assert!(repo.status().unwrap().authenticated);
    }

    #[test]
    fn restore_failure_returns_rollback_failed_and_disables_new_generation() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state.clone(), &root);
        repo.commit_login(tokens("old")).unwrap();

        state.fail_commit(2);
        secrets.script_save(
            OAUTH_KEYCHAIN_SERVICE,
            [SaveBehavior::Succeed, SaveBehavior::FailBeforeWrite],
        );
        secrets.script_save(
            THINKING_KEYCHAIN_SERVICE,
            [SaveBehavior::Succeed, SaveBehavior::Succeed],
        );
        let error = repo
            .commit_login(tokens("new-sentinel-secret"))
            .unwrap_err();

        assert_eq!(error, StorageError::RollbackFailed);
        assert!(!repo.status().unwrap().authenticated);
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("new-sentinel-secret"));
    }

    #[test]
    fn refresh_commits_one_generation_and_preserves_thinking_key() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state, &root);
        repo.commit_login(tokens("old")).unwrap();
        let old_thinking: ThinkingRecord = serde_json::from_slice(
            &secrets
                .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap()
                .unwrap(),
        )
        .unwrap();

        let guard = repo.begin_mutation().unwrap();
        let snapshot = repo.refresh_snapshot_guarded(&guard).unwrap();
        let status = repo
            .commit_refresh_guarded(
                &guard,
                &snapshot,
                refresh_update("new", Some("account-old")),
            )
            .unwrap();
        assert_eq!(status.auth_generation, 2);
        let oauth: OAuthRecord = serde_json::from_slice(
            &secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let thinking: ThinkingRecord = serde_json::from_slice(
            &secrets
                .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(oauth.access_token, "access-new");
        assert_eq!(oauth.refresh_token, "refresh-new");
        assert_eq!(oauth.auth_generation, 2);
        assert_eq!(thinking.auth_generation, 2);
        assert_eq!(thinking.key_b64, old_thinking.key_b64);
    }

    #[test]
    fn refresh_missing_access_and_id_preserves_existing_values() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let repo = repository(secrets.clone(), MemoryState::default(), &root);
        repo.commit_login(tokens("old")).unwrap();
        let guard = repo.begin_mutation().unwrap();
        let snapshot = repo.refresh_snapshot_guarded(&guard).unwrap();
        let status = repo
            .commit_refresh_guarded(
                &guard,
                &snapshot,
                RefreshUpdate {
                    access_token: None,
                    refresh_token: Some("refresh-only".into()),
                    id_token: None,
                    account_id: None,
                    expires_at: None,
                },
            )
            .unwrap();
        let oauth: OAuthRecord = serde_json::from_slice(
            &secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.auth_generation, 2);
        assert_eq!(oauth.access_token, "access-old");
        assert_eq!(oauth.id_token, "id-old");
        assert_eq!(oauth.refresh_token, "refresh-only");
        assert_eq!(oauth.expires_at, Some(2_000_000_000));
    }

    #[test]
    fn refresh_cas_rejects_changed_refresh_token_without_commit() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let repo = repository(secrets.clone(), MemoryState::default(), &root);
        repo.commit_login(tokens("old")).unwrap();
        let guard = repo.begin_mutation().unwrap();
        let snapshot = repo.refresh_snapshot_guarded(&guard).unwrap();
        let raw = secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap()
            .unwrap();
        let mut oauth: OAuthRecord = serde_json::from_slice(&raw).unwrap();
        oauth.refresh_token = "externally-changed".into();
        secrets
            .save(
                OAUTH_KEYCHAIN_SERVICE,
                KEYCHAIN_ACCOUNT,
                &serde_json::to_vec(&oauth).unwrap(),
            )
            .unwrap();

        assert_eq!(
            repo.commit_refresh_guarded(
                &guard,
                &snapshot,
                refresh_update("new", Some("account-old")),
            )
            .unwrap_err(),
            StorageError::AuthChanged
        );
        assert_eq!(repo.status().unwrap().auth_generation, 1);
    }

    #[test]
    fn refresh_thinking_failure_restores_both_records() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let repo = repository(secrets.clone(), MemoryState::default(), &root);
        repo.commit_login(tokens("old")).unwrap();
        let old_oauth = secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        let old_thinking = secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        secrets.script_save(
            THINKING_KEYCHAIN_SERVICE,
            [SaveBehavior::MutateThenFail, SaveBehavior::Succeed],
        );
        let guard = repo.begin_mutation().unwrap();
        let snapshot = repo.refresh_snapshot_guarded(&guard).unwrap();

        assert!(repo
            .commit_refresh_guarded(
                &guard,
                &snapshot,
                refresh_update("new", Some("account-old")),
            )
            .is_err());
        assert_eq!(
            secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_oauth
        );
        assert_eq!(
            secrets
                .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_thinking
        );
        assert_eq!(repo.status().unwrap().auth_generation, 1);
    }

    #[test]
    fn refresh_state_failure_restores_both_records() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state.clone(), &root);
        repo.commit_login(tokens("old")).unwrap();
        let old_oauth = secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        let old_thinking = secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        state.fail_commit(2);
        let guard = repo.begin_mutation().unwrap();
        let snapshot = repo.refresh_snapshot_guarded(&guard).unwrap();

        assert!(repo
            .commit_refresh_guarded(
                &guard,
                &snapshot,
                refresh_update("new", Some("account-old")),
            )
            .is_err());
        assert_eq!(
            secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_oauth
        );
        assert_eq!(
            secrets
                .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_thinking
        );
        assert_eq!(repo.status().unwrap().auth_generation, 1);
        assert!(repo.status().unwrap().authenticated);
    }

    #[test]
    fn refresh_account_mismatch_is_fail_closed_without_writes() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let repo = repository(secrets.clone(), MemoryState::default(), &root);
        repo.commit_login(tokens("old")).unwrap();
        let old_oauth = secrets
            .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap();
        let guard = repo.begin_mutation().unwrap();
        let snapshot = repo.refresh_snapshot_guarded(&guard).unwrap();
        assert_eq!(
            repo.commit_refresh_guarded(
                &guard,
                &snapshot,
                refresh_update("new", Some("different-account")),
            )
            .unwrap_err(),
            StorageError::AuthChanged
        );
        assert_eq!(
            secrets
                .load(OAUTH_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .unwrap(),
            old_oauth
        );
        assert_eq!(repo.status().unwrap().auth_generation, 1);
    }

    #[test]
    fn logout_invalidates_generation_before_attempting_both_deletes() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let repo = repository(secrets.clone(), MemoryState::default(), &root);
        repo.commit_login(tokens("old")).unwrap();
        secrets.fail_delete(OAUTH_KEYCHAIN_SERVICE);
        let guard = repo.begin_mutation().unwrap();
        assert_eq!(
            repo.revoke_token_guarded(&guard).unwrap().unwrap().kind,
            RevokeTokenKind::Refresh
        );
        assert!(repo.commit_logout_guarded(&guard).is_err());
        let status = repo.status().unwrap();
        assert!(!status.authenticated);
        assert_eq!(status.auth_generation, 2);
        let calls = secrets.delete_calls.lock().unwrap().clone();
        assert!(calls
            .iter()
            .any(|service| service == OAUTH_KEYCHAIN_SERVICE));
        assert!(calls
            .iter()
            .any(|service| service == THINKING_KEYCHAIN_SERVICE));
    }

    #[test]
    fn logout_state_failure_keeps_previous_credentials_active() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state.clone(), &root);
        repo.commit_login(tokens("old")).unwrap();
        state.fail_commit(2);
        let guard = repo.begin_mutation().unwrap();
        assert!(repo.commit_logout_guarded(&guard).is_err());
        assert!(repo.status().unwrap().authenticated);
        assert!(secrets.delete_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn mismatched_record_generation_is_fail_closed() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let state = MemoryState::default();
        let repo = repository(secrets.clone(), state, &root);
        repo.commit_login(tokens("one")).unwrap();
        let raw = secrets
            .load(THINKING_KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap()
            .unwrap();
        let mut record: ThinkingRecord = serde_json::from_slice(&raw).unwrap();
        record.auth_generation += 1;
        secrets
            .save(
                THINKING_KEYCHAIN_SERVICE,
                KEYCHAIN_ACCOUNT,
                &serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();

        assert!(!repo.status().unwrap().authenticated);
    }

    #[test]
    fn filesystem_state_is_private_atomic_and_rejects_symlink() {
        let root = TempRoot::new();
        let store = FsStateStore::new(root.0.clone());
        let state = AuthState::fresh().unwrap();
        store.commit(&state).unwrap();
        assert_eq!(store.load().unwrap(), Some(state));
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(root.0.join(AUTH_STATE_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(store.load(), Err(StorageError::InvalidState(_))));
            fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(
                root.0.join(AUTH_STATE_FILE),
                fs::Permissions::from_mode(0o400),
            )
            .unwrap();
            assert!(matches!(store.load(), Err(StorageError::InvalidState(_))));
            fs::set_permissions(
                root.0.join(AUTH_STATE_FILE),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert!(matches!(store.load(), Err(StorageError::InvalidState(_))));
        }

        fs::remove_file(root.0.join(AUTH_STATE_FILE)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("target", root.0.join(AUTH_STATE_FILE)).unwrap();
        #[cfg(unix)]
        assert!(matches!(store.load(), Err(StorageError::InvalidState(_))));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_state_load_rejects_root_symlink() {
        let base = TempRoot::new();
        fs::create_dir_all(&base.0).unwrap();
        fs::set_permissions(&base.0, fs::Permissions::from_mode(0o700)).unwrap();
        let target = base.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let store = FsStateStore::new(link);

        assert!(matches!(store.load(), Err(StorageError::InvalidState(_))));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_state_fifo_fails_closed_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = TempRoot::new();
        fs::create_dir_all(&root.0).unwrap();
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = root.0.join(AUTH_STATE_FILE);
        let c_path = CString::new(state_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let store = FsStateStore::new(root.0.clone());

        let started = Instant::now();
        assert!(matches!(store.load(), Err(StorageError::InvalidState(_))));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn mutation_lock_times_out_then_recovers_after_drop() {
        let root = TempRoot::new();
        let first = MutationLock::acquire(&root.0, Duration::from_millis(20)).unwrap();
        assert_eq!(
            MutationLock::acquire(&root.0, Duration::from_millis(20)).unwrap_err(),
            StorageError::Busy
        );
        drop(first);
        let recovered = MutationLock::acquire(&root.0, Duration::from_millis(20)).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                recovered.file.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn mutation_lock_child_helper() {
        let Some(root) = std::env::var_os("CSSWITCH_TEST_AUTH_LOCK_CHILD_ROOT") else {
            return;
        };
        let Some(ready) = std::env::var_os("CSSWITCH_TEST_AUTH_LOCK_CHILD_READY") else {
            return;
        };
        let _lock = MutationLock::acquire(Path::new(&root), Duration::from_secs(2)).unwrap();
        fs::write(ready, b"ready").unwrap();
        thread::sleep(Duration::from_secs(30));
    }

    #[cfg(unix)]
    #[test]
    fn mutation_lock_is_released_when_holder_process_is_killed() {
        let root = TempRoot::new();
        let ready = root.0.with_extension("ready");
        let current_test = std::env::current_exe().unwrap();
        let mut child = Command::new(current_test)
            .arg("--exact")
            .arg("codex_auth::storage::tests::mutation_lock_child_helper")
            .arg("--nocapture")
            .env("CSSWITCH_TEST_AUTH_LOCK_CHILD_ROOT", &root.0)
            .env("CSSWITCH_TEST_AUTH_LOCK_CHILD_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "child did not acquire lock in time");
        assert_eq!(
            MutationLock::acquire(&root.0, Duration::from_millis(50)).unwrap_err(),
            StorageError::Busy
        );

        child.kill().unwrap();
        child.wait().unwrap();
        MutationLock::acquire(&root.0, Duration::from_secs(1)).unwrap();
        let _ = fs::remove_file(ready);
    }

    #[test]
    fn auth_status_serialization_contains_no_secret_fields() {
        let root = TempRoot::new();
        let repo = repository(MemorySecrets::default(), MemoryState::default(), &root);
        let status = repo.commit_login(tokens("private-marker")).unwrap();
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("private-marker"));
        assert!(!json.contains("token"));
        assert!(json.contains("account_hash"));
    }

    #[test]
    fn invalid_tokens_never_write_any_secret() {
        let root = TempRoot::new();
        let secrets = MemorySecrets::default();
        let repo = repository(secrets.clone(), MemoryState::default(), &root);
        let mut invalid = tokens("bad");
        invalid.refresh_token.clear();
        assert!(matches!(
            repo.commit_login(invalid),
            Err(StorageError::InvalidState(_))
        ));
        assert!(secrets.values.lock().unwrap().is_empty());
    }
}
