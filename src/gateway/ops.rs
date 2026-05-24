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
use crate::config::LocalSshMode;
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

#[derive(Debug)]
pub(super) enum OperationError {
    InvalidRequest { message: String },
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
            _ => None,
        }
    }
}

pub(super) type OperationResult<T> = Result<T, OperationError>;

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
    values: std::collections::BTreeMap<String, SuppliedLaunchVarValue>,
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
                SuppliedLaunchVarValue::String(value.to_string()),
            )?;
        }
        Ok(vars)
    }

    pub(super) fn insert(
        &mut self,
        key: String,
        value: SuppliedLaunchVarValue,
    ) -> OperationResult<()> {
        if self.values.insert(key.clone(), value).is_some() {
            return Err(OperationError::invalid_launch_variable(format!(
                "duplicate launch variable {key:?}"
            )));
        }
        Ok(())
    }

    pub(super) fn get(&self, key: &str) -> Option<&SuppliedLaunchVarValue> {
        self.values.get(key)
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(super) enum SuppliedLaunchVarValue {
    String(String),
    Boolean(bool),
    Integer(i64),
    Float(f64),
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

    pub(super) fn from_ssh_action(action: GatewayAction) -> OperationResult<Option<Self>> {
        Ok(match action {
            GatewayAction::Up(target) => Some(Self::Up {
                target,
                session_id: None,
            }),
            GatewayAction::Run(action) => Some(Self::from_run_action(action)),
            GatewayAction::Launches { .. } => Some(Self::Launches),
            GatewayAction::LaunchShow { name, .. } => Some(Self::LaunchShow { name }),
            GatewayAction::LaunchRun {
                name,
                session_id,
                vars,
            } => {
                let vars = SuppliedLaunchVars::from_cli_pairs(vars)?;
                Some(Self::launch_run(name, session_id, vars))
            }
            GatewayAction::Status(action) => Some(Self::from_status_action(action)),
            GatewayAction::Targets { .. } => Some(Self::Targets),
            GatewayAction::Stop(target) => Some(Self::Stop {
                target,
                session_id: None,
            }),
            GatewayAction::Remove(target) => Some(Self::Remove { target }),
            GatewayAction::SetDefault(target_or_image) => {
                Some(Self::SetDefault { target_or_image })
            }
            GatewayAction::ShowDefault => Some(Self::ShowDefault),
            GatewayAction::ResetDefault => Some(Self::ResetDefault),
            GatewayAction::ClientConfig(action) => Some(Self::ClientConfig {
                target: action.target,
                identity_file: action.identity_file.map(PathBuf::from),
            }),
            GatewayAction::Connect(_)
            | GatewayAction::AddKey(_)
            | GatewayAction::AddHostKey(_)
            | GatewayAction::AddContainerKey(_)
            | GatewayAction::ClientBundle(_)
            | GatewayAction::Help => None,
        })
    }

    fn from_run_action(action: RunAction) -> Self {
        Self::Run {
            target: action.target,
            session_id: action.session_id,
            cwd: action.cwd,
            command: action.command,
            options: OperationExecutionOptions::STREAM,
        }
    }

    fn from_status_action(action: StatusAction) -> Self {
        if action.all {
            Self::StatusAll
        } else {
            Self::Status {
                target: action.target,
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
            let launch = cfg
                .effective_launch(&name)
                .map_err(|err| OperationError::unknown_launch(err.to_string()))?;
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

    #[test]
    fn constructs_requests_from_ssh_actions_without_transport_flags() {
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Run(RunAction {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
                cwd: Some("/work".into()),
                command: vec!["cargo".into(), "test".into()],
            }))
            .unwrap(),
            Some(
                GatewayOperation::from_run_args(RunArgs {
                    target: Some("dev".into()),
                    session_id: Some("abc123".into()),
                    cwd: Some("/work".into()),
                    command: vec!["cargo".into(), "test".into()],
                })
                .unwrap()
            )
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::LaunchRun {
                name: "repo-shell".into(),
                session_id: Some("abc123".into()),
                vars: vec!["repo=https://example.test/repo.git".into()],
            })
            .unwrap(),
            Some(GatewayOperation::launch_run(
                "repo-shell".into(),
                Some("abc123".into()),
                SuppliedLaunchVars::from_cli_pairs(vec![
                    "repo=https://example.test/repo.git".into()
                ])
                .unwrap(),
            ))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Launches { json: true }).unwrap(),
            Some(GatewayOperation::from_launches_args(LaunchesArgs {
                json: false
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Targets { json: true }).unwrap(),
            Some(GatewayOperation::from_targets_args(TargetsArgs {
                json: false
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::LaunchShow {
                name: "repo-shell".into(),
                json: true,
            })
            .unwrap(),
            Some(GatewayOperation::from_launch_show_args(LaunchShowArgs {
                name: "repo-shell".into(),
                json: false,
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Status(StatusAction {
                target: Some("dev".into()),
                all: false,
            }))
            .unwrap(),
            Some(GatewayOperation::from_status_args(StatusArg {
                target: Some("dev".into()),
                all: false,
                json: false,
                session_id: None,
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Status(StatusAction {
                target: None,
                all: true,
            }))
            .unwrap(),
            Some(GatewayOperation::from_status_args(StatusArg {
                target: None,
                all: true,
                json: false,
                session_id: None,
            }))
        );
    }
}
