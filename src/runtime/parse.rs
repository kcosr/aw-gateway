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

pub(super) fn parse_container_inspect(
    stdout: &[u8],
    runtime_label: &str,
) -> anyhow::Result<Option<ContainerInspect>> {
    let mut values: Vec<ContainerInspectRaw> = serde_json::from_slice(stdout)
        .with_context(|| format!("parse {runtime_label} inspect JSON"))?;
    Ok(values.pop().map(Into::into))
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

fn is_aw_gateway_managed_container(container: &ManagedContainer) -> bool {
    container
        .labels
        .get("io.aw-gateway.gateway")
        .is_some_and(|value| value == "true")
        && container.labels.contains_key("io.aw-gateway.user")
        && container.labels.contains_key("io.aw-gateway.uid")
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
