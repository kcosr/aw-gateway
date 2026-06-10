use anyhow::Context;
use glob::glob;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use toml::Value;

pub(super) fn compose_gateway_includes_value(
    value: &mut Value,
    root_path: &Path,
) -> anyhow::Result<()> {
    let Some(root_table) = value.as_table_mut() else {
        anyhow::bail!(
            "gateway config {} must be a TOML table",
            root_path.display()
        );
    };
    let includes: Vec<String> = root_table
        .get("includes")
        .map(|value| value.clone().try_into())
        .transpose()
        .with_context(|| format!("parse includes from {}", root_path.display()))?
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let root_dir = root_path.parent().unwrap_or_else(|| Path::new("."));
    let root_canonical = canonical_existing_path(root_path)?;
    let mut stack = BTreeSet::from([root_canonical]);
    ValueIncludeComposer {
        root: root_table,
        seen: &mut seen,
        stack: &mut stack,
    }
    .compose(&includes, root_dir)?;
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigIncludeFile {
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default, rename = "target_templates")]
    _target_templates: BTreeMap<String, Value>,
    #[serde(default, rename = "launch_templates")]
    _launch_templates: BTreeMap<String, Value>,
    #[serde(default, rename = "targets")]
    _targets: BTreeMap<String, Value>,
    #[serde(default, rename = "launches")]
    _launches: BTreeMap<String, Value>,
}

struct ValueIncludeComposer<'a> {
    root: &'a mut toml::map::Map<String, Value>,
    seen: &'a mut BTreeSet<PathBuf>,
    stack: &'a mut BTreeSet<PathBuf>,
}

impl ValueIncludeComposer<'_> {
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
            let mut include_value: Value =
                toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            let include_dir = path.parent().unwrap_or(base_dir);
            self.compose(&include.includes, include_dir)?;
            strip_include_loader_fields(&mut include_value);
            self.insert_definitions(include_value, &path)?;
            self.stack.remove(&canonical);
        }
        Ok(())
    }

    fn insert_definitions(&mut self, include: Value, path: &Path) -> anyhow::Result<()> {
        let Some(include_table) = include.as_table() else {
            return Ok(());
        };
        for key in [
            "target_templates",
            "launch_templates",
            "targets",
            "launches",
        ] {
            let Some(Value::Table(definitions)) = include_table.get(key) else {
                continue;
            };
            let root_definitions = ensure_table(self.root, key, path)?;
            for (name, definition) in definitions {
                if root_definitions.contains_key(name) {
                    anyhow::bail!(
                        "duplicate {} {name:?} from include {}",
                        singular_definition_kind(key),
                        path.display()
                    );
                }
                root_definitions.insert(name.clone(), definition.clone());
            }
        }
        Ok(())
    }
}

fn ensure_table<'a>(
    root: &'a mut toml::map::Map<String, Value>,
    key: &str,
    path: &Path,
) -> anyhow::Result<&'a mut toml::map::Map<String, Value>> {
    root.entry(key.to_owned())
        .or_insert_with(|| Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "include {} cannot merge into non-table root field {key:?}",
                path.display()
            )
        })
}

fn singular_definition_kind(key: &str) -> &'static str {
    match key {
        "target_templates" => "target template",
        "launch_templates" => "launch template",
        "targets" => "target",
        "launches" => "launch",
        _ => "definition",
    }
}

fn strip_include_loader_fields(value: &mut Value) {
    if let Some(table) = value.as_table_mut() {
        table.remove("includes");
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
        "http",
        "ssh_dispatch",
        "client_config",
        "target_defaults",
        "launch_defaults",
        "extends",
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
        let mut matched = false;
        for entry in glob(&pattern_text).with_context(|| format!("expand glob {pattern:?}"))? {
            matched = true;
            paths.push(entry.with_context(|| format!("read glob entry for {pattern:?}"))?);
        }
        if !matched {
            anyhow::bail!(
                "include pattern {pattern:?} matched no files under {}",
                base_dir.display()
            );
        }
    }
    paths.sort();
    Ok(paths)
}

fn canonical_existing_path(path: &Path) -> anyhow::Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))
}
