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
    pub includes: Vec<String>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub ssh_dispatch: SshDispatchConfig,
    #[serde(default)]
    pub client_config: ClientConfig,
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
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        cfg.compose_includes(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn compose_includes(&mut self, root_path: &Path) -> anyhow::Result<()> {
        let mut seen = BTreeSet::new();
        let root_dir = root_path.parent().unwrap_or_else(|| Path::new("."));
        let root_canonical = canonical_existing_path(root_path)?;
        let mut stack = BTreeSet::from([root_canonical]);
        IncludeComposer {
            target_templates: &mut self.target_templates,
            launch_templates: &mut self.launch_templates,
            targets: &mut self.targets,
            launches: &mut self.launches,
            seen: &mut seen,
            stack: &mut stack,
        }
        .compose(&self.includes, root_dir)?;
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
        self.logging
            .validate_templates("logging", GATEWAY_LOGGING_TEMPLATE_VARS)?;
        let effective_targets = self.effective_targets()?;
        for (name, launch) in &self.launches {
            validate_name("launch", name)?;
            if name == "show" {
                anyhow::bail!("launch name \"show\" is reserved for launch show");
            }
            launch.validate_partial(name)?;
            self.effective_launch_with_targets(name, launch, &effective_targets)
                .with_context(|| format!("validate effective launch {name:?}"))?;
        }
        self.validate_target_agent_compatibility(&effective_targets)?;
        self.ssh_dispatch.validate()?;
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
        let effective_input = self.overlay_target_templates(
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
                self.effective_launch_with_targets(name, launch, &targets)
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
        self.effective_launch_with_targets(name, launch, &targets)
    }

    fn effective_launch_with_targets(
        &self,
        name: &str,
        launch: &LaunchConfigInput,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<LaunchConfig> {
        let effective_input = self.overlay_launch_templates(
            self.launch_defaults.clone(),
            &format!("launch {name:?}"),
            &launch.use_templates,
        )?;
        let effective = effective_input.overlay(launch)?.into_effective(name)?;
        effective.validate(name, targets)?;
        Ok(effective)
    }

    fn validate_target_template_references(&self) -> anyhow::Result<()> {
        for name in self.target_templates.keys() {
            self.check_target_template_references(name, &mut Vec::new())?;
        }
        Ok(())
    }

    fn validate_launch_template_references(&self) -> anyhow::Result<()> {
        for name in self.launch_templates.keys() {
            self.check_launch_template_references(name, &mut Vec::new())?;
        }
        Ok(())
    }

    fn overlay_target_templates(
        &self,
        mut base: TargetConfigInput,
        owner: &str,
        templates: &[String],
    ) -> anyhow::Result<TargetConfigInput> {
        for template in templates {
            base = self
                .apply_target_template(base, template, &mut Vec::new())
                .with_context(|| format!("{owner} uses target template {template:?}"))?;
        }
        Ok(base)
    }

    fn check_target_template_references(
        &self,
        name: &str,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        if let Some(start) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_string());
            anyhow::bail!("target template cycle: {}", cycle.join(" -> "));
        }
        let template = self
            .target_templates
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown target template {name:?}"))?;

        stack.push(name.to_string());
        for dependency in &template.use_templates {
            self.check_target_template_references(dependency, stack)
                .with_context(|| {
                    format!("target template {name:?} uses target template {dependency:?}")
                })?;
        }
        stack.pop();
        Ok(())
    }

    fn apply_target_template(
        &self,
        mut base: TargetConfigInput,
        name: &str,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<TargetConfigInput> {
        if let Some(start) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_string());
            anyhow::bail!("target template cycle: {}", cycle.join(" -> "));
        }
        let template = self
            .target_templates
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown target template {name:?}"))?;

        stack.push(name.to_string());
        for dependency in &template.use_templates {
            base = self
                .apply_target_template(base, dependency, stack)
                .with_context(|| {
                    format!("target template {name:?} uses target template {dependency:?}")
                })?;
        }
        stack.pop();
        base.overlay(template)
    }

    fn overlay_launch_templates(
        &self,
        mut base: LaunchConfigInput,
        owner: &str,
        templates: &[String],
    ) -> anyhow::Result<LaunchConfigInput> {
        for template in templates {
            base = self
                .apply_launch_template(base, template, &mut Vec::new())
                .with_context(|| format!("{owner} uses launch template {template:?}"))?;
        }
        Ok(base)
    }

    fn check_launch_template_references(
        &self,
        name: &str,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        if let Some(start) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_string());
            anyhow::bail!("launch template cycle: {}", cycle.join(" -> "));
        }
        let template = self
            .launch_templates
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown launch template {name:?}"))?;

        stack.push(name.to_string());
        for dependency in &template.use_templates {
            self.check_launch_template_references(dependency, stack)
                .with_context(|| {
                    format!("launch template {name:?} uses launch template {dependency:?}")
                })?;
        }
        stack.pop();
        Ok(())
    }

    fn apply_launch_template(
        &self,
        mut base: LaunchConfigInput,
        name: &str,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<LaunchConfigInput> {
        if let Some(start) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_string());
            anyhow::bail!("launch template cycle: {}", cycle.join(" -> "));
        }
        let template = self
            .launch_templates
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown launch template {name:?}"))?;

        stack.push(name.to_string());
        for dependency in &template.use_templates {
            base = self
                .apply_launch_template(base, dependency, stack)
                .with_context(|| {
                    format!("launch template {name:?} uses launch template {dependency:?}")
                })?;
        }
        stack.pop();
        base.overlay(template)
    }

    pub fn effective_container_ssh(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ContainerSshConfig> {
        Ok(target.container_ssh.clone())
    }

    pub fn effective_control_sockets(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ControlSocketsConfig> {
        Ok(target.control_sockets.clone())
    }

    pub fn effective_lifecycle_steps(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<Vec<LifecycleStep>> {
        Ok(target.lifecycle_steps.clone())
    }

    pub fn effective_host_steps(&self, target: &TargetConfig) -> anyhow::Result<Vec<HostStep>> {
        Ok(target.host_steps.clone())
    }

    pub fn effective_container_bootstrap(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ContainerBootstrapConfig> {
        Ok(target.container_bootstrap.clone())
    }

    pub fn effective_container_bootstrap_steps(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<Vec<ContainerBootstrapStep>> {
        Ok(target.container_bootstrap_steps.clone())
    }

    pub fn effective_container_agent(
        &self,
        target: &TargetConfig,
    ) -> anyhow::Result<ContainerAgentConfig> {
        Ok(target.container_agent.clone())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigIncludeFile {
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    target_templates: BTreeMap<String, TargetConfigInput>,
    #[serde(default)]
    launch_templates: BTreeMap<String, LaunchConfigInput>,
    #[serde(default)]
    targets: BTreeMap<String, TargetConfigInput>,
    #[serde(default)]
    launches: BTreeMap<String, LaunchConfigInput>,
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
    pub fn validate(
        &self,
        launch_name: &str,
        targets: &BTreeMap<String, TargetConfig>,
    ) -> anyhow::Result<()> {
        if !targets.contains_key(&self.target) {
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfigInput {
    #[serde(default, rename = "use")]
    pub use_templates: Vec<String>,
    pub target: Option<String>,
    pub description: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub vars: BTreeMap<String, LaunchVarConfig>,
    #[serde(default)]
    pub steps: Vec<LaunchStep>,
}

impl LaunchConfigInput {
    fn validate_partial(&self, launch_name: &str) -> anyhow::Result<()> {
        for template in &self.use_templates {
            validate_name("launch template reference", template)?;
        }
        if let Some(target) = &self.target {
            validate_name("launch.target", target)?;
        }
        for (name, var) in &self.vars {
            validate_name("launch var", name)?;
            var.validate(launch_name, name)?;
        }
        let allowed = self.allowed_template_vars();
        let allowed_refs = allowed.iter().map(String::as_str).collect::<Vec<_>>();
        if let Some(cwd) = &self.cwd {
            validate_partial_launch_template("launch.cwd", cwd, &allowed_refs)?;
        }
        validate_partial_launch_env_map("launch.env", &self.env, &allowed_refs)?;
        if let Some(command) = &self.command {
            validate_command("launch.command", command)?;
            validate_partial_launch_command_templates("launch.command", command, &allowed_refs)?;
        }
        let mut step_names = BTreeSet::new();
        for step in &self.steps {
            step.validate_partial(launch_name, &allowed_refs)?;
            if !step_names.insert(step.name.clone()) {
                anyhow::bail!(
                    "launch {launch_name:?} defines duplicate step {:?}",
                    step.name
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

    fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
        if let Some(target) = &later.target {
            self.target = Some(target.clone());
        }
        if let Some(description) = &later.description {
            self.description = Some(description.clone());
        }
        if let Some(cwd) = &later.cwd {
            self.cwd = Some(cwd.clone());
        }
        self.env.extend(later.env.clone());
        if let Some(command) = &later.command {
            self.command = Some(command.clone());
        }
        self.vars.extend(later.vars.clone());
        self.steps = merge_launch_steps(self.steps, &later.steps)?;
        Ok(self)
    }

    fn into_effective(self, launch_name: &str) -> anyhow::Result<LaunchConfig> {
        Ok(LaunchConfig {
            target: self.target.ok_or_else(|| {
                anyhow::anyhow!("launch {launch_name:?} target is required after defaults")
            })?,
            description: self.description,
            cwd: self.cwd,
            env: self.env,
            command: self.command.ok_or_else(|| {
                anyhow::anyhow!("launch {launch_name:?} command is required after defaults")
            })?,
            vars: self.vars,
            steps: self.steps,
        })
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
        if self.var_type != LaunchVarType::Enum && self.values.is_some() {
            anyhow::bail!(
                "launch {launch_name:?} variable {var_name:?} values are only valid for enum variables"
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
    fn validate_partial(&self, launch_name: &str, allowed: &[&str]) -> anyhow::Result<()> {
        validate_name("launch step", &self.name)?;
        if self.phase != LaunchStepPhase::PostReady {
            anyhow::bail!(
                "launch {launch_name:?} step {:?} only supports phase = \"post_ready\"",
                self.name
            );
        }
        validate_command("launch.steps.command", &self.command)?;
        validate_partial_launch_command_templates("launch.steps.command", &self.command, allowed)?;
        if let Some(cwd) = &self.cwd {
            validate_partial_launch_template("launch.steps.cwd", cwd, allowed)?;
        }
        validate_partial_launch_env_map("launch.steps.env", &self.env, allowed)?;
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        Ok(())
    }

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

struct IncludeComposer<'a> {
    target_templates: &'a mut BTreeMap<String, TargetConfigInput>,
    launch_templates: &'a mut BTreeMap<String, LaunchConfigInput>,
    targets: &'a mut BTreeMap<String, TargetConfigInput>,
    launches: &'a mut BTreeMap<String, LaunchConfigInput>,
    seen: &'a mut BTreeSet<PathBuf>,
    stack: &'a mut BTreeSet<PathBuf>,
}

impl IncludeComposer<'_> {
    fn compose(&mut self, patterns: &[String], base_dir: &Path) -> anyhow::Result<()> {
        for path in expand_include_patterns(patterns, base_dir)? {
            let canonical = canonical_existing_path(&path)?;
            if self.stack.contains(&canonical) {
                anyhow::bail!("includes cycle detected at {}", path.display());
            }
            if !self.seen.insert(canonical.clone()) {
                continue;
            }
            self.stack.insert(canonical.clone());
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            reject_root_only_include_sections(&raw, &path)?;
            let include: ConfigIncludeFile =
                toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            let include_dir = path.parent().unwrap_or(base_dir);
            self.compose(&include.includes, include_dir)?;
            self.insert_definitions(include, &path)?;
            self.stack.remove(&canonical);
        }
        Ok(())
    }

    fn insert_definitions(
        &mut self,
        include: ConfigIncludeFile,
        path: &Path,
    ) -> anyhow::Result<()> {
        for (name, template) in include.target_templates {
            if self
                .target_templates
                .insert(name.clone(), template)
                .is_some()
            {
                anyhow::bail!(
                    "duplicate target template {name:?} from include {}",
                    path.display()
                );
            }
        }
        for (name, launch_template) in include.launch_templates {
            if self
                .launch_templates
                .insert(name.clone(), launch_template)
                .is_some()
            {
                anyhow::bail!(
                    "duplicate launch template {name:?} from include {}",
                    path.display()
                );
            }
        }
        for (name, target) in include.targets {
            if self.targets.insert(name.clone(), target).is_some() {
                anyhow::bail!("duplicate target {name:?} from include {}", path.display());
            }
        }
        for (name, launch) in include.launches {
            if self.launches.insert(name.clone(), launch).is_some() {
                anyhow::bail!("duplicate launch {name:?} from include {}", path.display());
            }
        }
        Ok(())
    }
}

fn reject_root_only_include_sections(raw: &str, path: &Path) -> anyhow::Result<()> {
    let value: toml::Value =
        toml::from_str(raw).with_context(|| format!("parse {}", path.display()))?;
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for key in [
        "schema_version",
        "default_target",
        "runtime",
        "logging",
        "ssh_dispatch",
        "client_config",
        "target_defaults",
        "launch_defaults",
    ] {
        if table.contains_key(key) {
            anyhow::bail!(
                "include {} defines root-only config section or field {key:?}",
                path.display()
            );
        }
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

    fn into_effective(self) -> WorkspaceConfig {
        WorkspaceConfig {
            path: self.path.unwrap_or_else(default_workspace_path),
            state_dir: self.state_dir.unwrap_or_else(default_workspace_state_dir),
            cleanup: self.cleanup.unwrap_or_default(),
        }
    }

    fn validate_partial(&self, target_name: &str) -> anyhow::Result<()> {
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
            "launches",
            "launch",
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
    fn builtin_defaults() -> Self {
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

    fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
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
        self.lifecycle_steps = merge_raw_steps(
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
        self.host_steps = merge_raw_steps(
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
        self.container_bootstrap_steps = merge_raw_steps(
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

    fn validate_partial(&self, target_name: &str) -> anyhow::Result<()> {
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
        if let Some(container_home) = &self.container_home
            && !container_home.is_absolute()
        {
            anyhow::bail!("target {target_name:?} container_home must be absolute");
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

    fn into_effective(self, target_name: &str) -> anyhow::Result<TargetConfig> {
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
        if let Some(container_home) = &self.container_home
            && !container_home.is_absolute()
        {
            anyhow::bail!("target {target_name:?} container_home must be absolute");
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

    fn overlay(mut self, later: &Self) -> Self {
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerSshConfig {
    pub transfer: Option<TargetContainerSshTransferConfig>,
}

impl Default for TargetContainerSshConfig {
    fn default() -> Self {
        Self {
            transfer: Some(TargetContainerSshTransferConfig {
                sftp: Some(SftpTransferMode::Allow),
                legacy_scp: Some(LegacyScpTransferMode::Allow),
            }),
        }
    }
}

impl TargetContainerSshConfig {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(transfer) = &later.transfer {
            self.transfer = Some(self.transfer.take().unwrap_or_default().overlay(transfer));
        }
        self
    }

    fn to_effective_config(&self) -> anyhow::Result<ContainerSshConfig> {
        let transfer = match &self.transfer {
            Some(transfer) => transfer.to_effective()?,
            None => TargetContainerSshTransferConfig::default().to_effective()?,
        };
        Ok(ContainerSshConfig { transfer })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerSshTransferConfig {
    pub sftp: Option<SftpTransferMode>,
    pub legacy_scp: Option<LegacyScpTransferMode>,
}

impl Default for TargetContainerSshTransferConfig {
    fn default() -> Self {
        Self {
            sftp: Some(SftpTransferMode::Allow),
            legacy_scp: Some(LegacyScpTransferMode::Allow),
        }
    }
}

impl TargetContainerSshTransferConfig {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(sftp) = later.sftp {
            self.sftp = Some(sftp);
        }
        if let Some(legacy_scp) = later.legacy_scp {
            self.legacy_scp = Some(legacy_scp);
        }
        self
    }

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

fn overlay_target_container_ssh(
    current: Option<TargetContainerSshConfig>,
    later: &TargetContainerSshConfig,
) -> TargetContainerSshConfig {
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdleCleanupConfigInput {
    pub owner: Option<IdleCleanupOwner>,
    pub action: Option<IdleCleanupAction>,
    pub idle_grace: Option<String>,
    pub preserve_processes: Option<Vec<String>>,
    pub poll_interval: Option<String>,
    pub shutdown_timeout: Option<String>,
    pub reap_signal: Option<String>,
    pub reap_kill_after: Option<String>,
}

impl IdleCleanupConfigInput {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(owner) = later.owner {
            self.owner = Some(owner);
        }
        if let Some(action) = later.action {
            self.action = Some(action);
        }
        if let Some(idle_grace) = &later.idle_grace {
            self.idle_grace = Some(idle_grace.clone());
        }
        if let Some(preserve_processes) = &later.preserve_processes {
            self.preserve_processes = Some(preserve_processes.clone());
        }
        if let Some(poll_interval) = &later.poll_interval {
            self.poll_interval = Some(poll_interval.clone());
        }
        if let Some(shutdown_timeout) = &later.shutdown_timeout {
            self.shutdown_timeout = Some(shutdown_timeout.clone());
        }
        if let Some(reap_signal) = &later.reap_signal {
            self.reap_signal = Some(reap_signal.clone());
        }
        if let Some(reap_kill_after) = &later.reap_kill_after {
            self.reap_kill_after = Some(reap_kill_after.clone());
        }
        self
    }

    fn into_effective(self) -> anyhow::Result<IdleCleanupConfig> {
        let cleanup = IdleCleanupConfig {
            owner: self.owner.unwrap_or_default(),
            action: self.action.unwrap_or_default(),
            idle_grace: self.idle_grace,
            preserve_processes: self.preserve_processes.unwrap_or_default(),
            poll_interval: self.poll_interval,
            shutdown_timeout: self.shutdown_timeout,
            reap_signal: self.reap_signal.unwrap_or_else(default_reap_signal),
            reap_kill_after: self.reap_kill_after,
        };
        cleanup.validate()?;
        Ok(cleanup)
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
    fn overlay(mut self, later: &Self) -> Self {
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

    fn into_effective(self) -> LocalSshConfig {
        LocalSshConfig {
            mode: self.mode.unwrap_or_default(),
            backend: self.backend.unwrap_or_default(),
            readiness: self.readiness.unwrap_or_default(),
            host: self.host.unwrap_or_else(default_listen_host),
            port: self.port,
        }
    }

    fn validate_partial(&self) -> anyhow::Result<()> {
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
    fn to_effective_without_inherited(&self) -> anyhow::Result<LifecycleStep> {
        self.to_effective(None)
    }

    fn from_effective(step: LifecycleStep) -> Self {
        Self {
            phase: step.phase,
            name: step.name,
            enabled: true,
            before: None,
            after: None,
            required: Some(step.required),
            command: Some(step.command),
            timeout: step.timeout,
        }
    }

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
    fn to_effective_without_inherited(&self) -> anyhow::Result<HostStep> {
        self.to_effective(None)
    }

    fn from_effective(step: HostStep) -> Self {
        Self {
            name: step.name,
            enabled: true,
            before: None,
            after: None,
            required: Some(step.required),
            command: Some(step.command),
            health_check: step.health_check,
            timeout: step.timeout,
        }
    }

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
    fn from_effective(step: ContainerBootstrapStep) -> Self {
        Self {
            name: step.name,
            enabled: true,
            before: None,
            after: None,
            required: Some(step.required),
            user: Some(step.user),
            command: Some(step.command),
            timeout: step.timeout,
        }
    }

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
        self.validate_common()
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
        self.validate_common()
    }

    fn validate_common(&self) -> anyhow::Result<()> {
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
    fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
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

    fn into_effective(self) -> anyhow::Result<ContainerAgentConfig> {
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

    fn validate_partial(&self) -> anyhow::Result<()> {
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
    fn overlay(mut self, later: &Self) -> Self {
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

    fn into_effective(self) -> SshBridgeConfig {
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

const CONTROL_SOCKET_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "target",
    "image",
    "image_slug",
    "container_name",
    "session_id",
    "runtime_id",
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
    "container_pid",
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

fn merge_services(
    mut inherited: Vec<ServiceConfig>,
    later: &[ServiceConfig],
) -> anyhow::Result<Vec<ServiceConfig>> {
    let mut keys: Vec<String> = inherited
        .iter()
        .map(|service| service.name.clone())
        .collect();
    if keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
        anyhow::bail!("container_agent.services contains duplicate inherited keys");
    }
    for service in later {
        if let Some(index) = keys.iter().position(|key| key == &service.name) {
            inherited[index] = service.clone();
        } else {
            keys.push(service.name.clone());
            inherited.push(service.clone());
        }
    }
    Ok(inherited)
}

fn merge_launch_steps(
    mut inherited: Vec<LaunchStep>,
    later: &[LaunchStep],
) -> anyhow::Result<Vec<LaunchStep>> {
    let mut keys: Vec<String> = inherited.iter().map(|step| step.name.clone()).collect();
    if keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
        anyhow::bail!("launch.steps contains duplicate inherited keys");
    }
    for step in later {
        if let Some(index) = keys.iter().position(|key| key == &step.name) {
            inherited[index] = step.clone();
        } else {
            keys.push(step.name.clone());
            inherited.push(step.clone());
        }
    }
    Ok(inherited)
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

fn validate_partial_launch_template(
    field: &str,
    value: &str,
    allowed: &[&str],
) -> anyhow::Result<()> {
    for key in template::referenced_keys(value).with_context(|| format!("validate {field}"))? {
        if allowed.contains(&key) {
            continue;
        }
        if let Some(var_name) = key.strip_prefix("var.") {
            validate_name("launch var", var_name).with_context(|| format!("validate {field}"))?;
            continue;
        }
        return Err(anyhow::anyhow!(
            "unknown interpolation variable {{{key}}} in {value:?}"
        ))
        .with_context(|| format!("validate {field}"));
    }
    Ok(())
}

fn validate_partial_launch_command_templates(
    field: &str,
    command: &[String],
    allowed: &[&str],
) -> anyhow::Result<()> {
    for arg in command {
        validate_partial_launch_template(field, arg, allowed)?;
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

fn reject_template_use(field: &str, templates: &[String]) -> anyhow::Result<()> {
    if !templates.is_empty() {
        anyhow::bail!("{field} does not support use");
    }
    Ok(())
}

pub(crate) fn validate_name(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
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

fn validate_partial_launch_env_map(
    field: &str,
    env: &BTreeMap<String, String>,
    allowed: &[&str],
) -> anyhow::Result<()> {
    for (key, value) in env {
        validate_env_key(key)?;
        validate_partial_launch_template(&format!("{field}.{key}"), value, allowed)?;
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

fn default_control_socket_host_dir() -> String {
    "/run/user/{uid}/aw-gateway/{runtime_id}".into()
}

fn default_control_socket_container_dir() -> String {
    "/run/aw-gateway".into()
}

fn default_true() -> bool {
    true
}

fn default_enabled_gateway_actions() -> Vec<String> {
    [
        "connect",
        "up",
        "run",
        "launches",
        "launch",
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
    fn target_workspace_cleanup_defaults_to_never() {
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
        cfg.validate().unwrap();

        assert_eq!(
            cfg.effective_target("default").unwrap().workspace.cleanup,
            WorkspaceCleanup::Never
        );
    }

    #[test]
    fn target_workspace_cleanup_accepts_ephemeral_target_workspace() {
        for value in ["success", "always"] {
            let cfg: GatewayConfig = toml::from_str(&format!(
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{{session_id}}"
stop_when_idle = true
[targets.default.workspace]
path = "{{home}}/.cache/aw-gateway/workspaces/{{target}}-{{session_id}}"
cleanup = "{value}"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#
            ))
            .unwrap();
            cfg.validate().unwrap();
        }
    }

    #[test]
    fn target_workspace_cleanup_rejects_fixed_targets() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
[targets.default.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
cleanup = "always"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
        )
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("workspace.cleanup requires mode = \"ephemeral\""),
            "{err}"
        );
    }

    #[test]
    fn fixed_target_rejects_inherited_cleanup_default() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
cleanup = "always"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("workspace.cleanup requires mode = \"ephemeral\""),
            "{err}"
        );
    }

    #[test]
    fn target_workspace_cleanup_rejects_workspace_without_session_id() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true
[targets.default.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}"
cleanup = "success"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
        )
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("workspace.cleanup requires workspace.path to reference {session_id}"),
            "{err}"
        );
    }

    #[test]
    fn target_workspace_cleanup_rejects_workspace_outside_aw_gateway_component() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true
[targets.default.workspace]
path = "{home}/sessions/{target}-{session_id}"
cleanup = "always"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
        )
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains(
                "workspace.cleanup requires workspace.path under an aw-gateway path component"
            ),
            "{err}"
        );
    }

    #[test]
    fn target_workspace_cleanup_requires_gateway_owned_exit_cleanup() {
        for (name, idle_cleanup, expected) in [
            (
                "missing",
                "",
                "workspace.cleanup requires gateway-owned idle_cleanup",
            ),
            (
                "agent",
                r#"
[targets.default.idle_cleanup]
owner = "agent"
action = "exit_container"
"#,
                "workspace.cleanup requires gateway-owned exit_container idle_cleanup",
            ),
            (
                "none-action",
                r#"
[targets.default.idle_cleanup]
owner = "gateway"
action = "none"
"#,
                "workspace.cleanup requires gateway-owned exit_container idle_cleanup",
            ),
            (
                "preserve",
                r#"
[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
preserve_processes = ["tmux"]
"#,
                "workspace.cleanup does not support preserve_processes",
            ),
        ] {
            let cfg: GatewayConfig = toml::from_str(&format!(
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{{session_id}}"
stop_when_idle = true
[targets.default.workspace]
path = "{{home}}/.cache/aw-gateway/workspaces/{{target}}-{{session_id}}"
cleanup = "always"
{idle_cleanup}
"#
            ))
            .unwrap();

            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "{name}: {err}");
        }
    }

    #[test]
    fn path_segment_names_reject_dot_segments() {
        assert!(validate_name("target", ".").is_err());
        assert!(validate_name("target", "..").is_err());
        validate_name("target", "dev.shell-1").unwrap();
    }

    #[test]
    fn control_sockets_defaults_and_overrides_are_effective() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.custom.control_sockets]
container_dir = "/tmp/aw-gateway"

[targets.custom]
image = "ubuntu/custom"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let default_target = cfg.effective_target("default").unwrap();
        let default = cfg.effective_control_sockets(&default_target).unwrap();
        assert_eq!(default.host_dir, "/run/user/{uid}/aw-gateway/{runtime_id}");
        assert_eq!(default.container_dir, "/run/aw-gateway");

        let custom_target = cfg.effective_target("custom").unwrap();
        let custom = cfg.effective_control_sockets(&custom_target).unwrap();
        assert_eq!(custom.host_dir, "/run/user/{uid}/aw-gateway/{runtime_id}");
        assert_eq!(custom.container_dir, "/tmp/aw-gateway");
    }

    #[test]
    fn control_sockets_global_override_can_be_overlaid_per_target() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults.control_sockets]
host_dir = "/tmp/aw/{runtime_id}"
container_dir = "/run/global"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.control_sockets]
host_dir = "/var/run/aw/{runtime_id}"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let target = cfg.effective_target("default").unwrap();
        let effective = cfg.effective_control_sockets(&target).unwrap();
        assert_eq!(effective.host_dir, "/var/run/aw/{runtime_id}");
        assert_eq!(effective.container_dir, "/run/global");
    }

    #[test]
    fn gateway_config_rejects_old_socket_path_sources() {
        for (config, expected) in [
            (
                r#"
schema_version = "1"

[container_agent]
control_socket = "/run/aw-gateway/agent.sock"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
                "unknown field `container_agent`",
            ),
            (
                r#"
schema_version = "1"

[target_defaults.container_agent.ssh_bridge]
enabled = true
socket = "/run/aw-gateway/ssh.sock"
target = "127.0.0.1:22"
mode = "0600"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
                "container_agent.ssh_bridge.socket is managed by control_sockets.container_dir",
            ),
        ] {
            let err = match toml::from_str::<GatewayConfig>(config) {
                Ok(cfg) => format!("{:#}", cfg.validate().unwrap_err()),
                Err(err) => err.to_string(),
            };
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn standalone_agent_config_still_accepts_explicit_socket_paths() {
        let cfg: ContainerAgentFile = toml::from_str(
            r#"
schema_version = "1"

[container_agent]
control_socket = "/run/aw-gateway/agent.sock"

[container_agent.ssh_bridge]
enabled = true
socket = "/run/aw-gateway/ssh.sock"
target = "127.0.0.1:22"
mode = "0600"
"#,
        )
        .unwrap();
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
        let target = cfg.effective_target("default").unwrap();
        assert_eq!(target.container_ssh.transfer.sftp, SftpTransferMode::Allow);
        assert_eq!(
            target.container_ssh.transfer.legacy_scp,
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

[target_defaults.container_ssh.transfer]
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
            let target = cfg.effective_target("default").unwrap();
            assert_eq!(target.container_ssh.transfer.sftp, sftp);
            assert_eq!(target.container_ssh.transfer.legacy_scp, legacy_scp);
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

[[target_defaults.container_mounts]]
source = "{state_dir}/bootstrap/aw-container-agent"
target = "/opt/aw-gateway/bin/aw-container-agent"
mode = "ro"

[[targets.default.container_mounts]]
source = "{state_dir}/bootstrap/target-only"
target = "/opt/aw-gateway/target-only"
mode = "ro"

[target_defaults.container_bootstrap]
enabled = true
entrypoint = "/opt/aw-gateway/bin/aw-container-bootstrap"
agent_program = "/opt/aw-gateway/bin/aw-container-agent"

[[target_defaults.container_bootstrap_steps]]
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

[[target_defaults.host_steps]]
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

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "250ms"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/true"]
timeout = "2m"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let target = cfg.effective_target("default").unwrap();
        assert_eq!(
            cfg.effective_lifecycle_steps(&target).unwrap()[0]
                .timeout
                .as_deref(),
            Some("250ms")
        );
        assert_eq!(
            cfg.effective_host_steps(&target).unwrap()[0]
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

[[target_defaults.lifecycle_steps]]
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

[[target_defaults.host_steps]]
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

[target_defaults.container_agent]
enabled = false

[target_defaults.container_agent.ssh_bridge]
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
    fn ssh_dispatch_defaults_include_launch_actions() {
        let cfg = SshDispatchConfig::default();
        assert!(
            cfg.enabled_gateway_actions
                .iter()
                .any(|action| action == "launches")
        );
        assert!(
            cfg.enabled_gateway_actions
                .iter()
                .any(|action| action == "launch")
        );
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
[targets.default.workspace]
path = "{home}/workspace-internal"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.effective_target("default").unwrap().workspace.path,
            "{home}/workspace-internal"
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

[[target_defaults.container_agent.services]]
name = "acl-proxy"
command = ["acl-proxy", "--config", "/etc/acl-proxy/acl-proxy.toml"]

[[target_defaults.container_agent.services]]
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
        let target = cfg.effective_target("default").unwrap();
        let effective = cfg.effective_container_agent(&target).unwrap();
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

[target_defaults.container_ssh.transfer]
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
        let target = cfg.effective_target("default").unwrap();
        let effective = cfg.effective_container_ssh(&target).unwrap();
        assert_eq!(effective.transfer.sftp, SftpTransferMode::Deny);
        assert_eq!(
            effective.transfer.legacy_scp,
            LegacyScpTransferMode::Outbound
        );
    }

    #[test]
    fn target_container_ssh_transfer_overlays_fields_independently() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults.container_ssh.transfer]
sftp = "allow"
legacy_scp = "inbound"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_ssh.transfer]
sftp = "deny"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let effective = cfg.effective_target("default").unwrap().container_ssh;
        assert_eq!(effective.transfer.sftp, SftpTransferMode::Deny);
        assert_eq!(
            effective.transfer.legacy_scp,
            LegacyScpTransferMode::Inbound
        );
    }

    #[test]
    fn target_container_bootstrap_overlays_global_fields() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults.container_bootstrap]
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
        let target = cfg.effective_target("default").unwrap();
        let effective = cfg.effective_container_bootstrap(&target).unwrap();
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

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "first"
command = ["/bin/first"]

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "replace-me"
command = ["/bin/old"]

[[target_defaults.lifecycle_steps]]
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
        let target = cfg.effective_target("default").unwrap();
        let effective = cfg.effective_lifecycle_steps(&target).unwrap();
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

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/prep"]
timeout = "10s"

[[target_defaults.host_steps]]
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
        let target = cfg.effective_target("default").unwrap();
        let lifecycle = cfg.effective_lifecycle_steps(&target).unwrap();
        assert_eq!(lifecycle[0].command, ["/bin/prep"]);
        assert_eq!(lifecycle[0].timeout.as_deref(), Some("20s"));
        let host = cfg.effective_host_steps(&target).unwrap();
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

[[target_defaults.host_steps]]
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

[[target_defaults.host_steps]]
name = "duplicate"
command = ["/bin/one"]

[[target_defaults.host_steps]]
name = "duplicate"
command = ["/bin/two"]
"#,
                "target \"target_defaults\" defines duplicate host_steps duplicate",
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

[[target_defaults.host_steps]]
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

[[target_defaults.lifecycle_steps]]
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
    fn target_defaults_overlay_into_effective_target_shape() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/base"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true

[target_defaults.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
state_dir = ".state"
cleanup = "success"

[target_defaults.runtime]
extra_run_args = ["--default"]

[target_defaults.container_env]
KEEP = "default"
OVERRIDE = "default"

[target_defaults.session_env]
SESSION = "default"

[[target_defaults.container_mounts]]
source = "/tmp/default"
target = "/mnt/default"
mode = "ro"

[target_defaults.idle_cleanup]
owner = "gateway"
action = "exit_container"

[target_defaults.container_ssh.transfer]
sftp = "deny"
legacy_scp = "inbound"

[target_defaults.control_sockets]
host_dir = "/tmp/aw/{runtime_id}"
container_dir = "/run/default"

[target_defaults.container_bootstrap]
enabled = true
entrypoint = "/default/bootstrap"
agent_program = "/default/agent"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/default-prep"]
timeout = "10s"

[[target_defaults.host_steps]]
name = "host-prep"
command = ["/bin/default-host"]

[[target_defaults.container_bootstrap_steps]]
name = "bootstrap-default"
command = ["/bin/default-bootstrap"]

[target_defaults.container_agent]
enabled = true

[[target_defaults.container_agent.services]]
name = "svc"
command = ["/bin/default-service"]

[targets.default]
image = "ubuntu/dev"
name = "{image_slug}"

[targets.default.container_env]
OVERRIDE = "target"
TARGET_ONLY = "target"

[[targets.default.container_mounts]]
source = "/tmp/target"
target = "/mnt/target"
mode = "rw"

[targets.default.runtime]
extra_run_args = ["--target"]

[targets.default.workspace]
cleanup = "always"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "prep"
timeout = "20s"

[[targets.default.host_steps]]
name = "host-prep"
command = ["/bin/target-host"]

[[targets.default.container_bootstrap_steps]]
name = "bootstrap-default"
enabled = false

[[targets.default.container_agent.services]]
name = "svc"
command = ["/bin/target-service"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let target = cfg.effective_target("default").unwrap();
        assert_eq!(target.image, "ubuntu/dev");
        assert_eq!(target.mode, TargetMode::Ephemeral);
        assert_eq!(target.workspace.state_dir, ".state");
        assert_eq!(target.workspace.cleanup, WorkspaceCleanup::Always);
        assert_eq!(target.runtime.extra_run_args, ["--target"]);
        assert_eq!(target.container_env["KEEP"], "default");
        assert_eq!(target.container_env["OVERRIDE"], "target");
        assert_eq!(target.container_mounts.len(), 2);
        assert_eq!(target.container_ssh.transfer.sftp, SftpTransferMode::Deny);
        assert_eq!(target.control_sockets.container_dir, "/run/default");
        assert_eq!(target.container_bootstrap.entrypoint, "/default/bootstrap");
        assert_eq!(target.lifecycle_steps[0].command, ["/bin/default-prep"]);
        assert_eq!(target.lifecycle_steps[0].timeout.as_deref(), Some("20s"));
        assert_eq!(target.host_steps[0].command, ["/bin/target-host"]);
        assert!(target.container_bootstrap_steps.is_empty());
        assert_eq!(
            target.container_agent.services[0].command,
            ["/bin/target-service"]
        );
    }

    #[test]
    fn target_templates_overlay_in_use_order_and_allow_concrete_overrides() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"
mode = "fixed"
name = "default-name"

[target_defaults.container_env]
KEEP = "default"
OVERRIDE = "default"

[target_templates.runtime]
image = "ubuntu/runtime"
container_user = "worker"

[target_templates.runtime.container_env]
RUNTIME = "true"
OVERRIDE = "runtime"

[target_templates.policy]
name = "policy-name"

[target_templates.policy.container_env]
POLICY = "true"
OVERRIDE = "policy"

[targets.default]
use = ["runtime", "policy"]
image = "ubuntu/final"
name = "final-name"

[targets.default.container_env]
TARGET = "true"
OVERRIDE = "target"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let target = cfg.effective_target("default").unwrap();
        assert_eq!(target.image, "ubuntu/final");
        assert_eq!(target.name.as_deref(), Some("final-name"));
        assert_eq!(target.container_user.as_deref(), Some("worker"));
        assert_eq!(target.container_env["KEEP"], "default");
        assert_eq!(target.container_env["RUNTIME"], "true");
        assert_eq!(target.container_env["POLICY"], "true");
        assert_eq!(target.container_env["TARGET"], "true");
        assert_eq!(target.container_env["OVERRIDE"], "target");
    }

    #[test]
    fn target_templates_can_nest_and_override_inherited_steps() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"
mode = "fixed"
name = "default-name"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/firewall"]
timeout = "10s"

[target_templates.timeout-policy]

[[target_templates.timeout-policy.host_steps]]
name = "firewall"
timeout = "30s"

[target_templates.runtime]
use = ["timeout-policy"]
container_home = "/home/worker"

[targets.default]
use = ["runtime"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let target = cfg.effective_target("default").unwrap();
        assert_eq!(
            target.container_home.as_deref(),
            Some(Path::new("/home/worker"))
        );
        assert_eq!(target.host_steps[0].command, ["/bin/firewall"]);
        assert_eq!(target.host_steps[0].timeout.as_deref(), Some("30s"));
    }

    #[test]
    fn target_template_cycles_and_unknown_names_are_rejected() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_templates.a]
use = ["b"]

[target_templates.b]
use = ["a"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("target template cycle: a -> b -> a"), "{err}");

        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
use = ["missing"]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("target \"default\" uses target template \"missing\""),
            "{err}"
        );
        assert!(err.contains("unknown target template \"missing\""), "{err}");
    }

    #[test]
    fn target_template_effective_validation_runs_after_overlay() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_templates.ephemeral]
mode = "ephemeral"

[targets.default]
use = ["ephemeral"]
image = "ubuntu/dev"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("ephemeral target \"default\" requires ephemeral_name"),
            "{err}"
        );
    }

    #[test]
    fn target_defaults_do_not_support_use() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults]
use = ["base"]

[target_templates.base]
image = "ubuntu/dev"

[targets.default]
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("target_defaults does not support use"),
            "{err}"
        );
    }

    #[test]
    fn template_names_use_target_and_launch_name_validation() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_templates."bad name"]
image = "ubuntu/dev"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("target template"), "{err}");
        assert!(err.contains("bad name"), "{err}");

        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates."bad name"]
target = "default"
command = ["true"]

[launches.agent]
target = "default"
command = ["true"]
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("launch template"), "{err}");
        assert!(err.contains("bad name"), "{err}");
    }

    #[test]
    fn target_defaults_can_supply_required_image() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"

[targets.default]
mode = "fixed"
name = "{image_slug}"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.effective_target("default").unwrap().image,
            "ubuntu/default"
        );
    }

    #[test]
    fn target_defaults_validate_present_fields_before_overlay() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"
default_target = "default"

[target_defaults]
image = "scratch/dev"
name = "scratch-dev"

[target_defaults.workspace]
path = "{bad}"

[targets.default]

[targets.default.workspace]
path = "workspace"
"#,
        )
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("unknown interpolation variable"), "{err}");
        assert!(err.contains("target.workspace.path"), "{err}");
    }

    #[test]
    fn root_only_rejects_target_shaped_sections_at_root() {
        for config in [
            r#"schema_version = "1"
[workspace]
path = "workspace"
"#,
            r#"schema_version = "1"
[control_sockets]
container_dir = "/run/aw-gateway"
"#,
            r#"schema_version = "1"
[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
"#,
            r#"schema_version = "1"
[container_agent]
enabled = false
"#,
        ] {
            let err = toml::from_str::<GatewayConfig>(config).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{err}");
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

[target_defaults.container_agent]
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

[target_defaults.container_agent.ssh_bridge]
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
env = { FLAG = "{var.flag}", LIMIT = "{var.limit}", PID = "{container_pid}" }
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
    fn launch_defaults_overlay_into_effective_launch_shape() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
description = "Default description"
cwd = "{container_home}/default"
env = { KEEP = "default", OVERRIDE = "default" }
command = ["default-command"]

[launch_defaults.vars]
repo = { type = "string", required = true }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }

[[launch_defaults.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["default-prep"]
env = { STEP = "default" }

[launches.agent]
cwd = "{container_home}/agent"
env = { OVERRIDE = "launch", LAUNCH_ONLY = "launch" }
command = ["agent", "{var.repo}"]

[launches.agent.vars]
branch = { type = "string", default = "main" }

[[launches.agent.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["launch-prep"]

[[launches.agent.steps]]
phase = "post_ready"
location = "host"
name = "host-prep"
command = ["host-prep"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let launch = cfg.effective_launch("agent").unwrap();
        assert_eq!(launch.target, "default");
        assert_eq!(launch.description.as_deref(), Some("Default description"));
        assert_eq!(launch.cwd.as_deref(), Some("{container_home}/agent"));
        assert_eq!(launch.command, ["agent", "{var.repo}"]);
        assert_eq!(launch.env["KEEP"], "default");
        assert_eq!(launch.env["OVERRIDE"], "launch");
        assert!(launch.vars.contains_key("repo"));
        assert!(launch.vars.contains_key("branch"));
        assert_eq!(launch.steps.len(), 2);
        assert_eq!(launch.steps[0].name, "prep");
        assert_eq!(launch.steps[0].command, ["launch-prep"]);
        assert_eq!(launch.steps[1].name, "host-prep");
    }

    #[test]
    fn launch_templates_overlay_in_use_order_and_allow_concrete_overrides() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
description = "Default description"
cwd = "{container_home}/default"
env = { KEEP = "default", OVERRIDE = "default" }
command = ["default-command"]

[launch_templates.repo]
cwd = "{container_home}/repo"
env = { REPO = "{var.repo}", OVERRIDE = "repo" }

[launch_templates.repo.vars]
repo = { type = "string", required = true }

[[launch_templates.repo.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["repo-prep", "{var.repo}"]

[launch_templates.codex]
command = ["codex", "exec", "{var.repo}"]
env = { CODEX_HOME = "{container_home}/.codex", OVERRIDE = "codex" }

[launches.review]
use = ["repo", "codex"]
description = "Review repo"
command = ["codex", "exec", "review", "{var.repo}"]
env = { OVERRIDE = "launch" }

[[launches.review.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["launch-prep", "{var.repo}"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let launch = cfg.effective_launch("review").unwrap();
        assert_eq!(launch.target, "default");
        assert_eq!(launch.description.as_deref(), Some("Review repo"));
        assert_eq!(launch.cwd.as_deref(), Some("{container_home}/repo"));
        assert_eq!(launch.command, ["codex", "exec", "review", "{var.repo}"]);
        assert_eq!(launch.env["KEEP"], "default");
        assert_eq!(launch.env["REPO"], "{var.repo}");
        assert_eq!(launch.env["CODEX_HOME"], "{container_home}/.codex");
        assert_eq!(launch.env["OVERRIDE"], "launch");
        assert!(launch.vars.contains_key("repo"));
        assert_eq!(launch.steps.len(), 1);
        assert_eq!(launch.steps[0].command, ["launch-prep", "{var.repo}"]);
    }

    #[test]
    fn launch_templates_can_nest() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.base]
target = "default"

[launch_templates.command]
use = ["base"]
command = ["true"]

[launches.agent]
use = ["command"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let launch = cfg.effective_launch("agent").unwrap();
        assert_eq!(launch.target, "default");
        assert_eq!(launch.command, ["true"]);
    }

    #[test]
    fn launch_template_cycles_and_unknown_names_are_rejected() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.a]
use = ["b"]

[launch_templates.b]
use = ["a"]

[launches.agent]
target = "default"
command = ["true"]
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("launch template cycle: a -> b -> a"), "{err}");

        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
use = ["missing"]
target = "default"
command = ["true"]
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("launch \"agent\" uses launch template \"missing\""),
            "{err}"
        );
        assert!(err.contains("unknown launch template \"missing\""), "{err}");
    }

    #[test]
    fn launch_template_effective_validation_runs_after_overlay() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.target]
target = "default"

[launches.agent]
use = ["target"]
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("launch \"agent\" command is required after defaults"),
            "{err}"
        );
    }

    #[test]
    fn launch_defaults_do_not_support_use() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
use = ["base"]

[launch_templates.base]
target = "default"
command = ["true"]

[launches.agent]
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(
            err.contains("launch_defaults does not support use"),
            "{err}"
        );
    }

    #[test]
    fn launch_defaults_are_validated_after_overlay() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"

[launches.agent]
command = ["true"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
command = ["true"]
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("target is required after defaults"), "{err}");
    }

    #[test]
    fn launch_defaults_validate_present_fields_without_launches() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"
default_target = "default"

[target_defaults]
image = "scratch/dev"
name = "scratch-dev"

[targets.default]

[launch_defaults.vars."bad name"]
type = "string"
"#,
        )
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("launch var"), "{err}");
        assert!(err.contains("bad name"), "{err}");
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
repo = { type = "string", values = ["main"] }
"#,
                "values are only valid for enum variables",
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
debug = { type = "boolean", values = ["true"] }
"#,
                "values are only valid for enum variables",
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
count = { type = "number", values = ["1"] }
"#,
                "values are only valid for enum variables",
            ),
        ] {
            let cfg: GatewayConfig = toml::from_str(config).unwrap();
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn pre_pid_templates_reject_container_pid() {
        for (config, expected) in [
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_env]
PID = "{container_pid}"
"#,
                "target.container_env.PID",
            ),
            (
                r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "pre"
command = ["echo", "{container_pid}"]
"#,
                "target.lifecycle_steps",
            ),
        ] {
            let cfg: GatewayConfig = toml::from_str(config).unwrap();
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "{err}");
            assert!(err.contains("unknown interpolation variable"), "{err}");
        }
    }

    #[test]
    fn includes_parse_and_compose_targets_and_launches() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workspace.toml"),
            r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"

[launch_templates.shell]
target = "default"
command = ["true"]

[targets.default]
use = ["base"]
name = "default"

[launches.agent]
use = ["shell"]
"#,
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            r#"
schema_version = "1"
includes = ["config.d/*.toml"]
"#,
        )
        .unwrap();

        let cfg = GatewayConfig::load(&root).unwrap();
        assert_eq!(
            cfg.effective_target("default").unwrap().image,
            "ubuntu/base"
        );
        assert_eq!(cfg.effective_launch("agent").unwrap().command, ["true"]);
    }

    #[test]
    fn includes_resolve_relative_to_declaring_file_and_support_nested() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        let nested_dir = config_dir.join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(
            config_dir.join("root-fragment.toml"),
            r#"
includes = ["nested/*.toml"]

[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"
"#,
        )
        .unwrap();
        std::fs::write(
            nested_dir.join("target.toml"),
            r#"
[targets.default]
use = ["base"]
"#,
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            r#"
schema_version = "1"
includes = ["config.d/root-fragment.toml"]
"#,
        )
        .unwrap();

        let cfg = GatewayConfig::load(&root).unwrap();
        assert_eq!(
            cfg.effective_target("default").unwrap().image,
            "ubuntu/base"
        );
    }

    #[test]
    fn includes_load_shared_fragments_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("frag1.toml"),
            r#"
includes = ["shared.toml"]
"#,
        )
        .unwrap();
        std::fs::write(
            config_dir.join("frag2.toml"),
            r#"
includes = ["shared.toml"]
"#,
        )
        .unwrap();
        std::fs::write(
            config_dir.join("shared.toml"),
            r#"
[targets.default]
image = "ubuntu/shared"
mode = "fixed"
name = "shared"
"#,
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            r#"
schema_version = "1"
includes = ["config.d/frag1.toml", "config.d/frag2.toml"]
"#,
        )
        .unwrap();

        let cfg = GatewayConfig::load(&root).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(
            cfg.effective_target("default").unwrap().image,
            "ubuntu/shared"
        );
    }

    #[test]
    fn includes_expand_globs_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("a.toml"),
            r#"
[targets.dup]
image = "ubuntu/a"
mode = "fixed"
name = "a"
"#,
        )
        .unwrap();
        std::fs::write(
            config_dir.join("b.toml"),
            r#"
[targets.dup]
image = "ubuntu/b"
mode = "fixed"
name = "b"
"#,
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            r#"
schema_version = "1"
default_target = "dup"
includes = ["config.d/*.toml"]
"#,
        )
        .unwrap();

        let err = GatewayConfig::load(&root).unwrap_err().to_string();
        assert!(err.contains("duplicate target"), "{err}");
        assert!(
            err.contains(config_dir.join("b.toml").to_str().unwrap()),
            "{err}"
        );
    }

    #[test]
    fn included_templates_can_be_used_by_root_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("templates.toml"),
            r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"

[launch_templates.base]
target = "default"
command = ["true"]
"#,
        )
        .unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            r#"
schema_version = "1"
includes = ["config.d/*.toml"]

[targets.default]
use = ["base"]

[launches.agent]
use = ["base"]
"#,
        )
        .unwrap();

        let cfg = GatewayConfig::load(&root).unwrap();
        assert_eq!(
            cfg.effective_target("default").unwrap().image,
            "ubuntu/base"
        );
        assert_eq!(cfg.effective_launch("agent").unwrap().command, ["true"]);
    }

    #[test]
    fn includes_reject_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        for (case, root_definition, include_definition, expected) in [
            (
                "target-template",
                "[target_templates.base]\nimage = \"ubuntu/root\"\n",
                "[target_templates.base]\nimage = \"ubuntu/include\"\n",
                "duplicate target template",
            ),
            (
                "launch-template",
                "[launch_templates.base]\ncommand = [\"root\"]\n",
                "[launch_templates.base]\ncommand = [\"include\"]\n",
                "duplicate launch template",
            ),
            (
                "target",
                "[targets.extra]\nimage = \"ubuntu/root\"\nmode = \"fixed\"\nname = \"root\"\n",
                "[targets.extra]\nimage = \"ubuntu/include\"\nmode = \"fixed\"\nname = \"include\"\n",
                "duplicate target",
            ),
            (
                "launch",
                "[launches.agent]\ntarget = \"default\"\ncommand = [\"root\"]\n",
                "[launches.agent]\ntarget = \"default\"\ncommand = [\"include\"]\n",
                "duplicate launch",
            ),
        ] {
            let case_dir = dir.path().join(case);
            let config_dir = case_dir.join("config.d");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(config_dir.join("fragment.toml"), include_definition).unwrap();
            let root = case_dir.join("gateway.toml");
            std::fs::write(
                &root,
                format!(
                    r#"
schema_version = "1"
includes = ["config.d/*.toml"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "default"

{root_definition}
"#
                ),
            )
            .unwrap();
            let err = GatewayConfig::load(&root).unwrap_err().to_string();
            assert!(err.contains(expected), "{case}: {err}");
        }
    }

    #[test]
    fn includes_reject_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml");
        let b = dir.path().join("b.toml");
        std::fs::write(
            &a,
            format!(
                r#"
includes = ["{}"]
"#,
                b.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &b,
            format!(
                r#"
includes = ["{}"]
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
includes = ["{}"]

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
    }

    #[test]
    fn includes_reject_root_only_sections_and_unknown_fields() {
        for (case, fragment, expected) in [
            ("schema-version", "schema_version = \"1\"\n", "root-only"),
            (
                "default-target",
                "default_target = \"other\"\n",
                "root-only",
            ),
            ("runtime", "[runtime]\ntype = \"podman\"\n", "root-only"),
            ("logging", "[logging]\nlevel = \"debug\"\n", "root-only"),
            (
                "ssh-dispatch",
                "[ssh_dispatch]\nallow_interactive_shell = false\n",
                "root-only",
            ),
            (
                "client-config",
                "[client_config]\nhost = \"example.test\"\n",
                "root-only",
            ),
            (
                "target-defaults",
                "[target_defaults]\nimage = \"ubuntu\"\n",
                "root-only",
            ),
            (
                "launch-defaults",
                "[launch_defaults]\ntarget = \"default\"\n",
                "root-only",
            ),
            ("unknown", "unknown = true\n", "unknown field"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("config.d");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(config_dir.join("fragment.toml"), fragment).unwrap();
            let root = dir.path().join("gateway.toml");
            std::fs::write(
                &root,
                r#"
schema_version = "1"
includes = ["config.d/*.toml"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "default"
"#,
            )
            .unwrap();

            let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
            assert!(err.contains(expected), "{case}: {err}");
        }
    }

    #[test]
    fn legacy_split_include_keys_are_rejected() {
        for (case, root_extra, include_extra) in [
            ("root-target", "target_includes = []\n", ""),
            ("root-launch", "launch_includes = []\n", ""),
            ("include-target", "", "target_includes = []\n"),
            ("include-launch", "", "launch_includes = []\n"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("config.d");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(config_dir.join("fragment.toml"), include_extra).unwrap();
            let root = dir.path().join("gateway.toml");
            std::fs::write(
                &root,
                format!(
                    r#"
schema_version = "1"
{root_extra}
includes = ["config.d/*.toml"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "default"
"#
                ),
            )
            .unwrap();

            let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
            assert!(
                err.contains("target_includes") || err.contains("launch_includes"),
                "{case}: {err}"
            );
        }
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
