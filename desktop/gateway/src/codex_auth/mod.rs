mod cli;
mod oauth;
mod storage;

use std::path::PathBuf;

pub use cli::{run_cli, CliRun};
pub use oauth::{OAuthErrorCode, OAuthFlowError};
pub use storage::AuthStatus;
pub(crate) use storage::InferenceSecrets;

pub const OAUTH_KEYCHAIN_SERVICE: &str = storage::OAUTH_KEYCHAIN_SERVICE;

pub fn expected_keychain_service_matches(expected: Option<&str>) -> bool {
    expected.is_none_or(|value| value == OAUTH_KEYCHAIN_SERVICE)
}

pub(crate) fn production_inference_snapshot(
    state_root: PathBuf,
) -> Result<InferenceSecrets, OAuthFlowError> {
    #[cfg(target_os = "macos")]
    {
        storage::AuthRepository::production(state_root)
            .inference_snapshot()
            .map_err(OAuthFlowError::from)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state_root;
        Err(storage::StorageError::UnsupportedPlatform.into())
    }
}

pub fn run_production_login(state_root: PathBuf) -> Result<AuthStatus, OAuthFlowError> {
    #[cfg(target_os = "macos")]
    {
        let repository = storage::AuthRepository::production(state_root);
        let transport = oauth::HttpOAuthTransport::production()?;
        oauth::run_login_flow(
            &repository,
            &oauth::SystemBrowser,
            &transport,
            &oauth::LoginOptions::production(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state_root;
        Err(storage::StorageError::UnsupportedPlatform.into())
    }
}

pub fn production_status(state_root: PathBuf) -> Result<AuthStatus, OAuthFlowError> {
    #[cfg(target_os = "macos")]
    {
        storage::AuthRepository::production(state_root)
            .status()
            .map_err(OAuthFlowError::from)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state_root;
        Err(storage::StorageError::UnsupportedPlatform.into())
    }
}

pub fn refresh_production_for_generation(
    state_root: PathBuf,
    expected_generation: u64,
) -> Result<AuthStatus, OAuthFlowError> {
    #[cfg(target_os = "macos")]
    {
        let repository = storage::AuthRepository::production(state_root);
        let transport = oauth::HttpOAuthTransport::production()?;
        refresh_for_generation(&repository, &transport, expected_generation)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state_root, expected_generation);
        Err(storage::StorageError::UnsupportedPlatform.into())
    }
}

pub fn run_production_logout(state_root: PathBuf) -> Result<AuthStatus, OAuthFlowError> {
    #[cfg(target_os = "macos")]
    {
        let repository = storage::AuthRepository::production(state_root);
        let transport = oauth::HttpOAuthTransport::production().ok();
        run_logout(
            &repository,
            transport
                .as_ref()
                .map(|transport| transport as &dyn oauth::RevokeTransport),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state_root;
        Err(storage::StorageError::UnsupportedPlatform.into())
    }
}

fn refresh_for_generation<S, T, R>(
    repository: &storage::AuthRepository<S, T>,
    transport: &R,
    expected_generation: u64,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: storage::SecretStore,
    T: storage::StateStore,
    R: oauth::RefreshTransport,
{
    let guard = repository.begin_mutation()?;
    let snapshot = repository.refresh_snapshot_guarded(&guard)?;
    if snapshot.auth_generation != expected_generation {
        return repository.status().map_err(OAuthFlowError::from);
    }
    let update = transport.refresh(&snapshot.refresh_token)?;
    repository
        .commit_refresh_guarded(&guard, &snapshot, update)
        .map_err(OAuthFlowError::from)
}

fn run_logout<S, T>(
    repository: &storage::AuthRepository<S, T>,
    transport: Option<&dyn oauth::RevokeTransport>,
) -> Result<AuthStatus, OAuthFlowError>
where
    S: storage::SecretStore,
    T: storage::StateStore,
{
    let guard = repository.begin_mutation()?;
    if let Some(transport) = transport {
        if let Ok(Some(token)) = repository.revoke_token_guarded(&guard) {
            let _ = transport.revoke(&token);
        }
    }
    repository
        .commit_logout_guarded(&guard)
        .map_err(OAuthFlowError::from)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use base64::Engine;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    type SecretMap = HashMap<(String, String), Vec<u8>>;

    #[derive(Clone, Default)]
    struct MemorySecrets(Arc<Mutex<SecretMap>>);

    impl storage::SecretStore for MemorySecrets {
        fn load(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<Vec<u8>>, storage::StorageError> {
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
        ) -> Result<(), storage::StorageError> {
            self.0
                .lock()
                .unwrap()
                .insert((service.to_string(), account.to_string()), value.to_vec());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), storage::StorageError> {
            self.0
                .lock()
                .unwrap()
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct MemoryState(Arc<Mutex<Option<storage::AuthState>>>);

    impl storage::StateStore for MemoryState {
        fn load(&self) -> Result<Option<storage::AuthState>, storage::StorageError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn commit(&self, state: &storage::AuthState) -> Result<(), storage::StorageError> {
            *self.0.lock().unwrap() = Some(state.clone());
            Ok(())
        }
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::getrandom(&mut random).unwrap();
            Self(std::env::temp_dir().join(format!(
                "csswitch-codex-lifecycle-test-{}-{}",
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

    fn repository(root: &TempRoot) -> storage::AuthRepository<MemorySecrets, MemoryState> {
        storage::AuthRepository::new(
            MemorySecrets::default(),
            MemoryState::default(),
            root.0.clone(),
        )
        .with_lock_timeout(Duration::from_millis(500))
    }

    fn login(repository: &storage::AuthRepository<MemorySecrets, MemoryState>) {
        repository
            .commit_login(storage::NewOAuthTokens {
                access_token: "access-old".into(),
                refresh_token: "refresh-old".into(),
                id_token: "id-old".into(),
                account_id: "account-old".into(),
                expires_at: Some(2_000_000_000),
            })
            .unwrap();
    }

    #[derive(Clone)]
    struct CountingRefresh {
        calls: Arc<AtomicUsize>,
        barrier: Arc<Barrier>,
    }

    impl oauth::RefreshTransport for CountingRefresh {
        fn refresh(&self, refresh_token: &str) -> Result<storage::RefreshUpdate, OAuthFlowError> {
            assert_eq!(refresh_token, "refresh-old");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait();
            thread::sleep(Duration::from_millis(30));
            Ok(storage::RefreshUpdate {
                access_token: Some("access-new".into()),
                refresh_token: Some("refresh-new".into()),
                id_token: None,
                account_id: None,
                expires_at: Some(2_100_000_000),
            })
        }
    }

    #[test]
    fn concurrent_refresh_for_same_generation_has_one_network_writer() {
        let root = TempRoot::new();
        let repository = repository(&root);
        login(&repository);
        let transport = CountingRefresh {
            calls: Arc::new(AtomicUsize::new(0)),
            barrier: Arc::new(Barrier::new(2)),
        };
        let repository_one = repository.clone();
        let repository_two = repository.clone();
        let transport_one = transport.clone();
        let transport_two = transport.clone();
        let first = thread::spawn(move || {
            refresh_for_generation(&repository_one, &transport_one, 1).unwrap()
        });
        transport.barrier.wait();
        let second = thread::spawn(move || {
            refresh_for_generation(&repository_two, &transport_two, 1).unwrap()
        });

        assert_eq!(first.join().unwrap().auth_generation, 2);
        assert_eq!(second.join().unwrap().auth_generation, 2);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        assert_eq!(repository.status().unwrap().auth_generation, 2);
    }

    struct FailedRefresh;

    impl oauth::RefreshTransport for FailedRefresh {
        fn refresh(&self, _refresh_token: &str) -> Result<storage::RefreshUpdate, OAuthFlowError> {
            Err(OAuthFlowError {
                code: OAuthErrorCode::OAuthNetwork,
                retryable: true,
                message: "injected refresh failure",
            })
        }
    }

    #[test]
    fn failed_refresh_keeps_generation_and_credentials_active() {
        let root = TempRoot::new();
        let repository = repository(&root);
        login(&repository);
        let error = refresh_for_generation(&repository, &FailedRefresh, 1).unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::OAuthNetwork);
        let status = repository.status().unwrap();
        assert!(status.authenticated);
        assert_eq!(status.auth_generation, 1);
        drop(repository.begin_mutation().unwrap());
    }

    struct FailedRevoke(Arc<AtomicUsize>);

    impl oauth::RevokeTransport for FailedRevoke {
        fn revoke(&self, _token: &storage::RevokeToken) -> Result<(), OAuthFlowError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(OAuthFlowError {
                code: OAuthErrorCode::OAuthNetwork,
                retryable: true,
                message: "injected revoke failure",
            })
        }
    }

    #[test]
    fn revoke_failure_never_prevents_local_logout() {
        let root = TempRoot::new();
        let repository = repository(&root);
        login(&repository);
        let calls = Arc::new(AtomicUsize::new(0));
        let revoker = FailedRevoke(Arc::clone(&calls));
        let status = run_logout(&repository, Some(&revoker)).unwrap();
        assert!(!status.authenticated);
        assert_eq!(status.auth_generation, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!repository.status().unwrap().authenticated);
    }

    #[test]
    fn unavailable_revoke_client_still_performs_local_logout() {
        let root = TempRoot::new();
        let repository = repository(&root);
        login(&repository);
        let status = run_logout(&repository, None).unwrap();
        assert!(!status.authenticated);
        assert_eq!(status.auth_generation, 2);
        assert!(!repository.status().unwrap().authenticated);
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod non_macos_tests {
    use super::*;

    #[test]
    fn production_facades_fail_before_touching_state() {
        let root = std::env::temp_dir().join("csswitch-codex-non-macos-must-not-exist");
        assert!(!root.exists());
        assert_eq!(
            run_production_login(root.clone()).unwrap_err().code,
            OAuthErrorCode::UnsupportedPlatform
        );
        assert_eq!(
            production_status(root.clone()).unwrap_err().code,
            OAuthErrorCode::UnsupportedPlatform
        );
        assert_eq!(
            refresh_production_for_generation(root.clone(), 1)
                .unwrap_err()
                .code,
            OAuthErrorCode::UnsupportedPlatform
        );
        assert_eq!(
            run_production_logout(root.clone()).unwrap_err().code,
            OAuthErrorCode::UnsupportedPlatform
        );
        assert!(!root.exists());
    }
}

#[cfg(test)]
mod handshake_tests {
    use super::*;

    #[test]
    fn managed_keychain_service_handshake_is_checked_before_auth_dispatch() {
        assert!(expected_keychain_service_matches(None));
        assert!(expected_keychain_service_matches(Some(
            OAUTH_KEYCHAIN_SERVICE
        )));
        assert!(!expected_keychain_service_matches(Some("wrong.service")));
        assert!(!expected_keychain_service_matches(Some("")));
    }
}
