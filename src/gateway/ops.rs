use super::model::{
    AllStatusEntry, GatewayStatus, LaunchDetail, LaunchSummary, ReadyStatus, TargetEntry,
};
use super::{
    Runtime, client, launch_detail, launch_execute_with_config, launch_summaries, load_config,
    run_container_command_with_runtime, status_all_entries, target_entries,
};
use crate::cli::{
    ClientConfigArgs, LaunchShowArgs, LaunchesArgs, RunArgs, SetDefaultArgs, StatusArg, StopArgs,
    TargetArg, TargetsArgs,
};
use crate::config::{self, GatewayConfig, LaunchConfig, LocalSshMode};
use crate::paths::{self, UserContext};
use crate::runtime::ContainerRuntime;
use crate::ssh_dispatch::{GatewayAction, RunAction, StatusAction};
use anyhow::Context;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum GatewayOperation {
    Targets,
    Status {
        target: Option<String>,
        session_id: Option<String>,
    },
    StatusAll,
    Up {
        target: Option<String>,
        session_id: Option<String>,
    },
    Run {
        target: Option<String>,
        session_id: Option<String>,
        cwd: Option<String>,
        command: Vec<String>,
        options: OperationExecutionOptions,
    },
    Launches,
    LaunchShow {
        name: String,
    },
    Launch {
        name: String,
        session_id: Option<String>,
        vars: SuppliedLaunchVars,
        options: OperationExecutionOptions,
    },
    Stop {
        target: Option<String>,
        session_id: Option<String>,
    },
    Remove {
        target: Option<String>,
    },
    SetDefault {
        target_or_image: String,
    },
    ShowDefault,
    ResetDefault,
    ClientConfig {
        target: Option<String>,
        identity_file: Option<PathBuf>,
    },
}

#[derive(Debug)]
pub(super) enum GatewayOperationResult {
    Targets(Vec<TargetEntry>),
    Status(GatewayStatus),
    StatusAll(Vec<AllStatusEntry>),
    Up(ReadyStatus),
    Run(ExecutionOutcome),
    Launches(Vec<LaunchSummary>),
    LaunchShow(LaunchDetail),
    Launch(ExecutionOutcome),
    Stop(StopResult),
    Remove(RemoveResult),
    DefaultSelection(String),
    ClientConfig {
        rendered: String,
        written_path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SshGatewayOperation {
    pub(super) operation: GatewayOperation,
    pub(super) render: SshRenderOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct SshRenderOptions {
    pub(super) json: bool,
}

impl SshGatewayOperation {
    pub(super) fn from_action(action: &GatewayAction) -> OperationResult<Option<Self>> {
        Ok(match action {
            GatewayAction::Up(target) => Some(Self {
                operation: GatewayOperation::Up {
                    target: target.clone(),
                    session_id: None,
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Run(action) => Some(Self {
                operation: GatewayOperation::from_run_action(action),
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Launches { json } => Some(Self {
                operation: GatewayOperation::Launches,
                render: SshRenderOptions { json: *json },
            }),
            GatewayAction::LaunchShow { name, json } => Some(Self {
                operation: GatewayOperation::LaunchShow { name: name.clone() },
                render: SshRenderOptions { json: *json },
            }),
            GatewayAction::LaunchRun {
                name,
                session_id,
                vars,
            } => {
                let vars = SuppliedLaunchVars::from_cli_pairs(vars.clone())?;
                Some(Self {
                    operation: GatewayOperation::launch_run(name.clone(), session_id.clone(), vars),
                    render: SshRenderOptions::default(),
                })
            }
            GatewayAction::Status(action) => Some(Self {
                operation: GatewayOperation::from_status_action(action),
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Targets { json } => Some(Self {
                operation: GatewayOperation::Targets,
                render: SshRenderOptions { json: *json },
            }),
            GatewayAction::Stop(target) => Some(Self {
                operation: GatewayOperation::Stop {
                    target: target.clone(),
                    session_id: None,
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Remove(target) => Some(Self {
                operation: GatewayOperation::Remove {
                    target: target.clone(),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::SetDefault(target_or_image) => Some(Self {
                operation: GatewayOperation::SetDefault {
                    target_or_image: target_or_image.clone(),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::ShowDefault => Some(Self {
                operation: GatewayOperation::ShowDefault,
                render: SshRenderOptions::default(),
            }),
            GatewayAction::ResetDefault => Some(Self {
                operation: GatewayOperation::ResetDefault,
                render: SshRenderOptions::default(),
            }),
            GatewayAction::ClientConfig(action) => Some(Self {
                operation: GatewayOperation::ClientConfig {
                    target: action.target.clone(),
                    identity_file: action.identity_file.clone().map(PathBuf::from),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Connect(_)
            | GatewayAction::AddKey(_)
            | GatewayAction::AddHostKey(_)
            | GatewayAction::AddContainerKey(_)
            | GatewayAction::ClientBundle(_)
            | GatewayAction::Help => None,
        })
    }
}

#[derive(Debug)]
pub(super) enum OperationError {
    InvalidRequest { message: String },
    // HTTP allowlist failures happen before operation dispatch, but use this
    // variant so transport-visible operation denials share one projection path.
    DisabledAction { message: String },
    UnknownLaunch { message: String },
    InvalidLaunchVariable { message: String },
    InvalidSession { message: String },
    OperationFailed { source: anyhow::Error },
}

impl OperationError {
    pub(super) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    pub(super) fn disabled_action(message: impl Into<String>) -> Self {
        Self::DisabledAction {
            message: message.into(),
        }
    }

    pub(super) fn unknown_launch(message: impl Into<String>) -> Self {
        Self::UnknownLaunch {
            message: message.into(),
        }
    }

    pub(super) fn invalid_launch_variable(message: impl Into<String>) -> Self {
        Self::InvalidLaunchVariable {
            message: message.into(),
        }
    }

    pub(super) fn invalid_session(message: impl Into<String>) -> Self {
        Self::InvalidSession {
            message: message.into(),
        }
    }

    pub(super) fn operation_failed(source: anyhow::Error) -> Self {
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
            Self::OperationFailed { source } => fmt::Display::fmt(source, formatter),
        }
    }
}

impl std::error::Error for OperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OperationFailed { source } => Some(source.as_ref()),
            Self::InvalidRequest { .. }
            | Self::DisabledAction { .. }
            | Self::UnknownLaunch { .. }
            | Self::InvalidLaunchVariable { .. }
            | Self::InvalidSession { .. } => None,
        }
    }
}

pub(super) type OperationResult<T> = Result<T, OperationError>;

pub(super) fn lookup_launch(cfg: &GatewayConfig, name: &str) -> OperationResult<LaunchConfig> {
    if !cfg.launches.contains_key(name) {
        return Err(OperationError::unknown_launch(format!(
            "unknown launch {name:?}"
        )));
    }
    cfg.effective_launch(name)
        .map_err(OperationError::operation_failed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum OperationMode {
    Wait,
    Stream,
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OutputSelection {
    pub(super) stdout: bool,
    pub(super) stderr: bool,
}

impl OutputSelection {
    pub(super) const BOTH: Self = Self {
        stdout: true,
        stderr: true,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OperationExecutionOptions {
    pub(super) mode: OperationMode,
    pub(super) output: OutputSelection,
}

impl OperationExecutionOptions {
    pub(super) const STREAM: Self = Self {
        mode: OperationMode::Stream,
        output: OutputSelection::BOTH,
    };

    #[cfg(test)]
    pub(super) const WAIT: Self = Self {
        mode: OperationMode::Wait,
        output: OutputSelection::BOTH,
    };

    #[cfg(test)]
    pub(super) const DETACH: Self = Self {
        mode: OperationMode::Detach,
        output: OutputSelection::BOTH,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExecutionOutcome {
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
    Detached {
        operation_id: String,
    },
}

impl ExecutionOutcome {
    pub(super) fn new(exit_code: i32) -> Self {
        Self::Streamed { exit_code }
    }

    pub(super) fn captured(
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

    pub(super) fn detached(operation_id: String) -> Self {
        Self::Detached { operation_id }
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Streamed { exit_code } | Self::Captured { exit_code, .. } => Some(*exit_code),
            Self::Detached { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct SuppliedLaunchVars {
    values: std::collections::BTreeMap<String, CanonicalLaunchVarValue>,
}

impl SuppliedLaunchVars {
    pub(super) fn from_cli_pairs(supplied: Vec<String>) -> OperationResult<Self> {
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

    pub(super) fn insert(
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

    pub(super) fn get(&self, key: &str) -> Option<&CanonicalLaunchVarValue> {
        self.values.get(key)
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum CanonicalLaunchVarValue {
    String(String),
    Boolean(bool),
    Number(String),
}

impl CanonicalLaunchVarValue {
    pub(super) fn from_config_default(value: &config::LaunchVarValue) -> Self {
        match value {
            config::LaunchVarValue::String(value) => Self::String(value.clone()),
            config::LaunchVarValue::Boolean(value) => Self::Boolean(*value),
            config::LaunchVarValue::Integer(value) => Self::Number(value.to_string()),
            config::LaunchVarValue::Float(value) => {
                Self::Number(config::canonical_number_string(*value))
            }
        }
    }

    pub(super) fn from_json_string(value: String) -> Self {
        Self::String(value)
    }

    pub(super) fn from_json_bool(value: bool) -> Self {
        Self::Boolean(value)
    }

    pub(super) fn from_json_number(
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

    pub(super) fn coerce_for_config(
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

    pub(super) fn rendered(&self) -> String {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StopResult {
    pub(super) container: String,
    pub(super) stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoveResult {
    pub(super) container: String,
    pub(super) removed: bool,
}

impl GatewayOperation {
    pub(super) fn from_targets_args(_args: TargetsArgs) -> Self {
        Self::Targets
    }

    pub(super) fn from_status_args(args: StatusArg) -> Self {
        if args.all {
            Self::StatusAll
        } else {
            Self::Status {
                target: args.target,
                session_id: args.session_id,
            }
        }
    }

    pub(super) fn from_run_args(args: RunArgs) -> OperationResult<Self> {
        if args.command.is_empty() {
            return Err(OperationError::invalid_request(
                "run requires -- followed by a command; use up to start or hold a target",
            ));
        }
        Ok(Self::Run {
            target: args.target,
            session_id: args.session_id,
            cwd: args.cwd,
            command: args.command,
            options: OperationExecutionOptions::STREAM,
        })
    }

    pub(super) fn from_launches_args(_args: LaunchesArgs) -> Self {
        Self::Launches
    }

    pub(super) fn from_launch_show_args(args: LaunchShowArgs) -> Self {
        Self::LaunchShow { name: args.name }
    }

    pub(super) fn launch_run(
        name: String,
        session_id: Option<String>,
        vars: SuppliedLaunchVars,
    ) -> Self {
        Self::Launch {
            name,
            session_id,
            vars,
            options: OperationExecutionOptions::STREAM,
        }
    }

    pub(super) fn from_stop_args(args: StopArgs) -> Self {
        Self::Stop {
            target: args.target,
            session_id: args.session_id,
        }
    }

    pub(super) fn from_remove_args(args: TargetArg) -> Self {
        Self::Remove {
            target: args.target,
        }
    }

    pub(super) fn from_set_default_args(args: SetDefaultArgs) -> OperationResult<Self> {
        if args.reset {
            return Ok(Self::ResetDefault);
        }
        let target_or_image = args.target_or_image.ok_or_else(|| {
            OperationError::invalid_request("target or image is required unless --reset is used")
        })?;
        Ok(Self::SetDefault { target_or_image })
    }

    pub(super) fn from_client_config_args(args: ClientConfigArgs) -> Self {
        Self::ClientConfig {
            target: args.target,
            identity_file: args.identity_file,
        }
    }

    fn from_run_action(action: &RunAction) -> Self {
        Self::Run {
            target: action.target.clone(),
            session_id: action.session_id.clone(),
            cwd: action.cwd.clone(),
            command: action.command.clone(),
            options: OperationExecutionOptions::STREAM,
        }
    }

    fn from_status_action(action: &StatusAction) -> Self {
        if action.all {
            Self::StatusAll
        } else {
            Self::Status {
                target: action.target.clone(),
                session_id: None,
            }
        }
    }
}

pub(super) async fn execute_gateway_operation(
    config_path: Option<PathBuf>,
    operation: GatewayOperation,
) -> OperationResult<GatewayOperationResult> {
    match operation {
        GatewayOperation::Targets => {
            let cfg = load_config(config_path)?;
            Ok(GatewayOperationResult::Targets(target_entries(&cfg)?))
        }
        GatewayOperation::Status { target, session_id } => Ok(GatewayOperationResult::Status(
            operation_status(config_path, target, session_id).await?,
        )),
        GatewayOperation::StatusAll => Ok(GatewayOperationResult::StatusAll(
            operation_status_all(config_path).await?,
        )),
        GatewayOperation::Up { target, session_id } => Ok(GatewayOperationResult::Up(
            operation_up(config_path, target, session_id).await?,
        )),
        GatewayOperation::Run {
            target,
            session_id,
            cwd,
            command,
            options,
        } => Ok(GatewayOperationResult::Run(
            operation_run(config_path, target, session_id, cwd, command, options).await?,
        )),
        GatewayOperation::Launches => {
            let cfg = load_config(config_path)?;
            Ok(GatewayOperationResult::Launches(launch_summaries(&cfg)?))
        }
        GatewayOperation::LaunchShow { name } => {
            let cfg = load_config(config_path)?;
            let launch = lookup_launch(&cfg, &name)?;
            Ok(GatewayOperationResult::LaunchShow(launch_detail(
                &name, &launch,
            )))
        }
        GatewayOperation::Launch {
            name,
            session_id,
            vars,
            options,
        } => {
            let cfg = load_config(config_path)?;
            Ok(GatewayOperationResult::Launch(
                launch_execute_with_config(cfg, &name, session_id, vars, options).await?,
            ))
        }
        GatewayOperation::Stop { target, session_id } => Ok(GatewayOperationResult::Stop(
            operation_stop(config_path, target, session_id).await?,
        )),
        GatewayOperation::Remove { target } => Ok(GatewayOperationResult::Remove(
            operation_remove(config_path, target).await?,
        )),
        GatewayOperation::SetDefault { target_or_image } => {
            Ok(GatewayOperationResult::DefaultSelection(
                operation_set_default(config_path, target_or_image)?,
            ))
        }
        GatewayOperation::ShowDefault => Ok(GatewayOperationResult::DefaultSelection(
            operation_show_default(config_path)?,
        )),
        GatewayOperation::ResetDefault => Ok(GatewayOperationResult::DefaultSelection(
            operation_reset_default(config_path)?,
        )),
        GatewayOperation::ClientConfig {
            target,
            identity_file,
        } => {
            let (rendered, written_path) =
                operation_client_config(config_path, target, identity_file).await?;
            Ok(GatewayOperationResult::ClientConfig {
                rendered,
                written_path: Some(written_path),
            })
        }
    }
}

async fn operation_run(
    config_path: Option<PathBuf>,
    target: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    command: Vec<String>,
    options: OperationExecutionOptions,
) -> OperationResult<ExecutionOutcome> {
    let runtime = Runtime::load(config_path, target.as_deref(), session_id, true).await?;
    run_container_command_with_runtime(runtime, cwd, command, options)
        .await
        .map_err(OperationError::operation_failed)
}

fn operation_set_default(
    config_path: Option<PathBuf>,
    target_or_image: String,
) -> anyhow::Result<String> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    let _ = client::resolve_target_selection(&cfg, Some(&target_or_image))
        .with_context(|| format!("validate default selection {target_or_image:?}"))?;
    paths::ensure_private_dir(&user.config_dir())?;
    let path = user.config_dir().join("default-target");
    std::fs::write(&path, format!("{target_or_image}\n"))?;
    Ok(target_or_image)
}

fn operation_show_default(config_path: Option<PathBuf>) -> anyhow::Result<String> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    let selection = client::read_default_selection(&user)
        .transpose()?
        .unwrap_or_else(|| client::configured_default_display(&cfg));
    let _ = client::resolve_target_selection(&cfg, Some(&selection))
        .with_context(|| format!("validate default selection {selection:?}"))?;
    Ok(selection)
}

fn operation_reset_default(config_path: Option<PathBuf>) -> anyhow::Result<String> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    let path = user.config_dir().join("default-target");
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
    }
    Ok(client::configured_default_display(&cfg))
}

async fn operation_client_config(
    config_path: Option<PathBuf>,
    target: Option<String>,
    identity_file: Option<PathBuf>,
) -> OperationResult<(String, PathBuf)> {
    let runtime = Runtime::load(config_path, target.as_deref(), None, false).await?;
    let config = runtime.render_client_config(identity_file.as_deref())?;
    let written_path = runtime.write_inner_config(&config)?;
    Ok((config, written_path))
}

async fn operation_up(
    config_path: Option<PathBuf>,
    target: Option<String>,
    session_id: Option<String>,
) -> OperationResult<ReadyStatus> {
    let runtime = Runtime::load(config_path, target.as_deref(), session_id, true).await?;
    operation_up_with_runtime(runtime)
        .await
        .map_err(OperationError::operation_failed)
}

pub(super) async fn operation_up_with_runtime(runtime: Runtime) -> anyhow::Result<ReadyStatus> {
    if let Some(local_ssh) = &runtime.target.local_ssh
        && local_ssh.mode == LocalSshMode::Listen
    {
        anyhow::bail!(
            "gateway action \"up\" over SSH is not supported for local_ssh.mode = \"listen\" targets; use connect or run aw-gateway up locally"
        );
    }
    runtime.ensure_ready().await
}

async fn operation_stop(
    config_path: Option<PathBuf>,
    target: Option<String>,
    session_id: Option<String>,
) -> OperationResult<StopResult> {
    let runtime = Runtime::load(config_path, target.as_deref(), session_id, false).await?;
    let _lock = runtime.acquire_lifecycle_lock().await?;
    let Some(inspect) = runtime
        .container_runtime
        .inspect(&runtime.identity.container_name)
        .await?
    else {
        return Ok(StopResult {
            container: runtime.identity.container_name,
            stopped: false,
        });
    };
    runtime.stop_inspected_container(&inspect).await?;
    Ok(StopResult {
        container: runtime.identity.container_name,
        stopped: true,
    })
}

async fn operation_remove(
    config_path: Option<PathBuf>,
    target: Option<String>,
) -> OperationResult<RemoveResult> {
    let runtime = Runtime::load(config_path, target.as_deref(), None, false).await?;
    let _lock = runtime.acquire_lifecycle_lock().await?;
    let Some(inspect) = runtime
        .container_runtime
        .inspect(&runtime.identity.container_name)
        .await?
    else {
        runtime.cleanup_control_socket_dir();
        return Ok(RemoveResult {
            container: runtime.identity.container_name,
            removed: false,
        });
    };
    runtime.validate_labels(&inspect)?;
    let was_running = inspect.state.running;
    if inspect.state.running {
        runtime.stop_inspected_container(&inspect).await?;
    }
    if let Some(current) = runtime
        .container_runtime
        .inspect(&runtime.identity.container_name)
        .await?
    {
        runtime.validate_labels(&current)?;
        runtime
            .container_runtime
            .rm(&runtime.identity.container_name)
            .await?;
    }
    if !was_running {
        runtime.cleanup_control_socket_dir();
    }
    Ok(RemoveResult {
        container: runtime.identity.container_name,
        removed: true,
    })
}

async fn operation_status(
    config_path: Option<PathBuf>,
    target: Option<String>,
    session_id: Option<String>,
) -> OperationResult<GatewayStatus> {
    let runtime = Runtime::load(config_path, target.as_deref(), session_id, false).await?;
    runtime
        .status()
        .await
        .map_err(OperationError::operation_failed)
}

async fn operation_status_all(config_path: Option<PathBuf>) -> anyhow::Result<Vec<AllStatusEntry>> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    let container_runtime = ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home)?;
    let containers = container_runtime
        .list_managed_containers(&user.user, user.uid)
        .await?;
    Ok(status_all_entries(&cfg, containers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LaunchShowArgs, LaunchesArgs, RunArgs, StatusArg, TargetsArgs};
    use crate::config::{LaunchVarConfig, LaunchVarType, LaunchVarValue};
    use crate::ssh_dispatch::{
        ClientBundleAction, ClientConfigAction, KeyAction, KeySourceAction, TargetSessionAction,
    };

    fn launch_var_config(var_type: LaunchVarType, values: Option<Vec<&str>>) -> LaunchVarConfig {
        LaunchVarConfig {
            var_type,
            required: false,
            default: None,
            values: values.map(|values| values.into_iter().map(str::to_string).collect()),
            description: None,
        }
    }

    fn assert_coerced_rendered(
        name: &str,
        var: LaunchVarConfig,
        value: CanonicalLaunchVarValue,
        expected: &str,
    ) {
        let rendered = value.coerce_for_config(name, &var).unwrap().rendered();
        assert_eq!(rendered, expected);
    }

    fn assert_coercion_error(
        name: &str,
        var: LaunchVarConfig,
        value: CanonicalLaunchVarValue,
        expected: &str,
    ) {
        let err = value.coerce_for_config(name, &var).unwrap_err();
        assert!(
            err.to_string().contains(expected),
            "expected {expected:?} in {err}"
        );
    }

    #[test]
    fn canonical_launch_var_values_coerce_render_and_convert_defaults_for_config_types() {
        assert_coerced_rendered(
            "repo",
            launch_var_config(LaunchVarType::String, None),
            CanonicalLaunchVarValue::String("https://example.test/repo.git".into()),
            "https://example.test/repo.git",
        );
        assert_coerced_rendered(
            "mode",
            launch_var_config(LaunchVarType::Enum, Some(vec!["fast", "safe"])),
            CanonicalLaunchVarValue::String("safe".into()),
            "safe",
        );
        assert_coerced_rendered(
            "debug",
            launch_var_config(LaunchVarType::Boolean, None),
            CanonicalLaunchVarValue::String("true".into()),
            "true",
        );
        assert_coerced_rendered(
            "debug",
            launch_var_config(LaunchVarType::Boolean, None),
            CanonicalLaunchVarValue::String("false".into()),
            "false",
        );
        assert_coerced_rendered(
            "debug",
            launch_var_config(LaunchVarType::Boolean, None),
            CanonicalLaunchVarValue::Boolean(true),
            "true",
        );
        assert_coerced_rendered(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("2.0".into()),
            "2",
        );
        assert_coerced_rendered(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("+2".into()),
            "2",
        );
        assert_coerced_rendered(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("001".into()),
            "001",
        );
        assert_coerced_rendered(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::Number("3".into()),
            "3",
        );
        assert_coerced_rendered(
            "ratio",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::Number("1.5".into()),
            "1.5",
        );
        assert_coerced_rendered(
            "limit",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::from_config_default(&LaunchVarValue::Float(2.0)),
            "2",
        );

        assert_coercion_error(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("abc".into()),
            "invalid number launch variable \"count\"",
        );
        assert_coercion_error(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("NaN".into()),
            "invalid number launch variable \"count\"; expected finite number",
        );
        assert_coercion_error(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("inf".into()),
            "invalid number launch variable \"count\"; expected finite number",
        );
        assert_coercion_error(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::String("-inf".into()),
            "invalid number launch variable \"count\"; expected finite number",
        );
        assert_coercion_error(
            "debug",
            launch_var_config(LaunchVarType::Boolean, None),
            CanonicalLaunchVarValue::String("yes".into()),
            "invalid boolean launch variable \"debug\"; expected true or false",
        );
        assert_coercion_error(
            "repo",
            launch_var_config(LaunchVarType::String, None),
            CanonicalLaunchVarValue::Boolean(true),
            "invalid string launch variable \"repo\"; expected string",
        );
        assert_coercion_error(
            "mode",
            launch_var_config(LaunchVarType::Enum, Some(vec!["fast", "safe"])),
            CanonicalLaunchVarValue::Boolean(true),
            "invalid enum launch variable \"mode\"; expected string",
        );
        assert_coercion_error(
            "count",
            launch_var_config(LaunchVarType::Number, None),
            CanonicalLaunchVarValue::Boolean(true),
            "invalid number launch variable \"count\"",
        );
    }

    #[test]
    fn canonical_launch_var_json_constructors_preserve_http_scalar_rules() {
        assert_eq!(
            CanonicalLaunchVarValue::from_json_string("repo".into()),
            CanonicalLaunchVarValue::String("repo".into())
        );
        assert_eq!(
            CanonicalLaunchVarValue::from_json_bool(true),
            CanonicalLaunchVarValue::Boolean(true)
        );
        assert_eq!(
            CanonicalLaunchVarValue::from_json_number("count", Some(3), Some(3.0)).unwrap(),
            CanonicalLaunchVarValue::Number("3".into())
        );
        assert_eq!(
            CanonicalLaunchVarValue::from_json_number("ratio", None, Some(1.5)).unwrap(),
            CanonicalLaunchVarValue::Number("1.5".into())
        );

        let err = CanonicalLaunchVarValue::from_json_number("count", None, None).unwrap_err();
        assert_eq!(
            err,
            "invalid launch variable \"count\": number must be finite"
        );
        let err = CanonicalLaunchVarValue::from_json_number("count", None, Some(f64::INFINITY))
            .unwrap_err();
        assert_eq!(
            err,
            "invalid launch variable \"count\": number must be finite"
        );
    }

    #[test]
    fn operation_error_display_preserves_messages_and_source() {
        for (err, expected) in [
            (
                OperationError::invalid_request("missing argument"),
                "missing argument",
            ),
            (
                OperationError::disabled_action("http action \"run\" is disabled"),
                "http action \"run\" is disabled",
            ),
            (
                OperationError::unknown_launch("unknown launch \"repo\""),
                "unknown launch \"repo\"",
            ),
            (
                OperationError::invalid_launch_variable(
                    "missing required launch variable \"repo\"",
                ),
                "missing required launch variable \"repo\"",
            ),
            (
                OperationError::invalid_session("invalid session id \"../bad\""),
                "invalid session id \"../bad\"",
            ),
        ] {
            assert_eq!(err.to_string(), expected);
            assert!(std::error::Error::source(&err).is_none());
        }

        let err = OperationError::operation_failed(anyhow::anyhow!("runtime failed"));
        assert_eq!(err.to_string(), "runtime failed");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn operation_error_constructors_set_expected_variants() {
        assert!(matches!(
            OperationError::invalid_request("x"),
            OperationError::InvalidRequest { .. }
        ));
        assert!(matches!(
            OperationError::disabled_action("x"),
            OperationError::DisabledAction { .. }
        ));
        assert!(matches!(
            OperationError::unknown_launch("x"),
            OperationError::UnknownLaunch { .. }
        ));
        assert!(matches!(
            OperationError::invalid_launch_variable("x"),
            OperationError::InvalidLaunchVariable { .. }
        ));
        assert!(matches!(
            OperationError::invalid_session("x"),
            OperationError::InvalidSession { .. }
        ));
        assert!(matches!(
            OperationError::operation_failed(anyhow::anyhow!("x")),
            OperationError::OperationFailed { .. }
        ));
    }

    #[test]
    fn launch_lookup_distinguishes_missing_launch_from_invalid_launch_config() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
use = ["missing-template"]
target = "default"
command = ["true"]
"#,
        )
        .unwrap();

        let err = lookup_launch(&cfg, "missing").unwrap_err();
        assert!(matches!(
            err,
            OperationError::UnknownLaunch { ref message }
                if message == "unknown launch \"missing\""
        ));

        let err = lookup_launch(&cfg, "agent").unwrap_err();
        assert!(matches!(err, OperationError::OperationFailed { .. }));
        assert!(
            err.to_string()
                .contains("launch \"agent\" uses launch template \"missing-template\""),
            "{err}"
        );
    }

    #[test]
    fn constructs_targets_request_without_rendering_flags() {
        assert_eq!(
            GatewayOperation::from_targets_args(TargetsArgs { json: true }),
            GatewayOperation::Targets
        );
    }

    #[test]
    fn constructs_status_and_status_all_requests_without_json() {
        assert_eq!(
            GatewayOperation::from_status_args(StatusArg {
                target: Some("dev".into()),
                all: false,
                json: true,
                session_id: Some("abc123".into()),
            }),
            GatewayOperation::Status {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
            }
        );
        assert_eq!(
            GatewayOperation::from_status_args(StatusArg {
                target: None,
                all: true,
                json: true,
                session_id: None,
            }),
            GatewayOperation::StatusAll
        );
    }

    #[test]
    fn constructs_run_request() {
        let operation = GatewayOperation::from_run_args(RunArgs {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
            cwd: Some("/work".into()),
            command: vec!["cargo".into(), "test".into()],
        })
        .unwrap();
        assert_eq!(
            operation,
            GatewayOperation::Run {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
                cwd: Some("/work".into()),
                command: vec!["cargo".into(), "test".into()],
                options: OperationExecutionOptions::STREAM,
            }
        );
    }

    #[test]
    fn constructs_launch_discovery_requests_without_json() {
        assert_eq!(
            GatewayOperation::from_launches_args(LaunchesArgs { json: true }),
            GatewayOperation::Launches
        );
        assert_eq!(
            GatewayOperation::from_launch_show_args(LaunchShowArgs {
                name: "repo-shell".into(),
                json: true,
            }),
            GatewayOperation::LaunchShow {
                name: "repo-shell".into(),
            }
        );
    }

    #[test]
    fn constructs_launch_run_request() {
        assert_eq!(
            GatewayOperation::launch_run(
                "repo-shell".into(),
                Some("abc123".into()),
                SuppliedLaunchVars::from_cli_pairs(vec![
                    "repo=https://example.test/repo.git".into()
                ])
                .unwrap(),
            ),
            GatewayOperation::Launch {
                name: "repo-shell".into(),
                session_id: Some("abc123".into()),
                vars: SuppliedLaunchVars::from_cli_pairs(vec![
                    "repo=https://example.test/repo.git".into()
                ])
                .unwrap(),
                options: OperationExecutionOptions::STREAM,
            }
        );
    }

    fn assert_ssh_operation_request(
        action: GatewayAction,
        operation: GatewayOperation,
        render: SshRenderOptions,
    ) {
        assert_eq!(
            SshGatewayOperation::from_action(&action).unwrap(),
            Some(SshGatewayOperation { operation, render })
        );
    }

    #[test]
    fn constructs_operation_requests_from_ssh_actions() {
        assert_ssh_operation_request(
            GatewayAction::Up(Some("dev".into())),
            GatewayOperation::Up {
                target: Some("dev".into()),
                session_id: None,
            },
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::Run(RunAction {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
                cwd: Some("/work".into()),
                command: vec!["cargo".into(), "test".into()],
            }),
            GatewayOperation::from_run_args(RunArgs {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
                cwd: Some("/work".into()),
                command: vec!["cargo".into(), "test".into()],
            })
            .unwrap(),
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::LaunchRun {
                name: "repo-shell".into(),
                session_id: Some("abc123".into()),
                vars: vec!["repo=https://example.test/repo.git".into()],
            },
            GatewayOperation::launch_run(
                "repo-shell".into(),
                Some("abc123".into()),
                SuppliedLaunchVars::from_cli_pairs(vec![
                    "repo=https://example.test/repo.git".into(),
                ])
                .unwrap(),
            ),
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::Status(StatusAction {
                target: Some("dev".into()),
                all: false,
            }),
            GatewayOperation::from_status_args(StatusArg {
                target: Some("dev".into()),
                all: false,
                json: false,
                session_id: None,
            }),
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::Status(StatusAction {
                target: None,
                all: true,
            }),
            GatewayOperation::from_status_args(StatusArg {
                target: None,
                all: true,
                json: false,
                session_id: None,
            }),
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::Stop(Some("dev".into())),
            GatewayOperation::Stop {
                target: Some("dev".into()),
                session_id: None,
            },
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::Remove(Some("dev".into())),
            GatewayOperation::Remove {
                target: Some("dev".into()),
            },
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::SetDefault("fedora-dev".into()),
            GatewayOperation::SetDefault {
                target_or_image: "fedora-dev".into(),
            },
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::ShowDefault,
            GatewayOperation::ShowDefault,
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::ResetDefault,
            GatewayOperation::ResetDefault,
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::ClientConfig(ClientConfigAction {
                target: Some("dev".into()),
                identity_file: Some("/tmp/id".into()),
            }),
            GatewayOperation::ClientConfig {
                target: Some("dev".into()),
                identity_file: Some(PathBuf::from("/tmp/id")),
            },
            SshRenderOptions::default(),
        );
    }

    #[test]
    fn preserves_ssh_render_options_for_metadata_actions() {
        for json in [false, true] {
            assert_ssh_operation_request(
                GatewayAction::Launches { json },
                GatewayOperation::from_launches_args(LaunchesArgs { json: false }),
                SshRenderOptions { json },
            );
            assert_ssh_operation_request(
                GatewayAction::Targets { json },
                GatewayOperation::from_targets_args(TargetsArgs { json: false }),
                SshRenderOptions { json },
            );
            assert_ssh_operation_request(
                GatewayAction::LaunchShow {
                    name: "repo-shell".into(),
                    json,
                },
                GatewayOperation::from_launch_show_args(LaunchShowArgs {
                    name: "repo-shell".into(),
                    json: false,
                }),
                SshRenderOptions { json },
            );
        }
    }

    #[test]
    fn status_ssh_requests_ignore_transport_json_and_render_as_status_operations() {
        assert_ssh_operation_request(
            GatewayAction::Status(StatusAction {
                target: Some("dev".into()),
                all: false,
            }),
            GatewayOperation::Status {
                target: Some("dev".into()),
                session_id: None,
            },
            SshRenderOptions::default(),
        );
        assert_ssh_operation_request(
            GatewayAction::Status(StatusAction {
                target: None,
                all: true,
            }),
            GatewayOperation::StatusAll,
            SshRenderOptions::default(),
        );
    }

    #[test]
    fn non_operation_ssh_actions_are_deferred() {
        for action in [
            GatewayAction::Connect(TargetSessionAction {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
            }),
            GatewayAction::AddKey(KeyAction {
                target: Some("dev".into()),
                public_key: Some("-".into()),
            }),
            GatewayAction::AddHostKey(KeySourceAction {
                public_key: Some("-".into()),
            }),
            GatewayAction::AddContainerKey(KeyAction {
                target: Some("dev".into()),
                public_key: Some("-".into()),
            }),
            GatewayAction::ClientBundle(ClientBundleAction {
                target: Some("dev".into()),
                identity_file: Some("/tmp/id".into()),
                rotate_key: true,
            }),
            GatewayAction::Help,
        ] {
            assert_eq!(SshGatewayOperation::from_action(&action).unwrap(), None);
        }
    }

    #[test]
    fn ssh_launch_var_conversion_errors_remain_typed() {
        let err = SshGatewayOperation::from_action(&GatewayAction::LaunchRun {
            name: "repo-shell".into(),
            session_id: None,
            vars: vec!["repo=a".into(), "repo=b".into()],
        })
        .unwrap_err();
        assert!(matches!(err, OperationError::InvalidLaunchVariable { .. }));
        assert_eq!(err.to_string(), "duplicate launch variable \"repo\"");
    }

    #[test]
    fn launch_var_conversion_uses_existing_cli_pair_validation() {
        let err = SshGatewayOperation::from_action(&GatewayAction::LaunchRun {
            name: "repo-shell".into(),
            session_id: None,
            vars: vec!["repo".into()],
        })
        .unwrap_err();
        assert!(matches!(err, OperationError::InvalidLaunchVariable { .. }));
        assert_eq!(err.to_string(), "--var must be key=value");
    }
}
