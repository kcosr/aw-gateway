use crate::agent_control::{IdleStateName, ProcessMatch, ReapResult};
use crate::config::{IdleCleanupAction, IdleCleanupConfig, IdleCleanupOwner};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use super::relay::{RelayControl, RelayFatalKind};
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
    pub(super) access_flow_relay: Option<Arc<RelayControl>>,
    pub(super) exit_arbitration: StdMutex<Option<RelayFatalKind>>,
    pub(super) relay_fatal_notify: Notify,
    pub(super) accepting_bridge: AtomicBool,
    pub(super) shutting_down: AtomicBool,
    pub(super) shutdown_complete: AtomicBool,
    pub(super) shutdown_complete_notify: Notify,
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
        access_flow_relay: Option<Arc<RelayControl>>,
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
            access_flow_relay,
            exit_arbitration: StdMutex::new(None),
            relay_fatal_notify: Notify::new(),
            accepting_bridge: AtomicBool::new(true),
            shutting_down: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            shutdown_complete_notify: Notify::new(),
            control_token,
            socket_owner,
        }
    }

    pub(super) fn publish_relay_fatal(&self, fatal: RelayFatalKind) {
        let mut current = self
            .exit_arbitration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_none() {
            *current = Some(fatal);
            self.relay_fatal_notify.notify_waiters();
        }
    }

    pub(super) fn relay_fatal(&self) -> Option<RelayFatalKind> {
        *self
            .exit_arbitration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SocketOwner {
    pub(super) uid: u32,
    pub(super) gid: u32,
}

impl SocketOwner {
    pub(super) fn from_env() -> anyhow::Result<Option<Self>> {
        let uid = optional_env("AW_AUTHENTICATED_UID")?;
        let gid = optional_env("AW_AUTHENTICATED_GID")?;
        Self::from_values(uid, gid)
    }

    fn from_values(uid: Option<String>, gid: Option<String>) -> anyhow::Result<Option<Self>> {
        match (uid, gid) {
            (None, None) => Ok(None),
            (Some(uid), Some(gid)) => Ok(Some(Self {
                uid: uid
                    .parse()
                    .map_err(|_| anyhow::anyhow!("AW_AUTHENTICATED_UID must be numeric"))?,
                gid: gid
                    .parse()
                    .map_err(|_| anyhow::anyhow!("AW_AUTHENTICATED_GID must be numeric"))?,
            })),
            _ => {
                anyhow::bail!("AW_AUTHENTICATED_UID and AW_AUTHENTICATED_GID must be set together")
            }
        }
    }
}

fn optional_env(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{name} must be valid Unicode")
        }
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

#[cfg(test)]
mod tests {
    use super::SocketOwner;

    #[test]
    fn socket_owner_absent_when_neither_uid_nor_gid_set() {
        assert!(SocketOwner::from_values(None, None).unwrap().is_none());
    }

    #[test]
    fn socket_owner_parses_numeric_uid_and_gid() {
        let owner = SocketOwner::from_values(Some("1000".into()), Some("1001".into()))
            .unwrap()
            .expect("owner present");
        assert_eq!(owner.uid, 1000);
        assert_eq!(owner.gid, 1001);
    }

    #[test]
    fn socket_owner_rejects_malformed_uid() {
        let err = SocketOwner::from_values(Some("root".into()), Some("0".into())).unwrap_err();
        assert!(
            err.to_string()
                .contains("AW_AUTHENTICATED_UID must be numeric")
        );
    }

    #[test]
    fn socket_owner_rejects_malformed_gid() {
        let err = SocketOwner::from_values(Some("0".into()), Some("wheel".into())).unwrap_err();
        assert!(
            err.to_string()
                .contains("AW_AUTHENTICATED_GID must be numeric")
        );
    }

    #[test]
    fn socket_owner_rejects_uid_without_gid() {
        let err = SocketOwner::from_values(Some("1000".into()), None).unwrap_err();
        assert!(err.to_string().contains("must be set together"));
    }
}
