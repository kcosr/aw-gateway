use crate::config::parse_duration;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, sleep};

use super::service::service_stop_order;
use super::state::AgentState;

const SHUTDOWN_WATCHDOG_MARGIN: Duration = Duration::from_secs(5);
const DEFAULT_SERVICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn shutdown_agent(state: Arc<AgentState>) -> bool {
    if state.shutting_down.swap(true, Ordering::SeqCst) {
        tracing::debug!("container agent shutdown already in progress");
        wait_for_shutdown_complete(&state).await;
        return false;
    }
    tracing::info!("container agent shutdown starting");
    state.accepting_bridge.store(false, Ordering::SeqCst);
    stop_services(&state).await;
    tracing::info!("container agent shutdown completed");
    state.shutdown_complete.store(true, Ordering::SeqCst);
    state.shutdown_complete_notify.notify_waiters();
    true
}

pub(super) async fn shutdown_watchdog_delay(state: &AgentState, minimum: Duration) -> Duration {
    let services = state.services.lock().await.clone();
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
        .saturating_add(SHUTDOWN_WATCHDOG_MARGIN);
    std::cmp::max(minimum, service_budget)
}

pub(super) fn schedule_forced_exit_after(delay: Duration, reason: &'static str) {
    tokio::spawn(async move {
        sleep(delay).await;
        tracing::warn!(
            reason,
            delay_ms = delay.as_millis(),
            "forcing container agent exit after shutdown grace elapsed"
        );
        exit_pid1_agent_process_success();
    });
}

pub(super) fn exit_pid1_agent_process_success() -> ! {
    // The container agent is expected to own PID 1/service-control shutdown in
    // bootstrap mode, so these exits intentionally terminate the container.
    std::process::exit(0)
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
