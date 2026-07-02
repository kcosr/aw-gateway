use crate::action;
use crate::context::{ContextVarConfig, validate_context_var_declarations};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

mod agent;
mod http;
mod include;
mod launch;
mod resolver;
mod root;
mod steps;
mod target;
mod validation;

pub use agent::{
    ContainerAgentConfig, ContainerAgentConfigInput, ControlSocketConfig, EnvValue, HealthCheck,
    RestartPolicy, ServiceConfig, SshBridgeConfig, SshBridgeConfigInput,
};
pub use http::{HttpAuthConfig, HttpAuthType, HttpConfig};
pub(crate) use launch::validate_launch_var_string_value;
pub use launch::{
    LaunchConfig, LaunchConfigInput, LaunchStep, LaunchStepLocation, LaunchStepPhase,
    LaunchVarConfig, LaunchVarType, LaunchVarValue,
};
use resolver::{
    TemplateChainResolver, launch_template_dependencies, overlay_launch_template,
    overlay_target_template, target_template_dependencies,
};
pub use steps::{
    ContainerBootstrapStep, HostStep, LifecyclePhase, LifecycleStep, RawContainerBootstrapStep,
    RawHostStep, RawLifecycleStep, RenderedContainerBootstrapStep,
};
pub(crate) use target::DEFAULT_EPHEMERAL_NAME_PATTERN;
pub use target::{
    ContainerBootstrapConfig, ContainerMountConfig, ContainerMountMode, ContainerSshConfig,
    ContainerSshTransferConfig, ControlSocketsConfig, IdleCleanupAction, IdleCleanupConfig,
    IdleCleanupConfigInput, IdleCleanupOwner, LegacyScpTransferMode, LocalSshBackend,
    LocalSshConfig, LocalSshConfigInput, LocalSshMode, LocalSshReadiness, SftpTransferMode,
    TargetAccessConfig, TargetAccessConfigInput, TargetAccessMethod, TargetConfig,
    TargetConfigInput, TargetContainerBootstrapConfig, TargetContainerSshConfig,
    TargetContainerSshTransferConfig, TargetControlSocketsConfig, TargetIdentityConfig, TargetMode,
    TargetRuntimeConfig, TargetRuntimeConfigInput, WorkspaceCleanup, WorkspaceConfig,
    WorkspaceConfigInput,
};
pub(crate) use validation::SERVICE_USER_TEMPLATE;
pub use validation::parse_duration;
use validation::*;
pub(crate) use validation::{
    canonical_number_string, default_control_socket_host_dir, validate_name, validate_passwd_scalar,
};

pub const GATEWAY_SCHEMA_VERSION: &str = "1";
pub const AGENT_SCHEMA_VERSION: &str = "1";
pub const BOOTSTRAP_SCHEMA_VERSION: &str = "2";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub schema_version: String,
    #[serde(default = "default_target")]
    pub default_target: String,
    #[serde(default)]
    pub includes: Vec<String>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub ssh_dispatch: SshDispatchConfig,
    #[serde(default)]
    pub client_config: ClientConfig,
    #[serde(default)]
    pub context_vars: BTreeMap<String, ContextVarConfig>,
    #[serde(default)]
    pub target_defaults: TargetConfigInput,
    #[serde(default)]
    pub target_templates: BTreeMap<String, TargetConfigInput>,
    #[serde(default)]
    pub launch_defaults: LaunchConfigInput,
    #[serde(default)]
    pub launch_templates: BTreeMap<String, LaunchConfigInput>,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetConfigInput>,
    #[serde(default)]
    pub launches: BTreeMap<String, LaunchConfigInput>,
}

impl GatewayConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let value = root::load_gateway_root(path)?;
        let cfg: Self = value
            .try_into()
            .with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != GATEWAY_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported gateway schema_version {:?}; expected {:?}",
                self.schema_version,
                GATEWAY_SCHEMA_VERSION
            );
        }
        validate_context_var_declarations(&self.context_vars)?;
        reject_template_use("target_defaults", &self.target_defaults.use_templates)?;
        self.target_defaults.validate_partial("target_defaults")?;
        for (name, template) in &self.target_templates {
            validate_name("target template", name)?;
            template.validate_partial(name)?;
        }
        self.validate_target_template_references()?;
        reject_template_use("launch_defaults", &self.launch_defaults.use_templates)?;
        self.launch_defaults.validate_partial("launch_defaults")?;
        for (name, template) in &self.launch_templates {
            validate_name("launch template", name)?;
            template.validate_partial(name)?;
        }
        self.validate_launch_template_references()?;
        validate_name("default_target", &self.default_target)?;
        if !self.targets.contains_key(&self.default_target) {
            anyhow::bail!(
                "default_target {:?} is not defined in targets",
                self.default_target
            );
        }
        if self.targets.is_empty() {
            anyhow::bail!("at least one target is required");
        }
        for (name, target) in &self.targets {
            validate_name("target", name)?;
            target.validate_partial(name)?;
        }
        self.runtime.validate()?;
        validate_template(
            "client_config.inner_alias_template",
            &self.client_config.inner_alias_template,
            CLIENT_TEMPLATE_VARS,
        )?;
        validate_template(
            "client_config.container_host_template",
            &self.client_config.container_host_template,
            CLIENT_TEMPLATE_VARS,
        )?;
        validate_template(
            "client_config.default_identity_dir",
            &self.client_config.default_identity_dir,
            CLIENT_TEMPLATE_VARS,
        )?;
        validate_ssh_config_scalar("client_config.host", &self.client_config.host)?;
        validate_ssh_config_scalar(
            "client_config.gateway_path",
            &self.client_config.gateway_path,
        )?;
        if let Some(directory) = &self.logging.directory {
            self.validate_context_template(
                "logging.directory",
                directory,
                GATEWAY_LOGGING_TEMPLATE_VARS,
            )?;
        }
        self.logging.validate_values("logging")?;
        self.http.validate()?;
        let effective_targets = self.effective_targets()?;
        self.validate_target_context_templates(&effective_targets)?;
        self.validate_required_context_fixed_targets(&effective_targets)?;
        self.validate_launch_definitions(&effective_targets)?;
        self.validate_target_agent_compatibility(&effective_targets)?;
        self.validate_runtime_target_compatibility(&effective_targets)?;
        self.ssh_dispatch.validate()?;
        Ok(())
    }

    fn validate_context_template(
        &self,
        field: &str,
        value: &str,
        allowed: &[&str],
    ) -> anyhow::Result<()> {
        validate_template_with_context(field, value, allowed, self.context_vars.keys())
    }

    fn validate_target_context_templates(
        &self,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<()> {
        self.target_defaults
            .validate_context_templates("target_defaults", &self.context_vars)?;
        for (name, template) in &self.target_templates {
            template.validate_context_templates(
                &format!("target_templates.{name}"),
                &self.context_vars,
            )?;
        }
        for (name, target) in targets {
            target.validate_context_templates(name, &self.context_vars)?;
        }
        for (name, target) in &self.targets {
            target.validate_context_templates(&format!("targets.{name}"), &self.context_vars)?;
        }
        Ok(())
    }

    fn validate_required_context_fixed_targets(
        &self,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<()> {
        let required: Vec<&str> = self
            .context_vars
            .iter()
            .filter_map(|(key, cfg)| cfg.required.then_some(key.as_str()))
            .collect();
        if required.is_empty() {
            return Ok(());
        }
        for (name, target) in targets {
            if target.mode != TargetMode::Fixed {
                continue;
            }
            let pattern = target.name.as_deref().unwrap_or("{image_slug}");
            let refs = crate::template::referenced_keys(pattern)?;
            for key in &required {
                let context_ref = format!("context.{key}");
                if !refs.iter().any(|reference| reference == &context_ref) {
                    anyhow::bail!(
                        "fixed target {name:?} name must reference required context key {{{context_ref}}}"
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_launch_definitions(
        &self,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<()> {
        for (name, launch) in &self.launches {
            validate_name("launch", name)?;
            if name == "show" {
                anyhow::bail!("launch name \"show\" is reserved for launch show");
            }
            launch.validate_partial(name)?;
            self.resolve_effective_launch(name, launch, targets)
                .with_context(|| format!("validate effective launch {name:?}"))?;
        }
        Ok(())
    }

    fn validate_target_agent_compatibility(
        &self,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<()> {
        for (name, target) in targets {
            if target.container_agent.enabled {
                continue;
            }
            if target.access.method != TargetAccessMethod::Ssh {
                continue;
            }
            if target
                .local_ssh
                .as_ref()
                .is_some_and(|local_ssh| local_ssh.backend == LocalSshBackend::PublishedPort)
            {
                anyhow::bail!(
                    "target {name:?} uses local_ssh.backend = \"published_port\" but container_agent.enabled = false; agent-disabled targets run sleep infinity and do not provide container SSH"
                );
            }
        }
        Ok(())
    }

    fn validate_runtime_target_compatibility(
        &self,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<()> {
        if self.runtime.runtime_type != ContainerRuntimeType::AppleContainer {
            return Ok(());
        }
        for (name, target) in targets {
            match target.access.method {
                TargetAccessMethod::Ssh => {
                    match &target.local_ssh {
                        Some(local_ssh) if local_ssh.backend == LocalSshBackend::PublishedPort => {
                            if local_ssh.readiness != LocalSshReadiness::SshOnly {
                                anyhow::bail!(
                                    "target {name:?} uses runtime type \"apple_container\" with access.method = \"ssh\" but local_ssh.readiness is not \"ssh_only\"; Apple container SSH support requires local_ssh.readiness = \"ssh_only\""
                                );
                            }
                        }
                        Some(_) => {
                            anyhow::bail!(
                                "target {name:?} uses local_ssh.backend = \"socket\" but runtime type \"apple_container\" only supports local_ssh.backend = \"published_port\" for SSH targets"
                            );
                        }
                        None => {
                            anyhow::bail!(
                                "target {name:?} must configure local_ssh.backend = \"published_port\" when runtime type is \"apple_container\" and access.method = \"ssh\""
                            );
                        }
                    }
                    if target
                        .container_agent
                        .control_socket
                        .as_ref()
                        .is_none_or(ControlSocketConfig::is_enabled)
                    {
                        anyhow::bail!(
                            "target {name:?} uses runtime type \"apple_container\" with access.method = \"ssh\" but container_agent.control_socket is enabled; Apple container SSH support requires container_agent.control_socket = false"
                        );
                    }
                }
                TargetAccessMethod::RuntimeExec => {
                    if target
                        .container_agent
                        .control_socket
                        .as_ref()
                        .is_none_or(ControlSocketConfig::is_enabled)
                    {
                        anyhow::bail!(
                            "target {name:?} uses runtime type \"apple_container\" with access.method = \"runtime_exec\" but container_agent.control_socket is enabled; Apple container runtime-exec targets require container_agent.control_socket = false"
                        );
                    }
                    if let Some(cleanup) = &target.idle_cleanup
                        && cleanup.owner == IdleCleanupOwner::Agent
                        && cleanup.action != IdleCleanupAction::None
                    {
                        anyhow::bail!(
                            "target {name:?} uses runtime type \"apple_container\" with access.method = \"runtime_exec\" but agent-owned idle_cleanup requires an agent control socket"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn effective_targets(&self) -> anyhow::Result<BTreeMap<String, TargetConfig>> {
        self.targets
            .iter()
            .map(|(name, target)| {
                self.effective_target_from_input(name, target)
                    .map(|target| (name.clone(), target))
            })
            .collect()
    }

    pub fn effective_target(&self, name: &str) -> anyhow::Result<TargetConfig> {
        let target = self
            .targets
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown target {name:?}"))?;
        self.effective_target_from_input(name, target)
    }

    fn effective_target_from_input(
        &self,
        name: &str,
        target: &TargetConfigInput,
    ) -> anyhow::Result<TargetConfig> {
        let effective_input = self.target_template_resolver().overlay_templates(
            TargetConfigInput::builtin_defaults().overlay(&self.target_defaults)?,
            &format!("target {name:?}"),
            &target.use_templates,
        )?;
        let effective = effective_input.overlay(target)?.into_effective(name)?;
        effective.validate(name)?;
        Ok(effective)
    }

    pub fn effective_workspace_defaults(&self) -> anyhow::Result<WorkspaceConfig> {
        Ok(TargetConfigInput::builtin_defaults()
            .overlay(&self.target_defaults)?
            .workspace
            .unwrap_or_default()
            .into_effective())
    }

    pub fn effective_container_ssh_defaults(&self) -> anyhow::Result<ContainerSshConfig> {
        TargetConfigInput::builtin_defaults()
            .overlay(&self.target_defaults)?
            .container_ssh
            .unwrap_or_default()
            .to_effective_config()
    }

    pub fn effective_launches(&self) -> anyhow::Result<BTreeMap<String, LaunchConfig>> {
        let targets = self.effective_targets()?;
        self.launches
            .iter()
            .map(|(name, launch)| {
                self.resolve_effective_launch(name, launch, &targets)
                    .map(|launch| (name.clone(), launch))
            })
            .collect()
    }

    pub fn effective_launch(&self, name: &str) -> anyhow::Result<LaunchConfig> {
        let targets = self.effective_targets()?;
        let launch = self
            .launches
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown launch {name:?}"))?;
        self.resolve_effective_launch(name, launch, &targets)
    }

    fn resolve_effective_launch(
        &self,
        name: &str,
        launch: &LaunchConfigInput,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<LaunchConfig> {
        let effective_input = self.launch_template_resolver().overlay_templates(
            self.launch_defaults.clone(),
            &format!("launch {name:?}"),
            &launch.use_templates,
        )?;
        let effective = effective_input.overlay(launch)?.into_effective(name)?;
        effective.validate(name, targets)?;
        Ok(effective)
    }

    fn validate_target_template_references(&self) -> anyhow::Result<()> {
        self.target_template_resolver().validate_references()
    }

    fn validate_launch_template_references(&self) -> anyhow::Result<()> {
        self.launch_template_resolver().validate_references()
    }

    fn target_template_resolver(&self) -> TemplateChainResolver<'_, TargetConfigInput> {
        TemplateChainResolver {
            kind: "target",
            templates: &self.target_templates,
            dependencies: target_template_dependencies,
            overlay: overlay_target_template,
        }
    }

    fn launch_template_resolver(&self) -> TemplateChainResolver<'_, LaunchConfigInput> {
        TemplateChainResolver {
            kind: "launch",
            templates: &self.launch_templates,
            dependencies: launch_template_dependencies,
            overlay: overlay_launch_template,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default, rename = "type")]
    pub runtime_type: ContainerRuntimeType,
    pub program: Option<String>,
    pub docker_host: Option<String>,
    pub profile: Option<String>,
}

impl RuntimeConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(program) = &self.program
            && program.trim().is_empty()
        {
            anyhow::bail!("runtime.program must not be empty");
        }
        if let Some(docker_host) = &self.docker_host {
            if self.runtime_type != ContainerRuntimeType::Docker {
                anyhow::bail!("runtime.docker_host is only valid for runtime type \"docker\"");
            }
            validate_template("runtime.docker_host", docker_host, RUNTIME_TEMPLATE_VARS)?;
        }
        if let Some(profile) = &self.profile {
            if self.runtime_type != ContainerRuntimeType::Colima {
                anyhow::bail!("runtime.profile is only valid for runtime type \"colima\"");
            }
            validate_name("runtime.profile", profile)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntimeType {
    #[default]
    Podman,
    Docker,
    Colima,
    AppleContainer,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerAgentFile {
    pub schema_version: String,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub container_agent: ContainerAgentConfig,
}

impl ContainerAgentFile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != AGENT_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported container agent schema_version {:?}; expected {:?}",
                self.schema_version,
                AGENT_SCHEMA_VERSION
            );
        }
        self.logging
            .validate_templates("logging", AGENT_TEMPLATE_VARS)?;
        self.logging.validate_values("logging")?;
        self.container_agent.validate_agent_file()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    pub directory: Option<String>,
    pub max_bytes: Option<u64>,
    pub max_files: Option<usize>,
    #[serde(default = "default_true")]
    pub console: bool,
}

pub const MAX_LOG_ROTATION_FILES: usize = 1024;

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            directory: None,
            max_bytes: None,
            max_files: None,
            console: true,
        }
    }
}

impl LoggingConfig {
    fn validate_templates(&self, field: &str, allowed: &[&str]) -> anyhow::Result<()> {
        if let Some(directory) = &self.directory {
            validate_template(&format!("{field}.directory"), directory, allowed)?;
        }
        Ok(())
    }

    fn validate_values(&self, field: &str) -> anyhow::Result<()> {
        if let Some(max_files) = self.max_files
            && max_files > MAX_LOG_ROTATION_FILES
        {
            anyhow::bail!("{field}.max_files must not exceed {MAX_LOG_ROTATION_FILES}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshDispatchConfig {
    #[serde(default = "default_true")]
    pub allow_interactive_shell: bool,
    #[serde(default = "default_true")]
    pub allow_container_commands: bool,
    #[serde(default = "default_enabled_actions")]
    pub enabled_actions: Vec<String>,
}

impl Default for SshDispatchConfig {
    fn default() -> Self {
        Self {
            allow_interactive_shell: true,
            allow_container_commands: true,
            enabled_actions: default_enabled_actions(),
        }
    }
}

impl SshDispatchConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        for enabled_action in &self.enabled_actions {
            if !action::is_gateway_action_name(enabled_action) {
                anyhow::bail!("unknown enabled_actions entry {enabled_action:?}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    #[serde(default = "default_inner_alias_template")]
    pub inner_alias_template: String,
    #[serde(default = "default_container_host_template")]
    pub container_host_template: String,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_gateway_path")]
    pub gateway_path: String,
    #[serde(default = "default_identity_dir")]
    pub default_identity_dir: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            inner_alias_template: default_inner_alias_template(),
            container_host_template: default_container_host_template(),
            host: default_host(),
            gateway_path: default_gateway_path(),
            default_identity_dir: default_identity_dir(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerBootstrapFile {
    pub schema_version: String,
    pub agent_program: String,
    pub agent_config: String,
    #[serde(default)]
    pub skip_identity_prepare: bool,
    pub chown_existing_identity_dirs: bool,
    pub identity: BootstrapIdentity,
    #[serde(default)]
    pub steps: Vec<RenderedContainerBootstrapStep>,
}

impl ContainerBootstrapFile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let schema: BootstrapSchemaOnly =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if schema.schema_version != BOOTSTRAP_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported bootstrap schema_version {:?}; expected {:?}",
                schema.schema_version,
                BOOTSTRAP_SCHEMA_VERSION
            );
        }
        let cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != BOOTSTRAP_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported bootstrap schema_version {:?}; expected {:?}",
                self.schema_version,
                BOOTSTRAP_SCHEMA_VERSION
            );
        }
        if self.agent_program.trim().is_empty() {
            anyhow::bail!("bootstrap agent_program must not be empty");
        }
        if self.agent_config.trim().is_empty() {
            anyhow::bail!("bootstrap agent_config must not be empty");
        }
        self.identity.validate()?;
        for step in &self.steps {
            step.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct BootstrapSchemaOnly {
    schema_version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapIdentity {
    pub session_user: String,
    pub session_uid: u32,
    pub session_gid: u32,
    pub session_home: String,
    pub session_shell: String,
    pub state_dir: String,
}

impl BootstrapIdentity {
    fn validate(&self) -> anyhow::Result<()> {
        validate_name("bootstrap identity session_user", &self.session_user)?;
        for (field, value) in [
            ("session_home", &self.session_home),
            ("session_shell", &self.session_shell),
            ("state_dir", &self.state_dir),
        ] {
            if value.trim().is_empty() {
                anyhow::bail!("bootstrap identity {field} must not be empty");
            }
            validate_passwd_scalar(&format!("bootstrap identity {field}"), value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
