use crate::agent_control::{IdleStateName, ProcessMatch, ReapResult};
use crate::config::{IdleCleanupAction, IdleCleanupConfig, IdleCleanupOwner};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::service::ManagedService;

#[derive(Debug)]
pub(super) struct AgentState {
    pub(super) state_dir: PathBuf,
    pub(super) services: Mutex<Vec<Arc<ManagedService>>>,
    pub(super) idle_cleanup: Option<IdleCleanupConfig>,
    pub(super) idle_state: Mutex<IdleRuntimeState>,
    pub(super) bridge_enabled: bool,
    pub(super) bridge_ready: AtomicBool,
    pub(super) active_streams: AtomicUsize,
    pub(super) active_sessions: AtomicUsize,
    pub(super) accepting_bridge: AtomicBool,
    pub(super) shutting_down: AtomicBool,
    pub(super) control_token: Option<String>,
    pub(super) socket_owner: Option<SocketOwner>,
}

impl AgentState {
    pub(super) fn new(
        state_dir: PathBuf,
        idle_cleanup: Option<IdleCleanupConfig>,
        bridge_enabled: bool,
        control_token: Option<String>,
        socket_owner: Option<SocketOwner>,
    ) -> Self {
        let idle_cleanup = idle_cleanup.filter(|config| {
            config.owner == IdleCleanupOwner::Agent && config.action != IdleCleanupAction::None
        });
        Self {
            state_dir,
            services: Mutex::new(Vec::new()),
            idle_cleanup,
            idle_state: Mutex::new(IdleRuntimeState::default()),
            bridge_enabled,
            bridge_ready: AtomicBool::new(!bridge_enabled),
            active_streams: AtomicUsize::new(0),
            active_sessions: AtomicUsize::new(0),
            accepting_bridge: AtomicBool::new(true),
            shutting_down: AtomicBool::new(false),
            control_token,
            socket_owner,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SocketOwner {
    pub(super) uid: u32,
    pub(super) gid: u32,
}

impl SocketOwner {
    pub(super) fn from_env() -> Option<Self> {
        let uid = std::env::var("AW_AUTHENTICATED_UID").ok()?.parse().ok()?;
        let gid = std::env::var("AW_AUTHENTICATED_GID").ok()?.parse().ok()?;
        Some(Self { uid, gid })
    }
}

#[derive(Debug, Default)]
pub(super) struct IdleRuntimeState {
    pub(super) state: IdleStateName,
    pub(super) idle_since: Option<Instant>,
    pub(super) preserve: bool,
    pub(super) preserve_reason: Option<String>,
    pub(super) matched_processes: Vec<ProcessMatch>,
    pub(super) last_reap_result: Option<ReapResult>,
}
