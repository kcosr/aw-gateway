use crate::VERSION;
use crate::agent_control::{AgentStatus, BridgeStatus, IdleCleanupStatus};
use crate::config::IdleCleanupAction;
use std::sync::atomic::Ordering;

use super::state::AgentState;

pub(super) async fn status_payload(state: &AgentState) -> AgentStatus {
    let services = state.services.lock().await.clone();
    let mut service_status = Vec::new();
    let mut ready = true;
    for service in services {
        let status = service.status().await;
        if service.config.required && !status.healthy {
            ready = false;
        }
        service_status.push(status);
    }
    if state.shutting_down.load(Ordering::SeqCst) {
        ready = false;
    }
    if state.bridge_enabled
        && (!state.bridge_ready.load(Ordering::SeqCst)
            || !state.accepting_bridge.load(Ordering::SeqCst))
    {
        ready = false;
    }
    AgentStatus {
        ready,
        version: VERSION.to_string(),
        services: service_status,
        ssh_bridge: BridgeStatus {
            enabled: state.bridge_enabled,
            ready: !state.shutting_down.load(Ordering::SeqCst)
                && state.accepting_bridge.load(Ordering::SeqCst)
                && state.bridge_ready.load(Ordering::SeqCst),
            active_streams: state.active_streams.load(Ordering::SeqCst),
            active_sessions: state.active_sessions.load(Ordering::SeqCst),
        },
        idle_cleanup: idle_cleanup_status(state).await,
        shutting_down: state.shutting_down.load(Ordering::SeqCst),
    }
}

async fn idle_cleanup_status(state: &AgentState) -> Option<IdleCleanupStatus> {
    let config = state.idle_cleanup.as_ref()?;
    let idle = state.idle_state.lock().await;
    Some(IdleCleanupStatus {
        owner: "agent".to_string(),
        action: idle_action_name(config.action).to_string(),
        state: idle.state,
        idle_for_ms: idle.idle_since.map(|since| since.elapsed().as_millis()),
        preserve: idle.preserve,
        preserve_reason: idle.preserve_reason.clone(),
        matched_processes: idle.matched_processes.clone(),
        last_reap_result: idle.last_reap_result.clone(),
    })
}

fn idle_action_name(action: IdleCleanupAction) -> &'static str {
    match action {
        IdleCleanupAction::None => "none",
        IdleCleanupAction::ExitContainer => "exit_container",
        IdleCleanupAction::ReapProcesses => "reap_processes",
    }
}
