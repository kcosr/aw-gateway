use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::service::service_stop_order;
use super::state::AgentState;

pub(super) async fn shutdown_agent(state: Arc<AgentState>) {
    state.shutting_down.store(true, Ordering::SeqCst);
    state.accepting_bridge.store(false, Ordering::SeqCst);
    stop_services(&state).await;
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
