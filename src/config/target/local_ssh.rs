use crate::config::validation::default_listen_host;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSshConfig {
    #[serde(default)]
    pub mode: LocalSshMode,
    #[serde(default)]
    pub backend: LocalSshBackend,
    #[serde(default)]
    pub readiness: LocalSshReadiness,
    #[serde(default = "default_listen_host")]
    pub host: String,
    pub port: Option<u16>,
}

impl LocalSshConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.mode == LocalSshMode::Listen && self.host != "127.0.0.1" && self.host != "::1" {
            anyhow::bail!("local_ssh listen host must be loopback-only");
        }
        if self.readiness == LocalSshReadiness::SshOnly
            && self.backend != LocalSshBackend::PublishedPort
        {
            anyhow::bail!("local_ssh readiness \"ssh_only\" requires backend = \"published_port\"");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSshConfigInput {
    pub mode: Option<LocalSshMode>,
    pub backend: Option<LocalSshBackend>,
    pub readiness: Option<LocalSshReadiness>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

impl LocalSshConfigInput {
    pub(super) fn overlay(mut self, later: &Self) -> Self {
        if let Some(mode) = later.mode {
            self.mode = Some(mode);
        }
        if let Some(backend) = later.backend {
            self.backend = Some(backend);
        }
        if let Some(readiness) = later.readiness {
            self.readiness = Some(readiness);
        }
        if let Some(host) = &later.host {
            self.host = Some(host.clone());
        }
        if let Some(port) = later.port {
            self.port = Some(port);
        }
        self
    }

    pub(super) fn into_effective(self) -> LocalSshConfig {
        LocalSshConfig {
            mode: self.mode.unwrap_or_default(),
            backend: self.backend.unwrap_or_default(),
            readiness: self.readiness.unwrap_or_default(),
            host: self.host.unwrap_or_else(default_listen_host),
            port: self.port,
        }
    }

    pub(super) fn validate_partial(&self) -> anyhow::Result<()> {
        if let Some(host) = &self.host
            && host != "127.0.0.1"
            && host != "::1"
        {
            anyhow::bail!("local_ssh listen host must be loopback-only");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalSshMode {
    #[default]
    ProxyCommand,
    Listen,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalSshBackend {
    #[default]
    Socket,
    PublishedPort,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalSshReadiness {
    #[default]
    AgentControl,
    SshOnly,
}
