use super::{ControlSocketPaths, Runtime, UNIX_SOCKET_PATH_MAX_BYTES};
use crate::config::{
    AGENT_SCHEMA_VERSION, BOOTSTRAP_SCHEMA_VERSION, BootstrapIdentity, ContainerAgentFile,
    ContainerBootstrapFile, ContainerRuntimeType, ControlSocketConfig, ControlSocketsConfig,
    IdleCleanupAction, IdleCleanupOwner, LoggingConfig, RenderedContainerBootstrapStep,
    TargetConfig, default_control_socket_host_dir, validate_name,
};
use crate::context::RuntimeContext;
use crate::fileutil::{AtomicWritePolicy, atomic_write_toml, write_private_file};
use crate::paths::UserContext;
use crate::runtime;
use crate::ssh_filter::SshCommandFilterPolicy;
use crate::template::{self, Vars};
use anyhow::Context;
use std::path::{Component, Path, PathBuf};
use tokio::net::UnixStream;

impl Runtime {
    pub(super) fn write_container_agent_config(&self) -> anyhow::Result<PathBuf> {
        let mut container_agent = self.target.container_agent.clone();
        if self.agent_control_enabled() {
            container_agent.control_socket = Some(ControlSocketConfig::Path(
                self.paths
                    .control_sockets
                    .container_agent_socket
                    .display()
                    .to_string(),
            ));
        }
        if let Some(bridge) = &mut container_agent.ssh_bridge
            && self.agent_enabled()
            && bridge.enabled
        {
            bridge.socket = Some(
                self.paths
                    .control_sockets
                    .container_ssh_socket
                    .display()
                    .to_string(),
            );
        }
        self.inject_container_sshd_env(&mut container_agent)?;
        if let Some(idle_cleanup) = &self.target.idle_cleanup {
            container_agent.idle_cleanup = match (idle_cleanup.owner, idle_cleanup.action) {
                (IdleCleanupOwner::Agent, action) if action != IdleCleanupAction::None => {
                    Some(idle_cleanup.clone())
                }
                _ => None,
            };
        }
        self.render_gateway_managed_service_fields(&mut container_agent)?;
        if let Some(relay) = &mut container_agent.access_flow_relay {
            relay.render(&self.vars(None))?;
        }
        let cfg = ContainerAgentFile {
            schema_version: AGENT_SCHEMA_VERSION.to_string(),
            logging: LoggingConfig::default(),
            container_agent,
        };
        cfg.validate()
            .context("validate rendered container-agent configuration")?;
        let path = self.container_agent_config_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn render_gateway_managed_service_fields(
        &self,
        container_agent: &mut crate::config::ContainerAgentConfig,
    ) -> anyhow::Result<()> {
        let vars = self.vars(None);
        for service in &mut container_agent.services {
            if service.user == crate::config::SERVICE_USER_TEMPLATE {
                service.user = template::render(&service.user, &vars)?;
            }
        }
        Ok(())
    }

    fn inject_container_sshd_env(
        &self,
        container_agent: &mut crate::config::ContainerAgentConfig,
    ) -> anyhow::Result<()> {
        for service in &mut container_agent.services {
            if service.name != "container-sshd" {
                continue;
            }
            let authorized_keys_file = self
                .inner_authorized_keys_in_container()
                .display()
                .to_string();
            if authorized_keys_file
                .chars()
                .any(|character| character.is_whitespace() || character == '#')
            {
                anyhow::bail!(
                    "managed container authorized-keys path {authorized_keys_file:?} cannot contain whitespace or '#'"
                );
            }
            service.env.insert(
                "AW_SSHD_POLICY_CONFIG".into(),
                crate::config::EnvValue {
                    value: Some(
                        self.ssh_command_filter_policy_in_container()
                            .display()
                            .to_string(),
                    ),
                    file: None,
                    inherit: None,
                    interpolate: false,
                    required: true,
                },
            );
            service.env.insert(
                "AW_SSHD_AUTHORIZED_KEYS_FILE".into(),
                crate::config::EnvValue {
                    value: Some(authorized_keys_file),
                    file: None,
                    inherit: None,
                    interpolate: false,
                    required: true,
                },
            );
            service
                .env
                .entry("AW_SSHD_SETENV_CONFIG".into())
                .or_insert(crate::config::EnvValue {
                    value: Some(
                        self.sshd_session_env_config_in_container()
                            .display()
                            .to_string(),
                    ),
                    file: None,
                    inherit: None,
                    interpolate: false,
                    required: true,
                });
        }
        Ok(())
    }

    pub(super) fn write_ssh_command_filter_policy(&self) -> anyhow::Result<PathBuf> {
        let cfg = SshCommandFilterPolicy {
            sftp: self.target.container_ssh.transfer.sftp,
            legacy_scp: self.target.container_ssh.transfer.legacy_scp,
        };
        let path = self.ssh_command_filter_policy_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    pub(super) fn write_container_bootstrap_config(&self) -> anyhow::Result<PathBuf> {
        let vars = self.vars(None);
        let steps = self
            .target
            .container_bootstrap_steps
            .iter()
            .map(|step| {
                Ok(RenderedContainerBootstrapStep {
                    name: step.name.clone(),
                    required: step.required,
                    user: template::render(&step.user, &vars)?,
                    command: step
                        .command
                        .iter()
                        .map(|arg| template::render(arg, &vars))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                    timeout: step.timeout.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let cfg = ContainerBootstrapFile {
            schema_version: BOOTSTRAP_SCHEMA_VERSION.to_string(),
            agent_program: template::render(&self.target.container_bootstrap.agent_program, &vars)?,
            agent_config: self
                .container_agent_config_in_container()
                .display()
                .to_string(),
            skip_identity_prepare: self.container_runtime.is_podman(),
            chown_existing_identity_dirs: self.container_runtime.kind()
                != ContainerRuntimeType::AppleContainer,
            identity: BootstrapIdentity {
                session_user: self.identity.container_user.clone(),
                session_uid: self.identity.session_uid,
                session_gid: self.identity.session_gid,
                session_home: self.identity.container_home.display().to_string(),
                session_shell: self.identity.session_shell.clone(),
                state_dir: self
                    .paths
                    .container_state_dir_in_container
                    .display()
                    .to_string(),
            },
            steps,
        };
        let path = self.container_bootstrap_config_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn container_agent_config_host(&self) -> PathBuf {
        self.paths.container_state_dir.join("container-agent.toml")
    }

    pub(super) fn container_agent_config_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("container-agent.toml")
    }

    fn container_bootstrap_config_host(&self) -> PathBuf {
        self.paths
            .container_state_dir
            .join("container-bootstrap.toml")
    }

    pub(super) fn container_bootstrap_config_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("container-bootstrap.toml")
    }

    fn sshd_session_env_config_host(&self) -> PathBuf {
        self.paths.container_state_dir.join("sshd-session-env.conf")
    }

    fn sshd_session_env_config_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("sshd-session-env.conf")
    }

    fn ssh_command_filter_policy_host(&self) -> PathBuf {
        self.paths
            .container_state_dir
            .join("ssh-command-filter.toml")
    }

    fn ssh_command_filter_policy_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("ssh-command-filter.toml")
    }

    pub(super) fn write_sshd_session_env_config(&self) -> anyhow::Result<PathBuf> {
        let path = self.sshd_session_env_config_host();
        let env = self.render_env_map(&self.target.session_env)?;
        let mut raw = String::from(
            "# Generated by aw-gateway. Included by container SSHD helpers when configured.\n",
        );
        if !env.is_empty() {
            raw.push_str("SetEnv");
            for (key, value) in env {
                if key.contains(char::is_whitespace) {
                    anyhow::bail!(
                        "target session_env key {key:?} contains whitespace and cannot be rendered into sshd SetEnv"
                    );
                }
                if value.contains(char::is_whitespace) {
                    anyhow::bail!(
                        "target session_env value for {key:?} contains whitespace and cannot be rendered into sshd SetEnv"
                    );
                }
                raw.push(' ');
                raw.push_str(&key);
                raw.push('=');
                raw.push_str(&value);
            }
            raw.push('\n');
        }
        write_private_file(&path, raw.as_bytes(), 0o600)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    pub(super) fn prepare_control_socket_dir(&self) -> anyhow::Result<()> {
        if self.paths.control_sockets.default_host_dir {
            let run_user_dir = PathBuf::from(format!("/run/user/{}", self.identity.user.uid));
            let metadata = std::fs::metadata(&run_user_dir).with_context(|| {
                format!(
                    "{} is required by the default control_sockets.host_dir; configure control_sockets.host_dir to a writable short absolute path if this host does not provide per-user runtime directories",
                    run_user_dir.display()
                )
            })?;
            if !metadata.is_dir() {
                anyhow::bail!(
                    "{} is not a directory; configure control_sockets.host_dir to a writable short absolute path",
                    run_user_dir.display()
                );
            }
            let probe = run_user_dir.join(format!(".aw-gateway-write-test-{}", std::process::id()));
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&probe)
            {
                Ok(_) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(err) => {
                    anyhow::bail!(
                        "{} is not writable: {err}; configure control_sockets.host_dir to a writable short absolute path",
                        run_user_dir.display()
                    );
                }
            }
        }
        ensure_control_socket_dir(&self.paths.control_sockets.host_dir)?;
        Ok(())
    }

    pub(super) fn remove_stale_control_socket_files(&self) -> anyhow::Result<()> {
        for socket in [
            &self.paths.control_sockets.host_agent_socket,
            &self.paths.control_sockets.host_ssh_socket,
        ] {
            match std::fs::symlink_metadata(socket) {
                Ok(metadata) if metadata.is_dir() => {
                    anyhow::bail!(
                        "control socket path {} exists as a directory; remove it or configure a different control_sockets.host_dir",
                        socket.display()
                    );
                }
                Ok(_) => {
                    std::fs::remove_file(socket).with_context(|| {
                        format!("remove stale control socket {}", socket.display())
                    })?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("inspect control socket {}", socket.display()));
                }
            }
        }
        Ok(())
    }

    pub(super) fn agent_socket(&self) -> PathBuf {
        self.paths.control_sockets.host_agent_socket.clone()
    }

    pub(super) fn ssh_socket(&self) -> PathBuf {
        self.paths.control_sockets.host_ssh_socket.clone()
    }

    fn container_agent_socket(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.agent_control_enabled() {
            return Ok(None);
        }
        Ok(Some(
            self.paths.control_sockets.container_agent_socket.clone(),
        ))
    }

    fn container_ssh_socket(&self) -> anyhow::Result<Option<PathBuf>> {
        let Some(_) = self
            .target
            .container_agent
            .ssh_bridge
            .as_ref()
            .filter(|bridge| self.agent_enabled() && bridge.enabled)
        else {
            return Ok(None);
        };
        Ok(Some(
            self.paths.control_sockets.container_ssh_socket.clone(),
        ))
    }

    pub(super) fn effective_unix_socket_paths(
        &self,
    ) -> anyhow::Result<Vec<(&'static str, PathBuf)>> {
        let mut paths = Vec::new();
        if self.agent_control_enabled() {
            paths.push(("host agent socket path", self.agent_socket()));
            if let Some(path) = self.container_agent_socket()? {
                paths.push(("container agent socket path", path));
            }
        }
        if self.ssh_endpoint_configured()
            && self.ssh_backend() == crate::config::LocalSshBackend::Socket
        {
            paths.push(("host ssh socket path", self.ssh_socket()));
        }
        if let Some(path) = self.container_ssh_socket()? {
            paths.push(("container ssh socket path", path));
        }
        Ok(paths)
    }

    pub(super) fn validate_unix_socket_paths(&self) -> anyhow::Result<()> {
        for (label, path) in self.effective_unix_socket_paths()? {
            let bytes = unix_socket_path_bytes(&path);
            if bytes > UNIX_SOCKET_PATH_MAX_BYTES {
                anyhow::bail!(
                    "{label} is too long for a Unix domain socket ({bytes} bytes, limit {UNIX_SOCKET_PATH_MAX_BYTES}): {}. Configure control_sockets.host_dir or control_sockets.container_dir to a shorter absolute path. For fixed targets, runtime_id is the target id {:?}; for ephemeral targets, runtime_id is the session id.",
                    path.display(),
                    self.identity.target_name
                );
            }
        }
        Ok(())
    }

    pub(super) async fn validate_agent_socket(&self) -> anyhow::Result<()> {
        runtime::socket_is_safe_for(
            &self.agent_socket(),
            self.identity.user.uid,
            self.identity.user.gid,
        )
    }

    pub(super) async fn validate_ssh_socket(&self) -> anyhow::Result<()> {
        self.validate_socket_path(&self.ssh_socket()).await
    }

    async fn validate_socket_path(&self, socket: &Path) -> anyhow::Result<()> {
        runtime::socket_is_safe_for(socket, self.identity.user.uid, self.identity.user.gid)?;
        let _ = UnixStream::connect(socket)
            .await
            .with_context(|| format!("test-connect {}", socket.display()))?;
        Ok(())
    }

    pub(super) fn cleanup_control_socket_dir(&self) {
        match std::fs::symlink_metadata(&self.paths.control_sockets.host_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    "not removing symlink control socket runtime directory"
                );
                return;
            }
            Ok(metadata) if !metadata.is_dir() => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    "not removing non-directory control socket runtime path"
                );
                return;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    error = %err,
                    "not removing control socket runtime directory after inspection failure"
                );
                return;
            }
        }

        for socket in [
            &self.paths.control_sockets.host_agent_socket,
            &self.paths.control_sockets.host_ssh_socket,
        ] {
            match std::fs::remove_file(socket) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(
                        path = %socket.display(),
                        error = %err,
                        "failed to remove control socket file"
                    );
                }
            }
        }

        match std::fs::remove_dir(&self.paths.control_sockets.host_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    error = %err,
                    "failed to remove control socket runtime directory"
                );
            }
        }
    }
}

fn ensure_control_socket_dir(path: &Path) -> anyhow::Result<()> {
    let created = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "control_sockets.host_dir {} must not be a symlink",
                path.display()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!(
                "control_sockets.host_dir {} exists but is not a directory",
                path.display()
            );
        }
        Ok(metadata) => {
            validate_control_socket_dir_permissions(path, &metadata)?;
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .with_context(|| format!("create control_sockets.host_dir {}", path.display()))?;
            true
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("inspect control_sockets.host_dir {}", path.display()));
        }
    };

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect control_sockets.host_dir {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "control_sockets.host_dir {} must not be a symlink",
            path.display()
        );
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "control_sockets.host_dir {} exists but is not a directory",
            path.display()
        );
    }
    if created {
        set_control_socket_dir_permissions(path)?;
    } else {
        validate_control_socket_dir_permissions(path, &metadata)?;
    }
    Ok(())
}

fn set_control_socket_dir_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 control_sockets.host_dir {}", path.display()))?;
    }
    Ok(())
}

fn validate_control_socket_dir_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "control_sockets.host_dir {} exists with permissions {mode:o}; use a private 0700 directory or remove it so aw-gateway can create it",
                path.display()
            );
        }
    }
    Ok(())
}

pub(super) fn resolve_workspace_state_path(
    base: &Path,
    configured: &str,
) -> anyhow::Result<PathBuf> {
    let path = Path::new(configured);
    if configured == "~"
        || configured.starts_with("~/")
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!(
            "target.workspace.state_dir must stay within the workspace mount and must not render as absolute, home-relative, or with '..' components"
        );
    }
    Ok(base.join(path).components().collect())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_control_socket_paths(
    cfg: &ControlSocketsConfig,
    target: &TargetConfig,
    target_name: &str,
    container_name: &str,
    session_id: Option<&str>,
    runtime_id: &str,
    user: &UserContext,
    context: &RuntimeContext,
) -> anyhow::Result<ControlSocketPaths> {
    validate_name("control_sockets runtime_id", runtime_id)?;
    let mut vars = Vars::new();
    vars.insert("user".into(), user.user.clone());
    vars.insert("uid".into(), user.uid.to_string());
    vars.insert("gid".into(), user.gid.to_string());
    vars.insert("home".into(), user.home.display().to_string());
    vars.insert("target".into(), target_name.to_string());
    vars.insert("image".into(), target.image.clone());
    vars.insert("image_slug".into(), template::image_slug(&target.image));
    vars.insert("container_name".into(), container_name.to_string());
    vars.insert("runtime_id".into(), runtime_id.to_string());
    if let Some(session_id) = session_id {
        vars.insert("session_id".into(), session_id.to_string());
    }
    context.insert_template_vars(&mut vars);

    let host_dir = PathBuf::from(template::render(&cfg.host_dir, &vars)?);
    if !host_dir.is_absolute() {
        anyhow::bail!(
            "control_sockets.host_dir must render to an absolute path, got {}",
            host_dir.display()
        );
    }
    validate_control_socket_host_dir(&host_dir, runtime_id, user)?;
    let container_dir = PathBuf::from(template::render(&cfg.container_dir, &vars)?);
    if !container_dir.is_absolute() {
        anyhow::bail!(
            "control_sockets.container_dir must render to an absolute path, got {}",
            container_dir.display()
        );
    }

    Ok(ControlSocketPaths {
        host_agent_socket: host_dir.join("agent.sock"),
        host_ssh_socket: host_dir.join("ssh.sock"),
        container_agent_socket: container_dir.join("agent.sock"),
        container_ssh_socket: container_dir.join("ssh.sock"),
        default_host_dir: cfg.host_dir == default_control_socket_host_dir(),
        host_dir,
        container_dir,
    })
}

fn validate_control_socket_host_dir(
    host_dir: &Path,
    runtime_id: &str,
    user: &UserContext,
) -> anyhow::Result<()> {
    if host_dir
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        anyhow::bail!(
            "control_sockets.host_dir must not contain '.' or '..' path components: {}",
            host_dir.display()
        );
    }

    let dangerous = [
        PathBuf::from("/"),
        user.home.clone(),
        PathBuf::from("/tmp"),
        PathBuf::from("/run"),
        PathBuf::from("/run/user"),
        PathBuf::from(format!("/run/user/{}", user.uid)),
    ];
    if dangerous.iter().any(|path| path == host_dir) {
        anyhow::bail!(
            "control_sockets.host_dir must be an isolated runtime-specific leaf directory, got dangerous shared path {}",
            host_dir.display()
        );
    }

    let Some(leaf) = host_dir.file_name().and_then(|value| value.to_str()) else {
        anyhow::bail!(
            "control_sockets.host_dir must end with runtime_id {runtime_id:?}, got {}",
            host_dir.display()
        );
    };
    if leaf != runtime_id {
        anyhow::bail!(
            "control_sockets.host_dir must end with runtime_id {runtime_id:?} so cleanup only removes the runtime leaf, got {}",
            host_dir.display()
        );
    }

    Ok(())
}

#[cfg(unix)]
fn unix_socket_path_bytes(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn unix_socket_path_bytes(path: &Path) -> usize {
    path.as_os_str().to_string_lossy().len()
}
