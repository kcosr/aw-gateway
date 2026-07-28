use super::{IdleCleanupConfig, IdleCleanupConfigInput, validation::*};
use crate::template;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddrV4;
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use access_flow_relay::{
    AccessFlowRelayPlan, AccessFlowRelayPlanError, AccessFlowRoute, AccessFlowRouteName,
};
use access_flow_tls::{TlsAccessFlowAddress, TlsAccessFlowServerName};
use access_flow_unix::{NormalizedUnixSocketPath, UnixAccessFlowEndpoint, UnixExecutionTarget};
use access_identity::{IdentityPresentation, SensitiveBearer};
use access_tls_trust::{
    TlsClientTrustMode, TlsClientTrustPlan, TlsTrustFileSource, TlsTrustLoadError,
};

pub const ACCESS_FLOW_RELAY_NODE: &str = "@access-flow-relay";
const REMOVED_LOCAL_FLOW_RELAY_NODE: &str = "@local-flow-relay";

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
    pub access_flow_relay: Option<AccessFlowRelayConfig>,
}

impl Default for ContainerAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            services: Vec::new(),
            ssh_bridge: None,
            control_socket: None,
            idle_cleanup: None,
            access_flow_relay: None,
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
        if let Some(relay) = &self.access_flow_relay {
            relay.validate(AccessFlowRelayValidationMode::Gateway)?;
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
        if let Some(relay) = &self.access_flow_relay {
            relay.validate(AccessFlowRelayValidationMode::Agent)?;
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
            if self.access_flow_relay.is_some() {
                anyhow::bail!(
                    "container_agent.access_flow_relay requires container_agent.enabled = true"
                );
            }
            return Ok(());
        }
        let mut names = BTreeSet::new();
        let presentation_source = self
            .access_flow_relay
            .as_ref()
            .and_then(|relay| relay.presentation.environment_variable());
        for service in &self.services {
            service.validate(service_user_template_mode)?;
            if let Some(source) = presentation_source {
                validate_service_presentation_source(service, source)?;
            }
            if !names.insert(service.name.clone()) {
                anyhow::bail!("duplicate container_agent service {:?}", service.name);
            }
        }
        for service in &self.services {
            for dep in &service.depends_on {
                if dep == REMOVED_LOCAL_FLOW_RELAY_NODE {
                    anyhow::bail!(
                        "service {:?} uses removed dependency {REMOVED_LOCAL_FLOW_RELAY_NODE:?}; use {ACCESS_FLOW_RELAY_NODE:?}",
                        service.name
                    );
                }
                if dep == ACCESS_FLOW_RELAY_NODE {
                    if self.access_flow_relay.is_none() {
                        anyhow::bail!(
                            "service {:?} depends on {ACCESS_FLOW_RELAY_NODE:?} but container_agent.access_flow_relay is not configured",
                            service.name
                        );
                    }
                } else if !names.contains(dep) {
                    anyhow::bail!(
                        "service {:?} depends on unknown service {:?}",
                        service.name,
                        dep
                    );
                }
            }
        }
        if let Some(relay) = &self.access_flow_relay {
            for dependency in &relay.start_after_services {
                let Some(service) = self
                    .services
                    .iter()
                    .find(|service| service.name == *dependency)
                else {
                    anyhow::bail!(
                        "container_agent.access_flow_relay.start_after_services references unknown service {dependency:?}"
                    );
                };
                if !service.required {
                    anyhow::bail!(
                        "container_agent.access_flow_relay.start_after_services requires service {dependency:?} to be required"
                    );
                }
            }
        }
        validate_service_dependency_graph(&self.services, self.access_flow_relay.as_ref())?;
        if let Some(cleanup) = &self.idle_cleanup {
            cleanup.validate()?;
        }
        Ok(())
    }

    pub fn needs_identity_token(&self) -> bool {
        self.enabled
            && (self.access_flow_presentation_source().is_some()
                || self.services_need_identity_token())
    }

    pub fn access_flow_presentation_source(&self) -> Option<&str> {
        self.enabled
            .then_some(self.access_flow_relay.as_ref())
            .flatten()
            .and_then(|relay| relay.presentation.environment_variable())
    }

    pub fn services_need_identity_token(&self) -> bool {
        self.enabled
            && self
                .services
                .iter()
                .any(ServiceConfig::needs_identity_token)
    }
}

fn validate_service_presentation_source(
    service: &ServiceConfig,
    source: &str,
) -> anyhow::Result<()> {
    for (key, value) in &service.env {
        let approved_identity_entry = source == "AW_IDENTITY_TOKEN"
            && key == "AW_IDENTITY_TOKEN"
            && value.inherit.as_deref() == Some("AW_IDENTITY_TOKEN");
        if !approved_identity_entry && (key == source || value.inherit.as_deref() == Some(source)) {
            anyhow::bail!(
                "container_agent service {:?} cannot use the access flow presentation source in this environment entry",
                service.name
            );
        }
    }
    Ok(())
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
    pub access_flow_relay: Option<AccessFlowRelayConfig>,
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
        if let Some(access_flow_relay) = &later.access_flow_relay {
            self.access_flow_relay = Some(access_flow_relay.clone());
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
            access_flow_relay: self.access_flow_relay,
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
                if dep != ACCESS_FLOW_RELAY_NODE && !names.contains(dep) {
                    anyhow::bail!(
                        "service {:?} depends on unknown service {:?}",
                        service.name,
                        dep
                    );
                }
            }
        }
        validate_service_dependency_graph(&self.services, self.access_flow_relay.as_ref())?;
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
        if let Some(relay) = &self.access_flow_relay {
            relay.validate(AccessFlowRelayValidationMode::Gateway)?;
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

fn validate_service_dependency_graph(
    services: &[ServiceConfig],
    relay: Option<&AccessFlowRelayConfig>,
) -> anyhow::Result<()> {
    let mut graph: BTreeMap<&str, Vec<&str>> = services
        .iter()
        .map(|service| {
            (
                service.name.as_str(),
                service.depends_on.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    if let Some(relay) = relay {
        graph.insert(
            ACCESS_FLOW_RELAY_NODE,
            relay
                .start_after_services
                .iter()
                .map(String::as_str)
                .collect(),
        );
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = Vec::new();

    for service in services {
        visit_service_dependency(
            service.name.as_str(),
            &graph,
            &mut visiting,
            &mut visited,
            &mut stack,
        )?;
    }
    if relay.is_some() {
        visit_service_dependency(
            ACCESS_FLOW_RELAY_NODE,
            &graph,
            &mut visiting,
            &mut visited,
            &mut stack,
        )?;
    }
    Ok(())
}

fn visit_service_dependency<'a>(
    name: &'a str,
    graph: &BTreeMap<&'a str, Vec<&'a str>>,
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
    if let Some(dependencies) = graph.get(name) {
        for dep in dependencies {
            visit_service_dependency(dep, graph, visiting, visited, stack)?;
        }
    }
    stack.pop();
    visiting.remove(name);
    visited.insert(name);
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
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

impl std::fmt::Debug for ServiceConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceConfig")
            .field("name", &self.name)
            .field("required", &self.required)
            .field("user", &self.user)
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("restart", &self.restart)
            .field("restart_backoff", &self.restart_backoff)
            .field("restart_backoff_max", &self.restart_backoff_max)
            .field("startup_timeout", &self.startup_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("depends_on", &self.depends_on)
            .field("env_entry_count", &self.env.len())
            .field("health_check", &self.health_check)
            .finish()
    }
}

impl ServiceConfig {
    fn validate(&self, user_template_mode: ServiceUserTemplateMode) -> anyhow::Result<()> {
        validate_name("service", &self.name)?;
        validate_service_user(&self.user, user_template_mode)?;
        validate_command("service.command", &self.command)?;
        for dep in &self.depends_on {
            if dep == REMOVED_LOCAL_FLOW_RELAY_NODE {
                anyhow::bail!(
                    "depends_on uses removed node {REMOVED_LOCAL_FLOW_RELAY_NODE:?}; use {ACCESS_FLOW_RELAY_NODE:?}"
                );
            }
            if dep != ACCESS_FLOW_RELAY_NODE {
                validate_name("depends_on", dep)?;
            }
        }
        self.validate_identity_environment_contract()?;
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

    fn validate_identity_environment_contract(&self) -> anyhow::Result<()> {
        for (key, value) in &self.env {
            let reserved_destination = key == "AW_IDENTITY_TOKEN";
            let canonical_source = value.inherit.as_deref() == Some("AW_IDENTITY_TOKEN");
            if (reserved_destination && !value.is_canonical_identity_inheritance())
                || (!reserved_destination && canonical_source)
            {
                anyhow::bail!(
                    "container_agent service deployment identity environment contract is invalid"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn needs_identity_token(&self) -> bool {
        self.env
            .get("AW_IDENTITY_TOKEN")
            .is_some_and(EnvValue::is_canonical_identity_inheritance)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessFlowRelayConfig {
    pub setup_timeout: String,
    pub drain_timeout: String,
    pub max_connections: usize,
    pub copy_buffer_bytes_per_direction: usize,
    #[serde(default)]
    pub start_after_services: Vec<String>,
    pub presentation: AccessFlowRelayPresentation,
    pub routes: Vec<AccessFlowRelayRoute>,
}

impl AccessFlowRelayConfig {
    fn validate(&self, mode: AccessFlowRelayValidationMode) -> anyhow::Result<()> {
        self.presentation.validate()?;
        let _ = self.compile(mode)?;
        Ok(())
    }

    pub(crate) fn compile(
        &self,
        mode: AccessFlowRelayValidationMode,
    ) -> anyhow::Result<CompiledAccessFlowRelayConfig> {
        self.compile_with_presentation(mode, self.presentation.validation_value()?)
    }

    pub(crate) fn compile_with_presentation(
        &self,
        mode: AccessFlowRelayValidationMode,
        presentation: IdentityPresentation,
    ) -> anyhow::Result<CompiledAccessFlowRelayConfig> {
        let has_tls_route = self
            .routes
            .iter()
            .any(|route| matches!(&route.transport, AccessFlowRelayTransport::TlsTcp { .. }));
        if has_tls_route
            && !matches!(
                &self.presentation,
                AccessFlowRelayPresentation::BearerEnvironment { .. }
            )
        {
            anyhow::bail!(
                "container_agent.access_flow_relay TLS routes require bearer_environment presentation"
            );
        }
        if has_tls_route && !matches!(&presentation, IdentityPresentation::Bearer(_)) {
            anyhow::bail!(
                "container_agent.access_flow_relay TLS routes require an activated bearer presentation"
            );
        }
        let setup_timeout = parse_duration(&self.setup_timeout)
            .context("container_agent.access_flow_relay.setup_timeout")?;
        let drain_timeout = parse_duration(&self.drain_timeout)
            .context("container_agent.access_flow_relay.drain_timeout")?;
        if drain_timeout.is_zero() || drain_timeout > Duration::from_secs(300) {
            anyhow::bail!(
                "container_agent.access_flow_relay.drain_timeout must be between 1ms and 5m"
            );
        }
        let max_connections = NonZeroUsize::new(self.max_connections).ok_or_else(|| {
            anyhow::anyhow!("container_agent.access_flow_relay.max_connections must be nonzero")
        })?;
        let copy_buffer_bytes_per_direction = NonZeroUsize::new(
            self.copy_buffer_bytes_per_direction,
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "container_agent.access_flow_relay.copy_buffer_bytes_per_direction must be nonzero"
            )
        })?;

        let mut dependencies = BTreeSet::new();
        for dependency in &self.start_after_services {
            validate_name(
                "container_agent.access_flow_relay.start_after_services",
                dependency,
            )?;
            if !dependencies.insert(dependency) {
                anyhow::bail!(
                    "container_agent.access_flow_relay.start_after_services contains duplicate {dependency:?}"
                );
            }
        }

        let mut routes = Vec::with_capacity(self.routes.len());
        let mut tls_index = 0_usize;
        for route in &self.routes {
            let route_tls_index =
                matches!(&route.transport, AccessFlowRelayTransport::TlsTcp { .. })
                    .then_some(tls_index);
            routes.push(route.compile(mode, route_tls_index)?);
            if route_tls_index.is_some() {
                tls_index = tls_index
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("access flow TLS route count overflow"))?;
            }
        }
        let plan = AccessFlowRelayPlan::new(
            routes,
            presentation,
            setup_timeout,
            max_connections,
            copy_buffer_bytes_per_direction,
        )
        .map_err(map_relay_plan_error)?;
        Ok(CompiledAccessFlowRelayConfig {
            plan,
            drain_timeout,
        })
    }

    pub(crate) fn render(&mut self, vars: &BTreeMap<String, String>) -> anyhow::Result<()> {
        for route in &mut self.routes {
            route.listen = template::render(&route.listen, vars)?;
            match &mut route.transport {
                AccessFlowRelayTransport::Unix { path } => {
                    *path = template::render(path, vars)?;
                }
                AccessFlowRelayTransport::TlsTcp { .. } => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum AccessFlowRelayValidationMode {
    Gateway,
    Agent,
}

pub(crate) struct CompiledAccessFlowRelayConfig {
    pub(crate) plan: AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
    pub(crate) drain_timeout: Duration,
}

pub(crate) enum CompiledAccessFlowRelayEndpoint {
    Unix(UnixAccessFlowEndpoint),
    TlsTcp {
        tls_index: usize,
        address: TlsAccessFlowAddress,
        server_name: TlsAccessFlowServerName,
        trust_mode: TlsClientTrustMode,
        trust_plan: TlsClientTrustPlan,
        trust_path: Option<PathBuf>,
    },
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessFlowRelayPresentation {
    Disabled {},
    Anonymous {},
    BearerEnvironment { variable: String },
}

impl std::fmt::Debug for AccessFlowRelayPresentation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled {} => formatter.write_str("Disabled"),
            Self::Anonymous {} => formatter.write_str("Anonymous"),
            Self::BearerEnvironment { .. } => formatter
                .debug_struct("BearerEnvironment")
                .field("variable", &"<redacted>")
                .finish(),
        }
    }
}

impl AccessFlowRelayPresentation {
    pub fn environment_variable(&self) -> Option<&str> {
        match self {
            Self::BearerEnvironment { variable } => Some(variable),
            Self::Disabled {} | Self::Anonymous {} => None,
        }
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        if let Some(variable) = self.environment_variable() {
            validate_presentation_source(variable)?;
        }
        Ok(())
    }

    fn validation_value(&self) -> anyhow::Result<IdentityPresentation> {
        match self {
            Self::Disabled {} => Ok(IdentityPresentation::Disabled),
            Self::Anonymous {} => Ok(IdentityPresentation::Anonymous),
            Self::BearerEnvironment { .. } => Ok(IdentityPresentation::Bearer(
                SensitiveBearer::new(b"abcdefghijklmnopqrstuvwxyzABCDEF")
                    .context("construct access flow bearer validation value")?,
            )),
        }
    }
}

fn validate_presentation_source(variable: &str) -> anyhow::Result<()> {
    let bytes = variable.as_bytes();
    let valid_start = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
    if bytes.len() > 256
        || !valid_start
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        anyhow::bail!(
            "container_agent.access_flow_relay.presentation.variable must match [A-Za-z_][A-Za-z0-9_]* and be at most 256 bytes"
        );
    }
    if variable != "AW_IDENTITY_TOKEN"
        && variable
            .strip_prefix("AW_ACCESS_FLOW_")
            .is_none_or(str::is_empty)
    {
        anyhow::bail!(
            "container_agent.access_flow_relay.presentation.variable must use the AW Access Flow source namespace"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessFlowRelayRoute {
    pub name: String,
    pub listen: String,
    pub allowed_destination_ports: Vec<u16>,
    pub transport: AccessFlowRelayTransport,
}

impl AccessFlowRelayRoute {
    fn compile(
        &self,
        mode: AccessFlowRelayValidationMode,
        tls_index: Option<usize>,
    ) -> anyhow::Result<AccessFlowRoute<CompiledAccessFlowRelayEndpoint>> {
        let listen = match mode {
            AccessFlowRelayValidationMode::Gateway => {
                validate_template(
                    "container_agent.access_flow_relay.routes.listen",
                    &self.listen,
                    GATEWAY_TEMPLATE_VARS,
                )?;
                match &self.transport {
                    AccessFlowRelayTransport::Unix { path } => {
                        validate_template(
                            "container_agent.access_flow_relay.routes.transport.path",
                            path,
                            GATEWAY_TEMPLATE_VARS,
                        )?;
                    }
                    AccessFlowRelayTransport::TlsTcp {
                        address,
                        server_name,
                        trust,
                        ca_certificate,
                    } => validate_tls_transport_templates(
                        address,
                        server_name,
                        *trust,
                        ca_certificate.as_deref(),
                    )?,
                }
                if self.listen.contains('{') {
                    anyhow::bail!(
                        "container_agent.access_flow_relay.routes.listen must be a literal IPv4 loopback address"
                    );
                }
                self.listen.parse::<SocketAddrV4>()?
            }
            AccessFlowRelayValidationMode::Agent => {
                validate_template(
                    "container_agent.access_flow_relay.routes.listen",
                    &self.listen,
                    &[],
                )?;
                match &self.transport {
                    AccessFlowRelayTransport::Unix { path } => {
                        validate_template(
                            "container_agent.access_flow_relay.routes.transport.path",
                            path,
                            &[],
                        )?;
                    }
                    AccessFlowRelayTransport::TlsTcp {
                        address,
                        server_name,
                        trust,
                        ca_certificate,
                    } => validate_tls_transport_templates(
                        address,
                        server_name,
                        *trust,
                        ca_certificate.as_deref(),
                    )?,
                }
                self.listen.parse::<SocketAddrV4>()?
            }
        };

        let endpoint = match &self.transport {
            AccessFlowRelayTransport::Unix { path } => {
                let path = if mode == AccessFlowRelayValidationMode::Gateway && path.contains('{') {
                    format!(
                        "/run/aw-gateway/{}.sock",
                        AccessFlowRouteName::new(self.name.clone())
                            .map_err(map_relay_plan_error)?
                            .as_str()
                    )
                } else {
                    path.clone()
                };
                CompiledAccessFlowRelayEndpoint::Unix(compile_unix_endpoint(&path)?)
            }
            AccessFlowRelayTransport::TlsTcp {
                address,
                server_name,
                trust,
                ca_certificate,
            } => {
                let tls_index =
                    tls_index.expect("the relay compiler assigns an index to every TLS transport");
                let trust_path = ca_certificate.as_ref().map(PathBuf::from);
                let authored_source = trust_path
                    .clone()
                    .map(TlsTrustFileSource::new)
                    .transpose()
                    .map_err(map_tls_trust_source_config_error)?;
                let trust_plan = TlsClientTrustPlan::new(
                    *trust,
                    authored_source
                        .as_ref()
                        .map(TlsTrustFileSource::authored_source_id),
                )
                .map_err(map_tls_trust_config_error)?;
                CompiledAccessFlowRelayEndpoint::TlsTcp {
                    tls_index,
                    address: TlsAccessFlowAddress::parse(address).context(
                        "container_agent.access_flow_relay.routes.transport.address is invalid",
                    )?,
                    server_name: TlsAccessFlowServerName::parse(server_name).context(
                        "container_agent.access_flow_relay.routes.transport.server_name is invalid",
                    )?,
                    trust_mode: *trust,
                    trust_plan,
                    trust_path,
                }
            }
        };
        self.compile_values(listen, endpoint)
    }

    fn compile_values(
        &self,
        listen: SocketAddrV4,
        endpoint: CompiledAccessFlowRelayEndpoint,
    ) -> anyhow::Result<AccessFlowRoute<CompiledAccessFlowRelayEndpoint>> {
        let ports = self
            .allowed_destination_ports
            .iter()
            .copied()
            .map(|port| {
                NonZeroU16::new(port).ok_or_else(|| {
                    anyhow::anyhow!(
                        "container_agent.access_flow_relay.routes.allowed_destination_ports cannot contain zero"
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        AccessFlowRoute::new(
            AccessFlowRouteName::new(self.name.clone()).map_err(map_relay_plan_error)?,
            listen,
            ports,
            endpoint,
        )
        .map_err(map_relay_plan_error)
    }
}

fn compile_unix_endpoint(path: &str) -> anyhow::Result<UnixAccessFlowEndpoint> {
    let endpoint = UnixAccessFlowEndpoint::new(
        NormalizedUnixSocketPath::new(path)
            .context("container_agent.access_flow_relay.routes.transport.path is invalid")?,
    );
    endpoint.validate_for(UnixExecutionTarget::Linux).context(
        "container_agent.access_flow_relay.routes.transport.path exceeds Linux pathname capacity",
    )?;
    Ok(endpoint)
}

fn validate_tls_transport_templates(
    address: &str,
    server_name: &str,
    trust: TlsClientTrustMode,
    ca_certificate: Option<&str>,
) -> anyhow::Result<()> {
    validate_template(
        "container_agent.access_flow_relay.routes.transport.address",
        address,
        &[],
    )?;
    validate_template(
        "container_agent.access_flow_relay.routes.transport.server_name",
        server_name,
        &[],
    )?;
    if let Some(path) = ca_certificate {
        validate_template(
            "container_agent.access_flow_relay.routes.transport.ca_certificate",
            path,
            &[],
        )?;
    }
    let source = ca_certificate
        .map(PathBuf::from)
        .map(TlsTrustFileSource::new)
        .transpose()
        .map_err(map_tls_trust_source_config_error)?;
    TlsClientTrustPlan::new(
        trust,
        source.as_ref().map(TlsTrustFileSource::authored_source_id),
    )
    .map(|_| ())
    .map_err(map_tls_trust_config_error)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessFlowRelayTransport {
    Unix {
        path: String,
    },
    TlsTcp {
        address: String,
        server_name: String,
        trust: TlsClientTrustMode,
        ca_certificate: Option<String>,
    },
}

fn map_tls_trust_config_error(error: TlsTrustLoadError) -> anyhow::Error {
    match error {
        TlsTrustLoadError::InvalidPlan => anyhow::anyhow!(
            "container_agent.access_flow_relay.routes.transport ca_certificate does not match trust mode"
        ),
        TlsTrustLoadError::ResourceLimit => anyhow::anyhow!(
            "container_agent.access_flow_relay.routes.transport.ca_certificate exceeds a resource bound"
        ),
        _ => anyhow::anyhow!(
            "container_agent.access_flow_relay.routes.transport.ca_certificate is invalid"
        ),
    }
}

fn map_tls_trust_source_config_error(error: TlsTrustLoadError) -> anyhow::Error {
    match error {
        TlsTrustLoadError::InvalidPlan => anyhow::anyhow!(
            "container_agent.access_flow_relay.routes.transport.ca_certificate must be a normalized absolute non-root path"
        ),
        TlsTrustLoadError::ResourceLimit => anyhow::anyhow!(
            "container_agent.access_flow_relay.routes.transport.ca_certificate exceeds a resource bound"
        ),
        _ => anyhow::anyhow!(
            "container_agent.access_flow_relay.routes.transport.ca_certificate is invalid"
        ),
    }
}

fn map_relay_plan_error(error: AccessFlowRelayPlanError) -> anyhow::Error {
    anyhow::anyhow!("invalid container_agent.access_flow_relay: {error}")
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

#[derive(Clone, Deserialize, Serialize)]
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

impl std::fmt::Debug for EnvValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnvValue")
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .field("file", &self.file.as_ref().map(|_| "<redacted>"))
            .field("inherit", &self.inherit.as_ref().map(|_| "<redacted>"))
            .field("interpolate", &self.interpolate)
            .field("required", &self.required)
            .finish()
    }
}

impl EnvValue {
    fn is_canonical_identity_inheritance(&self) -> bool {
        self.value.is_none()
            && self.file.is_none()
            && self.inherit.as_deref() == Some("AW_IDENTITY_TOKEN")
            && self.interpolate
            && self.required
    }

    pub fn resolve(&self, vars: &BTreeMap<String, String>) -> anyhow::Result<Option<String>> {
        self.validate()?;
        let value = if let Some(value) = &self.value {
            if self.interpolate {
                Some(template::render(value, vars)?)
            } else {
                Some(value.clone())
            }
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
