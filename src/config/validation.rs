use super::{LaunchStep, ServiceConfig};
use crate::context::validate_context_slug;
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

pub(super) const WORKSPACE_RUNTIME_TEMPLATE_VARS: &[&str] = &[
    "user",
    "uid",
    "gid",
    "home",
    "container_user",
    "container_home",
    "workspace",
    "target",
    "image",
    "image_slug",
    "container_name",
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
pub(crate) const SERVICE_USER_TEMPLATE: &str = "{container_user}";

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
    allow_unbound_context_prefix: bool,
}

impl TemplatePolicy {
    pub(super) const STRICT: Self = Self {
        allow_unbound_var_prefix: false,
        allow_unbound_context_prefix: false,
    };

    pub(super) const ALLOW_UNBOUND_VAR_PREFIX: Self = Self {
        allow_unbound_var_prefix: true,
        allow_unbound_context_prefix: false,
    };

    pub(super) const ALLOW_UNBOUND_CONTEXT_PREFIX: Self = Self {
        allow_unbound_var_prefix: false,
        allow_unbound_context_prefix: true,
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
        if policy.allow_unbound_context_prefix
            && let Some(context_name) = key.strip_prefix("context.")
        {
            validate_context_slug(context_name).with_context(|| format!("validate {field}"))?;
            continue;
        }
        return Err(anyhow::anyhow!(
            "unknown interpolation variable {{{key}}} in {value:?}"
        ))
        .with_context(|| format!("validate {field}"));
    }
    Ok(())
}

pub(super) fn validate_template_with_context<'a, I>(
    field: &str,
    value: &str,
    allowed: &[&str],
    context_keys: I,
) -> anyhow::Result<()>
where
    I: IntoIterator<Item = &'a String>,
{
    let allowed_context: BTreeSet<String> = context_keys.into_iter().cloned().collect();
    for key in template::referenced_keys(value).with_context(|| format!("validate {field}"))? {
        if allowed.contains(&key) {
            continue;
        }
        if let Some(context_name) = key.strip_prefix("context.") {
            validate_context_slug(context_name).with_context(|| format!("validate {field}"))?;
            if allowed_context.contains(context_name) {
                continue;
            }
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
        || value.starts_with(['.', '-'])
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        anyhow::bail!(
            "{field} value {value:?} must start with ASCII alnum or '_' and contain only ASCII alnum, '.', '-', '_'"
        );
    }
    Ok(())
}

pub(super) fn validate_image_reference(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} reference must not be empty");
    }
    if value != value.trim()
        || value
            .chars()
            .any(|ch| ch.is_ascii_whitespace() || ch.is_ascii_control())
    {
        anyhow::bail!("{field} reference must not contain whitespace or control characters");
    }
    if value.starts_with('-') {
        anyhow::bail!("{field} reference must not start with '-'");
    }
    if value.contains("://") {
        anyhow::bail!("{field} reference must not contain a URI scheme");
    }

    let at_count = value.bytes().filter(|byte| *byte == b'@').count();
    if at_count > 1 {
        anyhow::bail!("{field} reference must not contain multiple digest separators");
    }
    let (name_and_tag, digest) = match value.split_once('@') {
        Some((name, digest)) => {
            if name.is_empty() || digest.is_empty() {
                anyhow::bail!("{field} reference has an invalid digest suffix");
            }
            (name, Some(digest))
        }
        None => (value, None),
    };
    if let Some(digest) = digest {
        validate_image_digest(field, digest)?;
    }

    let last_slash = name_and_tag.rfind('/');
    let last_colon = name_and_tag.rfind(':');
    let (name, tag) =
        if last_colon.is_some_and(|colon| last_slash.is_none_or(|slash| colon > slash)) {
            let colon = last_colon.unwrap();
            let (name, tag) = name_and_tag.split_at(colon);
            (name, Some(&tag[1..]))
        } else {
            (name_and_tag, None)
        };
    if name.is_empty() {
        anyhow::bail!("{field} reference must include an image name");
    }
    if let Some(tag) = tag {
        validate_image_tag(field, tag)?;
    }

    let components: Vec<&str> = name.split('/').collect();
    if components.iter().any(|component| component.is_empty()) {
        anyhow::bail!("{field} reference must not contain empty path components");
    }
    if components.len() > 1 && is_registry_component(components[0]) {
        validate_image_registry(field, components[0])?;
        for component in &components[1..] {
            validate_image_name_component(field, component)?;
        }
    } else {
        for component in &components {
            validate_image_name_component(field, component)?;
        }
    }
    Ok(())
}

fn is_registry_component(component: &str) -> bool {
    component == "localhost" || component.contains('.') || component.contains(':')
}

fn validate_image_registry(field: &str, value: &str) -> anyhow::Result<()> {
    if value.starts_with('-')
        || value.ends_with('-')
        || value.chars().any(|ch| {
            !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '-' | ':'))
        })
    {
        anyhow::bail!("{field} reference has an invalid registry component");
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && (host.is_empty() || port.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit()))
    {
        anyhow::bail!("{field} reference has an invalid registry port");
    }
    Ok(())
}

fn validate_image_name_component(field: &str, value: &str) -> anyhow::Result<()> {
    let valid_edge = |ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit();
    if !value.chars().next().is_some_and(valid_edge)
        || !value.chars().last().is_some_and(valid_edge)
        || value.chars().any(|ch| {
            !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
        })
    {
        anyhow::bail!("{field} reference has an invalid repository component");
    }
    Ok(())
}

fn validate_image_tag(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        || value
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
    {
        anyhow::bail!("{field} reference has an invalid tag");
    }
    Ok(())
}

fn validate_image_digest(field: &str, value: &str) -> anyhow::Result<()> {
    let Some((algorithm, digest)) = value.split_once(':') else {
        anyhow::bail!("{field} reference has an invalid digest suffix");
    };
    if algorithm.is_empty()
        || digest.is_empty()
        || !algorithm
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '_' | '-'))
        || !digest.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        anyhow::bail!("{field} reference has an invalid digest suffix");
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

pub(crate) fn default_control_socket_host_dir() -> String {
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
