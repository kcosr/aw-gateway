use super::{
    LaunchStep, LifecyclePhase, RawContainerBootstrapStep, RawHostStep, RawLifecycleStep,
    ServiceConfig,
};
use crate::{action, template};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

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

pub(super) fn parse_socket_mode(input: &str) -> anyhow::Result<u32> {
    if input.len() != 4 || !input.chars().all(|ch| matches!(ch, '0'..='7')) {
        anyhow::bail!("socket mode must be four octal digits, got {input:?}");
    }
    Ok(u32::from_str_radix(input, 8)?)
}

pub(super) const GATEWAY_TEMPLATE_VARS: &[&str] = &[
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

pub(super) const GATEWAY_TEMPLATE_VARS_NO_PID: &[&str] = &[
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

pub(super) const TARGET_WORKSPACE_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "target",
    "image",
    "image_slug",
    "session_id",
];

pub(super) const CONTROL_SOCKET_TEMPLATE_VARS: &[&str] = &[
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

pub(super) const AGENT_TEMPLATE_VARS: &[&str] = &["container_state_dir"];

pub(super) const GATEWAY_LOGGING_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "workspace",
    "state",
    "state_dir",
];

pub(super) const CLIENT_TEMPLATE_VARS: &[&str] = &[
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

pub(super) const RUNTIME_TEMPLATE_VARS: &[&str] = &["user", "home"];

pub(super) const IDENTITY_TEMPLATE_VARS: &[&str] = &["user", "uid", "gid", "home"];

pub(super) const LAUNCH_TEMPLATE_BUILTINS: &[&str] = &[
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
pub(super) struct StepKey {
    pub(super) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LifecycleStepKey {
    pub(super) phase: Option<LifecyclePhase>,
    pub(super) name: String,
}

pub(super) trait MergeKey: Clone + Ord {
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

pub(super) trait RawStepEntry {
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

pub(super) fn validate_raw_target_steps<R, K, F>(
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

pub(super) fn merge_raw_steps<T, R, K, FK, RK, C>(
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

pub(super) fn merge_services(
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

pub(super) fn merge_launch_steps(
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

pub(super) fn lifecycle_phase_name(phase: LifecyclePhase) -> &'static str {
    match phase {
        LifecyclePhase::PreStart => "pre_start",
        LifecyclePhase::PostStartHost => "post_start_host",
        LifecyclePhase::PreStop => "pre_stop",
        LifecyclePhase::PostStop => "post_stop",
    }
}

pub(super) fn validate_template(field: &str, value: &str, allowed: &[&str]) -> anyhow::Result<()> {
    validate_template_with_policy(field, value, allowed, TemplatePolicy::STRICT)
}

pub(super) fn validate_command_templates(
    field: &str,
    command: &[String],
    allowed: &[&str],
) -> anyhow::Result<()> {
    for arg in command {
        validate_template(field, arg, allowed)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TemplatePolicy {
    allow_unbound_var_prefix: bool,
}

impl TemplatePolicy {
    pub(super) const STRICT: Self = Self {
        allow_unbound_var_prefix: false,
    };

    pub(super) const ALLOW_UNBOUND_VAR_PREFIX: Self = Self {
        allow_unbound_var_prefix: true,
    };
}

pub(super) fn validate_template_with_policy(
    field: &str,
    value: &str,
    allowed: &[&str],
    policy: TemplatePolicy,
) -> anyhow::Result<()> {
    for key in template::referenced_keys(value).with_context(|| format!("validate {field}"))? {
        if allowed.contains(&key) {
            continue;
        }
        if policy.allow_unbound_var_prefix
            && let Some(var_name) = key.strip_prefix("var.")
        {
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

pub(super) fn validate_command_templates_with_policy(
    field: &str,
    command: &[String],
    allowed: &[&str],
    policy: TemplatePolicy,
) -> anyhow::Result<()> {
    for arg in command {
        validate_template_with_policy(field, arg, allowed, policy)?;
    }
    Ok(())
}

pub(super) fn validate_command(field: &str, command: &[String]) -> anyhow::Result<()> {
    if command.is_empty() {
        anyhow::bail!("{field} command must not be empty");
    }
    if command[0].is_empty() {
        anyhow::bail!("{field} command argv[0] must not be empty");
    }
    Ok(())
}

pub(super) fn reject_template_use(field: &str, templates: &[String]) -> anyhow::Result<()> {
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

pub(super) fn validate_env_key(value: &str) -> anyhow::Result<()> {
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

pub(super) fn validate_env_map(field: &str, env: &BTreeMap<String, String>) -> anyhow::Result<()> {
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

pub(super) fn validate_env_keyed_template_map_with_policy(
    field: &str,
    env: &BTreeMap<String, String>,
    allowed: &[&str],
    policy: TemplatePolicy,
) -> anyhow::Result<()> {
    for (key, value) in env {
        validate_env_key(key)?;
        validate_template_with_policy(&format!("{field}.{key}"), value, allowed, policy)?;
    }
    Ok(())
}

pub(super) fn collect_var_references(
    input: Option<&str>,
    refs: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
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

pub(super) fn collect_var_references_from_map(
    values: &BTreeMap<String, String>,
    refs: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for value in values.values() {
        collect_var_references(Some(value), refs)?;
    }
    Ok(())
}

pub(super) fn collect_var_references_from_command(
    command: &[String],
    refs: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for arg in command {
        collect_var_references(Some(arg), refs)?;
    }
    Ok(())
}

pub(crate) fn canonical_number_string(value: f64) -> String {
    let text = value.to_string();
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

pub(super) fn validate_ssh_config_scalar(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    if value.contains(['\n', '\r']) {
        anyhow::bail!("{field} must not contain newlines");
    }
    Ok(())
}

pub(super) fn validate_container_name(value: &str) -> anyhow::Result<()> {
    validate_name("container name", value)
}

pub(super) fn default_target() -> String {
    "default".into()
}

pub(super) fn default_log_level() -> String {
    "info".into()
}

pub(super) fn default_workspace_path() -> String {
    "workspace".into()
}

pub(super) fn default_workspace_state_dir() -> String {
    ".aw-gateway".into()
}

pub(super) fn default_control_socket_host_dir() -> String {
    "/run/user/{uid}/aw-gateway/{runtime_id}".into()
}

pub(super) fn default_control_socket_container_dir() -> String {
    "/run/aw-gateway".into()
}

pub(super) fn default_true() -> bool {
    true
}

pub(super) fn default_enabled_actions() -> Vec<String> {
    action::default_enabled_actions()
}

pub(super) fn default_http_listen() -> String {
    "127.0.0.1:8080".into()
}

pub(super) fn default_inner_alias_template() -> String {
    "aw-{target}".into()
}

pub(super) fn default_container_host_template() -> String {
    "aw-container-{target}".into()
}

pub(super) fn default_host() -> String {
    "localhost".into()
}

pub(super) fn default_gateway_path() -> String {
    "/opt/aw-gateway/bin/aw-gateway".into()
}

pub(super) fn default_identity_dir() -> String {
    "~/.ssh/aw-gateway".into()
}

pub(super) fn default_listen_host() -> String {
    "127.0.0.1".into()
}

pub(super) fn default_root() -> String {
    "root".into()
}

pub(super) fn default_bridge_target() -> String {
    "127.0.0.1:22".into()
}

pub(super) fn default_socket_mode() -> String {
    "0600".into()
}

pub(super) fn default_reap_signal() -> String {
    "TERM".into()
}

pub(super) fn default_bootstrap_entrypoint() -> String {
    "/opt/aw-gateway/bin/aw-container-bootstrap".into()
}

pub(super) fn default_bootstrap_agent_program() -> String {
    "/opt/aw-gateway/bin/aw-container-agent".into()
}
