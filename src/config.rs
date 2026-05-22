use crate::template;
use anyhow::Context;
use glob::glob;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const GATEWAY_SCHEMA_VERSION: &str = "1";
pub const AGENT_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub schema_version: String,
    #[serde(default = "default_target")]
    pub default_target: String,
    #[serde(default)]
    pub target_includes: Vec<String>,
    #[serde(default)]
    pub launch_includes: Vec<String>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub ssh_dispatch: SshDispatchConfig,
    #[serde(default)]
    pub client_config: ClientConfig,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetConfig>,
    #[serde(default)]
    pub lifecycle_steps: Vec<LifecycleStep>,
    #[serde(default)]
    pub host_steps: Vec<HostStep>,
    #[serde(default)]
    pub container_mounts: Vec<ContainerMountConfig>,
    #[serde(default)]
    pub container_bootstrap: ContainerBootstrapConfig,
    #[serde(default)]
    pub container_bootstrap_steps: Vec<ContainerBootstrapStep>,
    #[serde(default)]
    pub container_agent: ContainerAgentConfig,
    #[serde(default)]
    pub container_ssh: ContainerSshConfig,
    #[serde(default)]
    pub launches: BTreeMap<String, LaunchConfig>,
}

impl GatewayConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        cfg.compose_includes(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn compose_includes(&mut self, root_path: &Path) -> anyhow::Result<()> {
        let mut target_seen = BTreeSet::new();
        let mut launch_seen = BTreeSet::new();
        let root_dir = root_path.parent().unwrap_or_else(|| Path::new("."));
        let root_canonical = canonical_existing_path(root_path)?;
        let mut target_stack = BTreeSet::from([root_canonical.clone()]);
        let mut launch_stack = BTreeSet::from([root_canonical]);
        compose_target_includes(
            &mut self.targets,
            &self.target_includes,
            root_dir,
            &mut target_seen,
            &mut target_stack,
        )?;
        compose_launch_includes(
            &mut self.launches,
            &self.launch_includes,
            root_dir,
            &mut launch_seen,
            &mut launch_stack,
        )?;
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != GATEWAY_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported gateway schema_version {:?}; expected {:?}",
                self.schema_version,
                GATEWAY_SCHEMA_VERSION
            );
        }
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
            target.validate(name)?;
        }
        for (name, launch) in &self.launches {
            validate_name("launch", name)?;
            if name == "show" {
                anyhow::bail!("launch name \"show\" is reserved for launch show");
            }
            launch.validate(name, self)?;
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
        for step in &self.lifecycle_steps {
            step.validate("lifecycle_steps")?;
        }
        for step in &self.host_steps {
            step.validate("host_steps")?;
        }
        for mount in &self.container_mounts {
            mount.validate()?;
        }
        self.container_bootstrap.validate()?;
        for step in &self.container_bootstrap_steps {
            step.validate()?;
        }
        self.logging
            .validate_templates("logging", GATEWAY_LOGGING_TEMPLATE_VARS)?;
        self.container_ssh.validate()?;
        self.container_agent.validate()?;
        for (name, target) in &self.targets {
            self.effective_container_ssh(target)
                .with_context(|| format!("validate effective container_ssh for target {name:?}"))?;
            self.effective_lifecycle_steps(target).with_context(|| {
                format!("validate effective lifecycle_steps for target {name:?}")
            })?;
            self.effective_host_steps(target)
                .with_context(|| format!("validate effective host_steps for target {name:?}"))?;
            self.effective_container_bootstrap(target)
                .with_context(|| {
                    format!("validate effective container_bootstrap for target {name:?}")
                })?;
            self.effective_container_bootstrap_steps(target)
                .with_context(|| {
                    format!("validate effective container_bootstrap_steps for target {name:?}")
                })?;
            self.effective_container_agent(target).with_context(|| {
                format!("validate effective container_agent for target {name:?}")
            })?;
        }
        self.validate_target_agent_compatibility()?;
        self.ssh_dispatch.validate()?;
        Ok(())
    }

    fn validate_target_agent_compatibility(&self) -> anyhow::Result<()> {
        for (name, target) in &self.targets {
            let container_agent = self.effective_container_agent(target)?;
            if container_agent.enabled {
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

    pub fn effective_container_ssh(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ContainerSshConfig> {
        let mut container_ssh = self.container_ssh.clone();
        if let Some(target_container_ssh) = &target.container_ssh
            && let Some(transfer) = &target_container_ssh.transfer
        {
            container_ssh.transfer = transfer.to_effective()?;
        }
        container_ssh.validate()?;
        Ok(container_ssh)
    }

    pub fn effective_lifecycle_steps(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<Vec<LifecycleStep>> {
        let steps = merge_raw_steps(
            "lifecycle_steps",
            self.lifecycle_steps.clone(),
            &target.lifecycle_steps,
            |step| LifecycleStepKey {
                phase: Some(step.phase),
                name: step.name.clone(),
            },
            |step| LifecycleStepKey {
                phase: Some(step.phase),
                name: step.name.clone(),
            },
            RawLifecycleStep::to_effective,
        )?;
        for step in &steps {
            step.validate("lifecycle_steps")?;
        }
        Ok(steps)
    }

    pub fn effective_host_steps(&self, target: &TargetConfig) -> anyhow::Result<Vec<HostStep>> {
        let steps = merge_raw_steps(
            "host_steps",
            self.host_steps.clone(),
            &target.host_steps,
            |step| StepKey {
                name: step.name.clone(),
            },
            |step| StepKey {
                name: step.name.clone(),
            },
            RawHostStep::to_effective,
        )?;
        for step in &steps {
            step.validate("host_steps")?;
        }
        Ok(steps)
    }

    pub fn effective_container_bootstrap(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ContainerBootstrapConfig> {
        let mut bootstrap = self.container_bootstrap.clone();
        if let Some(target_bootstrap) = &target.container_bootstrap {
            if let Some(enabled) = target_bootstrap.enabled {
                bootstrap.enabled = enabled;
            }
            if let Some(entrypoint) = &target_bootstrap.entrypoint {
                bootstrap.entrypoint = entrypoint.clone();
            }
            if let Some(agent_program) = &target_bootstrap.agent_program {
                bootstrap.agent_program = agent_program.clone();
            }
        }
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    pub fn effective_container_bootstrap_steps(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<Vec<ContainerBootstrapStep>> {
        let steps = merge_raw_steps(
            "container_bootstrap_steps",
            self.container_bootstrap_steps.clone(),
            &target.container_bootstrap_steps,
            |step| StepKey {
                name: step.name.clone(),
            },
            |step| StepKey {
                name: step.name.clone(),
            },
            |step, _| RawContainerBootstrapStep::to_effective(step),
        )?;
        for step in &steps {
            step.validate()?;
        }
        Ok(steps)
    }

    pub fn effective_container_agent(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ContainerAgentConfig> {
        let mut container_agent = self.container_agent.clone();
        if let Some(target_container_agent) = &target.container_agent {
            for override_service in &target_container_agent.services {
                if let Some(existing) = container_agent
                    .services
                    .iter_mut()
                    .find(|service| service.name == override_service.name)
                {
                    *existing = override_service.clone();
                } else {
                    container_agent.services.push(override_service.clone());
                }
            }
        }
        container_agent.validate()?;
        Ok(container_agent)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetIncludeFile {
    #[serde(default)]
    target_includes: Vec<String>,
    #[serde(default)]
    targets: BTreeMap<String, TargetConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchIncludeFile {
    #[serde(default)]
    launch_includes: Vec<String>,
    #[serde(default)]
    launches: BTreeMap<String, LaunchConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    pub target: String,
    pub description: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, LaunchVarConfig>,
    #[serde(default)]
    pub steps: Vec<LaunchStep>,
}

impl LaunchConfig {
    pub fn validate(&self, launch_name: &str, cfg: &GatewayConfig) -> anyhow::Result<()> {
        if !cfg.targets.contains_key(&self.target) {
            anyhow::bail!(
                "launch {launch_name:?} references unknown target {:?}",
                self.target
            );
        }
        validate_command("launch.command", &self.command)?;
        for (name, var) in &self.vars {
            validate_name("launch var", name)?;
            var.validate(launch_name, name)?;
        }
        let allowed = self.allowed_template_vars();
        let allowed_refs = allowed.iter().map(String::as_str).collect::<Vec<_>>();
        if let Some(cwd) = &self.cwd {
            validate_template("launch.cwd", cwd, &allowed_refs)?;
        }
        validate_env_keyed_template_map("launch.env", &self.env, &allowed_refs)?;
        validate_command_templates("launch.command", &self.command, &allowed_refs)?;
        let mut referenced_vars = BTreeSet::new();
        collect_var_references(self.cwd.as_deref(), &mut referenced_vars)?;
        collect_var_references_from_map(&self.env, &mut referenced_vars)?;
        collect_var_references_from_command(&self.command, &mut referenced_vars)?;
        let mut step_names = BTreeSet::new();
        for step in &self.steps {
            step.validate(launch_name, &allowed_refs)?;
            if !step_names.insert(step.name.clone()) {
                anyhow::bail!(
                    "launch {launch_name:?} defines duplicate step {:?}",
                    step.name
                );
            }
            collect_var_references(step.cwd.as_deref(), &mut referenced_vars)?;
            collect_var_references_from_map(&step.env, &mut referenced_vars)?;
            collect_var_references_from_command(&step.command, &mut referenced_vars)?;
        }
        for var_name in referenced_vars {
            let var = self.vars.get(&var_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "launch {launch_name:?} references undeclared variable {var_name:?}"
                )
            })?;
            if !var.required && var.default.is_none() {
                anyhow::bail!(
                    "launch {launch_name:?} optional variable {var_name:?} is referenced by a template and must define default"
                );
            }
        }
        Ok(())
    }

    fn allowed_template_vars(&self) -> Vec<String> {
        let mut allowed = LAUNCH_TEMPLATE_BUILTINS
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        allowed.extend(self.vars.keys().map(|name| format!("var.{name}")));
        allowed
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchVarConfig {
    #[serde(rename = "type")]
    pub var_type: LaunchVarType,
    #[serde(default)]
    pub required: bool,
    pub default: Option<LaunchVarValue>,
    pub values: Option<Vec<String>>,
    pub description: Option<String>,
}

impl LaunchVarConfig {
    fn validate(&self, launch_name: &str, var_name: &str) -> anyhow::Result<()> {
        if self.required && self.default.is_some() {
            anyhow::bail!(
                "launch {launch_name:?} variable {var_name:?} cannot set both required and default"
            );
        }
        match self.var_type {
            LaunchVarType::String => {
                if let Some(default) = &self.default
                    && !matches!(default, LaunchVarValue::String(_))
                {
                    anyhow::bail!(
                        "launch {launch_name:?} variable {var_name:?} string default must be a TOML string"
                    );
                }
            }
            LaunchVarType::Enum => {
                let values = self.values.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "launch {launch_name:?} enum variable {var_name:?} requires values"
                    )
                })?;
                if values.is_empty() {
                    anyhow::bail!(
                        "launch {launch_name:?} enum variable {var_name:?} values must not be empty"
                    );
                }
                for value in values {
                    if value.is_empty() {
                        anyhow::bail!(
                            "launch {launch_name:?} enum variable {var_name:?} values must not include empty strings"
                        );
                    }
                }
                if let Some(default) = &self.default {
                    let LaunchVarValue::String(default) = default else {
                        anyhow::bail!(
                            "launch {launch_name:?} enum variable {var_name:?} default must be a TOML string"
                        );
                    };
                    if !values.contains(default) {
                        anyhow::bail!(
                            "launch {launch_name:?} enum variable {var_name:?} default must match one configured value"
                        );
                    }
                }
            }
            LaunchVarType::Boolean => {
                if let Some(default) = &self.default
                    && !matches!(default, LaunchVarValue::Boolean(_))
                {
                    anyhow::bail!(
                        "launch {launch_name:?} boolean variable {var_name:?} default must be a TOML boolean"
                    );
                }
            }
            LaunchVarType::Number => {
                if let Some(default) = &self.default
                    && !matches!(
                        default,
                        LaunchVarValue::Integer(_) | LaunchVarValue::Float(_)
                    )
                {
                    anyhow::bail!(
                        "launch {launch_name:?} number variable {var_name:?} default must be a TOML number"
                    );
                }
                if let Some(LaunchVarValue::Float(value)) = &self.default
                    && !value.is_finite()
                {
                    anyhow::bail!(
                        "launch {launch_name:?} number variable {var_name:?} default must be finite"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn default_rendered(&self) -> Option<String> {
        self.default.as_ref().map(LaunchVarValue::rendered)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchVarType {
    String,
    Enum,
    Boolean,
    Number,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum LaunchVarValue {
    String(String),
    Boolean(bool),
    Integer(i64),
    Float(f64),
}

impl LaunchVarValue {
    pub fn rendered(&self) -> String {
        match self {
            LaunchVarValue::String(value) => value.clone(),
            LaunchVarValue::Boolean(value) => value.to_string(),
            LaunchVarValue::Integer(value) => value.to_string(),
            LaunchVarValue::Float(value) => canonical_number_string(*value),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchStep {
    pub phase: LaunchStepPhase,
    pub location: LaunchStepLocation,
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    pub timeout: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub command: Vec<String>,
}

impl LaunchStep {
    fn validate(&self, launch_name: &str, allowed: &[&str]) -> anyhow::Result<()> {
        validate_name("launch step", &self.name)?;
        if self.phase != LaunchStepPhase::PostReady {
            anyhow::bail!(
                "launch {launch_name:?} step {:?} only supports phase = \"post_ready\"",
                self.name
            );
        }
        validate_command("launch.steps.command", &self.command)?;
        validate_command_templates("launch.steps.command", &self.command, allowed)?;
        if let Some(cwd) = &self.cwd {
            validate_template("launch.steps.cwd", cwd, allowed)?;
        }
        validate_env_keyed_template_map("launch.steps.env", &self.env, allowed)?;
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStepPhase {
    PostReady,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStepLocation {
    Host,
    Container,
}

fn compose_target_includes(
    targets: &mut BTreeMap<String, TargetConfig>,
    patterns: &[String],
    base_dir: &Path,
    seen: &mut BTreeSet<PathBuf>,
    stack: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    for path in expand_include_patterns(patterns, base_dir)? {
        let canonical = canonical_existing_path(&path)?;
        if stack.contains(&canonical) {
            anyhow::bail!("target_includes cycle detected at {}", path.display());
        }
        if !seen.insert(canonical.clone()) {
            continue;
        }
        stack.insert(canonical.clone());
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let include: TargetIncludeFile =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        let include_dir = path.parent().unwrap_or(base_dir);
        compose_target_includes(targets, &include.target_includes, include_dir, seen, stack)?;
        for (name, target) in include.targets {
            if targets.insert(name.clone(), target).is_some() {
                anyhow::bail!("duplicate target {name:?} from include {}", path.display());
            }
        }
        stack.remove(&canonical);
    }
    Ok(())
}

fn compose_launch_includes(
    launches: &mut BTreeMap<String, LaunchConfig>,
    patterns: &[String],
    base_dir: &Path,
    seen: &mut BTreeSet<PathBuf>,
    stack: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    for path in expand_include_patterns(patterns, base_dir)? {
        let canonical = canonical_existing_path(&path)?;
        if stack.contains(&canonical) {
            anyhow::bail!("launch_includes cycle detected at {}", path.display());
        }
        if !seen.insert(canonical.clone()) {
            continue;
        }
        stack.insert(canonical.clone());
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let include: LaunchIncludeFile =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        let include_dir = path.parent().unwrap_or(base_dir);
        compose_launch_includes(launches, &include.launch_includes, include_dir, seen, stack)?;
        for (name, launch) in include.launches {
            if launches.insert(name.clone(), launch).is_some() {
                anyhow::bail!("duplicate launch {name:?} from include {}", path.display());
            }
        }
        stack.remove(&canonical);
    }
    Ok(())
}

fn expand_include_patterns(patterns: &[String], base_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for pattern in patterns {
        let pattern_path = Path::new(pattern);
        let full_pattern = if pattern_path.is_absolute() {
            pattern_path.to_path_buf()
        } else {
            base_dir.join(pattern_path)
        };
        let pattern_text = full_pattern.display().to_string();
        for entry in glob(&pattern_text).with_context(|| format!("expand glob {pattern:?}"))? {
            paths.push(entry.with_context(|| format!("read glob entry for {pattern:?}"))?);
        }
    }
    paths.sort();
    Ok(paths)
}

fn canonical_existing_path(path: &Path) -> anyhow::Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))
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
        self.container_agent.validate()
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    #[serde(default = "default_workspace_path")]
    pub path: String,
    #[serde(default = "default_workspace_state_dir")]
    pub state_dir: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            path: default_workspace_path(),
            state_dir: default_workspace_state_dir(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshDispatchConfig {
    #[serde(default = "default_true")]
    pub allow_interactive_shell: bool,
    #[serde(default = "default_true")]
    pub allow_container_commands: bool,
    #[serde(default = "default_enabled_gateway_actions")]
    pub enabled_gateway_actions: Vec<String>,
}

impl Default for SshDispatchConfig {
    fn default() -> Self {
        Self {
            allow_interactive_shell: true,
            allow_container_commands: true,
            enabled_gateway_actions: default_enabled_gateway_actions(),
        }
    }
}

impl SshDispatchConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let allowed = [
            "connect",
            "up",
            "run",
            "status",
            "targets",
            "stop",
            "remove",
            "set-default",
            "add-key",
            "add-host-key",
            "add-container-key",
            "client-config",
            "client-bundle",
            "show-default",
            "reset-default",
            "help",
        ];
        for action in &self.enabled_gateway_actions {
            if !allowed.contains(&action.as_str()) {
                anyhow::bail!("unknown enabled_gateway_actions entry {action:?}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    #[serde(default = "default_inner_alias_template", alias = "alias_template")]
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
pub struct TargetConfig {
    pub image: String,
    #[serde(default)]
    pub mode: TargetMode,
    pub name: Option<String>,
    pub ephemeral_name: Option<String>,
    pub workspace: Option<String>,
    #[serde(default)]
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
    #[serde(default)]
    pub stop_when_idle: bool,
    #[serde(default)]
    pub remove_on_stop: bool,
    pub idle_cleanup: Option<IdleCleanupConfig>,
    pub local_ssh: Option<LocalSshConfig>,
    pub container_ssh: Option<TargetContainerSshConfig>,
    pub container_bootstrap: Option<TargetContainerBootstrapConfig>,
    #[serde(default)]
    pub lifecycle_steps: Vec<RawLifecycleStep>,
    #[serde(default)]
    pub host_steps: Vec<RawHostStep>,
    #[serde(default)]
    pub container_bootstrap_steps: Vec<RawContainerBootstrapStep>,
    pub container_agent: Option<TargetContainerAgentConfig>,
}

impl TargetConfig {
    pub fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        if self.image.trim().is_empty() {
            anyhow::bail!("target {target_name:?} image is required");
        }
        if let Some(container_user) = &self.container_user {
            validate_name("target.container_user", container_user)?;
        }
        if let Some(container_home) = &self.container_home
            && !container_home.is_absolute()
        {
            anyhow::bail!("target {target_name:?} container_home must be absolute");
        }
        if let Some(identity) = &self.identity {
            identity.validate(target_name)?;
        }
        if let Some(workspace) = &self.workspace {
            if workspace.trim().is_empty() {
                anyhow::bail!("target {target_name:?} workspace must not be empty");
            }
            validate_template(
                "target.workspace",
                workspace,
                TARGET_WORKSPACE_TEMPLATE_VARS,
            )?;
        }
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
        if let Some(container_ssh) = &self.container_ssh {
            container_ssh.validate(target_name)?;
        }
        if let Some(container_bootstrap) = &self.container_bootstrap {
            container_bootstrap.validate(target_name)?;
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
        if let Some(container_agent) = &self.container_agent {
            container_agent.validate(target_name)?;
        }
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerSshConfig {
    pub transfer: Option<TargetContainerSshTransferConfig>,
}

impl TargetContainerSshConfig {
    fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        if let Some(transfer) = &self.transfer {
            transfer
                .to_effective()
                .with_context(|| format!("target {target_name:?} container_ssh.transfer"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerSshTransferConfig {
    pub sftp: Option<SftpTransferMode>,
    pub legacy_scp: Option<LegacyScpTransferMode>,
}

impl TargetContainerSshTransferConfig {
    fn to_effective(&self) -> anyhow::Result<ContainerSshTransferConfig> {
        let sftp = self
            .sftp
            .ok_or_else(|| anyhow::anyhow!("target container_ssh.transfer.sftp is required"))?;
        let legacy_scp = self.legacy_scp.ok_or_else(|| {
            anyhow::anyhow!("target container_ssh.transfer.legacy_scp is required")
        })?;
        Ok(ContainerSshTransferConfig { sftp, legacy_scp })
    }
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
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerAgentConfig {
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

impl TargetContainerAgentConfig {
    fn validate(&self, target_name: &str) -> anyhow::Result<()> {
        let mut names = BTreeSet::new();
        for service in &self.services {
            service.validate()?;
            if !names.insert(service.name.clone()) {
                anyhow::bail!(
                    "target {target_name:?} defines duplicate container_agent service {:?}",
                    service.name
                );
            }
        }
        Ok(())
    }
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
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode {
    #[default]
    Fixed,
    Ephemeral,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdleCleanupConfig {
    #[serde(default)]
    pub owner: IdleCleanupOwner,
    #[serde(default)]
    pub action: IdleCleanupAction,
    pub idle_grace: Option<String>,
    #[serde(default)]
    pub preserve_processes: Vec<String>,
    pub poll_interval: Option<String>,
    pub shutdown_timeout: Option<String>,
    #[serde(default = "default_reap_signal")]
    pub reap_signal: String,
    pub reap_kill_after: Option<String>,
}

impl Default for IdleCleanupConfig {
    fn default() -> Self {
        Self {
            owner: IdleCleanupOwner::default(),
            action: IdleCleanupAction::default(),
            idle_grace: None,
            preserve_processes: Vec::new(),
            poll_interval: None,
            shutdown_timeout: None,
            reap_signal: default_reap_signal(),
            reap_kill_after: None,
        }
    }
}

impl IdleCleanupConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [
            &self.idle_grace,
            &self.poll_interval,
            &self.shutdown_timeout,
            &self.reap_kill_after,
        ]
        .into_iter()
        .flatten()
        {
            parse_duration(value)?;
        }
        for process in &self.preserve_processes {
            validate_name("preserve_processes", process)?;
        }
        match self.reap_signal.as_str() {
            "TERM" | "KILL" | "INT" | "HUP" => {}
            _ => anyhow::bail!("unsupported reap_signal {:?}", self.reap_signal),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdleCleanupOwner {
    None,
    Gateway,
    #[default]
    Agent,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdleCleanupAction {
    None,
    #[default]
    ExitContainer,
    ReapProcesses,
}

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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawLifecycleStep {
    pub phase: LifecyclePhase,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub before: Option<String>,
    pub after: Option<String>,
    pub required: Option<bool>,
    pub command: Option<Vec<String>>,
    pub timeout: Option<String>,
}

impl RawLifecycleStep {
    fn to_effective(&self, inherited: Option<&LifecycleStep>) -> anyhow::Result<LifecycleStep> {
        let inherit_payload =
            self.timeout.is_some() && self.command.is_none() && self.required.is_none();
        let command = self.command.clone().or_else(|| {
            inherit_payload
                .then(|| inherited.map(|step| step.command.clone()))
                .flatten()
        });
        Ok(LifecycleStep {
            phase: self.phase,
            name: self.name.clone(),
            required: self
                .required
                .or_else(|| {
                    inherit_payload
                        .then(|| inherited.map(|step| step.required))
                        .flatten()
                })
                .unwrap_or(true),
            command: command.ok_or_else(|| {
                anyhow::anyhow!(
                    "lifecycle_steps {}/{} command must be provided when enabled",
                    lifecycle_phase_name(self.phase),
                    self.name
                )
            })?,
            timeout: self.timeout.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleStep {
    pub phase: LifecyclePhase,
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    pub command: Vec<String>,
    pub timeout: Option<String>,
}

impl LifecycleStep {
    pub fn validate(&self, field: &str) -> anyhow::Result<()> {
        validate_name(field, &self.name)?;
        validate_command(field, &self.command)?;
        let vars = match self.phase {
            LifecyclePhase::PreStart => GATEWAY_TEMPLATE_VARS_NO_PID,
            LifecyclePhase::PostStartHost | LifecyclePhase::PreStop | LifecyclePhase::PostStop => {
                GATEWAY_TEMPLATE_VARS
            }
        };
        validate_command_templates(field, &self.command, vars)?;
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    PreStart,
    PostStartHost,
    PreStop,
    PostStop,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostStep {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub before: Option<String>,
    pub after: Option<String>,
    pub required: Option<bool>,
    pub command: Option<Vec<String>>,
    pub health_check: Option<HealthCheck>,
    pub timeout: Option<String>,
}

impl RawHostStep {
    fn to_effective(&self, inherited: Option<&HostStep>) -> anyhow::Result<HostStep> {
        let inherit_payload = self.timeout.is_some()
            && self.command.is_none()
            && self.required.is_none()
            && self.health_check.is_none();
        let command = self.command.clone().or_else(|| {
            inherit_payload
                .then(|| inherited.map(|step| step.command.clone()))
                .flatten()
        });
        Ok(HostStep {
            name: self.name.clone(),
            required: self
                .required
                .or_else(|| {
                    inherit_payload
                        .then(|| inherited.map(|step| step.required))
                        .flatten()
                })
                .unwrap_or(true),
            command: command.ok_or_else(|| {
                anyhow::anyhow!(
                    "host_steps {} command must be provided when enabled",
                    self.name
                )
            })?,
            health_check: self.health_check.clone().or_else(|| {
                inherit_payload
                    .then(|| inherited.and_then(|step| step.health_check.clone()))
                    .flatten()
            }),
            timeout: self.timeout.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostStep {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    pub command: Vec<String>,
    pub health_check: Option<HealthCheck>,
    pub timeout: Option<String>,
}

impl HostStep {
    pub fn validate(&self, field: &str) -> anyhow::Result<()> {
        validate_name(field, &self.name)?;
        validate_command(field, &self.command)?;
        validate_command_templates(field, &self.command, GATEWAY_TEMPLATE_VARS)?;
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        if let Some(health_check) = &self.health_check {
            if matches!(health_check, HealthCheck::Process) {
                anyhow::bail!("host_step health_check does not support process checks");
            }
            health_check.validate()?;
            health_check.validate_templates(GATEWAY_TEMPLATE_VARS)?;
        }
        Ok(())
    }
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawContainerBootstrapStep {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub before: Option<String>,
    pub after: Option<String>,
    pub required: Option<bool>,
    pub user: Option<String>,
    pub command: Option<Vec<String>>,
    pub timeout: Option<String>,
}

impl RawContainerBootstrapStep {
    fn to_effective(&self) -> anyhow::Result<ContainerBootstrapStep> {
        Ok(ContainerBootstrapStep {
            name: self.name.clone(),
            required: self.required.unwrap_or(true),
            user: self.user.clone().unwrap_or_else(default_root),
            command: self.command.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "container_bootstrap_steps {} command must be provided when enabled",
                    self.name
                )
            })?,
            timeout: self.timeout.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerBootstrapStep {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default = "default_root")]
    pub user: String,
    pub command: Vec<String>,
    pub timeout: Option<String>,
}

impl ContainerBootstrapStep {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name("container_bootstrap.steps.name", &self.name)?;
        if self.user.trim().is_empty() {
            anyhow::bail!("container_bootstrap.steps.user must not be empty");
        }
        validate_template(
            "container_bootstrap.steps.user",
            &self.user,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        validate_command("container_bootstrap.steps.command", &self.command)?;
        validate_command_templates(
            "container_bootstrap.steps.command",
            &self.command,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        Ok(())
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
    pub identity: BootstrapIdentity,
    #[serde(default)]
    pub steps: Vec<RenderedContainerBootstrapStep>,
}

impl ContainerBootstrapFile {
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
                "unsupported bootstrap schema_version {:?}; expected {:?}",
                self.schema_version,
                AGENT_SCHEMA_VERSION
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedContainerBootstrapStep {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default = "default_root")]
    pub user: String,
    pub command: Vec<String>,
    pub timeout: Option<String>,
}

impl RenderedContainerBootstrapStep {
    fn validate(&self) -> anyhow::Result<()> {
        validate_name("bootstrap step", &self.name)?;
        if self.user.trim().is_empty() {
            anyhow::bail!("bootstrap step user must not be empty");
        }
        validate_command("bootstrap step command", &self.command)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerSshConfig {
    #[serde(default)]
    pub transfer: ContainerSshTransferConfig,
}

impl ContainerSshConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerSshTransferConfig {
    #[serde(default)]
    pub sftp: SftpTransferMode,
    #[serde(default)]
    pub legacy_scp: LegacyScpTransferMode,
}

impl Default for ContainerSshTransferConfig {
    fn default() -> Self {
        Self {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SftpTransferMode {
    #[default]
    Allow,
    Deny,
}

impl SftpTransferMode {
    pub fn allows(self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LegacyScpTransferMode {
    #[default]
    Allow,
    Deny,
    Inbound,
    Outbound,
}

impl LegacyScpTransferMode {
    pub fn allows_inbound(self) -> bool {
        matches!(self, Self::Allow | Self::Inbound)
    }

    pub fn allows_outbound(self) -> bool {
        matches!(self, Self::Allow | Self::Outbound)
    }
}

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
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(bridge) = &self.ssh_bridge {
            bridge.validate()?;
        }
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
            service.validate()?;
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
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name("service", &self.name)?;
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
        let present =
            self.value.is_some() as u8 + self.file.is_some() as u8 + self.inherit.is_some() as u8;
        if present != 1 {
            anyhow::bail!("environment value must specify exactly one of value, file, or inherit");
        }
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

    fn validate_templates(&self, allowed: &[&str]) -> anyhow::Result<()> {
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
            HealthCheck::Command { command } => validate_command("health_check.command", command)?,
        }
        Ok(())
    }

    fn validate_templates(&self, allowed: &[&str]) -> anyhow::Result<()> {
        match self {
            HealthCheck::Command { command } => {
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
    pub fn validate(&self) -> anyhow::Result<()> {
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

pub fn parse_duration(input: &str) -> anyhow::Result<Duration> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("duration is empty");
    }
    let (number, unit) = input.trim().split_at(
        input
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(input.len()),
    );
    if number.is_empty() {
        anyhow::bail!("duration {input:?} is missing a number");
    }
    if unit.is_empty() {
        anyhow::bail!("duration {input:?} is missing an explicit unit");
    }
    let value: u64 = number.parse()?;
    match unit {
        "ms" => Ok(Duration::from_millis(value)),
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value.checked_mul(60).ok_or_else(
            || anyhow::anyhow!("duration {input:?} is too large"),
        )?)),
        "h" => Ok(Duration::from_secs(
            value
                .checked_mul(60)
                .and_then(|value| value.checked_mul(60))
                .ok_or_else(|| anyhow::anyhow!("duration {input:?} is too large"))?,
        )),
        _ => anyhow::bail!("unsupported duration unit {unit:?} in {input:?}"),
    }
}

pub(crate) fn validate_passwd_scalar(field: &str, value: &str) -> anyhow::Result<()> {
    if value
        .chars()
        .any(|ch| matches!(ch, ':' | '\n' | '\r' | '\0'))
    {
        anyhow::bail!("{field} must not contain ':', newline, carriage return, or NUL");
    }
    Ok(())
}

fn parse_socket_mode(input: &str) -> anyhow::Result<u32> {
    if input.len() != 4 || !input.chars().all(|ch| matches!(ch, '0'..='7')) {
        anyhow::bail!("socket mode must be four octal digits, got {input:?}");
    }
    Ok(u32::from_str_radix(input, 8)?)
}

const GATEWAY_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "container_user",
    "container_home",
    "workspace",
    "state",
    "state_dir",
    "target",
    "image",
    "image_slug",
    "container_name",
    "container_state_dir",
    "container_state_dir_in_container",
    "container_pid",
    "session_id",
];

const GATEWAY_TEMPLATE_VARS_NO_PID: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "container_user",
    "container_home",
    "workspace",
    "state",
    "state_dir",
    "target",
    "image",
    "image_slug",
    "container_name",
    "container_state_dir",
    "container_state_dir_in_container",
    "container_pid",
    "session_id",
];

const TARGET_WORKSPACE_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "target",
    "image",
    "image_slug",
    "session_id",
];

const AGENT_TEMPLATE_VARS: &[&str] = &["container_state_dir"];

const GATEWAY_LOGGING_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "workspace",
    "state",
    "state_dir",
];

const CLIENT_TEMPLATE_VARS: &[&str] = &[
    "user",
    "home",
    "container_user",
    "container_home",
    "workspace",
    "state_dir",
    "target",
    "image",
    "image_slug",
    "container_name",
    "container_state_dir",
    "container_state_dir_in_container",
    "session_id",
    "host",
];

const RUNTIME_TEMPLATE_VARS: &[&str] = &["user", "home"];

const IDENTITY_TEMPLATE_VARS: &[&str] = &["user", "uid", "gid", "home"];

const LAUNCH_TEMPLATE_BUILTINS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "container_user",
    "container_home",
    "workspace",
    "state",
    "state_dir",
    "target",
    "image",
    "image_slug",
    "container_name",
    "container_state_dir",
    "container_state_dir_in_container",
    "session_id",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StepKey {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LifecycleStepKey {
    phase: Option<LifecyclePhase>,
    name: String,
}

trait MergeKey: Clone + Ord {
    fn reference_key(&self, name: &str) -> Self;
    fn label(&self) -> String;
}

impl MergeKey for StepKey {
    fn reference_key(&self, name: &str) -> Self {
        Self { name: name.into() }
    }

    fn label(&self) -> String {
        self.name.clone()
    }
}

impl MergeKey for LifecycleStepKey {
    fn reference_key(&self, name: &str) -> Self {
        Self {
            phase: self.phase,
            name: name.into(),
        }
    }

    fn label(&self) -> String {
        match self.phase {
            Some(phase) => format!("{}/{}", lifecycle_phase_name(phase), self.name),
            None => self.name.clone(),
        }
    }
}

trait RawStepEntry {
    fn enabled(&self) -> bool;
    fn before(&self) -> Option<&str>;
    fn after(&self) -> Option<&str>;
    fn has_payload(&self) -> bool;
}

impl RawStepEntry for RawLifecycleStep {
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn before(&self) -> Option<&str> {
        self.before.as_deref()
    }

    fn after(&self) -> Option<&str> {
        self.after.as_deref()
    }

    fn has_payload(&self) -> bool {
        self.required.is_some() || self.command.is_some() || self.timeout.is_some()
    }
}

impl RawStepEntry for RawHostStep {
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn before(&self) -> Option<&str> {
        self.before.as_deref()
    }

    fn after(&self) -> Option<&str> {
        self.after.as_deref()
    }

    fn has_payload(&self) -> bool {
        self.required.is_some()
            || self.command.is_some()
            || self.health_check.is_some()
            || self.timeout.is_some()
    }
}

impl RawStepEntry for RawContainerBootstrapStep {
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn before(&self) -> Option<&str> {
        self.before.as_deref()
    }

    fn after(&self) -> Option<&str> {
        self.after.as_deref()
    }

    fn has_payload(&self) -> bool {
        self.required.is_some()
            || self.user.is_some()
            || self.command.is_some()
            || self.timeout.is_some()
    }
}

fn validate_raw_target_steps<R, K, F>(
    target_name: &str,
    list_name: &str,
    steps: &[R],
    key: F,
) -> anyhow::Result<()>
where
    R: RawStepEntry,
    K: MergeKey,
    F: Fn(&R) -> K,
{
    let mut keys = BTreeSet::new();
    for step in steps {
        let step_key = key(step);
        if !keys.insert(step_key.clone()) {
            anyhow::bail!(
                "target {target_name:?} defines duplicate {list_name} {}",
                step_key.label()
            );
        }
        if step.before().is_some() && step.after().is_some() {
            anyhow::bail!(
                "target {target_name:?} {list_name} {} sets both before and after",
                step_key.label()
            );
        }
        if !step.enabled() && step.has_payload() {
            anyhow::bail!(
                "target {target_name:?} {list_name} {} is disabled but includes command payload",
                step_key.label()
            );
        }
    }
    Ok(())
}

fn merge_raw_steps<T, R, K, FK, RK, C>(
    list_name: &str,
    inherited: Vec<T>,
    raw: &[R],
    inherited_key: FK,
    raw_key: RK,
    convert: C,
) -> anyhow::Result<Vec<T>>
where
    T: Clone,
    R: RawStepEntry,
    K: MergeKey,
    FK: Fn(&T) -> K,
    RK: Fn(&R) -> K,
    C: Fn(&R, Option<&T>) -> anyhow::Result<T>,
{
    let mut result = inherited;
    let mut effective_keys: Vec<K> = result.iter().map(&inherited_key).collect();
    if effective_keys.iter().collect::<BTreeSet<_>>().len() != effective_keys.len() {
        anyhow::bail!("{list_name} contains duplicate inherited keys");
    }

    for entry in raw {
        let key = raw_key(entry);
        let existing_index = effective_keys
            .iter()
            .position(|candidate| candidate == &key);
        if let Some(index) = existing_index {
            if entry.before().is_some() || entry.after().is_some() {
                anyhow::bail!(
                    "{list_name} {} replaces an inherited entry and must not set before or after",
                    key.label()
                );
            }
            if entry.enabled() {
                result[index] = convert(entry, Some(&result[index]))?;
                effective_keys[index] = key;
            } else {
                result.remove(index);
                effective_keys.remove(index);
            }
            continue;
        }

        if !entry.enabled() {
            anyhow::bail!(
                "{list_name} {} is disabled but does not match an inherited entry",
                key.label()
            );
        }

        let insert_at = if let Some(before) = entry.before() {
            let reference = key.reference_key(before);
            effective_keys
                .iter()
                .position(|candidate| candidate == &reference)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{list_name} {} references missing before = {:?}",
                        key.label(),
                        before
                    )
                })?
        } else if let Some(after) = entry.after() {
            let reference = key.reference_key(after);
            effective_keys
                .iter()
                .position(|candidate| candidate == &reference)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{list_name} {} references missing after = {:?}",
                        key.label(),
                        after
                    )
                })?
        } else {
            result.len()
        };
        result.insert(insert_at, convert(entry, None)?);
        effective_keys.insert(insert_at, key);
    }

    if effective_keys.iter().collect::<BTreeSet<_>>().len() != effective_keys.len() {
        anyhow::bail!("{list_name} contains duplicate effective keys");
    }
    Ok(result)
}

fn lifecycle_phase_name(phase: LifecyclePhase) -> &'static str {
    match phase {
        LifecyclePhase::PreStart => "pre_start",
        LifecyclePhase::PostStartHost => "post_start_host",
        LifecyclePhase::PreStop => "pre_stop",
        LifecyclePhase::PostStop => "post_stop",
    }
}

fn validate_template(field: &str, value: &str, allowed: &[&str]) -> anyhow::Result<()> {
    template::validate_keys(value, allowed).with_context(|| format!("validate {field}"))
}

fn validate_command_templates(
    field: &str,
    command: &[String],
    allowed: &[&str],
) -> anyhow::Result<()> {
    for arg in command {
        validate_template(field, arg, allowed)?;
    }
    Ok(())
}

fn validate_command(field: &str, command: &[String]) -> anyhow::Result<()> {
    if command.is_empty() {
        anyhow::bail!("{field} command must not be empty");
    }
    if command[0].is_empty() {
        anyhow::bail!("{field} command argv[0] must not be empty");
    }
    Ok(())
}

pub(crate) fn validate_name(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        anyhow::bail!("{field} value {value:?} must contain only ASCII alnum, '.', '-', '_'");
    }
    Ok(())
}

fn validate_env_key(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        || value.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        anyhow::bail!("invalid environment key {value:?}");
    }
    Ok(())
}

fn validate_env_map(field: &str, env: &BTreeMap<String, String>) -> anyhow::Result<()> {
    for (key, value) in env {
        validate_env_key(key)?;
        validate_template(
            &format!("{field}.{key}"),
            value,
            GATEWAY_TEMPLATE_VARS_NO_PID,
        )?;
    }
    Ok(())
}

fn validate_env_keyed_template_map(
    field: &str,
    env: &BTreeMap<String, String>,
    allowed: &[&str],
) -> anyhow::Result<()> {
    for (key, value) in env {
        validate_env_key(key)?;
        validate_template(&format!("{field}.{key}"), value, allowed)?;
    }
    Ok(())
}

fn collect_var_references(input: Option<&str>, refs: &mut BTreeSet<String>) -> anyhow::Result<()> {
    let Some(input) = input else {
        return Ok(());
    };
    for key in template::referenced_keys(input)? {
        if let Some(var_name) = key.strip_prefix("var.") {
            refs.insert(var_name.to_string());
        }
    }
    Ok(())
}

fn collect_var_references_from_map(
    values: &BTreeMap<String, String>,
    refs: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for value in values.values() {
        collect_var_references(Some(value), refs)?;
    }
    Ok(())
}

fn collect_var_references_from_command(
    command: &[String],
    refs: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for arg in command {
        collect_var_references(Some(arg), refs)?;
    }
    Ok(())
}

fn canonical_number_string(value: f64) -> String {
    let text = value.to_string();
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

fn validate_ssh_config_scalar(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    if value.contains(['\n', '\r']) {
        anyhow::bail!("{field} must not contain newlines");
    }
    Ok(())
}

fn validate_container_name(value: &str) -> anyhow::Result<()> {
    validate_name("container name", value)
}

fn default_target() -> String {
    "default".into()
}

fn default_log_level() -> String {
    "info".into()
}

fn default_workspace_path() -> String {
    "workspace".into()
}

fn default_workspace_state_dir() -> String {
    ".aw-gateway".into()
}

fn default_true() -> bool {
    true
}

fn default_enabled_gateway_actions() -> Vec<String> {
    [
        "connect",
        "up",
        "run",
        "status",
        "targets",
        "stop",
        "remove",
        "set-default",
        "show-default",
        "reset-default",
        "add-key",
        "add-host-key",
        "add-container-key",
        "client-config",
        "client-bundle",
        "help",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_inner_alias_template() -> String {
    "aw-{target}".into()
}

fn default_container_host_template() -> String {
    "aw-container-{target}".into()
}

fn default_host() -> String {
    "localhost".into()
}

fn default_gateway_path() -> String {
    "/opt/aw-gateway/bin/aw-gateway".into()
}

fn default_identity_dir() -> String {
    "~/.ssh/aw-gateway".into()
}

fn default_listen_host() -> String {
    "127.0.0.1".into()
}

fn default_root() -> String {
    "root".into()
}

fn default_bridge_target() -> String {
    "127.0.0.1:22".into()
}

fn default_socket_mode() -> String {
    "0600".into()
}

fn default_reap_signal() -> String {
    "TERM".into()
}

fn default_bootstrap_entrypoint() -> String {
    "/opt/aw-gateway/bin/aw-container-bootstrap".into()
}

fn default_bootstrap_agent_program() -> String {
    "/opt/aw-gateway/bin/aw-container-agent".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_gateway_config_validates() {
        let cfg: GatewayConfig = toml::from_str(crate::gateway::DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn container_ssh_policy_defaults_to_allowing_transfers() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        assert_eq!(cfg.container_ssh.transfer.sftp, SftpTransferMode::Allow);
        assert_eq!(
            cfg.container_ssh.transfer.legacy_scp,
            LegacyScpTransferMode::Allow
        );
        cfg.validate().unwrap();
    }

    #[test]
    fn container_ssh_policy_allows_independent_transfer_controls() {
        for (sftp, legacy_scp) in [
            (SftpTransferMode::Allow, LegacyScpTransferMode::Allow),
            (SftpTransferMode::Allow, LegacyScpTransferMode::Deny),
            (SftpTransferMode::Deny, LegacyScpTransferMode::Allow),
            (SftpTransferMode::Deny, LegacyScpTransferMode::Inbound),
            (SftpTransferMode::Deny, LegacyScpTransferMode::Outbound),
        ] {
            let cfg: GatewayConfig = toml::from_str(&format!(
                r#"
schema_version = "1"

[container_ssh.transfer]
sftp = "{}"
legacy_scp = "{}"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                toml_transfer_mode(sftp),
                toml_legacy_scp_mode(legacy_scp),
            ))
            .unwrap();
            cfg.validate().unwrap();
            assert_eq!(cfg.container_ssh.transfer.sftp, sftp);
            assert_eq!(cfg.container_ssh.transfer.legacy_scp, legacy_scp);
        }
    }

    fn toml_transfer_mode(mode: SftpTransferMode) -> &'static str {
        match mode {
            SftpTransferMode::Allow => "allow",
            SftpTransferMode::Deny => "deny",
        }
    }

    fn toml_legacy_scp_mode(mode: LegacyScpTransferMode) -> &'static str {
        match mode {
            LegacyScpTransferMode::Allow => "allow",
            LegacyScpTransferMode::Deny => "deny",
            LegacyScpTransferMode::Inbound => "inbound",
            LegacyScpTransferMode::Outbound => "outbound",
        }
    }

    #[test]
    fn bootstrap_mounts_and_identity_validate() {
        let cfg = r#"
schema_version = "1"

[[container_mounts]]
source = "{state_dir}/bootstrap/aw-container-agent"
target = "/opt/aw-gateway/bin/aw-container-agent"
mode = "ro"

[[targets.default.container_mounts]]
source = "{state_dir}/bootstrap/target-only"
target = "/opt/aw-gateway/target-only"
mode = "ro"

[container_bootstrap]
enabled = true
entrypoint = "/opt/aw-gateway/bin/aw-container-bootstrap"
agent_program = "/opt/aw-gateway/bin/aw-container-agent"

[[container_bootstrap_steps]]
name = "validate-agent"
required = true
user = "root"
command = ["/usr/bin/test", "-x", "/opt/aw-gateway/bin/aw-container-agent"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.identity]
bootstrap_user = "root"
session_user = "awuser"
session_uid = "{uid}"
session_gid = "{gid}"
session_home = "/home/awuser"
session_shell = "/bin/bash"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn literal_session_user_requires_explicit_uid_and_gid() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.identity]
session_user = "awuser"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("literal session_user requires explicit session_uid and session_gid")
        );
    }

    #[test]
    fn bootstrap_identity_rejects_passwd_delimiters() {
        for (field, home, shell, state_dir) in [
            ("session_home", "/home/aw:user", "/bin/bash", "/state"),
            ("session_home", "/home/aw\nuser", "/bin/bash", "/state"),
            ("session_shell", "/home/awuser", "/bin/ba:sh", "/state"),
            ("session_shell", "/home/awuser", "/bin/ba\rsh", "/state"),
            ("state_dir", "/home/awuser", "/bin/bash", "/sta\0te"),
        ] {
            let identity = BootstrapIdentity {
                session_user: "awuser".into(),
                session_uid: 2450,
                session_gid: 2450,
                session_home: home.into(),
                session_shell: shell.into(),
                state_dir: state_dir.into(),
            };
            let err = identity.validate().unwrap_err();
            assert!(
                err.to_string().contains(field),
                "expected {field} in error, got {err}"
            );
        }
    }

    #[test]
    fn rejects_missing_default_target() {
        let cfg = r#"
schema_version = "1"
default_target = "missing"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_interpolation_variables() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{missing}"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_gateway_owned_exit_cleanup() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_gateway_owned_process_reaping() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.idle_cleanup]
owner = "gateway"
action = "reap_processes"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unsupported_ssh_bridge_group_mode() {
        let cfg = r#"
schema_version = "1"

[container_agent.ssh_bridge]
enabled = true
socket = "{container_state_dir}/ssh.sock"
mode = "0660"
"#;
        let cfg: ContainerAgentFile = toml::from_str(cfg).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn listen_mode_allows_omitted_port_for_dynamic_allocation() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
mode = "listen"
host = "127.0.0.1"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn local_ssh_allows_published_port_backend() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
mode = "listen"
backend = "published_port"
readiness = "ssh_only"
host = "127.0.0.1"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn local_ssh_rejects_ssh_only_with_socket_backend() {
        let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
mode = "listen"
backend = "socket"
readiness = "ssh_only"
host = "127.0.0.1"
"#;
        let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn env_value_requires_exactly_one_source() {
        let value = EnvValue {
            value: Some("a".into()),
            file: Some("/tmp/a".into()),
            inherit: None,
            interpolate: true,
            required: true,
        };
        assert!(value.validate().is_err());
    }

    #[test]
    fn env_value_renders_file_path_and_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "token-{name}\n").unwrap();
        let value = EnvValue {
            value: None,
            file: Some("{dir}/token".into()),
            inherit: None,
            interpolate: true,
            required: true,
        };
        let vars = BTreeMap::from([
            ("dir".into(), dir.path().display().to_string()),
            ("name".into(), "workspace".into()),
        ]);

        assert_eq!(
            value.resolve(&vars).unwrap(),
            Some("token-workspace".into())
        );
    }

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("1d").is_err());
        assert!(parse_duration("5000000000000000000h").is_err());
    }

    #[test]
    fn partial_logging_config_keeps_console_default() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[logging]
level = "debug"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        assert!(cfg.logging.console);
    }

    #[test]
    fn host_steps_reject_process_health_checks() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[host_steps]]
name = "bad"
command = ["/bin/true"]
health_check = { type = "process" }
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn lifecycle_and_host_step_timeouts_validate() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "250ms"

[[host_steps]]
name = "firewall"
command = ["/bin/true"]
timeout = "2m"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let target = cfg.targets.get("default").unwrap();
        assert_eq!(
            cfg.effective_lifecycle_steps(target).unwrap()[0]
                .timeout
                .as_deref(),
            Some("250ms")
        );
        assert_eq!(
            cfg.effective_host_steps(target).unwrap()[0]
                .timeout
                .as_deref(),
            Some("2m")
        );
    }

    #[test]
    fn lifecycle_and_host_step_timeouts_reject_invalid_durations() {
        for (config, expected) in [
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "5"
"#,
                "missing an explicit unit",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[host_steps]]
name = "firewall"
command = ["/bin/true"]
timeout = "1d"
"#,
                "unsupported duration unit",
            ),
        ] {
            let cfg: GatewayConfig = toml::from_str(config).unwrap();
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn client_config_rejects_newline_scalars() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[client_config]
host = "example.com\nProxyCommand bad"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn disabled_agent_allows_no_services_or_bridge() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[container_agent]
enabled = false

[container_agent.ssh_bridge]
enabled = false
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn enabled_agent_allows_disabled_control_socket() {
        let cfg: ContainerAgentFile = toml::from_str(
            r#"
schema_version = "1"

[container_agent]
control_socket = false

[[container_agent.services]]
name = "sshd"
command = ["/bin/true"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.container_agent.control_socket,
            Some(ControlSocketConfig::Enabled(false))
        );
    }

    #[test]
    fn target_runtime_and_env_knobs_validate() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu"
mode = "fixed"
name = "{image_slug}"

[targets.default.runtime]
extra_run_args = ["--cap-add", "SYS_ADMIN"]

[targets.default.container_env]
CODEX_HOME = "/var/lib/codex"

[targets.default.session_env]
CODEX_HOME = "/var/lib/codex"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn target_workspace_template_validates() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
workspace = "{home}/workspace-internal"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.targets.get("default").unwrap().workspace.as_deref(),
            Some("{home}/workspace-internal")
        );
    }

    #[test]
    fn target_container_agent_service_overrides_global_service() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[container_agent.services]]
name = "acl-proxy"
command = ["acl-proxy", "--config", "/etc/acl-proxy/acl-proxy.toml"]

[[container_agent.services]]
name = "container-sshd"
command = ["/opt/aw-gateway/bin/start-container-sshd"]
depends_on = ["acl-proxy"]

[[targets.default.container_agent.services]]
name = "acl-proxy"
command = ["acl-proxy", "--config", "/etc/acl-proxy/internal-acl-proxy.toml"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let target = cfg.targets.get("default").unwrap();
        let effective = cfg.effective_container_agent(target).unwrap();
        assert_eq!(effective.services.len(), 2);
        let acl_proxy = effective
            .services
            .iter()
            .find(|service| service.name == "acl-proxy")
            .unwrap();
        assert_eq!(
            acl_proxy.command,
            [
                "acl-proxy",
                "--config",
                "/etc/acl-proxy/internal-acl-proxy.toml"
            ]
        );
    }

    #[test]
    fn target_container_ssh_transfer_replaces_global_policy() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[container_ssh.transfer]
sftp = "allow"
legacy_scp = "allow"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_ssh.transfer]
sftp = "deny"
legacy_scp = "outbound"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let effective = cfg
            .effective_container_ssh(cfg.targets.get("default").unwrap())
            .unwrap();
        assert_eq!(effective.transfer.sftp, SftpTransferMode::Deny);
        assert_eq!(
            effective.transfer.legacy_scp,
            LegacyScpTransferMode::Outbound
        );
    }

    #[test]
    fn target_container_ssh_transfer_requires_complete_policy() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_ssh.transfer]
sftp = "deny"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("legacy_scp is required"), "{err}");
    }

    #[test]
    fn target_container_bootstrap_overlays_global_fields() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[container_bootstrap]
enabled = false
entrypoint = "/global/bootstrap"
agent_program = "/global/agent"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_bootstrap]
enabled = true
agent_program = "/target/agent"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let effective = cfg
            .effective_container_bootstrap(cfg.targets.get("default").unwrap())
            .unwrap();
        assert!(effective.enabled);
        assert_eq!(effective.entrypoint, "/global/bootstrap");
        assert_eq!(effective.agent_program, "/target/agent");
    }

    #[test]
    fn target_lifecycle_steps_replace_remove_append_and_order_by_phase() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[lifecycle_steps]]
phase = "pre_start"
name = "first"
command = ["/bin/first"]

[[lifecycle_steps]]
phase = "pre_start"
name = "replace-me"
command = ["/bin/old"]

[[lifecycle_steps]]
phase = "post_stop"
name = "first"
command = ["/bin/post"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "replace-me"
command = ["/bin/new"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "before-replace"
before = "replace-me"
command = ["/bin/before"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "after-first"
after = "first"
command = ["/bin/after"]

[[targets.default.lifecycle_steps]]
phase = "post_stop"
name = "first"
enabled = false
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let effective = cfg
            .effective_lifecycle_steps(cfg.targets.get("default").unwrap())
            .unwrap();
        let pre_start: Vec<_> = effective
            .iter()
            .filter(|step| step.phase == LifecyclePhase::PreStart)
            .map(|step| (step.name.as_str(), step.command[0].as_str()))
            .collect();
        assert_eq!(
            pre_start,
            [
                ("first", "/bin/first"),
                ("after-first", "/bin/after"),
                ("before-replace", "/bin/before"),
                ("replace-me", "/bin/new")
            ]
        );
        assert!(
            !effective
                .iter()
                .any(|step| step.phase == LifecyclePhase::PostStop)
        );
    }

    #[test]
    fn target_step_timeout_only_overrides_keep_inherited_payload() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/prep"]
timeout = "10s"

[[host_steps]]
name = "firewall"
command = ["/bin/firewall"]
timeout = "10s"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "prep"
timeout = "20s"

[[targets.default.host_steps]]
name = "firewall"
timeout = "30s"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let target = cfg.targets.get("default").unwrap();
        let lifecycle = cfg.effective_lifecycle_steps(target).unwrap();
        assert_eq!(lifecycle[0].command, ["/bin/prep"]);
        assert_eq!(lifecycle[0].timeout.as_deref(), Some("20s"));
        let host = cfg.effective_host_steps(target).unwrap();
        assert_eq!(host[0].command, ["/bin/firewall"]);
        assert_eq!(host[0].timeout.as_deref(), Some("30s"));
    }

    #[test]
    fn target_step_merge_rejects_invalid_controls() {
        for (config, expected) in [
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.host_steps]]
name = "missing"
enabled = false
"#,
                "disabled but does not match an inherited entry",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[host_steps]]
name = "firewall"
command = ["/bin/old"]

[[targets.default.host_steps]]
name = "firewall"
before = "other"
command = ["/bin/new"]
"#,
                "must not set before or after",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.container_bootstrap_steps]]
name = "disabled"
enabled = false
command = ["/bin/bad"]
"#,
                "disabled but includes command payload",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "new"
after = "missing"
command = ["/bin/new"]
"#,
                "references missing after",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "new"
before = "missing"
command = ["/bin/new"]
"#,
                "references missing before",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.host_steps]]
name = "duplicate"
command = ["/bin/one"]

[[targets.default.host_steps]]
name = "duplicate"
command = ["/bin/two"]
"#,
                "defines duplicate host_steps duplicate",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[host_steps]]
name = "duplicate"
command = ["/bin/one"]

[[host_steps]]
name = "duplicate"
command = ["/bin/two"]
"#,
                "host_steps contains duplicate inherited keys",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.host_steps]]
name = "one"
before = "a"
after = "b"
command = ["/bin/one"]
"#,
                "sets both before and after",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[host_steps]]
name = "firewall"
command = ["/bin/old"]

[[targets.default.host_steps]]
name = "firewall"
enabled = false
timeout = "1s"
"#,
                "disabled but includes command payload",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/old"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "prep"
enabled = false
timeout = "1s"
"#,
                "disabled but includes command payload",
            ),
        ] {
            let cfg: GatewayConfig = toml::from_str(config).unwrap();
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn disabled_agent_rejects_published_port_ssh_backend() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
backend = "published_port"
readiness = "ssh_only"

[container_agent]
enabled = false
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn disabled_bridge_still_validates_shape() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[container_agent.ssh_bridge]
enabled = false
target = "missing-port"
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn launch_schema_validates_vars_templates_and_steps() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
target = "default"
description = "Run agent"
cwd = "{container_home}/{var.repo}"
env = { FLAG = "{var.flag}", LIMIT = "{var.limit}" }
command = ["agent", "--mode", "{var.mode}"]

[launches.agent.vars]
repo = { type = "string", required = true }
flag = { type = "boolean", default = true }
limit = { type = "number", default = 2.5 }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }

[[launches.agent.steps]]
phase = "post_ready"
location = "container"
name = "prep"
required = false
timeout = "5s"
cwd = "{container_home}"
env = { STEP_FLAG = "{var.flag}" }
command = ["prep", "{var.repo}"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn launch_schema_rejects_bad_vars_and_templates() {
        for (config, expected) in [
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "missing"
command = ["true"]
"#,
                "unknown target",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true", "{repo}"]

[launches.bad.vars]
repo = { type = "string", required = true }
"#,
                "unknown interpolation variable",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true", "{var.repo}"]

[launches.bad.vars]
repo = { type = "string" }
"#,
                "must define default",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
mode = { type = "enum", values = [] }
"#,
                "values must not be empty",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
repo = { type = "string", required = true, default = "main" }
"#,
                "cannot set both required and default",
            ),
        ] {
            let cfg: GatewayConfig = toml::from_str(config).unwrap();
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn include_composition_expands_sorted_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let targets = dir.path().join("targets");
        let launches = dir.path().join("launches");
        std::fs::create_dir_all(&targets).unwrap();
        std::fs::create_dir_all(&launches).unwrap();
        std::fs::write(
            targets.join("b.toml"),
            r#"
[targets.b]
image = "ubuntu/b"
mode = "fixed"
name = "b"
"#,
        )
        .unwrap();
        std::fs::write(
            targets.join("a.toml"),
            r#"
[targets.a]
image = "ubuntu/a"
mode = "fixed"
name = "a"
"#,
        )
        .unwrap();
        std::fs::write(
            launches.join("agent.toml"),
            r#"
[launches.agent]
target = "a"
command = ["true"]
"#,
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            format!(
                r#"
schema_version = "1"
default_target = "a"
target_includes = ["{}/*.toml"]
launch_includes = ["{}/*.toml"]
"#,
                targets.display(),
                launches.display()
            ),
        )
        .unwrap();

        let cfg = GatewayConfig::load(&root).unwrap();
        assert_eq!(
            cfg.targets.keys().cloned().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(cfg.launches.contains_key("agent"));

        std::fs::write(
            targets.join("dup.toml"),
            r#"
[targets.a]
image = "ubuntu/dup"
mode = "fixed"
name = "dup"
"#,
        )
        .unwrap();
        let err = GatewayConfig::load(&root).unwrap_err().to_string();
        assert!(err.contains("duplicate target"), "{err}");
    }

    #[test]
    fn includes_reject_cycles_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml");
        let b = dir.path().join("b.toml");
        std::fs::write(
            &a,
            format!(
                r#"
target_includes = ["{}"]
"#,
                b.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &b,
            format!(
                r#"
target_includes = ["{}"]
"#,
                a.display()
            ),
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            format!(
                r#"
schema_version = "1"
target_includes = ["{}"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                a.display()
            ),
        )
        .unwrap();
        let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
        assert!(err.contains("cycle"), "{err}");

        std::fs::write(&a, "unknown = true\n").unwrap();
        let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn container_agent_rejects_service_dependency_cycles() {
        let cfg: ContainerAgentFile = toml::from_str(
            r#"
schema_version = "1"

[[container_agent.services]]
name = "acl-proxy"
command = ["/bin/true"]
depends_on = ["container-sshd"]

[[container_agent.services]]
name = "container-sshd"
command = ["/bin/true"]
depends_on = ["acl-proxy"]
"#,
        )
        .unwrap();

        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("dependency cycle"), "{err}");
    }
}
