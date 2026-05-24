use super::{
    LifecyclePhase, RawContainerBootstrapStep, RawHostStep, RawLifecycleStep,
    validation::lifecycle_phase_name,
};
use std::collections::BTreeSet;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetStepPatchState {
    Enabled,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TargetStepPatchPosition<K> {
    Append,
    Before(K),
    After(K),
    ReplaceExisting,
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
            (Some(before), None) => TargetStepPatchPosition::Before(key.reference_key(before)),
            (None, Some(after)) => TargetStepPatchPosition::After(key.reference_key(after)),
            _ => TargetStepPatchPosition::Append,
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

    fn mark_replace_existing(&mut self) {
        self.position = TargetStepPatchPosition::ReplaceExisting;
    }

    fn replaces_existing(&self) -> bool {
        matches!(self.position, TargetStepPatchPosition::ReplaceExisting)
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
        let mut patch = TargetStepPatch::from_raw(entry, &raw_key);
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
            patch.mark_replace_existing();
            debug_assert!(patch.replaces_existing());
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
            TargetStepPatchPosition::Before(reference) => effective_keys
                .iter()
                .position(|candidate| candidate == reference)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{list_name} {} references missing before = {:?}",
                        patch.key.label(),
                        entry.before().unwrap_or_default()
                    )
                })?,
            TargetStepPatchPosition::After(reference) => effective_keys
                .iter()
                .position(|candidate| candidate == reference)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{list_name} {} references missing after = {:?}",
                        patch.key.label(),
                        entry.after().unwrap_or_default()
                    )
                })?,
            TargetStepPatchPosition::Append => result.len(),
            TargetStepPatchPosition::ReplaceExisting => unreachable!("new patches cannot replace"),
        };
        result.insert(insert_at, convert(entry, None)?);
        effective_keys.insert(insert_at, patch.key);
    }

    if effective_keys.iter().collect::<BTreeSet<_>>().len() != effective_keys.len() {
        anyhow::bail!("{list_name} contains duplicate effective keys");
    }
    Ok(result)
}
