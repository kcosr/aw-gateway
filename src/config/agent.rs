use super::{IdleCleanupConfig, IdleCleanupConfigInput, validation::*};
use crate::template;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerAgentConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    pub ssh_bridge: Option<SshBridgeConfig>,
    pub control_socket: Option<ControlSocketConfig>,
    pub idle_cleanup: Option<IdleCleanupConfig>,
}

impl Default for ContainerAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            services: Vec::new(),
            ssh_bridge: None,
            control_socket: None,
            idle_cleanup: None,
        }
    }
}

impl ContainerAgentConfig {
    pub fn validate_gateway(&self) -> anyhow::Result<()> {
        if self
            .control_socket
            .as_ref()
            .is_some_and(|control_socket| matches!(control_socket, ControlSocketConfig::Path(_)))
        {
            anyhow::bail!(
                "container_agent.control_socket path values are managed by control_sockets.container_dir in gateway config; use false to disable the control socket"
            );
        }
        if let Some(bridge) = &self.ssh_bridge {
            bridge.validate_gateway()?;
        }
        self.validate_common(ServiceUserTemplateMode::GatewayManaged)
    }

    pub fn validate_agent_file(&self) -> anyhow::Result<()> {
        if let Some(bridge) = &self.ssh_bridge {
            bridge.validate_agent_file()?;
        }
        if let Some(control_socket) = self
            .control_socket
            .as_ref()
            .and_then(ControlSocketConfig::path)
        {
            validate_template(
                "container_agent.control_socket",
                control_socket,
                AGENT_TEMPLATE_VARS,
            )?;
        }
        self.validate_common(ServiceUserTemplateMode::Literal)
    }

    fn validate_common(
        &self,
        service_user_template_mode: ServiceUserTemplateMode,
    ) -> anyhow::Result<()> {
        if !self.enabled {
            if !self.services.is_empty() {
                anyhow::bail!("container_agent services require container_agent.enabled = true");
            }
            if self
                .ssh_bridge
                .as_ref()
                .is_some_and(|bridge| bridge.enabled)
            {
                anyhow::bail!("enabled ssh_bridge requires container_agent.enabled = true");
            }
            if self
                .control_socket
                .as_ref()
                .is_some_and(ControlSocketConfig::is_enabled)
            {
                anyhow::bail!(
                    "container_agent.control_socket requires container_agent.enabled = true"
                );
            }
            if self.idle_cleanup.is_some() {
                anyhow::bail!(
                    "container_agent.idle_cleanup requires container_agent.enabled = true"
                );
            }
            return Ok(());
        }
        let mut names = BTreeSet::new();
        for service in &self.services {
            service.validate(service_user_template_mode)?;
            if !names.insert(service.name.clone()) {
                anyhow::bail!("duplicate container_agent service {:?}", service.name);
            }
        }
        for service in &self.services {
            for dep in &service.depends_on {
                if !names.contains(dep) {
                    anyhow::bail!(
                        "service {:?} depends on unknown service {:?}",
                        service.name,
                        dep
                    );
                }
            }
        }
        validate_service_dependency_graph(&self.services)?;
        if let Some(cleanup) = &self.idle_cleanup {
            cleanup.validate()?;
        }
        Ok(())
    }

    pub fn needs_identity_token(&self) -> bool {
        // The gateway provisions AW_IDENTITY_TOKEN only for container-agent
        // services that explicitly inherit that variable. New token consumers
        // must either use the same EnvValue::inherit mechanism or update this
        // predicate and the container run environment together.
        self.enabled
            && self.services.iter().any(|service| {
                service.env.values().any(|value| {
                    value
                        .inherit
                        .as_deref()
                        .is_some_and(|name| name == "AW_IDENTITY_TOKEN")
                })
            })
    }
}

#[derive(Debug, Clone, Copy)]
enum ServiceUserTemplateMode {
    GatewayManaged,
    Literal,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerAgentConfigInput {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    pub ssh_bridge: Option<SshBridgeConfigInput>,
    pub control_socket: Option<ControlSocketConfig>,
    pub idle_cleanup: Option<IdleCleanupConfigInput>,
}

impl ContainerAgentConfigInput {
    pub(super) fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
        if let Some(enabled) = later.enabled {
            self.enabled = Some(enabled);
        }
        self.services = merge_services(self.services, &later.services)?;
        if let Some(ssh_bridge) = &later.ssh_bridge {
            self.ssh_bridge = Some(
                self.ssh_bridge
                    .take()
                    .unwrap_or_default()
                    .overlay(ssh_bridge),
            );
        }
        if let Some(control_socket) = &later.control_socket {
            self.control_socket = Some(control_socket.clone());
        }
        if let Some(idle_cleanup) = &later.idle_cleanup {
            self.idle_cleanup = Some(
                self.idle_cleanup
                    .take()
                    .unwrap_or_default()
                    .overlay(idle_cleanup),
            );
        }
        Ok(self)
    }

    pub(super) fn into_effective(self) -> anyhow::Result<ContainerAgentConfig> {
        let cfg = ContainerAgentConfig {
            enabled: self.enabled.unwrap_or(true),
            services: self.services,
            ssh_bridge: self.ssh_bridge.map(SshBridgeConfigInput::into_effective),
            control_socket: self.control_socket,
            idle_cleanup: self
                .idle_cleanup
                .map(IdleCleanupConfigInput::into_effective)
                .transpose()?,
        };
        cfg.validate_gateway()?;
        Ok(cfg)
    }

    pub(super) fn validate_partial(&self) -> anyhow::Result<()> {
        let mut names = BTreeSet::new();
        for service in &self.services {
            service.validate(ServiceUserTemplateMode::GatewayManaged)?;
            if !names.insert(service.name.clone()) {
                anyhow::bail!("duplicate container_agent service {:?}", service.name);
            }
        }
        for service in &self.services {
            for dep in &service.depends_on {
                if !names.contains(dep) {
                    anyhow::bail!(
                        "service {:?} depends on unknown service {:?}",
                        service.name,
                        dep
                    );
                }
            }
        }
        validate_service_dependency_graph(&self.services)?;
        if let Some(ssh_bridge) = &self.ssh_bridge {
            ssh_bridge.validate_partial_gateway()?;
        }
        if self
            .control_socket
            .as_ref()
            .is_some_and(|control_socket| matches!(control_socket, ControlSocketConfig::Path(_)))
        {
            anyhow::bail!(
                "container_agent.control_socket path values are managed by control_sockets.container_dir in gateway config; use false to disable the control socket"
            );
        }
        if let Some(idle_cleanup) = &self.idle_cleanup {
            idle_cleanup.clone().into_effective()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ControlSocketConfig {
    Path(String),
    Enabled(bool),
}

impl ControlSocketConfig {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Enabled(false))
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Path(path) => Some(path),
            Self::Enabled(_) => None,
        }
    }
}

fn validate_service_dependency_graph(services: &[ServiceConfig]) -> anyhow::Result<()> {
    let services_by_name: BTreeMap<&str, &ServiceConfig> = services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = Vec::new();

    for service in services {
        visit_service_dependency(
            service.name.as_str(),
            &services_by_name,
            &mut visiting,
            &mut visited,
            &mut stack,
        )?;
    }
    Ok(())
}

fn visit_service_dependency<'a>(
    name: &'a str,
    services_by_name: &BTreeMap<&'a str, &'a ServiceConfig>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
    stack: &mut Vec<&'a str>,
) -> anyhow::Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    if visiting.contains(name) {
        let start = stack.iter().position(|entry| *entry == name).unwrap_or(0);
        let mut cycle = stack[start..].to_vec();
        cycle.push(name);
        anyhow::bail!(
            "container_agent service dependency cycle: {}",
            cycle.join(" -> ")
        );
    }

    visiting.insert(name);
    stack.push(name);
    if let Some(service) = services_by_name.get(name) {
        for dep in &service.depends_on {
            visit_service_dependency(dep.as_str(), services_by_name, visiting, visited, stack)?;
        }
    }
    stack.pop();
    visiting.remove(name);
    visited.insert(name);
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default = "default_root")]
    pub user: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    pub restart_backoff: Option<String>,
    pub restart_backoff_max: Option<String>,
    pub startup_timeout: Option<String>,
    pub shutdown_timeout: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, EnvValue>,
    pub health_check: Option<HealthCheck>,
}

impl ServiceConfig {
    fn validate(&self, user_template_mode: ServiceUserTemplateMode) -> anyhow::Result<()> {
        validate_name("service", &self.name)?;
        validate_service_user(&self.user, user_template_mode)?;
        validate_command("service.command", &self.command)?;
        for dep in &self.depends_on {
            validate_name("depends_on", dep)?;
        }
        for key in self.env.keys() {
            validate_env_key(key)?;
        }
        validate_command_templates("service.command", &self.command, AGENT_TEMPLATE_VARS)?;
        if let Some(cwd) = &self.cwd {
            validate_template("service.cwd", cwd, AGENT_TEMPLATE_VARS)?;
        }
        for value in self.env.values() {
            value.validate()?;
            value.validate_templates(AGENT_TEMPLATE_VARS)?;
        }
        for value in [
            &self.restart_backoff,
            &self.restart_backoff_max,
            &self.startup_timeout,
            &self.shutdown_timeout,
        ]
        .into_iter()
        .flatten()
        {
            parse_duration(value)?;
        }
        if let Some(health_check) = &self.health_check {
            if matches!(health_check, HealthCheck::Command { .. }) {
                anyhow::bail!("service health_check does not support command checks");
            }
            health_check.validate()?;
            health_check.validate_templates(AGENT_TEMPLATE_VARS)?;
        }
        Ok(())
    }
}

fn validate_service_user(user: &str, template_mode: ServiceUserTemplateMode) -> anyhow::Result<()> {
    if user == SERVICE_USER_TEMPLATE
        && matches!(template_mode, ServiceUserTemplateMode::GatewayManaged)
    {
        return Ok(());
    }
    validate_name("service.user", user)
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    Never,
    OnFailure,
    #[default]
    Always,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvValue {
    pub value: Option<String>,
    pub file: Option<String>,
    pub inherit: Option<String>,
    #[serde(default = "default_true")]
    pub interpolate: bool,
    #[serde(default = "default_true")]
    pub required: bool,
}

impl EnvValue {
    pub fn resolve(&self, vars: &BTreeMap<String, String>) -> anyhow::Result<Option<String>> {
        self.validate()?;
        let mut value = if let Some(value) = &self.value {
            Some(value.clone())
        } else if let Some(path) = &self.file {
            let rendered_path = if self.interpolate {
                template::render(path, vars)?
            } else {
                path.clone()
            };
            match std::fs::read_to_string(&rendered_path) {
                Ok(contents) => {
                    let contents = contents.trim().to_string();
                    if contents.is_empty() && self.required {
                        anyhow::bail!("environment file {rendered_path:?} is empty");
                    }
                    (!contents.is_empty()).then_some(contents)
                }
                Err(err) if !self.required => {
                    tracing::warn!(path = rendered_path, error = %err, "optional environment file missing");
                    None
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("read environment file {rendered_path}"));
                }
            }
        } else if let Some(name) = &self.inherit {
            match std::env::var(name) {
                Ok(value) => Some(value),
                Err(err) if !self.required => {
                    tracing::warn!(name, error = %err, "optional inherited environment variable missing");
                    None
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("inherit environment variable {name}"));
                }
            }
        } else {
            None
        };
        if self.interpolate
            && let Some(raw) = &value
        {
            value = Some(template::render(raw, vars)?);
        }
        Ok(value)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let present =
            self.value.is_some() as u8 + self.file.is_some() as u8 + self.inherit.is_some() as u8;
        if present != 1 {
            anyhow::bail!("environment value must specify exactly one of value, file, or inherit");
        }
        if let Some(name) = &self.inherit {
            validate_env_key(name)?;
        }
        Ok(())
    }

    pub(super) fn validate_templates(&self, allowed: &[&str]) -> anyhow::Result<()> {
        if let Some(value) = &self.value
            && self.interpolate
        {
            validate_template("env.value", value, allowed)?;
        }
        if let Some(file) = &self.file
            && self.interpolate
        {
            validate_template("env.file", file, allowed)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HealthCheck {
    Process,
    Tcp {
        host: String,
        port: u16,
        interval: Option<String>,
        timeout: Option<String>,
    },
    Http {
        url: String,
        expect_status: Option<u16>,
        #[serde(default)]
        expect_json: BTreeMap<String, String>,
        interval: Option<String>,
        timeout: Option<String>,
    },
    Command {
        command: Vec<String>,
        timeout: Option<String>,
    },
}

impl HealthCheck {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            HealthCheck::Process => {}
            HealthCheck::Tcp {
                host,
                interval,
                timeout,
                ..
            } => {
                if host.is_empty() {
                    anyhow::bail!("tcp health check host is required");
                }
                for value in [interval, timeout].into_iter().flatten() {
                    parse_duration(value)?;
                }
            }
            HealthCheck::Http {
                url,
                interval,
                timeout,
                ..
            } => {
                if !url.starts_with("http://") {
                    anyhow::bail!("http health check url must start with http://");
                }
                for value in [interval, timeout].into_iter().flatten() {
                    parse_duration(value)?;
                }
            }
            HealthCheck::Command { command, timeout } => {
                validate_command("health_check.command", command)?;
                if let Some(timeout) = timeout {
                    parse_duration(timeout)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_templates(&self, allowed: &[&str]) -> anyhow::Result<()> {
        match self {
            HealthCheck::Command { command, .. } => {
                validate_command_templates("health_check.command", command, allowed)
            }
            HealthCheck::Http { url, .. } => validate_template("health_check.url", url, allowed),
            HealthCheck::Process | HealthCheck::Tcp { .. } => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshBridgeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub socket: Option<String>,
    #[serde(default = "default_bridge_target")]
    pub target: String,
    #[serde(default = "default_socket_mode")]
    pub mode: String,
}

impl SshBridgeConfig {
    pub fn validate_gateway(&self) -> anyhow::Result<()> {
        if self.socket.is_some() {
            anyhow::bail!(
                "container_agent.ssh_bridge.socket is managed by control_sockets.container_dir in gateway config"
            );
        }
        self.validate_common()
    }

    pub fn validate_agent_file(&self) -> anyhow::Result<()> {
        match self.socket.as_deref() {
            Some(socket) if socket.trim().is_empty() => {
                anyhow::bail!("ssh_bridge socket must not be empty when provided");
            }
            Some(socket) => validate_template("ssh_bridge.socket", socket, AGENT_TEMPLATE_VARS)?,
            None if self.enabled => {
                anyhow::bail!("ssh_bridge socket is required when enabled");
            }
            None => {}
        }
        self.validate_common()
    }

    fn validate_common(&self) -> anyhow::Result<()> {
        let mode = parse_socket_mode(&self.mode)?;
        if mode != 0o600 {
            anyhow::bail!("ssh_bridge mode currently supports only 0600");
        }
        if !self.target.contains(':') {
            anyhow::bail!("ssh_bridge target must be host:port");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshBridgeConfigInput {
    pub enabled: Option<bool>,
    pub socket: Option<String>,
    pub target: Option<String>,
    pub mode: Option<String>,
}

impl SshBridgeConfigInput {
    pub(super) fn overlay(mut self, later: &Self) -> Self {
        if let Some(enabled) = later.enabled {
            self.enabled = Some(enabled);
        }
        if let Some(socket) = &later.socket {
            self.socket = Some(socket.clone());
        }
        if let Some(target) = &later.target {
            self.target = Some(target.clone());
        }
        if let Some(mode) = &later.mode {
            self.mode = Some(mode.clone());
        }
        self
    }

    pub(super) fn into_effective(self) -> SshBridgeConfig {
        SshBridgeConfig {
            enabled: self.enabled.unwrap_or(true),
            socket: self.socket,
            target: self.target.unwrap_or_else(default_bridge_target),
            mode: self.mode.unwrap_or_else(default_socket_mode),
        }
    }

    fn validate_partial_gateway(&self) -> anyhow::Result<()> {
        if self.socket.is_some() {
            anyhow::bail!(
                "container_agent.ssh_bridge.socket is managed by control_sockets.container_dir in gateway config"
            );
        }
        if let Some(mode) = &self.mode {
            let mode = parse_socket_mode(mode)?;
            if mode != 0o600 {
                anyhow::bail!("ssh_bridge mode currently supports only 0600");
            }
        }
        if let Some(target) = &self.target
            && !target.contains(':')
        {
            anyhow::bail!("ssh_bridge target must be host:port");
        }
        Ok(())
    }
}
