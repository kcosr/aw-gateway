use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, sleep};

use super::service::service_stop_order;
use super::state::AgentState;

pub(super) async fn shutdown_agent(state: Arc<AgentState>) -> bool {
    if state.shutting_down.swap(true, Ordering::SeqCst) {
        tracing::debug!("container agent shutdown already in progress");
        return false;
    }
    tracing::info!("container agent shutdown starting");
    state.accepting_bridge.store(false, Ordering::SeqCst);
    stop_services(&state).await;
    tracing::info!("container agent shutdown completed");
    true
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

async fn stop_services(state: &AgentState) {
    let services = state.services.lock().await.clone();
    for service in service_stop_order(&services) {
        service.stop().await;
    }
}
