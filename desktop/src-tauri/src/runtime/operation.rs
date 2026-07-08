use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::runtime::system::append_operation_log;

pub(crate) const POLL_INTERVAL_MS: u64 = 100;
pub(crate) const LOCAL_HEALTH_TIMEOUT_MS: u64 = 400;
pub(crate) const SCRATCH_READY_BUDGET_MS: u64 = 4_000;
pub(crate) const UPSTREAM_PROBE_TIMEOUT_MS: u64 = 20_000;
pub(crate) const PROXY_REUSE_HEALTH_TIMEOUT_MS: u64 = 500;
pub(crate) const PROXY_HEALTH_BUDGET_MS: u64 = 4_000;
pub(crate) const VERIFY_KEY_TIMEOUT_MS: u64 = 15_000;
pub(crate) const SANDBOX_HEALTH_BUDGET_MS: u64 = 8_000;
pub(crate) const STATUS_HEALTH_TIMEOUT_MS: u64 = 150;
pub(crate) const STATUS_UPSTREAM_TIMEOUT_MS: u64 = 250;

static NEXT_OP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(crate) enum OperationKind {
    ActivateProfile,
    UpdateActiveConnection,
    ValidateConnection,
    FetchModels,
    StartProxy,
    VerifyKey,
    OneClickLogin,
}

impl OperationKind {
    fn as_str(self) -> &'static str {
        match self {
            OperationKind::ActivateProfile => "activate_profile",
            OperationKind::UpdateActiveConnection => "update_active_connection",
            OperationKind::ValidateConnection => "validate_connection",
            OperationKind::FetchModels => "fetch_models",
            OperationKind::StartProxy => "start_proxy",
            OperationKind::VerifyKey => "verify_key",
            OperationKind::OneClickLogin => "one_click_login",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OperationStage {
    Precheck,
    ScratchSpawn,
    ScratchHealth,
    ScratchUpstreamProbe,
    UpstreamProbe,
    ProxySpawn,
    ProxyHealth,
    Commit,
    Rollback,
    SandboxLogin,
    SandboxLaunch,
    SandboxHealth,
    OpenBrowser,
    Finish,
}

impl OperationStage {
    fn as_str(self) -> &'static str {
        match self {
            OperationStage::Precheck => "precheck",
            OperationStage::ScratchSpawn => "scratch_spawn",
            OperationStage::ScratchHealth => "scratch_health",
            OperationStage::ScratchUpstreamProbe => "scratch_upstream_probe",
            OperationStage::UpstreamProbe => "upstream_probe",
            OperationStage::ProxySpawn => "proxy_spawn",
            OperationStage::ProxyHealth => "proxy_health",
            OperationStage::Commit => "commit",
            OperationStage::Rollback => "rollback",
            OperationStage::SandboxLogin => "sandbox_login",
            OperationStage::SandboxLaunch => "sandbox_launch",
            OperationStage::SandboxHealth => "sandbox_health",
            OperationStage::OpenBrowser => "open_browser",
            OperationStage::Finish => "finish",
        }
    }
}

pub(crate) struct OperationTrace {
    id: String,
    kind: OperationKind,
    started: Instant,
    last_stage_elapsed_ms: Cell<u128>,
    finished: Cell<bool>,
}

impl OperationTrace {
    pub(crate) fn start(kind: OperationKind, detail: impl AsRef<str>) -> Self {
        let seq = NEXT_OP_ID.fetch_add(1, Ordering::Relaxed);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let trace = OperationTrace {
            id: format!("{now_ms:x}-{seq:x}"),
            kind,
            started: Instant::now(),
            last_stage_elapsed_ms: Cell::new(0),
            finished: Cell::new(false),
        };
        trace.stage(OperationStage::Precheck, detail);
        trace
    }

    pub(crate) fn stage(&self, stage: OperationStage, detail: impl AsRef<str>) {
        let elapsed_ms = self.started.elapsed().as_millis();
        let stage_ms = stage_delta_ms(self.last_stage_elapsed_ms.replace(elapsed_ms), elapsed_ms);
        let line = format!(
            "op_id={} op={} stage={} elapsed_ms={} stage_ms={} detail={}",
            self.id,
            self.kind.as_str(),
            stage.as_str(),
            elapsed_ms,
            stage_ms,
            sanitize_detail(detail.as_ref())
        );
        append_operation_log(&line);
    }

    pub(crate) fn finish(&self, outcome: impl AsRef<str>) {
        if !self.finished.replace(true) {
            self.stage(OperationStage::Finish, outcome);
        }
    }
}

impl Drop for OperationTrace {
    fn drop(&mut self) {
        if !self.finished.get() {
            self.stage(OperationStage::Finish, "dropped_without_finish");
        }
    }
}

fn sanitize_detail(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

fn stage_delta_ms(previous_elapsed_ms: u128, elapsed_ms: u128) -> u128 {
    elapsed_ms.saturating_sub(previous_elapsed_ms)
}

#[cfg(test)]
mod tests {
    use super::{sanitize_detail, stage_delta_ms};

    #[test]
    fn sanitize_detail_keeps_log_one_line() {
        assert_eq!(sanitize_detail("a\nb\tc"), "a b c");
    }

    #[test]
    fn stage_delta_is_since_previous_stage() {
        assert_eq!(stage_delta_ms(10, 25), 15);
        assert_eq!(stage_delta_ms(25, 10), 0);
    }
}
