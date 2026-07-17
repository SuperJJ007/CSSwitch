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
    pub(crate) error: Option<OperationErrorView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastAuthStatusSnapshot {
    pub(crate) status: String,
    pub(crate) reason: Option<String>,
    pub(crate) cause: Option<String>,
    pub(crate) checked_at_ms: i64,
}

impl OperationSnapshot {
    fn starting(operation_id: String) -> Self {
        let now = crate::config::now_ms();
        Self {
            schema_version: 2,
            operation_id,
            sequence: 1,
            method: "browser".into(),
            state: "starting".into(),
            started_at_ms: now,
            updated_at_ms: now,
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

struct PreflightOperation {
    id: u64,
    cancel: Arc<AtomicBool>,
    pid: Option<u32>,
}

#[derive(Default)]
struct SupervisorInner {
    codex_users: usize,
    other_mutation: bool,
    mutation_pid: Option<u32>,
    shutting_down: bool,
    active_login: Option<LoginOperation>,
    active_preflight: Option<PreflightOperation>,
    next_preflight_id: u64,
    last_snapshot: Option<OperationSnapshot>,
    last_auth_status: Option<LastAuthStatusSnapshot>,
}

#[derive(Default)]
pub(crate) struct CodexAuthSupervisor {
    inner: Mutex<SupervisorInner>,
    cancel_changed: Condvar,
    exit_cancel: Arc<AtomicBool>,
}

pub(crate) type SharedCodexAuthSupervisor = Arc<CodexAuthSupervisor>;

pub(crate) struct LoginReservation {
    pub(crate) operation_id: String,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) snapshot: OperationSnapshot,
}

pub(crate) struct CodexUseLease {
    supervisor: SharedCodexAuthSupervisor,
    exit_cancel: Arc<AtomicBool>,
    released: bool,
}

pub(crate) struct AuthPreflightReservation {
    supervisor: SharedCodexAuthSupervisor,
    id: u64,
    cancel: Arc<AtomicBool>,
    completed: bool,
}

pub(crate) struct CodexAuthReadyProof {
    _lease: CodexUseLease,
}

impl CodexAuthReadyProof {
    pub(crate) fn ensure_active(&self) -> Result<(), String> {
        if self._lease.exit_cancel.load(Ordering::SeqCst) {
            return Err("CODEX_AUTH_UNAVAILABLE：App 正在退出，Codex 认证 proof 已失效。".into());
        }
        let inner = self
            ._lease
            .supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down {
            Err("CODEX_AUTH_UNAVAILABLE：App 正在退出，Codex 认证 proof 已失效。".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn exit_cancel_flag(&self) -> &AtomicBool {
        self._lease.exit_cancel.as_ref()
    }
}

impl AuthPreflightReservation {
    pub(crate) fn cancel_flag(&self) -> &AtomicBool {
        self.cancel.as_ref()
    }

    pub(crate) fn set_pid(&self, pid: u32) -> Result<(), String> {
        self.supervisor.set_preflight_pid(self.id, Some(pid))
    }

    pub(crate) fn clear_pid(&self) {
        let _ = self.supervisor.set_preflight_pid(self.id, None);
    }

    pub(crate) fn promote_to_ready_proof(mut self) -> Result<CodexAuthReadyProof, String> {
        let mut inner = self
            .supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner
            .active_preflight
            .as_ref()
            .is_none_or(|operation| operation.id != self.id)
        {
            return Err("Codex 认证 preflight 已失效。".into());
        }
        if inner.shutting_down || self.cancel.load(Ordering::SeqCst) {
            return Err("Codex 正在退出，拒绝提升认证 preflight。".into());
        }
        inner.active_preflight = None;
        inner.codex_users = inner.codex_users.saturating_add(1);
        self.completed = true;
        let lease = CodexUseLease {
            supervisor: self.supervisor.clone(),
            exit_cancel: self.supervisor.exit_cancel.clone(),
            released: false,
        };
        drop(inner);
        self.supervisor.cancel_changed.notify_all();
        Ok(CodexAuthReadyProof { _lease: lease })
    }
}

impl Drop for AuthPreflightReservation {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.supervisor.release_preflight(self.id);
        self.completed = true;
    }
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
        drop(inner);
        self.supervisor.cancel_changed.notify_all();
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
        inner.mutation_pid = None;
        inner.other_mutation = false;
        self.released = true;
        drop(inner);
        self.supervisor.cancel_changed.notify_all();
    }
}

impl CodexMutationLease {
    pub(crate) fn set_pid(&self, pid: u32) -> Result<(), String> {
        let mut inner = self
            .supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down || !inner.other_mutation || inner.mutation_pid.is_some() {
            return Err("Codex 正在退出或认证变更已失效，拒绝登记 sidecar。".into());
        }
        inner.mutation_pid = Some(pid);
        Ok(())
    }

    pub(crate) fn clear_pid(&self) {
        let mut inner = self
            .supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inner.mutation_pid = None;
        drop(inner);
        self.supervisor.cancel_changed.notify_all();
    }
}

impl CodexAuthSupervisor {
    pub(crate) fn begin_login(&self) -> Result<LoginReservation, String> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down
            || inner.active_login.is_some()
            || inner.active_preflight.is_some()
            || inner.other_mutation
        {
            return Err("auth_busy：另一项 Codex 认证操作正在进行。".into());
        }
        if inner.codex_users != 0 {
            return Err("codex_busy：Codex 启动或模型探测正在进行，请稍后重试。".into());
        }
        let operation_id = crate::config::new_id();
        let snapshot = OperationSnapshot::starting(operation_id.clone());
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
            self.cancel_changed.notify_all();
        }
    }

    pub(crate) fn set_pid(&self, operation_id: &str, pid: u32) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down {
            return Err("Codex 正在退出，拒绝登记登录 sidecar。".into());
        }
        let operation = inner
            .active_login
            .as_mut()
            .filter(|operation| operation.snapshot.operation_id == operation_id)
            .ok_or_else(|| "Codex 登录 operation 已失效。".to_string())?;
        if operation.cancel.load(Ordering::SeqCst) {
            return Err("Codex 正在退出，拒绝登记登录 sidecar。".into());
        }
        operation.pid = Some(pid);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn acquire_use(
        supervisor: &SharedCodexAuthSupervisor,
    ) -> Result<CodexUseLease, String> {
        let mut inner = supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down
            || inner.active_login.is_some()
            || inner.active_preflight.is_some()
            || inner.other_mutation
        {
            return Err("auth_busy：Codex 正在登录或变更认证状态。".into());
        }
        inner.codex_users = inner.codex_users.saturating_add(1);
        Ok(CodexUseLease {
            supervisor: supervisor.clone(),
            exit_cancel: supervisor.exit_cancel.clone(),
            released: false,
        })
    }

    pub(crate) fn begin_auth_preflight(
        supervisor: &SharedCodexAuthSupervisor,
    ) -> Result<AuthPreflightReservation, String> {
        let mut inner = supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down
            || inner.active_login.is_some()
            || inner.active_preflight.is_some()
            || inner.other_mutation
            || inner.codex_users != 0
        {
            return Err("auth_busy：另一项 Codex 认证或启动操作正在进行。".into());
        }
        inner.next_preflight_id = inner.next_preflight_id.wrapping_add(1).max(1);
        let id = inner.next_preflight_id;
        let cancel = Arc::new(AtomicBool::new(false));
        inner.active_preflight = Some(PreflightOperation {
            id,
            cancel: cancel.clone(),
            pid: None,
        });
        Ok(AuthPreflightReservation {
            supervisor: supervisor.clone(),
            id,
            cancel,
            completed: false,
        })
    }

    fn set_preflight_pid(&self, id: u64, pid: Option<u32>) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if pid.is_some() && inner.shutting_down {
            return Err("Codex 正在退出，拒绝登记认证 preflight sidecar。".into());
        }
        let operation = inner
            .active_preflight
            .as_mut()
            .filter(|operation| operation.id == id)
            .ok_or_else(|| "Codex 认证 preflight 已失效。".to_string())?;
        if pid.is_some() && operation.cancel.load(Ordering::SeqCst) {
            return Err("Codex 正在退出，拒绝登记认证 preflight sidecar。".into());
        }
        operation.pid = pid;
        drop(inner);
        self.cancel_changed.notify_all();
        Ok(())
    }

    fn release_preflight(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner
            .active_preflight
            .as_ref()
            .is_some_and(|operation| operation.id == id)
        {
            inner.active_preflight = None;
        }
        drop(inner);
        self.cancel_changed.notify_all();
    }

    pub(crate) fn begin_mutation(
        supervisor: &SharedCodexAuthSupervisor,
    ) -> Result<CodexMutationLease, String> {
        let mut inner = supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.shutting_down
            || inner.active_login.is_some()
            || inner.active_preflight.is_some()
            || inner.other_mutation
        {
            return Err("auth_busy：另一项 Codex 认证操作正在进行。".into());
        }
        if inner.codex_users != 0 {
            return Err("codex_busy：Codex 启动或模型探测正在进行，请稍后重试。".into());
        }
        inner.other_mutation = true;
        inner.mutation_pid = None;
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

    pub(crate) fn record_auth_status(
        &self,
        status: &str,
        reason: Option<&str>,
        cause: Option<&str>,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.last_auth_status = Some(LastAuthStatusSnapshot {
            status: status.to_string(),
            reason: reason.map(str::to_string),
            cause: cause.map(str::to_string),
            checked_at_ms: crate::config::now_ms(),
        });
    }

    pub(crate) fn last_auth_status(&self) -> Option<LastAuthStatusSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_auth_status
            .clone()
    }

    pub(crate) fn update_progress(
        &self,
        operation_id: &str,
        state: &str,
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
                inner.last_snapshot = Some(operation.snapshot.clone());
            }
        }
        drop(inner);
        self.cancel_changed.notify_all();
    }

    pub(crate) fn cancel_for_exit(&self) -> Vec<u32> {
        self.exit_cancel.store(true, Ordering::SeqCst);
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.shutting_down = true;
        let mut pids = Vec::new();
        if let Some(operation) = inner.active_login.as_ref() {
            operation.cancel.store(true, Ordering::SeqCst);
            if let Some(pid) = operation.pid {
                pids.push(pid);
            }
        }
        if let Some(operation) = inner.active_preflight.as_ref() {
            operation.cancel.store(true, Ordering::SeqCst);
            if let Some(pid) = operation.pid {
                pids.push(pid);
            }
        }
        if let Some(pid) = inner.mutation_pid {
            pids.push(pid);
        }
        pids
    }

    pub(crate) fn wait_for_auth_children_exit(&self, timeout: Duration) -> Vec<u32> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let waited = self
            .cancel_changed
            .wait_timeout_while(inner, timeout, |inner| {
                inner.active_login.is_some()
                    || inner.active_preflight.is_some()
                    || inner.other_mutation
                    || inner.codex_users != 0
            });
        let inner = match waited {
            Ok((inner, _)) => inner,
            Err(error) => error.into_inner().0,
        };
        inner
            .active_login
            .as_ref()
            .and_then(|operation| operation.pid)
            .into_iter()
            .chain(
                inner
                    .active_preflight
                    .as_ref()
                    .and_then(|operation| operation.pid),
            )
            .chain(inner.mutation_pid)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_login_and_sequence_checked_snapshots_are_replayable() {
        let supervisor = CodexAuthSupervisor::default();
        let reservation = supervisor.begin_login().unwrap();
        assert_eq!(reservation.snapshot.sequence, 1);
        assert_eq!(reservation.snapshot.method, "browser");
        assert!(supervisor.begin_login().is_err());
        let waiting = supervisor
            .update_progress(&reservation.operation_id, "waiting")
            .unwrap();
        assert_eq!(waiting.sequence, 2);
        let terminal = supervisor
            .finish(&reservation.operation_id, "succeeded", None)
            .unwrap();
        assert_eq!(terminal.sequence, 3);
        assert_eq!(supervisor.snapshot(), Some(terminal));
        let next = supervisor.begin_login().unwrap();
        supervisor
            .finish(&next.operation_id, "cancelled", None)
            .unwrap();
        let preflight = CodexAuthSupervisor::begin_auth_preflight(&Arc::new(supervisor)).unwrap();
        drop(preflight);
    }

    #[test]
    fn use_and_mutation_leases_close_check_then_start_races() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let use_lease = CodexAuthSupervisor::acquire_use(&supervisor).unwrap();
        assert!(supervisor.begin_login().is_err());
        assert!(CodexAuthSupervisor::begin_mutation(&supervisor).is_err());
        drop(use_lease);
        let mutation = CodexAuthSupervisor::begin_mutation(&supervisor).unwrap();
        assert!(CodexAuthSupervisor::acquire_use(&supervisor).is_err());
        drop(mutation);
        assert!(CodexAuthSupervisor::acquire_use(&supervisor).is_ok());
    }

    #[test]
    fn preflight_is_exclusive_and_ready_proof_holds_the_use_lease() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor).unwrap();
        assert!(CodexAuthSupervisor::begin_auth_preflight(&supervisor).is_err());
        assert!(supervisor.begin_login().is_err());
        assert!(CodexAuthSupervisor::begin_mutation(&supervisor).is_err());
        assert!(CodexAuthSupervisor::acquire_use(&supervisor).is_err());

        let proof = reservation.promote_to_ready_proof().unwrap();
        assert!(CodexAuthSupervisor::begin_auth_preflight(&supervisor).is_err());
        assert!(supervisor.begin_login().is_err());
        drop(proof);
        assert!(CodexAuthSupervisor::begin_auth_preflight(&supervisor).is_ok());
    }

    #[test]
    fn ready_proof_is_invalidated_when_exit_starts() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor).unwrap();
        let proof = reservation.promote_to_ready_proof().unwrap();

        assert!(supervisor.cancel_for_exit().is_empty());
        assert!(proof.ensure_active().is_err());
        drop(proof);
        assert!(supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn exit_tracks_and_waits_for_mutation_sidecar() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let mutation = CodexAuthSupervisor::begin_mutation(&supervisor).unwrap();
        mutation.set_pid(4747).unwrap();

        assert_eq!(supervisor.cancel_for_exit(), vec![4747]);
        mutation.clear_pid();
        drop(mutation);
        assert!(supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn exit_cancellation_covers_interactive_preflight_pid() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor).unwrap();
        reservation.set_pid(4242).unwrap();
        assert_eq!(supervisor.cancel_for_exit(), vec![4242]);
        assert!(reservation.cancel_flag().load(Ordering::SeqCst));
        reservation.clear_pid();
        drop(reservation);
        assert!(supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn exit_cancellation_covers_login_pid_without_finishing_it_early() {
        let supervisor = CodexAuthSupervisor::default();
        let reservation = supervisor.begin_login().unwrap();
        supervisor.set_pid(&reservation.operation_id, 4343).unwrap();
        assert_eq!(supervisor.cancel_for_exit(), vec![4343]);
        assert!(reservation.cancel.load(Ordering::SeqCst));
        assert_eq!(
            supervisor.snapshot().unwrap().state,
            "starting",
            "exit cancellation must let the login waiter decide its terminal state"
        );
        supervisor
            .finish(&reservation.operation_id, "cancelled", None)
            .unwrap();
        assert!(supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn exit_before_pid_registration_closes_the_spawn_race() {
        let preflight_supervisor = Arc::new(CodexAuthSupervisor::default());
        let preflight = CodexAuthSupervisor::begin_auth_preflight(&preflight_supervisor).unwrap();
        assert!(preflight_supervisor.cancel_for_exit().is_empty());
        assert!(preflight.cancel_flag().load(Ordering::SeqCst));
        assert!(preflight.set_pid(4444).is_err());
        drop(preflight);
        assert!(preflight_supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
        assert!(CodexAuthSupervisor::begin_auth_preflight(&preflight_supervisor).is_err());

        let login_supervisor = CodexAuthSupervisor::default();
        let login = login_supervisor.begin_login().unwrap();
        assert!(login_supervisor.cancel_for_exit().is_empty());
        assert!(login.cancel.load(Ordering::SeqCst));
        assert!(login_supervisor.set_pid(&login.operation_id, 4545).is_err());
        login_supervisor
            .finish(&login.operation_id, "cancelled", None)
            .unwrap();
        assert!(login_supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());
        assert!(login_supervisor.begin_login().is_err());
    }

    #[test]
    fn exit_after_preflight_child_exit_rejects_ready_proof_promotion() {
        let supervisor = Arc::new(CodexAuthSupervisor::default());
        let reservation = CodexAuthSupervisor::begin_auth_preflight(&supervisor).unwrap();
        reservation.set_pid(4646).unwrap();
        reservation.clear_pid();

        assert!(supervisor.cancel_for_exit().is_empty());
        assert!(reservation.cancel_flag().load(Ordering::SeqCst));
        assert!(reservation.promote_to_ready_proof().is_err());
        assert!(supervisor
            .wait_for_auth_children_exit(Duration::from_millis(1))
            .is_empty());

        let inner = supervisor
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(inner.codex_users, 0);
        assert!(inner.active_preflight.is_none());
    }

    #[test]
    fn stale_cancel_cannot_cancel_new_operation_and_terminal_is_idempotent() {
        let supervisor = CodexAuthSupervisor::default();
        let first = supervisor.begin_login().unwrap();
        supervisor
            .finish(&first.operation_id, "cancelled", None)
            .unwrap();
        assert_eq!(
            supervisor.cancel(&first.operation_id).unwrap(),
            "already_terminal"
        );
        let second = supervisor.begin_login().unwrap();
        assert!(supervisor.cancel(&first.operation_id).is_err());
        assert!(!second.cancel.load(Ordering::SeqCst));
        supervisor.record_cancel_disposition(&second.operation_id, "accepted");
        assert_eq!(supervisor.cancel(&second.operation_id).unwrap(), "accepted");
        assert!(second.cancel.load(Ordering::SeqCst));
    }
}
