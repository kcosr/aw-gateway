use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(super) enum SshTarget {
    Unix(PathBuf),
    Tcp(TcpEndpoint),
}

#[derive(Debug, Serialize)]
pub(super) struct ReadyStatus {
    pub(super) target: String,
    pub(super) session_id: Option<String>,
    pub(super) mode: String,
    pub(super) user: String,
    pub(super) image: String,
    pub(super) container: String,
    pub(super) container_pid: i64,
    pub(super) ssh_socket: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ssh_tcp: Option<TcpEndpoint>,
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) local_ssh: Option<LocalSshReady>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) client_config: Option<PathBuf>,
}

impl ReadyStatus {
    pub(super) fn ssh_target(&self) -> SshTarget {
        self.ssh_tcp
            .clone()
            .map(SshTarget::Tcp)
            .unwrap_or_else(|| SshTarget::Unix(self.ssh_socket.clone()))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct LocalSshReady {
    pub(super) host: String,
    pub(super) port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TcpEndpoint {
    pub(super) host: String,
    pub(super) port: u16,
}

#[derive(Debug, Serialize)]
pub(super) struct GatewayStatus {
    pub(super) target: String,
    pub(super) session_id: Option<String>,
    pub(super) launch: Option<String>,
    pub(super) mode: String,
    pub(super) user: String,
    pub(super) image: String,
    pub(super) container: Option<String>,
    pub(super) container_pid: Option<i64>,
    pub(super) active_sessions: usize,
    pub(super) sessions: Vec<SessionStatus>,
    pub(super) agent_ready: bool,
    pub(super) ssh_socket: PathBuf,
    pub(super) status: String,
    pub(super) agent: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct SessionMarker {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) gateway_pid: u32,
    pub(super) gateway_start_time: String,
    pub(super) container: String,
    pub(super) target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) launch: Option<String>,
    pub(super) created_at_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionStatus {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) gateway_pid: u32,
    pub(super) container: String,
    pub(super) target: String,
    pub(super) launch: Option<String>,
    pub(super) created_at_ms: u128,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct LocalListenerStatus {
    pub(super) gateway_pid: u32,
    pub(super) gateway_start_time: String,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) created_at_ms: u128,
}

impl From<SessionMarker> for SessionStatus {
    fn from(marker: SessionMarker) -> Self {
        Self {
            id: marker.id,
            kind: marker.kind,
            gateway_pid: marker.gateway_pid,
            container: marker.container,
            target: marker.target,
            launch: marker.launch,
            created_at_ms: marker.created_at_ms,
        }
    }
}

pub(super) fn gateway_status_name(
    container_running: bool,
    agent_expected: bool,
    agent_seen: bool,
    agent_ready: bool,
) -> &'static str {
    if !container_running {
        "not-running"
    } else if !agent_expected {
        "container-running"
    } else if agent_ready {
        "ready"
    } else if agent_seen {
        "container-running-agent-not-ready"
    } else {
        "container-running-agent-unavailable"
    }
}
