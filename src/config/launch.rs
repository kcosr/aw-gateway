use super::{TargetConfig, validation::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
    pub(super) fn validate_partial(&self, launch_name: &str) -> anyhow::Result<()> {
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

    pub(super) fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
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

    pub(super) fn into_effective(self, launch_name: &str) -> anyhow::Result<LaunchConfig> {
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
