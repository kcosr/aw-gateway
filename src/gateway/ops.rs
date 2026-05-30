use super::model::{
    AllStatusEntry, GatewayStatus, LaunchDetail, LaunchSummary, ReadyStatus, TargetEntry,
};
use super::{
    OperationRunner, Runtime, client, launch_detail, launch_execute_with_config,
    launch_execute_with_config_cancelable, launch_summaries, load_config,
    run_container_command_with_runtime, status_all_entries, target_entries,
};
use crate::cli::{
    ClientConfigArgs, LaunchShowArgs, LaunchesArgs, RemoveArgs, RunArgs, SetDefaultArgs, StatusArg,
    StopArgs, TargetsArgs,
};
use crate::config::{GatewayConfig, LaunchConfig, LocalSshMode};
use crate::paths::{self, UserContext};
use crate::runtime::ContainerRuntime;
use anyhow::Context;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

mod ssh;
mod types;

pub(super) use ssh::{SshGatewayOperation, SshRenderOptions};
pub(super) use types::{
    CanonicalLaunchVarValue, ExecutionOutcome, LaunchPassthroughArgs, OperationError,
    OperationExecutionOptions, OperationMode, OperationResult, OutputSelection, RemoveResult,
    StopResult, SuppliedLaunchVars,
};

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
        args: LaunchPassthroughArgs,
        options: OperationExecutionOptions,
    },
    Stop {
        target: Option<String>,
        session_id: Option<String>,
    },
    Remove {
        target: Option<String>,
        session_id: Option<String>,
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

pub(super) fn lookup_launch(cfg: &GatewayConfig, name: &str) -> OperationResult<LaunchConfig> {
    if !cfg.launches.contains_key(name) {
        return Err(OperationError::unknown_launch(format!(
            "unknown launch {name:?}"
        )));
    }
    cfg.effective_launch(name)
        .map_err(OperationError::operation_failed)
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
        args: LaunchPassthroughArgs,
    ) -> Self {
        Self::Launch {
            name,
            session_id,
            vars,
            args,
            options: OperationExecutionOptions::STREAM,
        }
    }

    pub(super) fn from_stop_args(args: StopArgs) -> Self {
        Self::Stop {
            target: args.target,
            session_id: args.session_id,
        }
    }

    pub(super) fn from_remove_args(args: RemoveArgs) -> Self {
        Self::Remove {
            target: args.target,
            session_id: args.session_id,
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
                &cfg, &name, &launch,
            )?))
        }
        GatewayOperation::Launch {
            name,
            session_id,
            vars,
            args,
            options,
        } => {
            let cfg = load_config(config_path)?;
            Ok(GatewayOperationResult::Launch(
                launch_execute_with_config(cfg, &name, session_id, vars, args, options).await?,
            ))
        }
        GatewayOperation::Stop { target, session_id } => Ok(GatewayOperationResult::Stop(
            operation_stop(config_path, target, session_id).await?,
        )),
        GatewayOperation::Remove { target, session_id } => Ok(GatewayOperationResult::Remove(
            operation_remove(config_path, target, session_id).await?,
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

pub(super) async fn execute_gateway_operation_cancelable(
    config_path: Option<PathBuf>,
    operation: GatewayOperation,
    cancel: CancellationToken,
) -> OperationResult<GatewayOperationResult> {
    match operation {
        GatewayOperation::Run {
            target,
            session_id,
            cwd,
            command,
            options,
        } => Ok(GatewayOperationResult::Run(
            operation_run_cancelable(
                config_path,
                target,
                session_id,
                cwd,
                command,
                options,
                cancel,
            )
            .await?,
        )),
        GatewayOperation::Launch {
            name,
            session_id,
            vars,
            args,
            options,
        } => {
            let cfg = load_config(config_path)?;
            Ok(GatewayOperationResult::Launch(
                launch_execute_with_config_cancelable(
                    cfg, &name, session_id, vars, args, options, cancel,
                )
                .await?,
            ))
        }
        operation => execute_gateway_operation(config_path, operation).await,
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

async fn operation_run_cancelable(
    config_path: Option<PathBuf>,
    target: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    command: Vec<String>,
    options: OperationExecutionOptions,
    cancel: CancellationToken,
) -> OperationResult<ExecutionOutcome> {
    let runtime = Runtime::load(config_path, target.as_deref(), session_id, true).await?;
    OperationRunner::run_command(runtime, options, cwd, command)
        .run_cancelable(cancel)
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
    session_id: Option<String>,
) -> OperationResult<RemoveResult> {
    let runtime = Runtime::load(config_path, target.as_deref(), session_id, false).await?;
    let _lock = runtime.acquire_lifecycle_lock().await?;
    let Some(inspect) = runtime
        .container_runtime
        .inspect(&runtime.identity.container_name)
        .await?
    else {
        runtime.cleanup_control_socket_dir();
        runtime.apply_explicit_remove_workspace_cleanup().await;
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
    runtime.apply_explicit_remove_workspace_cleanup().await;
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
mod tests;
