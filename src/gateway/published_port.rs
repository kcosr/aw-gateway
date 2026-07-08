use super::Runtime;
use super::model::TcpEndpoint;
use crate::config::{ContainerRuntimeType, LocalSshBackend, LocalSshMode, TargetMode};
use crate::fileutil::{remove_if_exists, write_private_file};
use crate::paths;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const PUBLISHED_SSH_PORT_FILE: &str = "published-ssh-port";
const PUBLISHED_SSH_PORT_PENDING_FILE: &str = "published-ssh-port.pending";
const PUBLISHED_SSH_ENDPOINT_FILE: &str = "published-ssh-endpoint.json";

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
        self.uses_apple_published_ssh_port_state() || self.direct_published_ssh_enabled()
    }

    pub(super) fn direct_published_ssh_enabled(&self) -> bool {
        self.ssh_endpoint_configured()
            && self.ssh_backend() == LocalSshBackend::PublishedPort
            && self
                .target
                .local_ssh
                .as_ref()
                .is_some_and(|local_ssh| local_ssh.mode == LocalSshMode::Direct)
    }

    pub(super) fn uses_apple_published_ssh_port_state(&self) -> bool {
        self.container_runtime.kind() == ContainerRuntimeType::AppleContainer
            && self.ssh_endpoint_configured()
            && self.ssh_backend() == LocalSshBackend::PublishedPort
    }

    pub(super) fn prepare_published_ssh_host_port_for_create(
        &self,
    ) -> anyhow::Result<Option<PendingPublishedSshPort>> {
        if !self.needs_explicit_published_ssh_port() {
            return Ok(None);
        }
        paths::ensure_private_dir(&self.paths.container_state_dir)?;
        if self.uses_apple_published_ssh_port_state() {
            self.remove_pending_published_ssh_port();
        }
        let pending = match self.configured_direct_published_ssh_port() {
            Some(port) => reserve_specific_loopback_port(port)?,
            None => reserve_loopback_port()?,
        };
        if self.uses_apple_published_ssh_port_state() {
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
        }
        Ok(Some(pending))
    }

    pub(super) fn promote_pending_published_ssh_port(&self) -> anyhow::Result<()> {
        if !self.uses_apple_published_ssh_port_state() {
            return Ok(());
        }
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
        if !self.uses_apple_published_ssh_port_state() {
            return;
        }
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
            self.published_ssh_endpoint_path(),
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

    pub(super) fn configured_direct_published_ssh_port(&self) -> Option<u16> {
        self.direct_published_ssh_enabled()
            .then(|| {
                self.target
                    .local_ssh
                    .as_ref()
                    .and_then(|local_ssh| local_ssh.port)
            })
            .flatten()
    }

    pub(super) fn write_published_ssh_endpoint_state(
        &self,
        endpoint: &TcpEndpoint,
    ) -> anyhow::Result<()> {
        if !self.direct_published_ssh_enabled() {
            return Ok(());
        }
        paths::ensure_private_dir(&self.paths.container_state_dir)?;
        let state = PublishedSshEndpointState {
            host: endpoint.host.clone(),
            port: endpoint.port,
            container: self.identity.container_name.clone(),
            target: self.identity.target_name.clone(),
            target_mode: target_mode_name(self.target.mode).into(),
            runtime_type: runtime_type_name(self.container_runtime.kind()).into(),
            access: self.target.access.method.as_str().into(),
            local_ssh_mode: "direct".into(),
            local_ssh_backend: "published_port".into(),
            configured_port: self.configured_direct_published_ssh_port(),
            session_id: self.identity.session_id.clone(),
            context: self.context.as_map().clone(),
            updated_at_ms: unix_time_ms()?,
        };
        write_private_file(
            &self.published_ssh_endpoint_path(),
            &serde_json::to_vec_pretty(&state)?,
            0o600,
        )
        .with_context(|| {
            format!(
                "write published SSH endpoint {}",
                self.published_ssh_endpoint_path().display()
            )
        })
    }

    pub(super) fn read_matching_published_ssh_endpoint_state(
        &self,
    ) -> anyhow::Result<Option<TcpEndpoint>> {
        let Some(state) = self.read_published_ssh_endpoint_state()? else {
            return Ok(None);
        };
        if !state.matches_runtime(self) {
            return Ok(None);
        }
        Ok(Some(TcpEndpoint {
            host: state.host,
            port: state.port,
        }))
    }

    fn read_published_ssh_endpoint_state(
        &self,
    ) -> anyhow::Result<Option<PublishedSshEndpointState>> {
        let path = self.published_ssh_endpoint_path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        serde_json::from_slice(&raw)
            .with_context(|| format!("parse {}", path.display()))
            .map(Some)
    }

    fn configured_port_mismatch_published_ssh_endpoint_state(
        &self,
    ) -> anyhow::Result<Option<TcpEndpoint>> {
        let Some(configured_port) = self.configured_direct_published_ssh_port() else {
            return Ok(None);
        };
        let Some(state) = self.read_published_ssh_endpoint_state()? else {
            return Ok(None);
        };
        if !state.matches_runtime_except_configured_port(self) || state.port == configured_port {
            return Ok(None);
        };
        Ok(Some(TcpEndpoint {
            host: state.host,
            port: state.port,
        }))
    }

    pub(super) async fn direct_client_ssh_endpoint(&self) -> anyhow::Result<Option<TcpEndpoint>> {
        if !self.direct_published_ssh_enabled() {
            return Ok(None);
        }
        let inspect = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?;
        if let Some(inspect) = &inspect {
            self.validate_labels(inspect)?;
            let container_running = inspect.state.running;
            if let Some(endpoint) = self
                .direct_authoritative_published_ssh_endpoint(container_running)
                .await?
            {
                return Ok(Some(endpoint));
            }
            if container_running {
                anyhow::bail!(
                    "target {:?} is running but its published SSH endpoint could not be resolved",
                    self.identity.target_name
                );
            }
            if let Some(endpoint) = self.read_matching_published_ssh_endpoint_state()? {
                self.validate_direct_endpoint_matches_runtime_state(&endpoint)?;
                self.validate_direct_endpoint_matches_config(&endpoint)?;
                return Ok(Some(endpoint));
            }
            if let Some(endpoint) = self.configured_port_mismatch_published_ssh_endpoint_state()? {
                self.validate_direct_endpoint_matches_runtime_state(&endpoint)?;
                self.validate_direct_endpoint_matches_config(&endpoint)?;
            }
            anyhow::bail!(
                "target {:?} has a stopped container but no matching published SSH endpoint state; run `aw-gateway up {}` first",
                self.identity.target_name,
                self.identity.target_name
            );
        }
        if let Some(port) = self.configured_direct_published_ssh_port() {
            return Ok(Some(TcpEndpoint {
                host: "127.0.0.1".into(),
                port,
            }));
        }
        Ok(None)
    }

    pub(super) async fn direct_status_ssh_endpoint(
        &self,
        inspect: Option<&crate::runtime::ContainerInspect>,
    ) -> anyhow::Result<Option<TcpEndpoint>> {
        if !self.direct_published_ssh_enabled() {
            return Ok(None);
        }
        let Some(inspect) = inspect else {
            return Ok(None);
        };
        self.validate_labels(inspect)?;
        if let Some(endpoint) = self
            .direct_authoritative_published_ssh_endpoint(inspect.state.running)
            .await?
        {
            return Ok(Some(endpoint));
        }
        if inspect.state.running {
            return Ok(None);
        }
        if let Some(endpoint) = self.read_matching_published_ssh_endpoint_state()? {
            self.validate_direct_endpoint_matches_runtime_state(&endpoint)?;
            self.validate_direct_endpoint_matches_config(&endpoint)?;
            return Ok(Some(endpoint));
        }
        Ok(None)
    }

    async fn direct_authoritative_published_ssh_endpoint(
        &self,
        container_running: bool,
    ) -> anyhow::Result<Option<TcpEndpoint>> {
        let endpoint = if self.uses_apple_published_ssh_port_state() {
            if container_running {
                self.published_ssh_endpoint().await?
            } else {
                self.apple_direct_endpoint_from_published_ssh_port_state()?
            }
        } else {
            self.published_ssh_endpoint().await?
        };
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        if self.uses_apple_published_ssh_port_state()
            && let Some(saved) = self.read_matching_published_ssh_endpoint_state()?
        {
            self.validate_direct_endpoint_matches_runtime_state(&saved)?;
        }
        self.validate_direct_endpoint_matches_config(&endpoint)?;
        self.write_published_ssh_endpoint_state(&endpoint)?;
        Ok(Some(endpoint))
    }

    fn apple_direct_endpoint_from_published_ssh_port_state(
        &self,
    ) -> anyhow::Result<Option<TcpEndpoint>> {
        if !self.uses_apple_published_ssh_port_state() {
            return Ok(None);
        }
        Ok(self.read_published_ssh_port()?.map(|port| TcpEndpoint {
            host: "127.0.0.1".into(),
            port,
        }))
    }

    fn validate_direct_endpoint_matches_config(
        &self,
        endpoint: &TcpEndpoint,
    ) -> anyhow::Result<()> {
        if endpoint.host != "127.0.0.1" {
            anyhow::bail!(
                "direct published SSH endpoint for target {:?} resolved to non-loopback host {}",
                self.identity.target_name,
                endpoint.host
            );
        }
        if let Some(configured) = self.configured_direct_published_ssh_port()
            && endpoint.port != configured
        {
            anyhow::bail!(
                "direct published SSH endpoint for target {:?} uses host port {}, but local_ssh.port is {}; remove and recreate the target or update local_ssh.port",
                self.identity.target_name,
                endpoint.port,
                configured
            );
        }
        Ok(())
    }

    fn validate_direct_endpoint_matches_runtime_state(
        &self,
        endpoint: &TcpEndpoint,
    ) -> anyhow::Result<()> {
        if !self.uses_apple_published_ssh_port_state() {
            return Ok(());
        }
        let port = self.read_published_ssh_port()?.ok_or_else(|| {
            anyhow::anyhow!(
                "Apple container target {:?} has saved direct SSH endpoint state but {} is missing; run `aw-gateway up {}` or remove and recreate the target",
                self.identity.target_name,
                self.published_ssh_port_path().display(),
                self.identity.target_name
            )
        })?;
        if endpoint.port != port {
            anyhow::bail!(
                "Apple container target {:?} saved direct SSH endpoint port {} does not match authoritative published SSH port {}; run `aw-gateway up {}` or remove and recreate the target",
                self.identity.target_name,
                endpoint.port,
                port,
                self.identity.target_name
            );
        }
        Ok(())
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

    pub(super) fn read_published_ssh_port(&self) -> anyhow::Result<Option<u16>> {
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

    fn published_ssh_endpoint_path(&self) -> PathBuf {
        self.paths
            .container_state_dir
            .join(PUBLISHED_SSH_ENDPOINT_FILE)
    }
}

pub(super) fn published_port_bind_conflict(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("address already in use")
        || message.contains("port is already allocated")
        || (message.contains("bind")
            && (message.contains("listen")
                || message.contains("port")
                || message.contains("in use")))
}

fn reserve_loopback_port() -> anyhow::Result<PendingPublishedSshPort> {
    reserve_loopback_port_at(0)
}

fn reserve_specific_loopback_port(port: u16) -> anyhow::Result<PendingPublishedSshPort> {
    reserve_loopback_port_at(port)
}

fn reserve_loopback_port_at(port: u16) -> anyhow::Result<PendingPublishedSshPort> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("allocate loopback published SSH port {port}"))?;
    Ok(PendingPublishedSshPort {
        port: listener.local_addr()?.port(),
        _listener: listener,
    })
}

#[derive(Debug, Deserialize, Serialize)]
struct PublishedSshEndpointState {
    host: String,
    port: u16,
    container: String,
    target: String,
    target_mode: String,
    runtime_type: String,
    access: String,
    local_ssh_mode: String,
    local_ssh_backend: String,
    configured_port: Option<u16>,
    session_id: Option<String>,
    context: BTreeMap<String, String>,
    updated_at_ms: u128,
}

impl PublishedSshEndpointState {
    fn matches_runtime(&self, runtime: &Runtime) -> bool {
        self.matches_runtime_except_configured_port(runtime)
            && self.configured_port == runtime.configured_direct_published_ssh_port()
    }

    fn matches_runtime_except_configured_port(&self, runtime: &Runtime) -> bool {
        self.container == runtime.identity.container_name
            && self.target == runtime.identity.target_name
            && self.target_mode == target_mode_name(runtime.target.mode)
            && self.runtime_type == runtime_type_name(runtime.container_runtime.kind())
            && self.access == runtime.target.access.method.as_str()
            && self.local_ssh_mode == "direct"
            && self.local_ssh_backend == "published_port"
            && self.session_id == runtime.identity.session_id
            && self.context == *runtime.context.as_map()
    }
}

fn target_mode_name(mode: TargetMode) -> &'static str {
    match mode {
        TargetMode::Fixed => "fixed",
        TargetMode::Ephemeral => "ephemeral",
    }
}

fn runtime_type_name(kind: ContainerRuntimeType) -> &'static str {
    match kind {
        ContainerRuntimeType::Podman => "podman",
        ContainerRuntimeType::Docker => "docker",
        ContainerRuntimeType::Colima => "colima",
        ContainerRuntimeType::AppleContainer => "apple_container",
    }
}

fn unix_time_ms() -> anyhow::Result<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time is before UNIX_EPOCH")?
        .as_millis())
}
