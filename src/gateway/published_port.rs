use super::Runtime;
use super::model::TcpEndpoint;
use crate::config::{ContainerRuntimeType, LocalSshBackend};
use crate::fileutil::{remove_if_exists, write_private_file};
use crate::paths;
use anyhow::Context;
use std::path::PathBuf;

const PUBLISHED_SSH_PORT_FILE: &str = "published-ssh-port";
const PUBLISHED_SSH_PORT_PENDING_FILE: &str = "published-ssh-port.pending";

pub(super) struct PendingPublishedSshPort {
    port: u16,
    _listener: std::net::TcpListener,
}

impl PendingPublishedSshPort {
    pub(super) fn port(&self) -> u16 {
        self.port
    }
}

impl Runtime {
    pub(super) fn needs_explicit_published_ssh_port(&self) -> bool {
        self.container_runtime.kind() == ContainerRuntimeType::AppleContainer
            && self.ssh_endpoint_configured()
            && self.ssh_backend() == LocalSshBackend::PublishedPort
    }

    pub(super) fn prepare_pending_published_ssh_port(
        &self,
    ) -> anyhow::Result<Option<PendingPublishedSshPort>> {
        if !self.needs_explicit_published_ssh_port() {
            return Ok(None);
        }
        paths::ensure_private_dir(&self.paths.container_state_dir)?;
        self.remove_pending_published_ssh_port();
        let pending = reserve_loopback_port()?;
        write_private_file(
            &self.pending_published_ssh_port_path(),
            format!("{}\n", pending.port()).as_bytes(),
            0o600,
        )
        .with_context(|| {
            format!(
                "write pending published SSH port {}",
                self.pending_published_ssh_port_path().display()
            )
        })?;
        Ok(Some(pending))
    }

    pub(super) fn promote_pending_published_ssh_port(&self) -> anyhow::Result<()> {
        std::fs::rename(
            self.pending_published_ssh_port_path(),
            self.published_ssh_port_path(),
        )
        .with_context(|| {
            format!(
                "promote pending published SSH port to {}",
                self.published_ssh_port_path().display()
            )
        })
    }

    pub(super) fn remove_pending_published_ssh_port(&self) {
        if let Err(err) = remove_if_exists(&self.pending_published_ssh_port_path()) {
            tracing::warn!(
                path = %self.pending_published_ssh_port_path().display(),
                error = %err,
                "failed to remove pending published SSH port state"
            );
        }
    }

    pub(super) fn cleanup_published_ssh_port_state(&self) {
        for path in [
            self.published_ssh_port_path(),
            self.pending_published_ssh_port_path(),
        ] {
            if let Err(err) = remove_if_exists(&path) {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to remove published SSH port state"
                );
            }
        }
    }

    pub(super) async fn apple_published_ssh_endpoint(&self) -> anyhow::Result<Option<TcpEndpoint>> {
        let Some(inspect) = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?
        else {
            return Ok(None);
        };
        self.validate_labels(&inspect)?;
        if !inspect.state.running {
            return Ok(None);
        }
        let port = self.read_published_ssh_port()?.ok_or_else(|| {
            anyhow::anyhow!(
                "Apple container target {:?} is running but {} is missing; remove and recreate the target to allocate a published SSH port",
                self.identity.target_name,
                self.published_ssh_port_path().display()
            )
        })?;
        Ok(Some(TcpEndpoint {
            host: "127.0.0.1".into(),
            port,
        }))
    }

    fn read_published_ssh_port(&self) -> anyhow::Result<Option<u16>> {
        let path = self.published_ssh_port_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        let port = raw
            .trim()
            .parse::<u16>()
            .with_context(|| format!("parse published SSH port {}", path.display()))?;
        Ok(Some(port))
    }

    fn published_ssh_port_path(&self) -> PathBuf {
        self.paths.container_state_dir.join(PUBLISHED_SSH_PORT_FILE)
    }

    fn pending_published_ssh_port_path(&self) -> PathBuf {
        self.paths
            .container_state_dir
            .join(PUBLISHED_SSH_PORT_PENDING_FILE)
    }
}

pub(super) fn apple_published_port_bind_conflict(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("address already in use")
        || message.contains("port is already allocated")
        || (message.contains("bind")
            && (message.contains("listen")
                || message.contains("port")
                || message.contains("in use")))
}

fn reserve_loopback_port() -> anyhow::Result<PendingPublishedSshPort> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("allocate loopback published SSH port")?;
    Ok(PendingPublishedSshPort {
        port: listener.local_addr()?.port(),
        _listener: listener,
    })
}
