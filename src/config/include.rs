use super::{GatewayConfig, LaunchConfigInput, TargetConfigInput};
use anyhow::Context;
use glob::glob;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(super) fn compose_gateway_includes(
    cfg: &mut GatewayConfig,
    root_path: &Path,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    let root_dir = root_path.parent().unwrap_or_else(|| Path::new("."));
    let root_canonical = canonical_existing_path(root_path)?;
    let mut stack = BTreeSet::from([root_canonical]);
    IncludeComposer {
        target_templates: &mut cfg.target_templates,
        launch_templates: &mut cfg.launch_templates,
        targets: &mut cfg.targets,
        launches: &mut cfg.launches,
        seen: &mut seen,
        stack: &mut stack,
    }
    .compose(&cfg.includes, root_dir)?;
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigIncludeFile {
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    target_templates: BTreeMap<String, TargetConfigInput>,
    #[serde(default)]
    launch_templates: BTreeMap<String, LaunchConfigInput>,
    #[serde(default)]
    targets: BTreeMap<String, TargetConfigInput>,
    #[serde(default)]
    launches: BTreeMap<String, LaunchConfigInput>,
}

struct IncludeComposer<'a> {
    target_templates: &'a mut BTreeMap<String, TargetConfigInput>,
    launch_templates: &'a mut BTreeMap<String, LaunchConfigInput>,
    targets: &'a mut BTreeMap<String, TargetConfigInput>,
    launches: &'a mut BTreeMap<String, LaunchConfigInput>,
    seen: &'a mut BTreeSet<PathBuf>,
    stack: &'a mut BTreeSet<PathBuf>,
}

impl IncludeComposer<'_> {
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
            let include_dir = path.parent().unwrap_or(base_dir);
            self.compose(&include.includes, include_dir)?;
            self.insert_definitions(include, &path)?;
            self.stack.remove(&canonical);
        }
        Ok(())
    }

    fn insert_definitions(
        &mut self,
        include: ConfigIncludeFile,
        path: &Path,
    ) -> anyhow::Result<()> {
        for (name, template) in include.target_templates {
            if self
                .target_templates
                .insert(name.clone(), template)
                .is_some()
            {
                anyhow::bail!(
                    "duplicate target template {name:?} from include {}",
                    path.display()
                );
            }
        }
        for (name, launch_template) in include.launch_templates {
            if self
                .launch_templates
                .insert(name.clone(), launch_template)
                .is_some()
            {
                anyhow::bail!(
                    "duplicate launch template {name:?} from include {}",
                    path.display()
                );
            }
        }
        for (name, target) in include.targets {
            if self.targets.insert(name.clone(), target).is_some() {
                anyhow::bail!("duplicate target {name:?} from include {}", path.display());
            }
        }
        for (name, launch) in include.launches {
            if self.launches.insert(name.clone(), launch).is_some() {
                anyhow::bail!("duplicate launch {name:?} from include {}", path.display());
            }
        }
        Ok(())
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
        for entry in glob(&pattern_text).with_context(|| format!("expand glob {pattern:?}"))? {
            paths.push(entry.with_context(|| format!("read glob entry for {pattern:?}"))?);
        }
    }
    paths.sort();
    Ok(paths)
}

fn canonical_existing_path(path: &Path) -> anyhow::Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))
}
