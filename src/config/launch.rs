use super::{TargetConfig, validation::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    pub target: String,
    pub description: Option<String>,
    #[serde(default)]
    pub allow_args: bool,
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
        let policy = LaunchValidationPolicy::effective(targets);
        validate_launch_shape(
            launch_name,
            LaunchShape {
                target: Some(&self.target),
                allow_args: Some(self.allow_args),
                cwd: self.cwd.as_deref(),
                env: &self.env,
                command: Some(&self.command),
                vars: &self.vars,
                steps: &self.steps,
            },
            policy,
        )
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfigInput {
    #[serde(default, rename = "use")]
    pub use_templates: Vec<String>,
    pub target: Option<String>,
    pub description: Option<String>,
    pub allow_args: Option<bool>,
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
        validate_launch_shape(
            launch_name,
            LaunchShape {
                target: self.target.as_deref(),
                allow_args: self.allow_args,
                cwd: self.cwd.as_deref(),
                env: &self.env,
                command: self.command.as_deref(),
                vars: &self.vars,
                steps: &self.steps,
            },
            LaunchValidationPolicy::partial(),
        )
    }

    pub(super) fn overlay(mut self, later: &Self) -> anyhow::Result<Self> {
        if let Some(target) = &later.target {
            self.target = Some(target.clone());
        }
        if let Some(description) = &later.description {
            self.description = Some(description.clone());
        }
        if let Some(allow_args) = later.allow_args {
            self.allow_args = Some(allow_args);
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
            allow_args: self.allow_args.unwrap_or(false),
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

#[derive(Debug, Clone, Copy)]
struct LaunchValidationPolicy<'a> {
    template_policy: TemplatePolicy,
    targets: Option<&'a BTreeMap<String, TargetConfig>>,
    collect_references: bool,
    validate_command_before_vars: bool,
}

impl<'a> LaunchValidationPolicy<'a> {
    fn effective(targets: &'a BTreeMap<String, TargetConfig>) -> Self {
        Self {
            template_policy: TemplatePolicy::STRICT,
            targets: Some(targets),
            collect_references: true,
            validate_command_before_vars: true,
        }
    }

    fn partial() -> Self {
        Self {
            template_policy: TemplatePolicy::ALLOW_UNBOUND_VAR_PREFIX,
            targets: None,
            collect_references: false,
            validate_command_before_vars: false,
        }
    }
}

struct LaunchShape<'a> {
    target: Option<&'a str>,
    allow_args: Option<bool>,
    cwd: Option<&'a str>,
    env: &'a BTreeMap<String, String>,
    command: Option<&'a [String]>,
    vars: &'a BTreeMap<String, LaunchVarConfig>,
    steps: &'a [LaunchStep],
}

fn validate_launch_shape(
    launch_name: &str,
    shape: LaunchShape<'_>,
    policy: LaunchValidationPolicy<'_>,
) -> anyhow::Result<()> {
    if let Some(targets) = policy.targets {
        let Some(target) = shape.target else {
            anyhow::bail!("launch {launch_name:?} target is required after defaults");
        };
        if !targets.contains_key(target) {
            anyhow::bail!("launch {launch_name:?} references unknown target {target:?}");
        }
    } else if let Some(target) = shape.target {
        validate_name("launch.target", target)?;
    }

    if policy.validate_command_before_vars
        && let Some(command) = shape.command
    {
        validate_command("launch.command", command)?;
    }

    for (name, var) in shape.vars {
        validate_name("launch var", name)?;
        var.validate(launch_name, name)?;
    }
    let allowed = allowed_template_vars(shape.vars);
    let allowed_refs = allowed.iter().map(String::as_str).collect::<Vec<_>>();
    if let Some(cwd) = shape.cwd {
        validate_template_with_policy("launch.cwd", cwd, &allowed_refs, policy.template_policy)?;
    }
    validate_env_keyed_template_map_with_policy(
        "launch.env",
        shape.env,
        &allowed_refs,
        policy.template_policy,
    )?;
    if let Some(command) = shape.command {
        if !policy.validate_command_before_vars {
            validate_command("launch.command", command)?;
        }
        validate_launch_command_templates_with_policy(
            launch_name,
            "launch.command",
            command,
            &allowed_refs,
            policy.template_policy,
            shape.allow_args,
            policy.collect_references,
        )?;
    }

    let mut referenced_vars = BTreeSet::new();
    if policy.collect_references {
        collect_var_references(shape.cwd, &mut referenced_vars)?;
        collect_var_references_from_map(shape.env, &mut referenced_vars)?;
        if let Some(command) = shape.command {
            collect_var_references_from_command(command, &mut referenced_vars)?;
        }
    }

    let mut step_names = BTreeSet::new();
    for step in shape.steps {
        step.validate(launch_name, &allowed_refs, policy.template_policy)?;
        if !step_names.insert(step.name.clone()) {
            anyhow::bail!(
                "launch {launch_name:?} defines duplicate step {:?}",
                step.name
            );
        }
        if policy.collect_references {
            collect_var_references(step.cwd.as_deref(), &mut referenced_vars)?;
            collect_var_references_from_map(&step.env, &mut referenced_vars)?;
            collect_var_references_from_command(&step.command, &mut referenced_vars)?;
        }
    }

    for var_name in referenced_vars {
        let var = shape.vars.get(&var_name).ok_or_else(|| {
            anyhow::anyhow!("launch {launch_name:?} references undeclared variable {var_name:?}")
        })?;
        if !var.required && var.default.is_none() {
            anyhow::bail!(
                "launch {launch_name:?} optional variable {var_name:?} is referenced by a template and must define default"
            );
        }
    }
    Ok(())
}

fn validate_launch_command_templates_with_policy(
    launch_name: &str,
    field: &str,
    command: &[String],
    allowed: &[&str],
    policy: TemplatePolicy,
    allow_args: Option<bool>,
    effective: bool,
) -> anyhow::Result<()> {
    let mut sentinel_positions = Vec::new();
    for (index, arg) in command.iter().enumerate() {
        if arg == "{args}" {
            sentinel_positions.push(index);
            continue;
        }
        if arg.contains("{args}") {
            anyhow::bail!(
                "launch {launch_name:?} {field} must use {{args}} only as a whole argv element"
            );
        }
        validate_template_with_policy(field, arg, allowed, policy)?;
    }

    validate_launch_args_sentinel(
        launch_name,
        field,
        allow_args,
        effective,
        &sentinel_positions,
    )
}

fn validate_launch_args_sentinel(
    launch_name: &str,
    field: &str,
    allow_args: Option<bool>,
    effective: bool,
    sentinel_positions: &[usize],
) -> anyhow::Result<()> {
    if sentinel_positions
        .first()
        .is_some_and(|position| *position == 0)
    {
        anyhow::bail!("launch {launch_name:?} {field} must not place {{args}} at argv[0]");
    }
    if sentinel_positions.len() > 1 {
        anyhow::bail!(
            "launch {launch_name:?} {field} must not contain duplicate {{args}} argv elements"
        );
    }

    if effective && allow_args.unwrap_or(false) && sentinel_positions.is_empty() {
        anyhow::bail!(
            "launch {launch_name:?} allow_args = true requires exactly one {{args}} argv element"
        );
    }

    let allow_args_is_false = if effective {
        !allow_args.unwrap_or(false)
    } else {
        allow_args == Some(false)
    };
    if allow_args_is_false && !sentinel_positions.is_empty() {
        anyhow::bail!("launch {launch_name:?} {field} uses {{args}} but allow_args is false");
    }

    Ok(())
}

fn allowed_template_vars(vars: &BTreeMap<String, LaunchVarConfig>) -> Vec<String> {
    let mut allowed = LAUNCH_TEMPLATE_BUILTINS
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    allowed.extend(vars.keys().map(|name| format!("var.{name}")));
    allowed
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
                if let Some(default) = &self.default {
                    let LaunchVarValue::String(default) = default else {
                        anyhow::bail!(
                            "launch {launch_name:?} variable {var_name:?} string default must be a TOML string"
                        );
                    };
                    if let Err(reason) = validate_launch_var_string_value(default) {
                        anyhow::bail!(
                            "launch {launch_name:?} variable {var_name:?} string default {reason}"
                        );
                    }
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
                    if let Err(reason) = validate_launch_var_string_value(value) {
                        anyhow::bail!(
                            "launch {launch_name:?} enum variable {var_name:?} values {reason}"
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

pub(crate) fn validate_launch_var_string_value(value: &str) -> Result<(), &'static str> {
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        Err("must not contain NUL, LF, or CR")
    } else {
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
    fn validate(
        &self,
        launch_name: &str,
        allowed: &[&str],
        policy: TemplatePolicy,
    ) -> anyhow::Result<()> {
        validate_name("launch step", &self.name)?;
        if self.phase != LaunchStepPhase::PostReady {
            anyhow::bail!(
                "launch {launch_name:?} step {:?} only supports phase = \"post_ready\"",
                self.name
            );
        }
        validate_command("launch.steps.command", &self.command)?;
        validate_command_templates_with_policy(
            "launch.steps.command",
            &self.command,
            allowed,
            policy,
        )?;
        if let Some(cwd) = &self.cwd {
            validate_template_with_policy("launch.steps.cwd", cwd, allowed, policy)?;
        }
        validate_env_keyed_template_map_with_policy(
            "launch.steps.env",
            &self.env,
            allowed,
            policy,
        )?;
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
