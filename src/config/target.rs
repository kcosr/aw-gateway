use super::steps::{
    ContainerBootstrapStep, HostStep, LifecycleStep, LifecycleStepKey, RawContainerBootstrapStep,
    RawHostStep, RawLifecycleStep, StepKey, merge_target_step_patches, validate_raw_target_steps,
};
use super::validation::*;
use super::{ContainerAgentConfig, ContainerAgentConfigInput};
use crate::template;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod container_ssh;
mod idle_cleanup;
mod local_ssh;

use container_ssh::overlay_target_container_ssh;
pub use container_ssh::{
    ContainerSshConfig, ContainerSshTransferConfig, LegacyScpTransferMode, SftpTransferMode,
    TargetContainerSshConfig, TargetContainerSshTransferConfig,
};
pub use idle_cleanup::{
    IdleCleanupAction, IdleCleanupConfig, IdleCleanupConfigInput, IdleCleanupOwner,
};
pub use local_ssh::{
    LocalSshBackend, LocalSshConfig, LocalSshConfigInput, LocalSshMode, LocalSshReadiness,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    #[serde(default = "default_workspace_path")]
    pub path: String,
    #[serde(default = "default_workspace_state_dir")]
    pub state_dir: String,
    #[serde(default)]
    pub cleanup: WorkspaceCleanup,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            path: default_workspace_path(),
            state_dir: default_workspace_state_dir(),
            cleanup: WorkspaceCleanup::Never,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfigInput {
    pub path: Option<String>,
    pub state_dir: Option<String>,
    pub cleanup: Option<WorkspaceCleanup>,
}

impl WorkspaceConfigInput {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(path) = &later.path {
            self.path = Some(path.clone());
        }
        if let Some(state_dir) = &later.state_dir {
            self.state_dir = Some(state_dir.clone());
        }
        if let Some(cleanup) = later.cleanup {
            self.cleanup = Some(cleanup);
        }
        self
    }

    pub(super) fn into_effective(self) -> WorkspaceConfig {
        WorkspaceConfig {
            path: self.path.unwrap_or_else(default_workspace_path),
            state_dir: self.state_dir.unwrap_or_else(default_workspace_state_dir),
            cleanup: self.cleanup.unwrap_or_default(),
        }
    }

    pub(super) fn validate_partial(&self, target_name: &str) -> anyhow::Result<()> {
        if let Some(path) = &self.path {
            if path.trim().is_empty() {
                anyhow::bail!("target {target_name:?} workspace.path must not be empty");
            }
            validate_template(
                "target.workspace.path",
                path,
                TARGET_WORKSPACE_TEMPLATE_VARS,
            )?;
        }
        if let Some(state_dir) = &self.state_dir {
            if state_dir.trim().is_empty() {
                anyhow::bail!("target {target_name:?} workspace.state_dir must not be empty");
            }
            validate_template(
                "target.workspace.state_dir",
                state_dir,
                GATEWAY_TEMPLATE_VARS_NO_PID,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSocketsConfig {
    #[serde(default = "default_control_socket_host_dir")]
    pub host_dir: String,
    #[serde(default = "default_control_socket_container_dir")]
    pub container_dir: String,
}

impl Default for ControlSocketsConfig {
    fn default() -> Self {
        Self {
            host_dir: default_control_socket_host_dir(),
            container_dir: default_control_socket_container_dir(),
        }
    }
}

impl ControlSocketsConfig {
    fn validate(&self, field: &str) -> anyhow::Result<()> {
        if self.host_dir.trim().is_empty() {
            anyhow::bail!("{field}.host_dir must not be empty");
        }
        if self.container_dir.trim().is_empty() {
            anyhow::bail!("{field}.container_dir must not be empty");
        }
        validate_template(
            &format!("{field}.host_dir"),
            &self.host_dir,
            CONTROL_SOCKET_TEMPLATE_VARS,
        )?;
        validate_template(
            &format!("{field}.container_dir"),
            &self.container_dir,
            CONTROL_SOCKET_TEMPLATE_VARS,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfigInput {
    #[serde(default, rename = "use")]
    pub use_templates: Vec<String>,
    pub image: Option<String>,
    pub mode: Option<TargetMode>,
    pub name: Option<String>,
    pub ephemeral_name: Option<String>,
    pub workspace: Option<WorkspaceConfigInput>,
    pub runtime: Option<TargetRuntimeConfigInput>,
    pub identity: Option<TargetIdentityConfig>,
    pub container_user: Option<String>,
    pub container_home: Option<PathBuf>,
    #[serde(default)]
    pub container_env: BTreeMap<String, String>,
    #[serde(default)]
    pub session_env: BTreeMap<String, String>,
    #[serde(default)]
    pub container_mounts: Vec<ContainerMountConfig>,
    pub stop_when_idle: Option<bool>,
    pub remove_on_stop: Option<bool>,
    pub idle_cleanup: Option<IdleCleanupConfigInput>,
    pub local_ssh: Option<LocalSshConfigInput>,
    pub container_ssh: Option<TargetContainerSshConfig>,
    pub control_sockets: Option<TargetControlSocketsConfig>,
    pub container_bootstrap: Option<TargetContainerBootstrapConfig>,
    #[serde(default)]
    pub lifecycle_steps: Vec<RawLifecycleStep>,
    #[serde(default)]
    pub host_steps: Vec<RawHostStep>,
    #[serde(default)]
    pub container_bootstrap_steps: Vec<RawContainerBootstrapStep>,
    pub container_agent: Option<ContainerAgentConfigInput>,
}

impl TargetConfigInput {
    pub(super) fn builtin_defaults() -> Self {
        Self {
            mode: Some(TargetMode::Fixed),
            workspace: Some(WorkspaceConfigInput {
                path: Some(default_workspace_path()),
                state_dir: Some(default_workspace_state_dir()),
                cleanup: Some(WorkspaceCleanup::Never),
            }),
            runtime: Some(TargetRuntimeConfigInput {
                extra_run_args: Some(Vec::new()),
            }),
            stop_when_idle: Some(false),
            remove_on_stop: Some(false),
            control_sockets: Some(TargetControlSocketsConfig {
                host_dir: Some(default_control_socket_host_dir()),
                container_dir: Some(default_control_socket_container_dir()),
            }),
            container_bootstrap: Some(TargetContainerBootstrapConfig {
                enabled: Some(false),
                entrypoint: Some(default_bootstrap_entrypoint()),
                agent_program: Some(default_bootstrap_agent_program()),
            }),
            container_agent: Some(ContainerAgentConfigInput {
                enabled: Some(true),
                services: Vec::new(),
                ssh_bridge: None,
                control_socket: None,
                idle_cleanup: None,
            }),
            container_ssh: Some(TargetContainerSshConfig {
                transfer: Some(TargetContainerSshTransferConfig {
                    sftp: Some(SftpTransferMode::Allow),
                    legacy_scp: Some(LegacyScpTransferMode::Allow),
                }),
            }),
            ..Self::default()
        }
    }

    pub(super) fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
        if let Some(image) = &later.image {
            self.image = Some(image.clone());
        }
        if let Some(mode) = later.mode {
            self.mode = Some(mode);
        }
        if let Some(name) = &later.name {
            self.name = Some(name.clone());
        }
        if let Some(ephemeral_name) = &later.ephemeral_name {
            self.ephemeral_name = Some(ephemeral_name.clone());
        }
        if let Some(workspace) = &later.workspace {
            self.workspace = Some(self.workspace.take().unwrap_or_default().overlay(workspace));
        }
        if let Some(runtime) = &later.runtime {
            self.runtime = Some(self.runtime.take().unwrap_or_default().overlay(runtime));
        }
        if let Some(identity) = &later.identity {
            self.identity = Some(overlay_identity(self.identity.take(), identity));
        }
        if let Some(container_user) = &later.container_user {
            self.container_user = Some(container_user.clone());
        }
        if let Some(container_home) = &later.container_home {
            self.container_home = Some(container_home.clone());
        }
        self.container_env.extend(later.container_env.clone());
        self.session_env.extend(later.session_env.clone());
        self.container_mounts.extend(later.container_mounts.clone());
        if let Some(stop_when_idle) = later.stop_when_idle {
            self.stop_when_idle = Some(stop_when_idle);
        }
        if let Some(remove_on_stop) = later.remove_on_stop {
            self.remove_on_stop = Some(remove_on_stop);
        }
        if let Some(idle_cleanup) = &later.idle_cleanup {
            self.idle_cleanup = Some(
                self.idle_cleanup
                    .take()
                    .unwrap_or_default()
                    .overlay(idle_cleanup),
            );
        }
        if let Some(local_ssh) = &later.local_ssh {
            self.local_ssh = Some(self.local_ssh.take().unwrap_or_default().overlay(local_ssh));
        }
        if let Some(container_ssh) = &later.container_ssh {
            self.container_ssh = Some(overlay_target_container_ssh(
                self.container_ssh.take(),
                container_ssh,
            ));
        }
        if let Some(control_sockets) = &later.control_sockets {
            self.control_sockets = Some(overlay_control_sockets(
                self.control_sockets.take(),
                control_sockets,
            ));
        }
        if let Some(container_bootstrap) = &later.container_bootstrap {
            self.container_bootstrap = Some(overlay_container_bootstrap(
                self.container_bootstrap.take(),
                container_bootstrap,
            ));
        }
        self.lifecycle_steps = merge_target_step_patches(
            "lifecycle_steps",
            self.lifecycle_steps
                .iter()
                .map(RawLifecycleStep::to_effective_without_inherited)
                .collect::<anyhow::Result<Vec<_>>>()?,
            &later.lifecycle_steps,
            |step| LifecycleStepKey {
                phase: Some(step.phase),
                name: step.name.clone(),
            },
            |step| LifecycleStepKey {
                phase: Some(step.phase),
                name: step.name.clone(),
            },
            RawLifecycleStep::to_effective,
        )?
        .into_iter()
        .map(RawLifecycleStep::from_effective)
        .collect();
        self.host_steps = merge_target_step_patches(
            "host_steps",
            self.host_steps
                .iter()
                .map(RawHostStep::to_effective_without_inherited)
                .collect::<anyhow::Result<Vec<_>>>()?,
            &later.host_steps,
            |step| StepKey {
                name: step.name.clone(),
            },
            |step| StepKey {
                name: step.name.clone(),
            },
            RawHostStep::to_effective,
        )?
        .into_iter()
        .map(RawHostStep::from_effective)
        .collect();
        self.container_bootstrap_steps = merge_target_step_patches(
            "container_bootstrap_steps",
            self.container_bootstrap_steps
                .iter()
                .map(RawContainerBootstrapStep::to_effective)
                .collect::<anyhow::Result<Vec<_>>>()?,
            &later.container_bootstrap_steps,
            |step| StepKey {
                name: step.name.clone(),
            },
            |step| StepKey {
                name: step.name.clone(),
            },
            |step, _| RawContainerBootstrapStep::to_effective(step),
        )?
        .into_iter()
        .map(RawContainerBootstrapStep::from_effective)
        .collect();
        if let Some(container_agent) = &later.container_agent {
            self.container_agent = Some(
                self.container_agent
                    .take()
                    .unwrap_or_default()
                    .overlay(container_agent)?,
            );
        }
        Ok(self)
    }

    pub(super) fn validate_partial(&self, target_name: &str) -> anyhow::Result<()> {
        for template in &self.use_templates {
            validate_name("target template reference", template)?;
        }
        if let Some(image) = &self.image
            && image.trim().is_empty()
        {
            anyhow::bail!("target {target_name:?} image is required");
        }
        if let Some(name) = &self.name {
            validate_template("target.name", name, &["image_slug"])?;
        }
        if let Some(ephemeral_name) = &self.ephemeral_name {
            validate_template(
                "target.ephemeral_name",
                ephemeral_name,
                &["image_slug", "session_id"],
            )?;
        }
        if let Some(workspace) = &self.workspace {
            workspace.validate_partial(target_name)?;
        }
        if let Some(runtime) = &self.runtime {
            runtime.validate_partial(target_name)?;
        }
        if let Some(identity) = &self.identity {
            identity.validate_partial(target_name)?;
        }
        if let Some(container_user) = &self.container_user {
            validate_name("target.container_user", container_user)?;
        }
        if let Some(container_home) = &self.container_home {
            validate_container_home(target_name, container_home)?;
        }
        validate_env_map("target.container_env", &self.container_env)?;
        validate_env_map("target.session_env", &self.session_env)?;
        for mount in &self.container_mounts {
            mount.validate()?;
        }
        if let Some(idle_cleanup) = &self.idle_cleanup {
            idle_cleanup.clone().into_effective()?;
        }
        if let Some(local_ssh) = &self.local_ssh {
            local_ssh.validate_partial()?;
        }
        validate_raw_target_steps(
            target_name,
            "lifecycle_steps",
            &self.lifecycle_steps,
            |step| LifecycleStepKey {
                phase: Some(step.phase),
                name: step.name.clone(),
            },
        )?;
        validate_raw_target_steps(target_name, "host_steps", &self.host_steps, |step| {
            StepKey {
                name: step.name.clone(),
            }
        })?;
        validate_raw_target_steps(
            target_name,
            "container_bootstrap_steps",
            &self.container_bootstrap_steps,
            |step| StepKey {
                name: step.name.clone(),
            },
        )?;
        if let Some(control_sockets) = &self.control_sockets {
            control_sockets.validate(target_name)?;
        }
        if let Some(container_bootstrap) = &self.container_bootstrap {
            container_bootstrap.validate(target_name)?;
        }
        if let Some(container_agent) = &self.container_agent {
            container_agent.validate_partial()?;
        }
        Ok(())
    }

    pub(super) fn into_effective(self, target_name: &str) -> anyhow::Result<TargetConfig> {
        let lifecycle_steps = self
            .lifecycle_steps
            .iter()
            .map(RawLifecycleStep::to_effective_without_inherited)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let host_steps = self
            .host_steps
            .iter()
            .map(RawHostStep::to_effective_without_inherited)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let container_bootstrap_steps = self
            .container_bootstrap_steps
            .iter()
            .map(RawContainerBootstrapStep::to_effective)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let container_agent = self.container_agent.unwrap_or_default().into_effective()?;
        Ok(TargetConfig {
            image: self.image.ok_or_else(|| {
                anyhow::anyhow!("target {target_name:?} image is required after defaults")
            })?,
            mode: self.mode.unwrap_or_default(),
            name: self.name,
            ephemeral_name: self.ephemeral_name,
            workspace: self.workspace.unwrap_or_default().into_effective(),
            runtime: self.runtime.unwrap_or_default().into_effective(),
            identity: self.identity,
            container_user: self.container_user,
            container_home: self.container_home,
            container_env: self.container_env,
            session_env: self.session_env,
            container_mounts: self.container_mounts,
            stop_when_idle: self.stop_when_idle.unwrap_or(false),
            remove_on_stop: self.remove_on_stop.unwrap_or(false),
            idle_cleanup: self
                .idle_cleanup
                .map(IdleCleanupConfigInput::into_effective)
                .transpose()?,
            local_ssh: self.local_ssh.map(LocalSshConfigInput::into_effective),
            container_ssh: self
                .container_ssh
                .unwrap_or_default()
                .to_effective_config()?,
            control_sockets: self.control_sockets.unwrap_or_default().to_effective(),
            container_bootstrap: self
                .container_bootstrap
                .unwrap_or_default()
                .to_effective_config(),
            lifecycle_steps,
            host_steps,
            container_bootstrap_steps,
            container_agent,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub image: String,
    pub mode: TargetMode,
    pub name: Option<String>,
    pub ephemeral_name: Option<String>,
    pub workspace: WorkspaceConfig,
    pub runtime: TargetRuntimeConfig,
    pub identity: Option<TargetIdentityConfig>,
    pub container_user: Option<String>,
    pub container_home: Option<PathBuf>,
    #[serde(default)]
    pub container_env: BTreeMap<String, String>,
    #[serde(default)]
    pub session_env: BTreeMap<String, String>,
    #[serde(default)]
    pub container_mounts: Vec<ContainerMountConfig>,
    pub stop_when_idle: bool,
    pub remove_on_stop: bool,
    pub idle_cleanup: Option<IdleCleanupConfig>,
    pub local_ssh: Option<LocalSshConfig>,
    pub container_ssh: ContainerSshConfig,
    pub control_sockets: ControlSocketsConfig,
    pub container_bootstrap: ContainerBootstrapConfig,
    pub lifecycle_steps: Vec<LifecycleStep>,
    pub host_steps: Vec<HostStep>,
    pub container_bootstrap_steps: Vec<ContainerBootstrapStep>,
    pub container_agent: ContainerAgentConfig,
}

impl TargetConfig {
    pub fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        if self.image.trim().is_empty() {
            anyhow::bail!("target {target_name:?} image is required");
        }
        if let Some(container_user) = &self.container_user {
            validate_name("target.container_user", container_user)?;
        }
        if let Some(container_home) = &self.container_home {
            validate_container_home(target_name, container_home)?;
        }
        if let Some(identity) = &self.identity {
            identity.validate(target_name)?;
        }
        if self.workspace.path.trim().is_empty() {
            anyhow::bail!("target {target_name:?} workspace.path must not be empty");
        }
        validate_template(
            "target.workspace.path",
            &self.workspace.path,
            TARGET_WORKSPACE_TEMPLATE_VARS,
        )?;
        if self.workspace.state_dir.trim().is_empty() {
            anyhow::bail!("target {target_name:?} workspace.state_dir must not be empty");
        }
        validate_template(
            "target.workspace.state_dir",
            &self.workspace.state_dir,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        self.runtime.validate(target_name)?;
        validate_env_map("target.container_env", &self.container_env)?;
        validate_env_map("target.session_env", &self.session_env)?;
        for mount in &self.container_mounts {
            mount.validate()?;
        }
        match self.mode {
            TargetMode::Fixed => {
                if let Some(name) = &self.name {
                    validate_template("target.name", name, &["image_slug"])?;
                } else {
                    anyhow::bail!("fixed target {target_name:?} requires name");
                }
            }
            TargetMode::Ephemeral => {
                if let Some(name) = &self.ephemeral_name {
                    validate_template(
                        "target.ephemeral_name",
                        name,
                        &["image_slug", "session_id"],
                    )?;
                } else {
                    anyhow::bail!("ephemeral target {target_name:?} requires ephemeral_name");
                }
                if !self.stop_when_idle {
                    anyhow::bail!(
                        "ephemeral target {target_name:?} requires stop_when_idle = true"
                    );
                }
            }
        }
        if self.workspace.cleanup != WorkspaceCleanup::Never {
            if self.mode != TargetMode::Ephemeral {
                anyhow::bail!(
                    "target {target_name:?} workspace.cleanup requires mode = \"ephemeral\""
                );
            }
            let refs = template::referenced_keys(&self.workspace.path)?;
            if !refs.contains(&"session_id") {
                anyhow::bail!(
                    "target {target_name:?} workspace.cleanup requires workspace.path to reference {{session_id}}"
                );
            }
            if !Path::new(&self.workspace.path)
                .components()
                .any(|component| component.as_os_str() == "aw-gateway")
            {
                anyhow::bail!(
                    "target {target_name:?} workspace.cleanup requires workspace.path under an aw-gateway path component"
                );
            }
            let Some(cleanup) = &self.idle_cleanup else {
                anyhow::bail!(
                    "target {target_name:?} workspace.cleanup requires gateway-owned idle_cleanup"
                );
            };
            if cleanup.owner != IdleCleanupOwner::Gateway
                || cleanup.action != IdleCleanupAction::ExitContainer
            {
                anyhow::bail!(
                    "target {target_name:?} workspace.cleanup requires gateway-owned exit_container idle_cleanup"
                );
            }
            if !cleanup.preserve_processes.is_empty() {
                anyhow::bail!(
                    "target {target_name:?} workspace.cleanup does not support preserve_processes"
                );
            }
        }
        if let Some(cleanup) = &self.idle_cleanup {
            cleanup.validate()?;
            if cleanup.owner == IdleCleanupOwner::Gateway
                && cleanup.action == IdleCleanupAction::ReapProcesses
            {
                anyhow::bail!(
                    "target {target_name:?} gateway-owned idle cleanup does not support reap_processes"
                );
            }
        }
        if let Some(local_ssh) = &self.local_ssh {
            local_ssh.validate()?;
        }
        self.container_ssh.validate()?;
        self.control_sockets.validate("target.control_sockets")?;
        self.container_bootstrap.validate()?;
        for step in &self.lifecycle_steps {
            step.validate("target.lifecycle_steps")?;
        }
        for step in &self.host_steps {
            step.validate("target.host_steps")?;
        }
        for step in &self.container_bootstrap_steps {
            step.validate()?;
        }
        self.container_agent.validate_gateway()?;
        Ok(())
    }

    pub fn container_name(&self, session_id: Option<&str>) -> anyhow::Result<String> {
        let mut vars = BTreeMap::new();
        vars.insert("image_slug".into(), template::image_slug(&self.image));
        if let Some(session_id) = session_id {
            vars.insert("session_id".into(), session_id.to_string());
        }
        let pattern = match self.mode {
            TargetMode::Fixed => self.name.as_deref().unwrap_or("{image_slug}"),
            TargetMode::Ephemeral => self
                .ephemeral_name
                .as_deref()
                .unwrap_or("{image_slug}-{session_id}"),
        };
        let rendered = template::render(pattern, &vars)?;
        validate_container_name(&rendered)?;
        Ok(rendered)
    }
}

fn validate_container_home(target_name: &str, container_home: &Path) -> anyhow::Result<()> {
    let value = container_home.display().to_string();
    validate_template("target.container_home", &value, IDENTITY_TEMPLATE_VARS)?;

    let mut vars = template::Vars::new();
    vars.insert("user".into(), "user".into());
    vars.insert("uid".into(), "1000".into());
    vars.insert("gid".into(), "1000".into());
    vars.insert("home".into(), "/home/user".into());
    let rendered = template::render(&value, &vars)?;
    if !Path::new(&rendered).is_absolute() {
        anyhow::bail!("target {target_name:?} container_home must render to an absolute path");
    }

    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetControlSocketsConfig {
    pub host_dir: Option<String>,
    pub container_dir: Option<String>,
}

impl TargetControlSocketsConfig {
    fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        if let Some(host_dir) = &self.host_dir {
            if host_dir.trim().is_empty() {
                anyhow::bail!("target {target_name:?} control_sockets.host_dir must not be empty");
            }
            validate_template(
                "target.control_sockets.host_dir",
                host_dir,
                CONTROL_SOCKET_TEMPLATE_VARS,
            )?;
        }
        if let Some(container_dir) = &self.container_dir {
            if container_dir.trim().is_empty() {
                anyhow::bail!(
                    "target {target_name:?} control_sockets.container_dir must not be empty"
                );
            }
            validate_template(
                "target.control_sockets.container_dir",
                container_dir,
                CONTROL_SOCKET_TEMPLATE_VARS,
            )?;
        }
        Ok(())
    }

    pub(super) fn overlay(mut self, later: &Self) -> Self {
        if let Some(host_dir) = &later.host_dir {
            self.host_dir = Some(host_dir.clone());
        }
        if let Some(container_dir) = &later.container_dir {
            self.container_dir = Some(container_dir.clone());
        }
        self
    }

    fn to_effective(&self) -> ControlSocketsConfig {
        ControlSocketsConfig {
            host_dir: self
                .host_dir
                .clone()
                .unwrap_or_else(default_control_socket_host_dir),
            container_dir: self
                .container_dir
                .clone()
                .unwrap_or_else(default_control_socket_container_dir),
        }
    }
}

fn overlay_control_sockets(
    current: Option<TargetControlSocketsConfig>,
    later: &TargetControlSocketsConfig,
) -> TargetControlSocketsConfig {
    current.unwrap_or_default().overlay(later)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerBootstrapConfig {
    pub enabled: Option<bool>,
    pub entrypoint: Option<String>,
    pub agent_program: Option<String>,
}

impl TargetContainerBootstrapConfig {
    fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        if let Some(entrypoint) = &self.entrypoint {
            if entrypoint.trim().is_empty() {
                anyhow::bail!(
                    "target {target_name:?} container_bootstrap.entrypoint must not be empty"
                );
            }
            validate_template(
                "target.container_bootstrap.entrypoint",
                entrypoint,
                GATEWAY_TEMPLATE_VARS_NO_PID,
            )?;
        }
        if let Some(agent_program) = &self.agent_program {
            if agent_program.trim().is_empty() {
                anyhow::bail!(
                    "target {target_name:?} container_bootstrap.agent_program must not be empty"
                );
            }
            validate_template(
                "target.container_bootstrap.agent_program",
                agent_program,
                GATEWAY_TEMPLATE_VARS_NO_PID,
            )?;
        }
        Ok(())
    }

    fn overlay(mut self, later: &Self) -> Self {
        if let Some(enabled) = later.enabled {
            self.enabled = Some(enabled);
        }
        if let Some(entrypoint) = &later.entrypoint {
            self.entrypoint = Some(entrypoint.clone());
        }
        if let Some(agent_program) = &later.agent_program {
            self.agent_program = Some(agent_program.clone());
        }
        self
    }

    fn to_effective_config(&self) -> ContainerBootstrapConfig {
        ContainerBootstrapConfig {
            enabled: self.enabled.unwrap_or(false),
            entrypoint: self
                .entrypoint
                .clone()
                .unwrap_or_else(default_bootstrap_entrypoint),
            agent_program: self
                .agent_program
                .clone()
                .unwrap_or_else(default_bootstrap_agent_program),
        }
    }
}

fn overlay_container_bootstrap(
    current: Option<TargetContainerBootstrapConfig>,
    later: &TargetContainerBootstrapConfig,
) -> TargetContainerBootstrapConfig {
    current.unwrap_or_default().overlay(later)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetRuntimeConfig {
    #[serde(default)]
    pub extra_run_args: Vec<String>,
}

impl TargetRuntimeConfig {
    fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        for arg in &self.extra_run_args {
            if arg.is_empty() {
                anyhow::bail!(
                    "target {target_name:?} runtime.extra_run_args must not contain empty arguments"
                );
            }
            validate_template(
                "target.runtime.extra_run_args",
                arg,
                GATEWAY_TEMPLATE_VARS_NO_PID,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetRuntimeConfigInput {
    pub extra_run_args: Option<Vec<String>>,
}

impl TargetRuntimeConfigInput {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(extra_run_args) = &later.extra_run_args {
            self.extra_run_args = Some(extra_run_args.clone());
        }
        self
    }

    fn into_effective(self) -> TargetRuntimeConfig {
        TargetRuntimeConfig {
            extra_run_args: self.extra_run_args.unwrap_or_default(),
        }
    }

    fn validate_partial(&self, target_name: &str) -> anyhow::Result<()> {
        if let Some(extra_run_args) = &self.extra_run_args {
            TargetRuntimeConfig {
                extra_run_args: extra_run_args.clone(),
            }
            .validate(target_name)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentityConfig {
    pub bootstrap_user: Option<String>,
    pub session_user: Option<String>,
    pub session_uid: Option<String>,
    pub session_gid: Option<String>,
    pub session_home: Option<String>,
    pub session_shell: Option<String>,
}

impl TargetIdentityConfig {
    fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        self.validate_partial(target_name)?;
        if let Some(session_user) = &self.session_user
            && !session_user.contains('{')
            && (self.session_uid.is_none() || self.session_gid.is_none())
        {
            anyhow::bail!(
                "target {target_name:?} identity with literal session_user requires explicit session_uid and session_gid"
            );
        }
        Ok(())
    }

    fn validate_partial(&self, target_name: &str) -> anyhow::Result<()> {
        for (field, value) in [
            ("bootstrap_user", &self.bootstrap_user),
            ("session_user", &self.session_user),
            ("session_uid", &self.session_uid),
            ("session_gid", &self.session_gid),
            ("session_home", &self.session_home),
            ("session_shell", &self.session_shell),
        ]
        .into_iter()
        .filter_map(|(field, value)| value.as_ref().map(|value| (field, value)))
        {
            if value.trim().is_empty() {
                anyhow::bail!("target {target_name:?} identity.{field} must not be empty");
            }
            validate_template(
                &format!("target.identity.{field}"),
                value,
                IDENTITY_TEMPLATE_VARS,
            )?;
        }
        Ok(())
    }
}

fn overlay_identity(
    current: Option<TargetIdentityConfig>,
    later: &TargetIdentityConfig,
) -> TargetIdentityConfig {
    let mut identity = current.unwrap_or_default();
    if let Some(value) = &later.bootstrap_user {
        identity.bootstrap_user = Some(value.clone());
    }
    if let Some(value) = &later.session_user {
        identity.session_user = Some(value.clone());
    }
    if let Some(value) = &later.session_uid {
        identity.session_uid = Some(value.clone());
    }
    if let Some(value) = &later.session_gid {
        identity.session_gid = Some(value.clone());
    }
    if let Some(value) = &later.session_home {
        identity.session_home = Some(value.clone());
    }
    if let Some(value) = &later.session_shell {
        identity.session_shell = Some(value.clone());
    }
    identity
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode {
    #[default]
    Fixed,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCleanup {
    #[default]
    Never,
    Success,
    Always,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerMountConfig {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub mode: ContainerMountMode,
}

impl ContainerMountConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.source.trim().is_empty() {
            anyhow::bail!("container_mounts source must not be empty");
        }
        if self.target.trim().is_empty() {
            anyhow::bail!("container_mounts target must not be empty");
        }
        validate_template(
            "container_mounts.source",
            &self.source,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        validate_template(
            "container_mounts.target",
            &self.target,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerMountMode {
    #[default]
    Ro,
    Rw,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerBootstrapConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_bootstrap_entrypoint")]
    pub entrypoint: String,
    #[serde(default = "default_bootstrap_agent_program")]
    pub agent_program: String,
}

impl Default for ContainerBootstrapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            entrypoint: default_bootstrap_entrypoint(),
            agent_program: default_bootstrap_agent_program(),
        }
    }
}

impl ContainerBootstrapConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.entrypoint.trim().is_empty() {
            anyhow::bail!("container_bootstrap.entrypoint must not be empty");
        }
        if self.agent_program.trim().is_empty() {
            anyhow::bail!("container_bootstrap.agent_program must not be empty");
        }
        validate_template(
            "container_bootstrap.entrypoint",
            &self.entrypoint,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        validate_template(
            "container_bootstrap.agent_program",
            &self.agent_program,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        Ok(())
    }
}
