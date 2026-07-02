use super::{ContainerConfig, ContainerInspect, ContainerState, ManagedContainer};
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
struct ContainerInspectRaw {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "State")]
    state: ContainerState,
    #[serde(rename = "Config")]
    config: ContainerConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ManagedContainerRaw {
    #[serde(default, alias = "Name")]
    names: ContainerNames,
    #[serde(default)]
    image: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: LabelField,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AppleContainerConfigurationRaw {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    image: Option<AppleImageField>,
    #[serde(default)]
    labels: LabelField,
    #[serde(default, alias = "Pid", alias = "processID", alias = "process_id")]
    pid: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct AppleContainerInspectRaw {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: AppleStatusField,
    #[serde(default, alias = "Pid", alias = "processID", alias = "process_id")]
    pid: Option<i64>,
    #[serde(default)]
    labels: LabelField,
    #[serde(default)]
    configuration: AppleContainerConfigurationRaw,
}

#[derive(Debug, Clone, Deserialize)]
struct AppleManagedContainerRaw {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    image: Option<AppleImageField>,
    #[serde(default)]
    status: AppleStatusField,
    #[serde(default)]
    labels: LabelField,
    #[serde(default)]
    configuration: AppleContainerConfigurationRaw,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum ContainerNames {
    Many(Vec<String>),
    One(String),
    #[default]
    Empty,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum LabelField {
    Map(BTreeMap<String, String>),
    Text(String),
    #[default]
    Empty,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum AppleImageField {
    Text(String),
    Object {
        #[serde(default)]
        reference: Option<String>,
    },
    #[default]
    Empty,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum AppleStatusField {
    Text(String),
    Object {
        #[serde(default)]
        state: Option<String>,
    },
    #[default]
    Empty,
}

impl From<ContainerInspectRaw> for ContainerInspect {
    fn from(value: ContainerInspectRaw) -> Self {
        Self {
            id: value.id,
            name: value.name.trim_start_matches('/').to_string(),
            state: value.state,
            config: value.config,
        }
    }
}

impl ManagedContainer {
    fn from_raw(raw: ManagedContainerRaw) -> Option<Self> {
        let labels = raw.labels.into_map();
        let name = raw.names.first_name()?;
        let state = raw.state.to_ascii_lowercase();
        let status = raw.status.to_ascii_lowercase();
        Some(Self {
            name,
            image: raw.image,
            running: state == "running" || status.starts_with("up "),
            labels,
        })
    }

    fn from_apple_raw(raw: AppleManagedContainerRaw) -> Option<Self> {
        let labels =
            non_empty_labels(raw.labels).unwrap_or_else(|| raw.configuration.labels.into_map());
        let name = raw
            .name
            .or(raw.id)
            .or(raw.configuration.id)
            .map(normalize_container_name)
            .filter(|value| !value.is_empty())?;
        let image = raw
            .image
            .and_then(AppleImageField::into_reference)
            .or_else(|| {
                raw.configuration
                    .image
                    .and_then(AppleImageField::into_reference)
            })
            .unwrap_or_default();
        let status = raw.status.state().to_ascii_lowercase();
        Some(Self {
            name,
            image,
            running: status == "running",
            labels,
        })
    }
}

impl ContainerNames {
    fn first_name(self) -> Option<String> {
        match self {
            ContainerNames::Many(names) => names,
            ContainerNames::One(names) => names.split(',').map(str::to_string).collect(),
            ContainerNames::Empty => Vec::new(),
        }
        .into_iter()
        .map(|value| value.trim().trim_start_matches('/').to_string())
        .find(|value| !value.is_empty())
    }
}

impl LabelField {
    fn into_map(self) -> BTreeMap<String, String> {
        match self {
            LabelField::Map(labels) => labels,
            LabelField::Text(labels) => labels
                .split(',')
                .filter_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    Some((key.trim().to_string(), value.trim().to_string()))
                })
                .filter(|(key, _)| !key.is_empty())
                .collect(),
            LabelField::Empty => BTreeMap::new(),
        }
    }
}

impl AppleImageField {
    fn into_reference(self) -> Option<String> {
        match self {
            AppleImageField::Text(value) => Some(value),
            AppleImageField::Object { reference } => reference,
            AppleImageField::Empty => None,
        }
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    }
}

impl AppleStatusField {
    fn state(&self) -> &str {
        match self {
            AppleStatusField::Text(value) => value,
            AppleStatusField::Object { state } => state.as_deref().unwrap_or(""),
            AppleStatusField::Empty => "",
        }
    }
}

pub(super) fn parse_container_inspect(
    stdout: &[u8],
    runtime_label: &str,
) -> anyhow::Result<Option<ContainerInspect>> {
    let values: Vec<ContainerInspectRaw> = serde_json::from_slice(stdout)
        .with_context(|| format!("parse {runtime_label} inspect JSON"))?;
    match values.len() {
        0 => Ok(None),
        1 => Ok(values.into_iter().next().map(Into::into)),
        count => anyhow::bail!("{runtime_label} inspect returned {count} containers; expected one"),
    }
}

pub(super) fn parse_apple_container_inspect(
    stdout: &[u8],
    runtime_label: &str,
) -> anyhow::Result<Option<ContainerInspect>> {
    let values: Vec<AppleContainerInspectRaw> = serde_json::from_slice(stdout)
        .with_context(|| format!("parse {runtime_label} inspect JSON"))?;
    match values.len() {
        0 => Ok(None),
        1 => values
            .into_iter()
            .next()
            .map(container_inspect_from_apple_raw)
            .transpose(),
        count => anyhow::bail!("{runtime_label} inspect returned {count} containers; expected one"),
    }
}

pub fn parse_managed_containers(stdout: &[u8]) -> anyhow::Result<Vec<ManagedContainer>> {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let raw = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<ManagedContainerRaw>>(trimmed)?
    } else {
        trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<ManagedContainerRaw>)
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(raw
        .into_iter()
        .filter_map(ManagedContainer::from_raw)
        .filter(is_aw_gateway_managed_container)
        .collect())
}

pub(super) fn parse_apple_container_list(stdout: &[u8]) -> anyhow::Result<Vec<ManagedContainer>> {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let raw = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<AppleManagedContainerRaw>>(trimmed)?
    } else {
        trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<AppleManagedContainerRaw>)
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(raw
        .into_iter()
        .filter_map(ManagedContainer::from_apple_raw)
        .collect())
}

fn is_aw_gateway_managed_container(container: &ManagedContainer) -> bool {
    container
        .labels
        .get("io.aw-gateway.gateway")
        .is_some_and(|value| value == "true")
        && container.labels.contains_key("io.aw-gateway.user")
        && container.labels.contains_key("io.aw-gateway.uid")
}

fn normalize_container_name(value: String) -> String {
    value.trim().trim_start_matches('/').to_string()
}

fn non_empty_labels(labels: LabelField) -> Option<BTreeMap<String, String>> {
    let labels = labels.into_map();
    (!labels.is_empty()).then_some(labels)
}

fn container_inspect_from_apple_raw(
    value: AppleContainerInspectRaw,
) -> anyhow::Result<ContainerInspect> {
    let id = value
        .id
        .or(value.configuration.id.clone())
        .map(normalize_container_name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("apple container inspect JSON did not include configuration.id")
        })?;
    let pid = value.pid.or(value.configuration.pid);
    let labels =
        non_empty_labels(value.labels).unwrap_or_else(|| value.configuration.labels.into_map());
    Ok(ContainerInspect {
        id: id.clone(),
        name: value
            .configuration
            .hostname
            .map(normalize_container_name)
            .filter(|value| !value.is_empty())
            .unwrap_or(id),
        state: ContainerState {
            running: value.status.state().eq_ignore_ascii_case("running"),
            pid,
        },
        config: ContainerConfig { labels },
    })
}

pub(super) fn is_aw_gateway_managed_container_for(
    container: &ManagedContainer,
    user: &str,
    uid: u32,
) -> bool {
    is_aw_gateway_managed_container(container)
        && container
            .labels
            .get("io.aw-gateway.user")
            .is_some_and(|value| value == user)
        && container
            .labels
            .get("io.aw-gateway.uid")
            .is_some_and(|value| value == &uid.to_string())
}
