use anyhow::Context;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

pub const CONTEXT_LABEL_PREFIX: &str = "io.aw-gateway.context.";
pub const MAX_CONTEXT_FILE_BYTES: u64 = 64 * 1024;
pub const MAX_CONTEXT_ENTRIES: usize = 64;

#[derive(Debug)]
pub struct ContextValidationError {
    message: String,
}

impl ContextValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ContextValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ContextValidationError {}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextVarConfig {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub format: ContextValueFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextValueFormat {
    #[default]
    Slug,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeContext {
    values: BTreeMap<String, String>,
}

impl RuntimeContext {
    pub fn empty() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    pub fn from_map(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    pub fn into_map(self) -> BTreeMap<String, String> {
        self.values
    }

    pub fn insert_template_vars(&self, vars: &mut crate::template::Vars) {
        for (key, value) in &self.values {
            vars.insert(format!("context.{key}"), value.clone());
        }
    }

    pub fn label_filters(&self) -> BTreeMap<String, String> {
        self.values
            .iter()
            .map(|(key, value)| (context_label_key(key), value.clone()))
            .collect()
    }

    pub fn matches_stored(&self, stored: &BTreeMap<String, String>) -> bool {
        if self.values.is_empty() {
            return stored.is_empty();
        }
        self.values
            .iter()
            .all(|(key, value)| stored.get(key).is_some_and(|stored| stored == value))
    }
}

pub fn context_label_key(key: &str) -> String {
    format!("{CONTEXT_LABEL_PREFIX}{key}")
}

pub fn context_from_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter_map(|(key, value)| {
            let context_key = key.strip_prefix(CONTEXT_LABEL_PREFIX)?;
            validate_context_slug(context_key)
                .is_ok()
                .then(|| (context_key.to_string(), value.clone()))
        })
        .collect()
}

pub fn validate_context_slug(value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 64 {
        anyhow::bail!("context slug {value:?} must be 1-64 characters");
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        anyhow::bail!(
            "context slug {value:?} must not start or end with '-' or contain consecutive '-'"
        );
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        anyhow::bail!(
            "context slug {value:?} must contain only lowercase ASCII letters, numbers, and '-'"
        );
    }
    Ok(())
}

pub fn validate_context_var_declarations(
    declarations: &BTreeMap<String, ContextVarConfig>,
) -> anyhow::Result<()> {
    if declarations.len() > MAX_CONTEXT_ENTRIES {
        anyhow::bail!("context_vars contains more than {MAX_CONTEXT_ENTRIES} entries");
    }
    for key in declarations.keys() {
        validate_context_slug(key).with_context(|| format!("validate context_vars.{key}"))?;
    }
    Ok(())
}

pub fn validate_runtime_context(
    declarations: &BTreeMap<String, ContextVarConfig>,
    supplied: &RuntimeContext,
) -> anyhow::Result<()> {
    validate_supplied_context(declarations, supplied)?;
    for (key, declaration) in declarations {
        if declaration.required && !supplied.as_map().contains_key(key) {
            return Err(ContextValidationError::new(format!(
                "missing required context key {key:?}"
            ))
            .into());
        }
    }
    Ok(())
}

pub fn validate_supplied_context(
    declarations: &BTreeMap<String, ContextVarConfig>,
    supplied: &RuntimeContext,
) -> anyhow::Result<()> {
    if supplied.len() > MAX_CONTEXT_ENTRIES {
        return Err(ContextValidationError::new(format!(
            "context contains more than {MAX_CONTEXT_ENTRIES} entries"
        ))
        .into());
    }
    for (key, value) in supplied.as_map() {
        validate_context_slug(key).map_err(|err| {
            ContextValidationError::new(format!("validate context key {key:?}: {err}"))
        })?;
        let Some(declaration) = declarations.get(key) else {
            return Err(ContextValidationError::new(format!("unknown context key {key:?}")).into());
        };
        match declaration.format {
            ContextValueFormat::Slug => validate_context_slug(value).map_err(|err| {
                ContextValidationError::new(format!("validate context value for {key:?}: {err}"))
            })?,
        }
    }
    Ok(())
}

pub fn parse_context_sources(
    files: &[PathBuf],
    pairs: &[String],
) -> anyhow::Result<RuntimeContext> {
    let mut values = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for path in files {
        let file_values = parse_context_file(path)?;
        for (key, value) in file_values {
            insert_unique_context_value(&mut values, &mut seen, key, value)?;
        }
    }
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("context value {pair:?} must use key=value"))?;
        if key.is_empty() || value.is_empty() {
            anyhow::bail!("context value {pair:?} must use non-empty key=value");
        }
        insert_unique_context_value(&mut values, &mut seen, key.to_string(), value.to_string())?;
    }
    if values.len() > MAX_CONTEXT_ENTRIES {
        anyhow::bail!("context contains more than {MAX_CONTEXT_ENTRIES} entries");
    }
    Ok(RuntimeContext::from_map(values))
}

fn insert_unique_context_value(
    values: &mut BTreeMap<String, String>,
    seen: &mut BTreeSet<String>,
    key: String,
    value: String,
) -> anyhow::Result<()> {
    if !seen.insert(key.clone()) {
        anyhow::bail!("duplicate context key {key:?}");
    }
    values.insert(key, value);
    Ok(())
}

fn parse_context_file(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("read context file {}", path.display()))?;
    if metadata.len() == 0 {
        anyhow::bail!("context file {} is empty", path.display());
    }
    if metadata.len() > MAX_CONTEXT_FILE_BYTES {
        anyhow::bail!(
            "context file {} exceeds {MAX_CONTEXT_FILE_BYTES} bytes",
            path.display()
        );
    }
    let raw =
        std::fs::read(path).with_context(|| format!("read context file {}", path.display()))?;
    let mut deserializer = serde_json::Deserializer::from_slice(&raw);
    let values = deserialize_context_object(&mut deserializer)
        .with_context(|| format!("parse context file {}", path.display()))?;
    deserializer
        .end()
        .with_context(|| format!("parse context file {}", path.display()))?;
    Ok(values)
}

pub(crate) fn deserialize_context_object<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_map(ContextObjectVisitor)
}

struct ContextObjectVisitor;

impl<'de> Visitor<'de> for ContextObjectVisitor {
    type Value = BTreeMap<String, String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object with string values")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate context key {key:?}"
                )));
            }
            let Some(value) = value.as_str() else {
                return Err(serde::de::Error::custom(format!(
                    "context key {key:?} must have a string value"
                )));
            };
            values.insert(key, value.to_string());
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declarations() -> BTreeMap<String, ContextVarConfig> {
        BTreeMap::from([
            (
                "tenant".into(),
                ContextVarConfig {
                    required: true,
                    format: ContextValueFormat::Slug,
                    description: None,
                },
            ),
            (
                "workspace".into(),
                ContextVarConfig {
                    required: false,
                    format: ContextValueFormat::Slug,
                    description: None,
                },
            ),
        ])
    }

    #[test]
    fn parses_context_files_and_flags_without_override_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context.json");
        std::fs::write(&file, r#"{"tenant":"acme"}"#).unwrap();

        let context = parse_context_sources(&[file], &["workspace=web".into()]).unwrap();

        assert_eq!(
            context.as_map().get("tenant").map(String::as_str),
            Some("acme")
        );
        assert_eq!(
            context.as_map().get("workspace").map(String::as_str),
            Some("web")
        );
    }

    #[test]
    fn rejects_duplicate_context_keys_across_sources() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context.json");
        std::fs::write(&file, r#"{"tenant":"acme"}"#).unwrap();

        let err = parse_context_sources(&[file], &["tenant=other".into()])
            .unwrap_err()
            .to_string();

        assert!(err.contains("duplicate context key"), "{err}");
    }

    #[test]
    fn rejects_duplicate_context_keys_within_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context.json");
        std::fs::write(&file, r#"{"tenant":"acme","tenant":"other"}"#).unwrap();

        let err = format!("{:#}", parse_context_sources(&[file], &[]).unwrap_err());

        assert!(err.contains("duplicate context key"), "{err}");
    }

    #[test]
    fn rejects_trailing_garbage_after_context_file_object() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context.json");
        std::fs::write(&file, r#"{"tenant":"acme"} garbage"#).unwrap();

        let err = format!("{:#}", parse_context_sources(&[file], &[]).unwrap_err());

        assert!(err.contains("trailing characters"), "{err}");
    }

    #[test]
    fn validates_required_unknown_and_slug_context_values() {
        let declarations = declarations();

        let valid = RuntimeContext::from_map(BTreeMap::from([("tenant".into(), "acme".into())]));
        validate_runtime_context(&declarations, &valid).unwrap();

        let missing = RuntimeContext::empty();
        let err = validate_runtime_context(&declarations, &missing)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing required context key"), "{err}");

        let unknown = RuntimeContext::from_map(BTreeMap::from([
            ("tenant".into(), "acme".into()),
            ("other".into(), "value".into()),
        ]));
        let err = validate_runtime_context(&declarations, &unknown)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown context key"), "{err}");

        let invalid = RuntimeContext::from_map(BTreeMap::from([("tenant".into(), "Acme".into())]));
        let err = format!(
            "{:#}",
            validate_runtime_context(&declarations, &invalid).unwrap_err()
        );
        assert!(err.contains("context slug"), "{err}");
    }

    #[test]
    fn empty_context_is_not_wildcard_for_stored_context() {
        let empty = RuntimeContext::empty();
        assert!(empty.matches_stored(&BTreeMap::new()));
        assert!(!empty.matches_stored(&BTreeMap::from([("tenant".into(), "acme".into())])));

        let supplied = RuntimeContext::from_map(BTreeMap::from([("tenant".into(), "acme".into())]));
        assert!(supplied.matches_stored(&BTreeMap::from([
            ("tenant".into(), "acme".into()),
            ("workspace".into(), "web".into()),
        ])));
    }
}
