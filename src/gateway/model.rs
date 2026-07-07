use crate::agent_control::AgentStatus;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(super) enum SshTarget {
    Unix(PathBuf),
    Tcp(TcpEndpoint),
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ReadyStatus {
    pub(super) target: String,
    pub(super) session_id: Option<String>,
    pub(super) mode: String,
    pub(super) user: String,
    pub(super) image: String,
    pub(super) container: String,
    pub(super) access: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub(super) context: BTreeMap<String, String>,
    pub(super) container_pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ssh_socket: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ssh_tcp: Option<TcpEndpoint>,
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) local_ssh: Option<LocalSshReady>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) client_config: Option<PathBuf>,
}

impl ReadyStatus {
    pub(super) fn ssh_target(&self) -> anyhow::Result<SshTarget> {
        if let Some(endpoint) = self.ssh_tcp.clone() {
            return Ok(SshTarget::Tcp(endpoint));
        }
        if let Some(socket) = self.ssh_socket.clone() {
            return Ok(SshTarget::Unix(socket));
        }
        anyhow::bail!(
            "target {:?} uses access.method = {:?} and does not expose an SSH endpoint",
            self.target,
            self.access
        )
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct LocalSshReady {
    pub(super) host: String,
    pub(super) port: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
    pub(super) access: String,
    pub(super) container: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub(super) context: BTreeMap<String, String>,
    pub(super) container_pid: Option<i64>,
    pub(super) active_sessions: usize,
    pub(super) sessions: Vec<SessionStatus>,
    pub(super) agent_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ssh_socket: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ssh_tcp: Option<TcpEndpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) local_ssh: Option<LocalSshReady>,
    pub(super) status: String,
    pub(super) agent: Option<Box<AgentStatus>>,
}

fn deserialize_required_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Serialize)]
pub(super) struct TargetEntry {
    pub(super) target: String,
    pub(super) image: String,
    pub(super) access: String,
    pub(super) mode: String,
    pub(super) container: String,
    pub(super) default: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct LaunchSummary {
    pub(super) name: String,
    pub(super) target: String,
    pub(super) allow_args: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    pub(super) vars: BTreeMap<String, LaunchVarMetadata>,
}

#[derive(Debug, Serialize)]
pub(super) struct LaunchDetail {
    pub(super) name: String,
    pub(super) target: String,
    pub(super) target_mode: String,
    pub(super) allow_args: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) target_container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    pub(super) vars: BTreeMap<String, LaunchVarMetadata>,
    pub(super) steps: Vec<LaunchStepDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) env: BTreeMap<String, String>,
    pub(super) command: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct LaunchStepDetail {
    pub(super) name: String,
    pub(super) phase: String,
    pub(super) location: String,
    pub(super) required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) env: BTreeMap<String, String>,
    pub(super) command: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct LaunchVarMetadata {
    #[serde(rename = "type")]
    pub(super) var_type: &'static str,
    pub(super) required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) default: Option<crate::config::LaunchVarValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct AllStatusEntry {
    pub(super) target: String,
    pub(super) session_id: Option<String>,
    pub(super) launch: Option<String>,
    pub(super) mode: String,
    pub(super) user: String,
    pub(super) uid: String,
    pub(super) image: String,
    pub(super) container: String,
    pub(super) access: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub(super) context: BTreeMap<String, String>,
    pub(super) status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct SessionMarker {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) gateway_pid: u32,
    pub(super) gateway_start_time: String,
    pub(super) container: String,
    pub(super) target: String,
    #[serde(default, deserialize_with = "deserialize_required_optional_string")]
    pub(super) launch: Option<String>,
    #[serde(default)]
    pub(super) context: BTreeMap<String, String>,
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub(super) context: BTreeMap<String, String>,
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
            context: marker.context,
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
