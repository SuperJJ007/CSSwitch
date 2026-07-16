use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;

const TERMINAL_STATES: &[&str] = &["succeeded", "failed", "cancelled"];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OperationErrorView {
    pub(crate) code: String,
    pub(crate) stage: String,
    pub(crate) retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) challenge_detected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transport_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OperationSnapshot {
    pub(crate) schema_version: u32,
    pub(crate) operation_id: String,
    pub(crate) sequence: u64,
    pub(crate) method: String,
    pub(crate) state: String,
    pub(crate) started_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verification_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<OperationErrorView>,
}

impl OperationSnapshot {
    fn starting(operation_id: String, method: &str) -> Self {
        let now = crate::config::now_ms();
        Self {
            schema_version: 2,
            operation_id,
            sequence: 1,
            method: method.to_string(),
            state: "starting".into(),
            started_at_ms: now,
            updated_at_ms: now,
            expires_at_ms: None,
            verification_url: None,
            user_code: None,
            error: None,
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        TERMINAL_STATES.contains(&self.state.as_str())
    }
}

#[derive(Clone)]
struct LoginOperation {
    snapshot: OperationSnapshot,
    cancel: Arc<AtomicBool>,
    pid: Option<u32>,
    cancel_disposition: Option<String>,
}

#[derive(Default)]
struct SupervisorInner {
    codex_users: usize,
    other_mutation: bool,
    active_login: Option<LoginOperation>,
    last_snapshot: Option<OperationSnapshot>,
}

#[derive(Default)]
pub(crate) struct CodexAuthSupervisor {
    inner: Mutex<SupervisorInner>,
    cancel_changed: Condvar,
}

pub(crate) type SharedCodexAuthSupervisor = Arc<CodexAuthSupervisor>;

pub(crate) struct LoginReservation {
    pub(crate) operation_id: String,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) snapshot: OperationSnapshot,
}

pub(crate) struct CodexUseLease {
    supervisor: SharedCodexAuthSupervisor,
    released: bool,
}

impl Drop for CodexUseLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut inner = self
            .supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inner.codex_users = inner.codex_users.saturating_sub(1);
        self.released = true;
    }
}

pub(crate) struct CodexMutationLease {
    supervisor: SharedCodexAuthSupervisor,
    released: bool,
}

impl Drop for CodexMutationLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut inner = self
            .supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inner.other_mutation = false;
        self.released = true;
    }
}

impl CodexAuthSupervisor {
    pub(crate) fn begin_login(&self, method: &str) -> Result<LoginReservation, String> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.active_login.is_some() || inner.other_mutation {
            return Err("auth_busy：另一项 Codex 认证操作正在进行。".into());
        }
        if inner.codex_users != 0 {
            return Err("codex_busy：Codex 启动或模型探测正在进行，请稍后重试。".into());
        }
        let operation_id = crate::config::new_id();
        let snapshot = OperationSnapshot::starting(operation_id.clone(), method);
        let cancel = Arc::new(AtomicBool::new(false));
        inner.active_login = Some(LoginOperation {
            snapshot: snapshot.clone(),
            cancel: cancel.clone(),
            pid: None,
            cancel_disposition: None,
        });
        inner.last_snapshot = Some(snapshot.clone());
        Ok(LoginReservation {
            operation_id,
            cancel,
            snapshot,
        })
    }

    pub(crate) fn abort_login_start(&self, operation_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner
            .active_login
            .as_ref()
            .is_some_and(|operation| operation.snapshot.operation_id == operation_id)
        {
            inner.active_login = None;
            inner.last_snapshot = None;
        }
    }

    pub(crate) fn set_pid(&self, operation_id: &str, pid: u32) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let operation = inner
            .active_login
            .as_mut()
            .filter(|operation| operation.snapshot.operation_id == operation_id)
            .ok_or_else(|| "Codex 登录 operation 已失效。".to_string())?;
        operation.pid = Some(pid);
        Ok(())
    }

    pub(crate) fn acquire_use(
        supervisor: &SharedCodexAuthSupervisor,
    ) -> Result<CodexUseLease, String> {
        let mut inner = supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.active_login.is_some() || inner.other_mutation {
            return Err("auth_busy：Codex 正在登录或变更认证状态。".into());
        }
        inner.codex_users = inner.codex_users.saturating_add(1);
        Ok(CodexUseLease {
            supervisor: supervisor.clone(),
            released: false,
        })
    }

    pub(crate) fn begin_mutation(
        supervisor: &SharedCodexAuthSupervisor,
    ) -> Result<CodexMutationLease, String> {
        let mut inner = supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.active_login.is_some() || inner.other_mutation {
            return Err("auth_busy：另一项 Codex 认证操作正在进行。".into());
        }
        if inner.codex_users != 0 {
            return Err("codex_busy：Codex 启动或模型探测正在进行，请稍后重试。".into());
        }
        inner.other_mutation = true;
        Ok(CodexMutationLease {
            supervisor: supervisor.clone(),
            released: false,
        })
    }

    pub(crate) fn snapshot(&self) -> Option<OperationSnapshot> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .active_login
            .as_ref()
            .map(|operation| operation.snapshot.clone())
            .or_else(|| inner.last_snapshot.clone())
    }

    pub(crate) fn update_progress(
        &self,
        operation_id: &str,
        state: &str,
        expires_at_ms: Option<i64>,
        verification_url: Option<String>,
        user_code: Option<String>,
    ) -> Result<OperationSnapshot, String> {
        if TERMINAL_STATES.contains(&state) {
            return Err("Codex 登录 progress 不能直接写入终态。".into());
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let snapshot = {
            let operation = inner
                .active_login
                .as_mut()
                .filter(|operation| operation.snapshot.operation_id == operation_id)
                .ok_or_else(|| "Codex 登录 operation 已失效。".to_string())?;
            operation.snapshot.sequence = operation.snapshot.sequence.saturating_add(1);
            operation.snapshot.state = state.to_string();
            operation.snapshot.updated_at_ms = crate::config::now_ms();
            if state == "verification_required" {
                operation.snapshot.expires_at_ms = expires_at_ms;
                operation.snapshot.verification_url = verification_url;
                operation.snapshot.user_code = user_code;
            } else if matches!(state, "exchanging" | "committing") {
                operation.snapshot.expires_at_ms = None;
                operation.snapshot.verification_url = None;
                operation.snapshot.user_code = None;
            }
            operation.snapshot.error = None;
            operation.snapshot.clone()
        };
        inner.last_snapshot = Some(snapshot.clone());
        self.cancel_changed.notify_all();
        Ok(snapshot)
    }

    pub(crate) fn finish(
        &self,
        operation_id: &str,
        state: &str,
        error: Option<OperationErrorView>,
    ) -> Result<OperationSnapshot, String> {
        if !TERMINAL_STATES.contains(&state) {
            return Err("Codex 登录终态非法。".into());
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut operation = inner
            .active_login
            .take()
            .filter(|operation| operation.snapshot.operation_id == operation_id)
            .ok_or_else(|| "Codex 登录 operation 已失效。".to_string())?;
        operation.snapshot.sequence = operation.snapshot.sequence.saturating_add(1);
        operation.snapshot.state = state.to_string();
        operation.snapshot.updated_at_ms = crate::config::now_ms();
        operation.snapshot.expires_at_ms = None;
        operation.snapshot.verification_url = None;
        operation.snapshot.user_code = None;
        operation.snapshot.error = error;
        let snapshot = operation.snapshot;
        inner.last_snapshot = Some(snapshot.clone());
        self.cancel_changed.notify_all();
        Ok(snapshot)
    }

    pub(crate) fn cancel(&self, operation_id: &str) -> Result<&'static str, String> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(operation) = inner.active_login.as_ref() {
            if operation.snapshot.operation_id != operation_id {
                return Err("operation_not_found：Codex 登录 operation 不存在或已被替换。".into());
            }
            if operation.snapshot.state == "committing" {
                return Ok("commit_in_progress");
            }
            if let Some(disposition) = operation.cancel_disposition.as_deref() {
                operation.cancel.store(true, Ordering::SeqCst);
                return Ok(match disposition {
                    "commit_in_progress" => "commit_in_progress",
                    "already_terminal" => "already_terminal",
                    _ => "accepted",
                });
            }
            operation.cancel.store(true, Ordering::SeqCst);
            let waited =
                self.cancel_changed
                    .wait_timeout_while(inner, Duration::from_secs(2), |inner| {
                        inner.active_login.as_ref().is_some_and(|operation| {
                            operation.snapshot.operation_id == operation_id
                                && operation.snapshot.state != "committing"
                                && operation.cancel_disposition.is_none()
                        })
                    });
            inner = match waited {
                Ok((inner, _)) => inner,
                Err(error) => error.into_inner().0,
            };
            if let Some(operation) = inner
                .active_login
                .as_ref()
                .filter(|operation| operation.snapshot.operation_id == operation_id)
            {
                if operation.snapshot.state == "committing"
                    || operation.cancel_disposition.as_deref() == Some("commit_in_progress")
                {
                    return Ok("commit_in_progress");
                }
                return Ok(match operation.cancel_disposition.as_deref() {
                    Some("already_terminal") => "already_terminal",
                    _ => "accepted",
                });
            }
            if inner.last_snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.operation_id == operation_id && snapshot.is_terminal()
            }) {
                return Ok("already_terminal");
            }
            return Ok("accepted");
        }
        if inner
            .last_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.operation_id == operation_id && snapshot.is_terminal())
        {
            return Ok("already_terminal");
        }
        Err("operation_not_found：Codex 登录 operation 不存在或已被替换。".into())
    }

    pub(crate) fn record_cancel_disposition(&self, operation_id: &str, disposition: &str) {
        if !matches!(
            disposition,
            "accepted" | "commit_in_progress" | "already_terminal"
        ) {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(operation) = inner
            .active_login
            .as_mut()
            .filter(|operation| operation.snapshot.operation_id == operation_id)
        {
            operation.cancel_disposition = Some(disposition.to_string());
            if disposition == "commit_in_progress" && operation.snapshot.state != "committing" {
                operation.snapshot.sequence = operation.snapshot.sequence.saturating_add(1);
                operation.snapshot.state = "committing".into();
                operation.snapshot.updated_at_ms = crate::config::now_ms();
                operation.snapshot.expires_at_ms = None;
                operation.snapshot.verification_url = None;
                operation.snapshot.user_code = None;
                inner.last_snapshot = Some(operation.snapshot.clone());
            }
        }
        drop(inner);
        self.cancel_changed.notify_all();
    }

    pub(crate) fn cancel_for_exit(&self) -> Option<u32> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.active_login.as_ref().and_then(|operation| {
            operation.cancel.store(true, Ordering::SeqCst);
            operation.pid
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_login_and_sequence_checked_snapshots_are_replayable() {
        let supervisor = CodexAuthSupervisor::default();
        let reservation = supervisor.begin_login("browser").unwrap();
        assert_eq!(reservation.snapshot.sequence, 1);
        assert!(supervisor.begin_login("device").is_err());
        let waiting = supervisor
            .update_progress(&reservation.operation_id, "waiting", None, None, None)
            .unwrap();
        assert_eq!(waiting.sequence, 2);
        let terminal = supervisor
            .finish(&reservation.operation_id, "succeeded", None)
            .unwrap();
        assert_eq!(terminal.sequence, 3);
        assert_eq!(supervisor.snapshot(), Some(terminal));
    }

    #[test]
    fn use_and_mutation_leases_close_check_then_start_races() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let use_lease = CodexAuthSupervisor::acquire_use(&supervisor).unwrap();
        assert!(supervisor.begin_login("device").is_err());
        assert!(CodexAuthSupervisor::begin_mutation(&supervisor).is_err());
        drop(use_lease);
        let mutation = CodexAuthSupervisor::begin_mutation(&supervisor).unwrap();
        assert!(CodexAuthSupervisor::acquire_use(&supervisor).is_err());
        drop(mutation);
        assert!(CodexAuthSupervisor::acquire_use(&supervisor).is_ok());
    }

    #[test]
    fn stale_cancel_cannot_cancel_new_operation_and_terminal_is_idempotent() {
        let supervisor = CodexAuthSupervisor::default();
        let first = supervisor.begin_login("browser").unwrap();
        supervisor
            .finish(&first.operation_id, "cancelled", None)
            .unwrap();
        assert_eq!(
            supervisor.cancel(&first.operation_id).unwrap(),
            "already_terminal"
        );
        let second = supervisor.begin_login("device").unwrap();
        assert!(supervisor.cancel(&first.operation_id).is_err());
        assert!(!second.cancel.load(Ordering::SeqCst));
        supervisor.record_cancel_disposition(&second.operation_id, "accepted");
        assert_eq!(supervisor.cancel(&second.operation_id).unwrap(), "accepted");
        assert!(second.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn device_code_is_replayed_while_waiting_and_cleared_before_commit() {
        let supervisor = CodexAuthSupervisor::default();
        let login = supervisor.begin_login("device").unwrap();
        let verification = supervisor
            .update_progress(
                &login.operation_id,
                "verification_required",
                Some(crate::config::now_ms() + 900_000),
                Some("https://auth.openai.com/codex/device".into()),
                Some("ABCD-1234".into()),
            )
            .unwrap();
        assert_eq!(verification.user_code.as_deref(), Some("ABCD-1234"));
        let waiting = supervisor
            .update_progress(&login.operation_id, "waiting", None, None, None)
            .unwrap();
        assert_eq!(waiting.user_code.as_deref(), Some("ABCD-1234"));
        let committing = supervisor
            .update_progress(&login.operation_id, "committing", None, None, None)
            .unwrap();
        assert!(committing.user_code.is_none());
        assert!(committing.verification_url.is_none());
    }
}
