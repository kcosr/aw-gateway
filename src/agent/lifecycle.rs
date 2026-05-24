use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::service::service_stop_order;
use super::state::AgentState;

pub(super) async fn shutdown_agent(state: Arc<AgentState>) {
    state.shutting_down.store(true, Ordering::SeqCst);
    state.accepting_bridge.store(false, Ordering::SeqCst);
    stop_services(&state).await;
}

async fn stop_services(state: &AgentState) {
    let services = state.services.lock().await.clone();
    for service in service_stop_order(&services) {
        service.stop().await;
    }
}
