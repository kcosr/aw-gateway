use crate::config;
use crate::gateway::failures::{AgentNotReady, ContainerNotFound};
use crate::runtime::GatewayLabelError;
use serde::Serialize;
use std::fmt;

#[derive(Debug)]
pub(in crate::gateway) enum OperationError {
    InvalidRequest { message: String },
    // HTTP allowlist failures happen before operation dispatch, but use this
    // variant so transport-visible operation denials share one projection path.
    DisabledAction { message: String },
    UnknownLaunch { message: String },
    InvalidLaunchVariable { message: String },
    InvalidSession { message: String },
    AgentNotReady { source: anyhow::Error },
    ContainerNotFound { source: anyhow::Error },
    ContainerLabelMismatch { source: anyhow::Error },
    OperationFailed { source: anyhow::Error },
}

impl OperationError {
    pub(in crate::gateway) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    pub(in crate::gateway) fn disabled_action(message: impl Into<String>) -> Self {
        Self::DisabledAction {
            message: message.into(),
        }
    }

    pub(in crate::gateway) fn unknown_launch(message: impl Into<String>) -> Self {
        Self::UnknownLaunch {
            message: message.into(),
        }
    }

    pub(in crate::gateway) fn invalid_launch_variable(message: impl Into<String>) -> Self {
        Self::InvalidLaunchVariable {
            message: message.into(),
        }
    }

    pub(in crate::gateway) fn invalid_session(message: impl Into<String>) -> Self {
        Self::InvalidSession {
            message: message.into(),
        }
    }

    pub(in crate::gateway) fn operation_failed(source: anyhow::Error) -> Self {
        if source.chain().any(|err| err.is::<AgentNotReady>()) {
            return Self::AgentNotReady { source };
        }
        if source.chain().any(|err| err.is::<ContainerNotFound>()) {
            return Self::ContainerNotFound { source };
        }
        if source.chain().any(|err| err.is::<GatewayLabelError>()) {
            return Self::ContainerLabelMismatch { source };
        }
        Self::OperationFailed { source }
    }
}

impl From<anyhow::Error> for OperationError {
    fn from(source: anyhow::Error) -> Self {
        Self::operation_failed(source)
    }
}

impl fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest { message }
            | Self::DisabledAction { message }
            | Self::UnknownLaunch { message }
            | Self::InvalidLaunchVariable { message }
            | Self::InvalidSession { message } => formatter.write_str(message),
            Self::AgentNotReady { source }
            | Self::ContainerNotFound { source }
            | Self::ContainerLabelMismatch { source }
            | Self::OperationFailed { source } => fmt::Display::fmt(source, formatter),
        }
    }
}

impl std::error::Error for OperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AgentNotReady { source }
            | Self::ContainerNotFound { source }
            | Self::ContainerLabelMismatch { source }
            | Self::OperationFailed { source } => Some(source.as_ref()),
            Self::InvalidRequest { .. }
            | Self::DisabledAction { .. }
            | Self::UnknownLaunch { .. }
            | Self::InvalidLaunchVariable { .. }
            | Self::InvalidSession { .. } => None,
        }
    }
}

pub(in crate::gateway) type OperationResult<T> = Result<T, OperationError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(in crate::gateway) enum OperationMode {
    Wait,
    Stream,
    // Fire-and-forget execution. The returned operation_id is a log
    // correlation handle only; there is no queryable operation registry.
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gateway) struct OutputSelection {
    pub(in crate::gateway) stdout: bool,
    pub(in crate::gateway) stderr: bool,
}

impl OutputSelection {
    pub(in crate::gateway) const BOTH: Self = Self {
        stdout: true,
        stderr: true,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gateway) struct OperationExecutionOptions {
    pub(in crate::gateway) mode: OperationMode,
    pub(in crate::gateway) output: OutputSelection,
}

impl OperationExecutionOptions {
    pub(in crate::gateway) const STREAM: Self = Self {
        mode: OperationMode::Stream,
        output: OutputSelection::BOTH,
    };

    #[cfg(test)]
    pub(in crate::gateway) const WAIT: Self = Self {
        mode: OperationMode::Wait,
        output: OutputSelection::BOTH,
    };

    #[cfg(test)]
    pub(in crate::gateway) const DETACH: Self = Self {
        mode: OperationMode::Detach,
        output: OutputSelection::BOTH,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::gateway) enum ExecutionOutcome {
    Streamed {
        exit_code: i32,
    },
    // Captured output remains bytes internally. The future HTTP JSON layer must
    // choose its own encoding policy instead of inheriting an accidental string conversion.
    Captured {
        exit_code: i32,
        stdout: Option<Vec<u8>>,
        stderr: Option<Vec<u8>>,
    },
    // The operation_id is intentionally not a registry key. Detached
    // background failures are logged and cannot be queried through the API.
    Detached {
        operation_id: String,
    },
}

impl ExecutionOutcome {
    pub(in crate::gateway) fn new(exit_code: i32) -> Self {
        Self::Streamed { exit_code }
    }

    pub(in crate::gateway) fn captured(
        exit_code: i32,
        stdout: Option<Vec<u8>>,
        stderr: Option<Vec<u8>>,
    ) -> Self {
        Self::Captured {
            exit_code,
            stdout,
            stderr,
        }
    }

    pub(in crate::gateway) fn detached(operation_id: String) -> Self {
        Self::Detached { operation_id }
    }

    pub(in crate::gateway) fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Streamed { exit_code } | Self::Captured { exit_code, .. } => Some(*exit_code),
            Self::Detached { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(in crate::gateway) struct SuppliedLaunchVars {
    values: std::collections::BTreeMap<String, CanonicalLaunchVarValue>,
}

impl SuppliedLaunchVars {
    pub(in crate::gateway) fn from_cli_pairs(supplied: Vec<String>) -> OperationResult<Self> {
        let mut vars = Self::default();
        for raw in supplied {
            let Some((key, value)) = raw.split_once('=') else {
                return Err(OperationError::invalid_launch_variable(
                    "--var must be key=value",
                ));
            };
            vars.insert(
                key.to_string(),
                CanonicalLaunchVarValue::String(value.to_string()),
            )?;
        }
        Ok(vars)
    }

    pub(in crate::gateway) fn insert(
        &mut self,
        key: String,
        value: CanonicalLaunchVarValue,
    ) -> OperationResult<()> {
        if self.values.insert(key.clone(), value).is_some() {
            return Err(OperationError::invalid_launch_variable(format!(
                "duplicate launch variable {key:?}"
            )));
        }
        Ok(())
    }

    pub(in crate::gateway) fn get(&self, key: &str) -> Option<&CanonicalLaunchVarValue> {
        self.values.get(key)
    }

    pub(in crate::gateway) fn keys(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::gateway) enum CanonicalLaunchVarValue {
    String(String),
    Boolean(bool),
    Number(String),
}

impl CanonicalLaunchVarValue {
    pub(in crate::gateway) fn from_config_default(value: &config::LaunchVarValue) -> Self {
        match value {
            config::LaunchVarValue::String(value) => Self::String(value.clone()),
            config::LaunchVarValue::Boolean(value) => Self::Boolean(*value),
            config::LaunchVarValue::Integer(value) => Self::Number(value.to_string()),
            config::LaunchVarValue::Float(value) => {
                Self::Number(config::canonical_number_string(*value))
            }
        }
    }

    pub(in crate::gateway) fn from_json_string(value: String) -> Self {
        Self::String(value)
    }

    pub(in crate::gateway) fn from_json_bool(value: bool) -> Self {
        Self::Boolean(value)
    }

    pub(in crate::gateway) fn from_json_number(
        key: &str,
        integer: Option<i64>,
        float: Option<f64>,
    ) -> Result<Self, String> {
        if let Some(value) = integer {
            Ok(Self::Number(value.to_string()))
        } else {
            let value = float
                .ok_or_else(|| format!("invalid launch variable {key:?}: number must be finite"))?;
            if !value.is_finite() {
                return Err(format!(
                    "invalid launch variable {key:?}: number must be finite"
                ));
            }
            Ok(Self::Number(config::canonical_number_string(value)))
        }
    }

    pub(in crate::gateway) fn coerce_for_config(
        &self,
        name: &str,
        var: &config::LaunchVarConfig,
    ) -> OperationResult<Self> {
        match var.var_type {
            config::LaunchVarType::String => match self {
                Self::String(value) => Ok(Self::String(value.clone())),
                _ => Err(OperationError::invalid_launch_variable(format!(
                    "invalid string launch variable {name:?}; expected string"
                ))),
            },
            config::LaunchVarType::Enum => {
                let Self::String(value) = self else {
                    return Err(OperationError::invalid_launch_variable(format!(
                        "invalid enum launch variable {name:?}; expected string"
                    )));
                };
                let values = var.values.as_deref().unwrap_or(&[]);
                if values.iter().any(|allowed| allowed == value) {
                    Ok(Self::String(value.clone()))
                } else {
                    Err(OperationError::invalid_launch_variable(format!(
                        "invalid enum launch variable {name:?}; expected one of {}",
                        values.join(", ")
                    )))
                }
            }
            config::LaunchVarType::Boolean => match self {
                Self::Boolean(value) => Ok(Self::Boolean(*value)),
                Self::String(value) if value == "true" || value == "false" => {
                    Ok(Self::Boolean(value == "true"))
                }
                _ => Err(OperationError::invalid_launch_variable(format!(
                    "invalid boolean launch variable {name:?}; expected true or false"
                ))),
            },
            config::LaunchVarType::Number => match self {
                Self::Number(value) => Ok(Self::Number(value.clone())),
                Self::String(value) => {
                    let parsed = value.parse::<f64>().map_err(|_| {
                        OperationError::invalid_launch_variable(format!(
                            "invalid number launch variable {name:?}"
                        ))
                    })?;
                    if !parsed.is_finite() {
                        return Err(OperationError::invalid_launch_variable(format!(
                            "invalid number launch variable {name:?}; expected finite number"
                        )));
                    }
                    Ok(Self::Number(canonical_cli_number(value, parsed)))
                }
                Self::Boolean(_) => Err(OperationError::invalid_launch_variable(format!(
                    "invalid number launch variable {name:?}"
                ))),
            },
        }
    }

    pub(in crate::gateway) fn rendered(&self) -> String {
        match self {
            Self::String(value) | Self::Number(value) => value.clone(),
            Self::Boolean(value) => value.to_string(),
        }
    }
}

fn canonical_cli_number(raw: &str, parsed: f64) -> String {
    if raw.parse::<i64>().is_ok() {
        raw.trim_start_matches('+').to_string()
    } else {
        config::canonical_number_string(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(in crate::gateway) struct StopResult {
    pub(in crate::gateway) container: String,
    pub(in crate::gateway) stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(in crate::gateway) struct RemoveResult {
    pub(in crate::gateway) container: String,
    pub(in crate::gateway) removed: bool,
}
