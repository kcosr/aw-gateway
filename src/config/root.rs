use super::include;
use anyhow::Context;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use toml::Value;
use toml::map::Map;

const MAX_EXTENDS_DEPTH: usize = 64;

pub(super) fn load_gateway_root(path: &Path) -> anyhow::Result<Value> {
    let mut stack = BTreeSet::new();
    load_gateway_root_inner(path, &mut stack)
}

fn load_gateway_root_inner(path: &Path, stack: &mut BTreeSet<PathBuf>) -> anyhow::Result<Value> {
    if stack.len() >= MAX_EXTENDS_DEPTH {
        anyhow::bail!(
            "extends chain exceeds maximum depth of {MAX_EXTENDS_DEPTH} at {}",
            path.display()
        );
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    if !stack.insert(canonical.clone()) {
        anyhow::bail!("extends cycle detected at {}", path.display());
    }

    let mut value = load_root_value(path)?;
    let extends = root_extends(&value, path)?;
    include::compose_gateway_includes_value(&mut value, path)?;
    strip_loader_fields(&mut value);

    let result = if let Some(extends) = extends {
        let base_path = resolve_extends_path(path, &extends);
        let base = load_gateway_root_inner(&base_path, stack).with_context(|| {
            format!(
                "load base config {} extended by {}",
                base_path.display(),
                path.display()
            )
        })?;
        merge_values(base, value, &[])
    } else {
        Ok(value)
    };

    stack.remove(&canonical);
    result
}

fn load_root_value(path: &Path) -> anyhow::Result<Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn root_extends(value: &Value, path: &Path) -> anyhow::Result<Option<String>> {
    let Some(table) = value.as_table() else {
        anyhow::bail!("gateway config {} must be a TOML table", path.display());
    };
    match table.get("extends") {
        None => Ok(None),
        Some(Value::String(extends)) => {
            if extends.contains(['*', '?', '[']) {
                anyhow::bail!(
                    "gateway config {} extends must be a single path; globs are not supported",
                    path.display()
                );
            }
            Ok(Some(extends.clone()))
        }
        Some(_) => anyhow::bail!("gateway config {} extends must be a string", path.display()),
    }
}

fn resolve_extends_path(path: &Path, extends: &str) -> PathBuf {
    let extends_path = Path::new(extends);
    if extends_path.is_absolute() {
        extends_path.to_path_buf()
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(extends_path)
    }
}

fn strip_loader_fields(value: &mut Value) {
    if let Some(table) = value.as_table_mut() {
        table.remove("extends");
        table.remove("includes");
    }
}

fn merge_values(base: Value, child: Value, path: &[String]) -> anyhow::Result<Value> {
    match (base, child) {
        (Value::Table(mut base), Value::Table(child)) => {
            merge_tables(&mut base, child, path)?;
            Ok(Value::Table(base))
        }
        (Value::Array(base), Value::Array(child)) if is_named_array_path(path) => {
            Ok(Value::Array(merge_named_arrays(base, child, path)?))
        }
        (_, child) => Ok(child),
    }
}

fn merge_tables(
    base: &mut Map<String, Value>,
    child: Map<String, Value>,
    path: &[String],
) -> anyhow::Result<()> {
    for (key, child_value) in child {
        let mut child_path = path.to_vec();
        child_path.push(key.clone());
        if is_service_env_table_path(path) {
            base.insert(key, child_value);
            continue;
        }
        if is_atomic_host_socket_exposure_map(path)
            || is_atomic_access_flow_relay_component(&child_path)
        {
            base.insert(key, child_value);
        } else if let Some(base_value) = base.remove(&key) {
            base.insert(key, merge_values(base_value, child_value, &child_path)?);
        } else {
            base.insert(key, child_value);
        }
    }
    Ok(())
}

fn is_atomic_access_flow_relay_component(path: &[String]) -> bool {
    matches_path(
        path,
        &["target_defaults", "container_agent", "access_flow_relay"],
    ) || matches_path(
        path,
        &[
            "target_templates",
            "*",
            "container_agent",
            "access_flow_relay",
        ],
    ) || matches_path(
        path,
        &["targets", "*", "container_agent", "access_flow_relay"],
    )
}

fn is_atomic_host_socket_exposure_map(path: &[String]) -> bool {
    matches_path(path, &["target_defaults", "host_socket_exposures"])
        || matches_path(path, &["target_templates", "*", "host_socket_exposures"])
        || matches_path(path, &["targets", "*", "host_socket_exposures"])
}

fn merge_named_arrays(
    mut base: Vec<Value>,
    child: Vec<Value>,
    path: &[String],
) -> anyhow::Result<Vec<Value>> {
    let mut keys = Vec::with_capacity(base.len());
    for item in &base {
        keys.push(named_array_key(item, path)?);
    }
    if keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
        anyhow::bail!("{} contains duplicate inherited names", path.join("."));
    }

    let mut child_keys = BTreeSet::new();
    for child_item in child {
        let child_key = named_array_key(&child_item, path)?;
        if !child_keys.insert(child_key.clone()) {
            anyhow::bail!("{} contains duplicate extending names", path.join("."));
        }
        if let Some(index) = keys.iter().position(|key| key == &child_key) {
            base[index] = merge_values(base[index].clone(), child_item, path)?;
            keys[index] = child_key;
        } else {
            keys.push(child_key);
            base.push(child_item);
        }
    }
    Ok(base)
}

fn named_array_key(value: &Value, path: &[String]) -> anyhow::Result<String> {
    value
        .as_table()
        .and_then(|table| table.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} entries must be tables with string name fields",
                path.join(".")
            )
        })
}

fn is_named_array_path(path: &[String]) -> bool {
    matches_path(path, &["target_defaults", "container_agent", "services"])
        || matches_path(path, &["target_defaults", "lifecycle_steps"])
        || matches_path(path, &["target_defaults", "host_steps"])
        || matches_path(path, &["target_defaults", "container_bootstrap_steps"])
        || matches_path(
            path,
            &["target_templates", "*", "container_agent", "services"],
        )
        || matches_path(path, &["target_templates", "*", "lifecycle_steps"])
        || matches_path(path, &["target_templates", "*", "host_steps"])
        || matches_path(
            path,
            &["target_templates", "*", "container_bootstrap_steps"],
        )
        || matches_path(path, &["targets", "*", "container_agent", "services"])
        || matches_path(path, &["targets", "*", "lifecycle_steps"])
        || matches_path(path, &["targets", "*", "host_steps"])
        || matches_path(path, &["targets", "*", "container_bootstrap_steps"])
        || matches_path(path, &["launch_defaults", "steps"])
        || matches_path(path, &["launch_templates", "*", "steps"])
        || matches_path(path, &["launches", "*", "steps"])
}

fn is_service_env_table_path(path: &[String]) -> bool {
    matches_path(
        path,
        &["target_defaults", "container_agent", "services", "env"],
    ) || matches_path(
        path,
        &[
            "target_templates",
            "*",
            "container_agent",
            "services",
            "env",
        ],
    ) || matches_path(
        path,
        &["targets", "*", "container_agent", "services", "env"],
    )
}

fn matches_path(path: &[String], pattern: &[&str]) -> bool {
    path.len() == pattern.len()
        && path
            .iter()
            .zip(pattern)
            .all(|(actual, expected)| *expected == "*" || actual == expected)
}
