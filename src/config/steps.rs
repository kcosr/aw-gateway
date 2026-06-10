use super::{HealthCheck, validation::*};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
    pub(super) fn to_effective_without_inherited(&self) -> anyhow::Result<LifecycleStep> {
        self.to_effective(None)
    }

    pub(super) fn from_effective(step: LifecycleStep) -> Self {
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

    pub(super) fn to_effective(
        &self,
        inherited: Option<&LifecycleStep>,
    ) -> anyhow::Result<LifecycleStep> {
        let policy = PayloadInheritancePolicy::TimeoutOnlyReplacement;
        let inherit_payload = policy.lifecycle_inherits_payload(self);
        let command = self.command.clone().or_else(|| {
            policy.inherit_optional(inherit_payload, inherited, |step| step.command.clone())
        });
        Ok(LifecycleStep {
            phase: self.phase,
            name: self.name.clone(),
            required: self
                .required
                .or_else(|| {
                    policy.inherit_optional(inherit_payload, inherited, |step| step.required)
                })
                .unwrap_or(true),
            command: command.ok_or_else(|| {
                anyhow::anyhow!(
                    "lifecycle_steps {}/{} command must be provided when enabled",
                    self.phase.name(),
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

impl LifecyclePhase {
    fn name(self) -> &'static str {
        match self {
            Self::PreStart => "pre_start",
            Self::PostStartHost => "post_start_host",
            Self::PreStop => "pre_stop",
            Self::PostStop => "post_stop",
        }
    }
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
    pub(super) fn to_effective_without_inherited(&self) -> anyhow::Result<HostStep> {
        self.to_effective(None)
    }

    pub(super) fn from_effective(step: HostStep) -> Self {
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

    pub(super) fn to_effective(&self, inherited: Option<&HostStep>) -> anyhow::Result<HostStep> {
        let policy = PayloadInheritancePolicy::TimeoutOnlyReplacement;
        let inherit_payload = policy.host_inherits_payload(self);
        let command = self.command.clone().or_else(|| {
            policy.inherit_optional(inherit_payload, inherited, |step| step.command.clone())
        });
        Ok(HostStep {
            name: self.name.clone(),
            required: self
                .required
                .or_else(|| {
                    policy.inherit_optional(inherit_payload, inherited, |step| step.required)
                })
                .unwrap_or(true),
            command: command.ok_or_else(|| {
                anyhow::anyhow!(
                    "host_steps {} command must be provided when enabled",
                    self.name
                )
            })?,
            health_check: self.health_check.clone().or_else(|| {
                policy.inherit_optional(inherit_payload, inherited, |step| {
                    step.health_check.clone()
                })?
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
    pub(super) fn from_effective(step: ContainerBootstrapStep) -> Self {
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

    pub(super) fn to_effective(&self) -> anyhow::Result<ContainerBootstrapStep> {
        let inheritance_policy = PayloadInheritancePolicy::NoInheritedPayload;
        debug_assert!(!inheritance_policy.allows_inherited_payload());
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
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        validate_name("bootstrap step", &self.name)?;
        if self.user.trim().is_empty() {
            anyhow::bail!("bootstrap step user must not be empty");
        }
        validate_command("bootstrap step command", &self.command)
    }
}

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
            Some(phase) => format!("{}/{}", phase.name(), self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetStepPatchState {
    Enabled,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TargetStepPatchPosition<K> {
    Append,
    Before { reference: K, name: String },
    After { reference: K, name: String },
    ConflictingBeforeAfter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TargetStepPatchPayload {
    present: bool,
}

impl TargetStepPatchPayload {
    fn new(present: bool) -> Self {
        Self { present }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TargetStepPatch<K> {
    key: K,
    state: TargetStepPatchState,
    position: TargetStepPatchPosition<K>,
    payload: TargetStepPatchPayload,
}

impl<K: MergeKey> TargetStepPatch<K> {
    fn from_raw<R, F>(raw: &R, key: F) -> Self
    where
        R: RawTargetStepPatchEntry,
        F: Fn(&R) -> K,
    {
        let key = key(raw);
        let position = match (raw.before(), raw.after()) {
            (Some(before), None) => TargetStepPatchPosition::Before {
                reference: key.reference_key(before),
                name: before.to_owned(),
            },
            (None, Some(after)) => TargetStepPatchPosition::After {
                reference: key.reference_key(after),
                name: after.to_owned(),
            },
            (Some(_), Some(_)) => TargetStepPatchPosition::ConflictingBeforeAfter,
            (None, None) => TargetStepPatchPosition::Append,
        };
        Self {
            key,
            state: if raw.enabled() {
                TargetStepPatchState::Enabled
            } else {
                TargetStepPatchState::Remove
            },
            position,
            payload: TargetStepPatchPayload::new(raw.has_payload()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PayloadInheritancePolicy {
    TimeoutOnlyReplacement,
    NoInheritedPayload,
}

impl PayloadInheritancePolicy {
    fn allows_inherited_payload(self) -> bool {
        matches!(self, Self::TimeoutOnlyReplacement)
    }

    fn lifecycle_inherits_payload(self, step: &RawLifecycleStep) -> bool {
        self.allows_inherited_payload()
            && step.timeout.is_some()
            && step.command.is_none()
            && step.required.is_none()
    }

    fn host_inherits_payload(self, step: &RawHostStep) -> bool {
        self.allows_inherited_payload()
            && step.timeout.is_some()
            && step.command.is_none()
            && step.required.is_none()
            && step.health_check.is_none()
    }

    fn inherit_optional<T, U, F>(
        self,
        should_inherit: bool,
        inherited: Option<&T>,
        field: F,
    ) -> Option<U>
    where
        F: FnOnce(&T) -> U,
    {
        self.allows_inherited_payload()
            .then(|| should_inherit.then(|| inherited.map(field)).flatten())
            .flatten()
    }
}

pub(super) trait RawTargetStepPatchEntry {
    fn enabled(&self) -> bool;
    fn before(&self) -> Option<&str>;
    fn after(&self) -> Option<&str>;
    fn has_payload(&self) -> bool;
}

impl RawTargetStepPatchEntry for RawLifecycleStep {
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

impl RawTargetStepPatchEntry for RawHostStep {
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

impl RawTargetStepPatchEntry for RawContainerBootstrapStep {
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
    R: RawTargetStepPatchEntry,
    K: MergeKey,
    F: Fn(&R) -> K,
{
    let mut keys = BTreeSet::new();
    for step in steps {
        let patch = TargetStepPatch::from_raw(step, &key);
        if !keys.insert(patch.key.clone()) {
            anyhow::bail!(
                "target {target_name:?} defines duplicate {list_name} {}",
                patch.key.label()
            );
        }
        if step.before().is_some() && step.after().is_some() {
            anyhow::bail!(
                "target {target_name:?} {list_name} {} sets both before and after",
                patch.key.label()
            );
        }
        if patch.state == TargetStepPatchState::Remove && patch.payload.present {
            anyhow::bail!(
                "target {target_name:?} {list_name} {} is disabled but includes command payload",
                patch.key.label()
            );
        }
    }
    Ok(())
}

pub(super) fn merge_target_step_patches<T, R, K, FK, RK, C>(
    list_name: &str,
    inherited: Vec<T>,
    raw: &[R],
    inherited_key: FK,
    raw_key: RK,
    convert: C,
) -> anyhow::Result<Vec<T>>
where
    T: Clone,
    R: RawTargetStepPatchEntry,
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
        let patch = TargetStepPatch::from_raw(entry, &raw_key);
        let existing_index = effective_keys
            .iter()
            .position(|candidate| candidate == &patch.key);
        if let Some(index) = existing_index {
            if !matches!(patch.position, TargetStepPatchPosition::Append) {
                anyhow::bail!(
                    "{list_name} {} replaces an inherited entry and must not set before or after",
                    patch.key.label()
                );
            }
            match patch.state {
                TargetStepPatchState::Enabled => {
                    result[index] = convert(entry, Some(&result[index]))?;
                    effective_keys[index] = patch.key;
                }
                TargetStepPatchState::Remove => {
                    result.remove(index);
                    effective_keys.remove(index);
                }
            }
            continue;
        }

        if patch.state == TargetStepPatchState::Remove {
            anyhow::bail!(
                "{list_name} {} is disabled but does not match an inherited entry",
                patch.key.label()
            );
        }

        let insert_at = match &patch.position {
            TargetStepPatchPosition::Before { reference, name } => effective_keys
                .iter()
                .position(|candidate| candidate == reference)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{list_name} {} references missing before = {:?}",
                        patch.key.label(),
                        name
                    )
                })?,
            TargetStepPatchPosition::After { reference, name } => effective_keys
                .iter()
                .position(|candidate| candidate == reference)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{list_name} {} references missing after = {:?}",
                        patch.key.label(),
                        name
                    )
                })?,
            TargetStepPatchPosition::Append => result.len(),
            TargetStepPatchPosition::ConflictingBeforeAfter => {
                anyhow::bail!(
                    "{list_name} {} sets both before and after",
                    patch.key.label()
                );
            }
        };
        result.insert(insert_at, convert(entry, None)?);
        effective_keys.insert(insert_at, patch.key);
    }

    if effective_keys.iter().collect::<BTreeSet<_>>().len() != effective_keys.len() {
        anyhow::bail!("{list_name} contains duplicate effective keys");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_target_step_patches_rejects_conflicting_before_after() {
        let raw = vec![RawHostStep {
            name: "firewall".into(),
            enabled: true,
            before: Some("setup".into()),
            after: Some("cleanup".into()),
            required: None,
            command: Some(vec!["/bin/true".into()]),
            health_check: None,
            timeout: None,
        }];

        let err = merge_target_step_patches(
            "host_steps",
            Vec::<HostStep>::new(),
            &raw,
            |step| StepKey {
                name: step.name.clone(),
            },
            |step| StepKey {
                name: step.name.clone(),
            },
            |step, inherited| step.to_effective(inherited),
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "host_steps firewall sets both before and after"
        );
    }
}
