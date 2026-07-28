use crate::config::parse_duration;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, sleep};

use super::service::{relay_dependent_service_stop_order, service_stop_order};
use super::state::AgentState;

const SHUTDOWN_WATCHDOG_MARGIN: Duration = Duration::from_secs(5);
const DEFAULT_SERVICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ForcedExitStatus {
    Success,
    Fatal,
}

impl ForcedExitStatus {
    pub(super) const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Fatal => 1,
        }
    }
}

const fn effective_forced_exit_status(
    scheduled: ForcedExitStatus,
    fatal_exit_requested: bool,
) -> ForcedExitStatus {
    if fatal_exit_requested {
        ForcedExitStatus::Fatal
    } else {
        scheduled
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownPhase {
    CloseRelayAdmission,
    StopRelayDependents,
    DrainRelay,
    StopRemainingServices,
}

fn shutdown_phases(has_relay: bool) -> &'static [ShutdownPhase] {
    const WITH_RELAY: &[ShutdownPhase] = &[
        ShutdownPhase::CloseRelayAdmission,
        ShutdownPhase::StopRelayDependents,
        ShutdownPhase::DrainRelay,
        ShutdownPhase::StopRemainingServices,
    ];
    const WITHOUT_RELAY: &[ShutdownPhase] = &[ShutdownPhase::StopRemainingServices];
    if has_relay { WITH_RELAY } else { WITHOUT_RELAY }
}

pub(super) async fn shutdown_agent(state: Arc<AgentState>) -> bool {
    let started = !state.shutting_down.swap(true, Ordering::SeqCst);
    if started {
        let shutdown_state = state.clone();
        tokio::spawn(async move {
            perform_shutdown(shutdown_state).await;
        });
    } else {
        tracing::debug!("container agent shutdown already in progress");
    }
    wait_for_shutdown_complete(&state).await;
    started
}

async fn perform_shutdown(state: Arc<AgentState>) {
    tracing::info!("container agent shutdown starting");
    state.accepting_bridge.store(false, Ordering::SeqCst);
    let relay_close_deadline = state.access_flow_relay.as_ref().map(|relay| {
        std::time::Instant::now()
            .checked_add(relay.drain_timeout())
            .unwrap_or_else(std::time::Instant::now)
    });
    for phase in shutdown_phases(state.access_flow_relay.is_some()) {
        match phase {
            ShutdownPhase::CloseRelayAdmission => {
                state
                    .access_flow_relay
                    .as_ref()
                    .expect("relay shutdown phase requires configured relay")
                    .close_admission_by(
                        relay_close_deadline.expect("relay shutdown deadline must be available"),
                    )
                    .await;
            }
            ShutdownPhase::StopRelayDependents => stop_relay_dependents(&state).await,
            ShutdownPhase::DrainRelay => {
                let relay = state
                    .access_flow_relay
                    .as_ref()
                    .expect("relay shutdown phase requires configured relay");
                relay
                    .shutdown(
                        std::time::Instant::now()
                            .checked_add(relay.drain_timeout())
                            .unwrap_or_else(std::time::Instant::now),
                    )
                    .await;
            }
            ShutdownPhase::StopRemainingServices => stop_services(&state).await,
        }
    }
    tracing::info!("container agent shutdown completed");
    state.shutdown_complete.store(true, Ordering::SeqCst);
    state.shutdown_complete_notify.notify_waiters();
}

pub(super) async fn shutdown_watchdog_delay(state: &AgentState, minimum: Duration) -> Duration {
    let services = state.services.lock().await.clone();
    let relay_budget = state
        .access_flow_relay
        .as_ref()
        .map_or(Duration::ZERO, |relay| relay.drain_timeout());
    let service_budget = service_stop_order(&services)
        .iter()
        .map(|service| {
            service
                .config
                .shutdown_timeout
                .as_deref()
                .and_then(|value| parse_duration(value).ok())
                .unwrap_or(DEFAULT_SERVICE_SHUTDOWN_TIMEOUT)
        })
        .fold(Duration::ZERO, |total, timeout| {
            total.saturating_add(timeout)
        })
        .saturating_add(relay_budget)
        .saturating_add(SHUTDOWN_WATCHDOG_MARGIN);
    std::cmp::max(minimum, service_budget)
}

pub(super) fn schedule_forced_exit_after(
    state: Arc<AgentState>,
    delay: Duration,
    reason: &'static str,
    status: ForcedExitStatus,
) {
    tokio::spawn(async move {
        sleep(delay).await;
        exit_pid1_agent_process(&state, status, Some(ForcedExitLog { reason, delay }));
    });
}

pub(super) fn exit_pid1_agent_process_for_state(
    state: &AgentState,
    scheduled: ForcedExitStatus,
) -> ! {
    exit_pid1_agent_process(state, scheduled, None)
}

struct ForcedExitLog {
    reason: &'static str,
    delay: Duration,
}

fn exit_pid1_agent_process(
    state: &AgentState,
    scheduled: ForcedExitStatus,
    log: Option<ForcedExitLog>,
) -> ! {
    let (arbitration, poisoned) = match state.exit_arbitration.lock() {
        Ok(arbitration) => (arbitration, false),
        Err(poisoned) => (poisoned.into_inner(), true),
    };
    let status = effective_forced_exit_status(scheduled, poisoned || arbitration.is_some());
    if let Some(log) = log {
        tracing::warn!(
            reason = log.reason,
            delay_ms = log.delay.as_millis(),
            exit_code = status.code(),
            "forcing container agent exit after shutdown grace elapsed"
        );
    }
    // The container agent is expected to own PID 1/service-control shutdown in
    // bootstrap mode. Keep arbitration locked through exit so fatal publication
    // and final status selection have one linearization point.
    std::process::exit(status.code())
}

async fn wait_for_shutdown_complete(state: &AgentState) {
    loop {
        let notified = state.shutdown_complete_notify.notified();
        if state.shutdown_complete.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

async fn stop_services(state: &AgentState) {
    let services = state.services.lock().await.clone();
    for service in service_stop_order(&services) {
        service.stop().await;
    }
}

async fn stop_relay_dependents(state: &AgentState) {
    let services = state.services.lock().await.clone();
    for service in relay_dependent_service_stop_order(&services) {
        service.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::{ForcedExitStatus, ShutdownPhase, effective_forced_exit_status, shutdown_phases};
    use crate::agent::relay::RelayFatalKind;
    use crate::agent::service::ManagedService;
    use crate::agent::state::AgentState;
    use crate::config::{LoggingConfig, RestartPolicy, ServiceConfig};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[test]
    fn fatal_and_normal_relay_shutdown_use_the_exact_safe_order() {
        assert_eq!(
            shutdown_phases(true),
            [
                ShutdownPhase::CloseRelayAdmission,
                ShutdownPhase::StopRelayDependents,
                ShutdownPhase::DrainRelay,
                ShutdownPhase::StopRemainingServices,
            ]
        );
        assert_eq!(
            shutdown_phases(false),
            [ShutdownPhase::StopRemainingServices]
        );
    }

    #[test]
    fn relay_fatal_upgrades_an_already_scheduled_success_watchdog() {
        assert_eq!(
            effective_forced_exit_status(ForcedExitStatus::Success, false),
            ForcedExitStatus::Success
        );
        assert_eq!(
            effective_forced_exit_status(ForcedExitStatus::Success, true),
            ForcedExitStatus::Fatal
        );
        assert_eq!(
            effective_forced_exit_status(ForcedExitStatus::Fatal, false),
            ForcedExitStatus::Fatal
        );
    }

    fn test_state() -> Arc<AgentState> {
        Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            None,
            None,
            None,
        ))
    }

    #[tokio::test]
    async fn shutdown_continues_when_the_first_waiter_is_cancelled() {
        let state = test_state();
        let service = Arc::new(ManagedService::new(
            ServiceConfig {
                name: "blocked-stop".into(),
                required: true,
                user: "root".into(),
                command: vec!["sleep".into(), "infinity".into()],
                cwd: None,
                restart: RestartPolicy::Never,
                restart_backoff: None,
                restart_backoff_max: None,
                startup_timeout: None,
                shutdown_timeout: Some("50ms".into()),
                depends_on: Vec::new(),
                env: BTreeMap::new(),
                health_check: None,
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        *state.services.lock().await = vec![service.clone()];
        let child_guard = service.child.lock().await;

        let first_state = state.clone();
        let first_waiter = tokio::spawn(async move { super::shutdown_agent(first_state).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !service.stopping.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        first_waiter.abort();
        assert!(first_waiter.await.unwrap_err().is_cancelled());

        drop(child_guard);
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), super::shutdown_agent(state.clone()))
                .await
                .expect("detached shutdown coordinator did not complete")
        );
        assert!(state.shutdown_complete.load(Ordering::SeqCst));
        assert!(service.stopping.load(Ordering::SeqCst));
    }

    #[test]
    fn fatal_publication_before_finalization_selects_failure() {
        let state = test_state();
        state.publish_relay_fatal(RelayFatalKind::RuntimeFailure);
        let arbitration = state.exit_arbitration.lock().unwrap();
        assert_eq!(
            effective_forced_exit_status(ForcedExitStatus::Success, arbitration.is_some(),),
            ForcedExitStatus::Fatal
        );
    }

    #[test]
    fn finalizer_gate_blocks_later_fatal_publication() {
        let state = test_state();
        let arbitration = state.exit_arbitration.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let fatal_state = state.clone();
        let publisher = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            fatal_state.publish_relay_fatal(RelayFatalKind::RuntimeFailure);
            completed_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            completed_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "fatal publication must block behind the finalizer gate"
        );
        assert_eq!(
            effective_forced_exit_status(ForcedExitStatus::Success, arbitration.is_some(),),
            ForcedExitStatus::Success
        );
        drop(arbitration);
        completed_rx.recv().unwrap();
        publisher.join().unwrap();
        assert_eq!(state.relay_fatal(), Some(RelayFatalKind::RuntimeFailure));
    }
}
