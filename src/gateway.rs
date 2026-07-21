use crate::cli::{
    AddContainerKeyArgs, AddHostKeyArgs, AddKeyArgs, ClientBundleArgs, ClientConfigArgs,
    ConfigPathsArgs, ConnectArgs, GatewayArgs, GatewayCommand, GatewayConfigCommand, LaunchCommand,
    LaunchesArgs, RemoveArgs, RunArgs, SetDefaultArgs, ShellArgs, StatusArg, StopArgs, TargetsArgs,
    UpArgs,
};
use crate::config::{
    ContainerRuntimeType, ControlSocketConfig, GatewayConfig, LaunchConfig, LaunchStep,
    LaunchStepLocation, LaunchVarConfig, LaunchVarType, LifecyclePhase, LocalSshBackend,
    LocalSshMode, LocalSshReadiness, TargetAccessMethod, TargetConfig, TargetMode, validate_name,
    validate_passwd_scalar,
};
use crate::context::{RuntimeContext, parse_context_sources, validate_runtime_context};
use crate::launch_args::{LaunchRunArgRole, LaunchRunArgs, parse_launch_run_args_from};
use crate::paths::{self, UserContext};
use crate::runtime::{
    ContainerExecCaptureResult, ContainerExecSpec, ContainerExecStatusResult, ContainerRuntime,
};
use crate::ssh_dispatch::{self, Dispatch, GatewayAction};
use crate::ssh_filter::{
    SshCommandDecision, SshCommandFilterPolicy, decide_command, format_ssh_original_command,
};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_GATEWAY_CONFIG: &str = include_str!("../aw-gateway.sample.toml");
const MAX_SSH_ORIGINAL_COMMAND_BYTES: usize = 64 * 1024;

#[cfg(target_os = "linux")]
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;
#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd"
))]
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 103;
#[cfg(not(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 103;

mod agent_client;
mod client;
mod container_spec;
mod control_sockets;
mod execution;
mod failures;
mod health;
mod http;
mod identity;
mod lifecycle;
mod lifecycle_hooks;
mod listener;
mod model;
mod ops;
mod published_port;
mod render;
mod session;
mod status_view;
mod token;
mod workspace;

use agent_client::AgentSessionHold;
use client::{read_default_selection, resolve_target_selection};
#[cfg(test)]
use container_spec::DEFAULT_SESSION_SHELL_ENV;
use control_sockets::render_control_socket_paths;
use execution::{OperationRunner, PreparedExecution, run_container_command_with_runtime};
use health::run_argv_with_options;
#[cfg(test)]
use health::run_health_check;
use lifecycle::{FailedStartCleanup, readiness_plan};
use lifecycle_hooks::host_hook_timeout;
use model::{
    GatewayStatus, LaunchDetail, LaunchStepDetail, LaunchSummary, LaunchVarMetadata, LocalSshReady,
    ReadyStatus, TargetEntry, TcpEndpoint, gateway_status_name,
};
use ops::{
    CanonicalLaunchVarValue, CapturedStream, ExecutionOutcome, GatewayOperation,
    GatewayOperationResult, LaunchPassthroughArgs, OperationError, OperationExecutionOptions,
    OperationMode, OperationResult, SshGatewayOperation, SshRenderOptions, SuppliedLaunchVars,
    execute_gateway_operation, execute_gateway_operation_with_context, lookup_launch,
    operation_up_with_runtime,
};
use render::{
    render_default_selection, render_launch_detail, render_launches, render_remove_result,
    render_status_all, render_status_result, render_stop_result, render_targets, render_up_result,
};
use session::{generate_session_id_value, validate_session_id};
use status_view::{status_all_entries, status_launch};
use workspace::resolve_target_workspace;

#[cfg(test)]
use crate::config::{
    ContainerMountMode, HealthCheck, IdleCleanupAction, IdleCleanupOwner, WorkspaceCleanup,
};
#[cfg(test)]
use crate::runtime;
#[cfg(test)]
use crate::runtime::{ContainerInspect, ManagedContainer};
#[cfg(test)]
use client::{configured_default_display, normalize_image_selection};
#[cfg(test)]
use execution::detach_discard_options;
#[cfg(test)]
use identity::{
    ensure_identity_token_file, is_plausible_public_key, read_control_token_file,
    validate_control_token_content, validate_identity_token_content, validate_public_key_content,
};
#[cfg(test)]
use lifecycle::ContainerReadinessPlan;
#[cfg(test)]
use model::{AllStatusEntry, LocalListenerStatus, SessionMarker};
#[cfg(test)]
use ops::OutputSelection;
#[cfg(test)]
use ops::{RemoveResult, StopResult};
#[cfg(test)]
use render::{remove_result_text, status_result_text, stop_result_text};
#[cfg(test)]
use session::{
    local_listener_is_active, parse_process_start_time, process_start_time,
    session_marker_is_active,
};
#[cfg(test)]
use tokio::time::{Duration, sleep};
#[cfg(test)]
use workspace::validate_workspace_cleanup_path;

pub async fn run(args: GatewayArgs) -> anyhow::Result<()> {
    let context = parse_context_sources(&args.context_files, &args.context)?;
    run_with_context(args, context).await
}

pub async fn run_with_context(args: GatewayArgs, context: RuntimeContext) -> anyhow::Result<()> {
    match args.command {
        Some(GatewayCommand::Config(command)) => run_config(command, args.config).await,
        Some(GatewayCommand::Connect(connect_args)) => {
            connect(args.config, connect_args, context).await
        }
        Some(GatewayCommand::Up(status)) => up(args.config, status, context).await,
        Some(GatewayCommand::Run(run_args)) => {
            run_container_command(args.config, run_args, context).await
        }
        Some(GatewayCommand::Shell(shell_args)) => {
            shell_container(args.config, shell_args, context).await
        }
        Some(GatewayCommand::Launch(launch_command)) => {
            launch(args.config, launch_command, context).await
        }
        Some(GatewayCommand::Launches(launches_args)) => {
            launches(args.config, launches_args, context).await
        }
        Some(GatewayCommand::Stop(stop_args)) => stop(args.config, stop_args, context).await,
        Some(GatewayCommand::Remove(target_arg)) => remove(args.config, target_arg, context).await,
        Some(GatewayCommand::Status(status_args)) => {
            status(args.config, status_args, context).await
        }
        Some(GatewayCommand::Targets(targets_args)) => targets(args.config, targets_args).await,
        Some(GatewayCommand::Http) => http_listener(args.config).await,
        Some(GatewayCommand::SetDefault(set_default_args)) => {
            set_default(args.config, set_default_args).await
        }
        Some(GatewayCommand::ShowDefault) => show_default(args.config).await,
        Some(GatewayCommand::ResetDefault) => {
            set_default(
                args.config,
                SetDefaultArgs {
                    target_or_image: None,
                    reset: true,
                },
            )
            .await
        }
        Some(GatewayCommand::AddKey(add_key_args)) => {
            client::add_key(args.config, add_key_args).await
        }
        Some(GatewayCommand::AddHostKey(add_host_key_args)) => {
            client::add_host_key(add_host_key_args).await
        }
        Some(GatewayCommand::AddContainerKey(add_container_key_args)) => {
            client::add_container_key(args.config, add_container_key_args).await
        }
        Some(GatewayCommand::Help) => gateway_help(args.config).await,
        Some(GatewayCommand::ClientConfig(client_config_args)) => {
            client_config(args.config, client_config_args).await
        }
        Some(GatewayCommand::ClientBundle(client_bundle_args)) => {
            client::client_bundle(args.config, client_bundle_args).await
        }
        None => dispatch_from_ssh(args.config).await,
    }
}

async fn run_config(command: GatewayConfigCommand, config: Option<PathBuf>) -> anyhow::Result<()> {
    match command {
        GatewayConfigCommand::Validate => {
            let path = paths::resolve_gateway_config(config)?.selected_path()?;
            GatewayConfig::load(&path)?;
            println!("ok");
            Ok(())
        }
        GatewayConfigCommand::Paths(args) => config_paths(config, args),
    }
}

fn config_paths(config: Option<PathBuf>, args: ConfigPathsArgs) -> anyhow::Result<()> {
    let resolution = paths::resolve_gateway_config(config)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resolution)?);
        return Ok(());
    }

    if let Some(user) = &resolution.user {
        println!("user: {}", user.user);
        println!("uid: {}", user.uid);
        println!("gid: {}", user.gid);
        println!("home: {}", user.home.display());
    } else {
        println!("user: unavailable");
    }
    print_optional_path("user_config_dir", resolution.user_config_dir.as_deref());
    print_optional_path("user_state_dir", resolution.user_state_dir.as_deref());
    match &resolution.user_config_file {
        Some(path) => println!(
            "user_config_file: {} ({})",
            path.display(),
            exists_label(path.exists())
        ),
        None => println!("user_config_file: unavailable"),
    }
    println!(
        "system_config_file: {} ({})",
        resolution.system_config_file.display(),
        exists_label(resolution.system_config_file.exists())
    );
    match &resolution.selected_path {
        Some(path) => println!(
            "selected: {} ({})",
            path.display(),
            source_label(resolution.selected_source)
        ),
        None => println!("selected: none"),
    }
    Ok(())
}

fn print_optional_path(label: &str, path: Option<&std::path::Path>) {
    match path {
        Some(path) => println!("{label}: {}", path.display()),
        None => println!("{label}: unavailable"),
    }
}

fn exists_label(exists: bool) -> &'static str {
    if exists { "exists" } else { "missing" }
}

fn source_label(source: paths::GatewayConfigSource) -> &'static str {
    match source {
        paths::GatewayConfigSource::ExplicitFlag => "explicit_flag",
        paths::GatewayConfigSource::Environment => "environment",
        paths::GatewayConfigSource::User => "user",
        paths::GatewayConfigSource::System => "system",
        paths::GatewayConfigSource::None => "none",
    }
}

async fn http_listener(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    http::serve(config_path).await
}

async fn dispatch_from_ssh(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = load_config(config_path.clone())?;
    let original = std::env::var("SSH_ORIGINAL_COMMAND").ok();
    if original
        .as_ref()
        .is_some_and(|value| value.len() > MAX_SSH_ORIGINAL_COMMAND_BYTES)
    {
        anyhow::bail!("SSH_ORIGINAL_COMMAND exceeds {MAX_SSH_ORIGINAL_COMMAND_BYTES} bytes");
    }
    if let Some(command) = original.as_deref() {
        let transfer = cfg.effective_container_ssh_defaults()?.transfer;
        let policy = SshCommandFilterPolicy {
            sftp: transfer.sftp,
            legacy_scp: transfer.legacy_scp,
        };
        match decide_command(&policy, Some(command)) {
            SshCommandDecision::RejectLegacyScp => {
                anyhow::bail!("blocked by policy: legacy scp is not allowed");
            }
            SshCommandDecision::RejectSftp => {
                anyhow::bail!("blocked by policy: sftp is not allowed");
            }
            SshCommandDecision::RejectComposedTransfer => {
                anyhow::bail!(
                    "blocked by policy: shell composition invokes a restricted transfer command\nrejected SSH_ORIGINAL_COMMAND: {}",
                    format_ssh_original_command(command)
                );
            }
            SshCommandDecision::LoginShell | SshCommandDecision::RunCommand(_) => {}
        }
    }
    let has_pty = std::io::stdin().is_terminal();
    match ssh_dispatch::dispatch(original.as_deref(), has_pty, &cfg.ssh_dispatch) {
        Dispatch::InteractiveShell => {
            run_container_command(
                config_path,
                RunArgs {
                    target: None,
                    session_id: None,
                    cwd: None,
                    command: vec!["/usr/bin/bash".into()],
                },
                RuntimeContext::empty(),
            )
            .await
        }
        Dispatch::ContainerCommand(command) => {
            run_container_command(
                config_path,
                RunArgs {
                    target: None,
                    session_id: None,
                    cwd: None,
                    command: vec!["/usr/bin/bash".into(), "-lc".into(), command],
                },
                RuntimeContext::empty(),
            )
            .await
        }
        Dispatch::Gateway(action) => run_gateway_action(config_path, action).await,
        Dispatch::Reject(reason) => anyhow::bail!("{reason}"),
    }
}

async fn run_gateway_action(
    config_path: Option<PathBuf>,
    action: GatewayAction,
) -> anyhow::Result<()> {
    if let Some(request) = SshGatewayOperation::from_action(&action)? {
        let operation = request.operation;
        let render_operation = operation.clone();
        let result = execute_gateway_operation(config_path, operation).await?;
        return render_operation_result(&render_operation, result, request.render);
    }
    match action {
        GatewayAction::Connect(action) => {
            connect(
                config_path,
                ConnectArgs {
                    target: action.target,
                    session_id: action.session_id,
                },
                RuntimeContext::empty(),
            )
            .await
        }
        GatewayAction::AddKey(action) => {
            client::add_key(
                config_path,
                AddKeyArgs {
                    target: action.target,
                    public_key: action.public_key.map(PathBuf::from),
                },
            )
            .await
        }
        GatewayAction::AddHostKey(action) => {
            client::add_host_key(AddHostKeyArgs {
                public_key: action.public_key.map(PathBuf::from),
            })
            .await
        }
        GatewayAction::AddContainerKey(action) => {
            client::add_container_key(
                config_path,
                AddContainerKeyArgs {
                    target: action.target,
                    public_key: action.public_key.map(PathBuf::from),
                },
            )
            .await
        }
        GatewayAction::Help => gateway_help(config_path).await,
        GatewayAction::ClientBundle(action) => {
            client::client_bundle_from_ssh_dispatch(
                config_path,
                ClientBundleArgs {
                    target: action.target,
                    identity_file: action.identity_file.map(PathBuf::from),
                    rotate_key: action.rotate_key,
                },
            )
            .await
        }
        GatewayAction::Up(_)
        | GatewayAction::Run(_)
        | GatewayAction::Launches { .. }
        | GatewayAction::LaunchShow { .. }
        | GatewayAction::LaunchRun { .. }
        | GatewayAction::Status(_)
        | GatewayAction::Targets { .. }
        | GatewayAction::Stop(_)
        | GatewayAction::Remove(_)
        | GatewayAction::SetDefault(_)
        | GatewayAction::ShowDefault
        | GatewayAction::ResetDefault
        | GatewayAction::ClientConfig(_) => {
            unreachable!("operation-backed SSH actions return before deferred dispatch")
        }
    }
}

fn render_operation_result(
    operation: &GatewayOperation,
    result: GatewayOperationResult,
    render: SshRenderOptions,
) -> anyhow::Result<()> {
    match (operation, result) {
        (GatewayOperation::Up { .. }, GatewayOperationResult::Up(ready)) => render_up_result(ready),
        (GatewayOperation::Run { .. }, GatewayOperationResult::Run(outcome))
        | (GatewayOperation::Launch { .. }, GatewayOperationResult::Launch(outcome)) => {
            exit_with_execution_outcome(outcome)
        }
        (GatewayOperation::Launches, GatewayOperationResult::Launches(entries)) => {
            render_launches(entries, render.json)
        }
        (GatewayOperation::LaunchShow { .. }, GatewayOperationResult::LaunchShow(detail)) => {
            render_launch_detail(detail, render.json)
        }
        (GatewayOperation::Status { .. }, GatewayOperationResult::Status(status)) => {
            render_status_result(status, true)
        }
        (GatewayOperation::StatusAll, GatewayOperationResult::StatusAll(entries)) => {
            render_status_all(entries, true)
        }
        (GatewayOperation::Targets, GatewayOperationResult::Targets(entries)) => {
            render_targets(entries, render.json)
        }
        (GatewayOperation::Stop { .. }, GatewayOperationResult::Stop(result)) => {
            render_stop_result(&result);
            Ok(())
        }
        (GatewayOperation::Remove { .. }, GatewayOperationResult::Remove(result)) => {
            render_remove_result(&result);
            Ok(())
        }
        (
            GatewayOperation::SetDefault { .. }
            | GatewayOperation::ShowDefault
            | GatewayOperation::ResetDefault,
            GatewayOperationResult::DefaultSelection(selection),
        ) => {
            render_default_selection(&selection);
            Ok(())
        }
        (
            GatewayOperation::ClientConfig { .. },
            GatewayOperationResult::ClientConfig {
                rendered,
                written_path,
            },
        ) => {
            let _written_path = written_path;
            println!("{rendered}");
            Ok(())
        }
        _ => unreachable!("operation result did not match SSH renderer"),
    }
}

async fn gateway_help(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = load_config(config_path)?;
    println!("{}", gateway_help_text(&cfg));
    Ok(())
}

fn gateway_help_text(cfg: &GatewayConfig) -> String {
    let mut lines = vec!["AW Gateway commands:".to_string(), String::new()];
    let commands = [
        (
            "up [target]",
            "Start or reuse the container and print readiness",
        ),
        ("status [target]", "Show container status"),
        ("status --all", "List your managed containers"),
        ("targets", "List configured targets"),
        (
            "run [--session-id ID] [target] [--cwd DIR] -- <command>",
            "Run a command in the container",
        ),
        ("launches", "List configured launches"),
        ("launch show <name>", "Show launch variables and steps"),
        (
            "launch <name> [--session-id ID] [--var key=value]",
            "Run a configured launch",
        ),
        ("stop [target]", "Stop the container"),
        ("remove [target]", "Stop and remove the container"),
        ("show-default", "Show your default target"),
        ("set-default <target>", "Set your default target"),
        ("reset-default", "Reset default target to the site default"),
        (
            "client-config [target]",
            "Print SSH config for VS Code/SCP/SFTP",
        ),
        (
            "client-bundle [target]",
            "Generate a managed SSH key/config bundle",
        ),
        (
            "add-key [target]",
            "Add an SSH public key to host and container",
        ),
        ("add-host-key", "Add an SSH public key to the host"),
        (
            "add-container-key [target]",
            "Add an SSH public key to the container",
        ),
        (
            "connect [--session-id ID] [target]",
            "Connect to the container SSH service",
        ),
        ("help", "Show this help"),
    ];
    for (command, description) in commands {
        if let Some(action) = command.split_whitespace().next()
            && cfg
                .ssh_dispatch
                .enabled_actions
                .iter()
                .any(|enabled| enabled == action)
        {
            lines.push(format!("  {command:<38} {description}"));
        }
    }
    lines.extend([
        String::new(),
        "Examples:".to_string(),
        "  ssh host status".to_string(),
        "  ssh host targets".to_string(),
        "  ssh host set-default fedora-dev".to_string(),
        "  cat ~/.ssh/id_ed25519.pub | ssh host 'add-key ubuntu-dev --public-key -'".to_string(),
        "  ssh host client-config ubuntu-dev".to_string(),
        "  ssh host client-bundle ubuntu-dev".to_string(),
    ]);
    lines.join("\n")
}

async fn connect(
    config_path: Option<PathBuf>,
    args: ConnectArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let runtime = Runtime::load_with_context(
        config_path,
        args.target.as_deref(),
        args.session_id,
        true,
        context,
    )
    .await?;
    runtime.ensure_ssh_endpoint_configured()?;
    let (session, ready_result) = runtime.begin_ready_session("connect", false).await?;
    let proxy_result = async {
        let ready = ready_result?;
        listener::proxy_ready_to_stdio(&ready).await
    }
    .await;
    let outcome = SessionOutcome::from_result(&proxy_result);
    runtime
        .finish_post_session(session, proxy_result, outcome)
        .await
}

async fn up(
    config_path: Option<PathBuf>,
    status: UpArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let runtime = Runtime::load_with_context(
        config_path,
        status.target.as_deref(),
        status.session_id,
        true,
        context,
    )
    .await?;
    if let Some(local_ssh) = &runtime.target.local_ssh
        && local_ssh.mode == LocalSshMode::Listen
    {
        runtime.ensure_ssh_endpoint_configured()?;
        let (session, ready_result) = runtime.begin_ready_session("local-listen", false).await?;
        let up_result = async {
            let mut ready = ready_result?;
            let bound = listener::bind_local_ssh(&runtime).await?;
            ready.local_ssh = Some(bound.ready.clone());
            let config = runtime
                .render_client_config(None, client::ClientConfigOrigin::LocalCli)
                .await?;
            ready.client_config = Some(runtime.write_inner_config(&config)?);
            println!("{}", serde_json::to_string_pretty(&ready)?);
            let target = ready.ssh_target()?;
            listener::serve_local_ssh(bound, target).await
        }
        .await;
        let outcome = SessionOutcome::from_result(&up_result);
        return runtime
            .finish_post_session(session, up_result, outcome)
            .await;
    }
    // Non-listen `up` is a warm-up operation: it starts or validates the
    // target and exits without holding an active session marker.
    let ready = operation_up_with_runtime(runtime).await?;
    render_up_result(ready)
}

async fn run_container_command(
    config_path: Option<PathBuf>,
    args: RunArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_run_args(args)?;
    let result = execute_gateway_operation_with_context(config_path, operation, context).await?;
    let GatewayOperationResult::Run(outcome) = result else {
        unreachable!("run operation returned a different result");
    };
    exit_with_execution_outcome(outcome)
}

async fn shell_container(
    config_path: Option<PathBuf>,
    args: ShellArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let runtime = Runtime::load_with_context(
        config_path,
        args.target.as_deref(),
        args.session_id,
        true,
        context,
    )
    .await?;
    let command = shell_command(&runtime.identity.session_shell, args.args);
    let outcome = run_container_command_with_runtime(
        runtime,
        args.cwd,
        command,
        OperationExecutionOptions::STREAM,
    )
    .await?;
    exit_with_execution_outcome(outcome)
}

fn shell_command(session_shell: &str, args: Vec<String>) -> Vec<String> {
    let mut command = Vec::with_capacity(args.len() + 1);
    command.push(session_shell.to_string());
    command.extend(args);
    command
}

fn exit_with_execution_outcome(outcome: ExecutionOutcome) -> ! {
    let code = outcome
        .exit_code()
        .expect("CLI/SSH command execution must return a completed outcome");
    std::process::exit(code);
}

#[cfg(test)]
async fn exec_final_container_command(
    runtime: &Runtime,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
) -> anyhow::Result<ExecutionOutcome> {
    exec_final_container_command_with_options(
        runtime,
        command,
        cwd,
        env,
        OperationExecutionOptions::STREAM,
    )
    .await
}

#[cfg(test)]
async fn exec_final_container_command_with_options(
    runtime: &Runtime,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    let exec_spec = final_container_exec_spec(runtime, command, cwd, env, options.mode);
    exec_container_command_with_options(runtime, &exec_spec, options).await
}

fn final_container_exec_spec(
    runtime: &Runtime,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    mode: OperationMode,
) -> ContainerExecSpec {
    ContainerExecSpec {
        stdin_tty: mode == OperationMode::Stream,
        stdout_tty: std::io::stdout().is_terminal(),
        user: runtime.exec_identity(),
        cwd,
        env,
        container_name: runtime.identity.container_name.clone(),
        command,
    }
}

async fn exec_container_command_with_options(
    runtime: &Runtime,
    exec_spec: &ContainerExecSpec,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    exec_container_command_with_cancel(runtime, exec_spec, options, None).await
}

async fn exec_container_command_with_cancel(
    runtime: &Runtime,
    exec_spec: &ContainerExecSpec,
    options: OperationExecutionOptions,
    cancel: Option<CancellationToken>,
) -> anyhow::Result<ExecutionOutcome> {
    match options.mode {
        OperationMode::Stream => {
            if let Some(cancel) = cancel {
                return match runtime
                    .container_runtime
                    .exec_cancelable(exec_spec, cancel)
                    .await?
                {
                    ContainerExecStatusResult::Completed(exit_code) => {
                        Ok(ExecutionOutcome::new(exit_code))
                    }
                    ContainerExecStatusResult::Canceled => Ok(ExecutionOutcome::canceled(None)),
                };
            }
            Ok(ExecutionOutcome::new(
                runtime.container_runtime.exec(exec_spec).await?,
            ))
        }
        OperationMode::Wait => {
            let output = if let Some(cancel) = cancel {
                match runtime
                    .container_runtime
                    .exec_capture_cancelable(exec_spec, cancel)
                    .await?
                {
                    ContainerExecCaptureResult::Completed(output) => output,
                    ContainerExecCaptureResult::Canceled => {
                        return Ok(ExecutionOutcome::canceled(None));
                    }
                }
            } else {
                runtime.container_runtime.exec_capture(exec_spec).await?
            };
            Ok(ExecutionOutcome::captured_streams(
                output.exit_code,
                options
                    .output
                    .stdout
                    .then(|| CapturedStream::new(output.stdout, output.stdout_truncated)),
                options
                    .output
                    .stderr
                    .then(|| CapturedStream::new(output.stderr, output.stderr_truncated)),
            ))
        }
        OperationMode::Detach => Ok(ExecutionOutcome::new(
            runtime.container_runtime.exec_discard(exec_spec).await?,
        )),
    }
}

async fn stop(
    config_path: Option<PathBuf>,
    args: StopArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_stop_args(args);
    let result = execute_gateway_operation_with_context(config_path, operation, context).await?;
    let GatewayOperationResult::Stop(result) = result else {
        unreachable!("stop operation returned a different result");
    };
    render_stop_result(&result);
    Ok(())
}

async fn remove(
    config_path: Option<PathBuf>,
    args: RemoveArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_remove_args(args);
    let result = execute_gateway_operation_with_context(config_path, operation, context).await?;
    let GatewayOperationResult::Remove(result) = result else {
        unreachable!("remove operation returned a different result");
    };
    render_remove_result(&result);
    Ok(())
}

async fn set_default(config_path: Option<PathBuf>, args: SetDefaultArgs) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_set_default_args(args)?;
    let result = execute_gateway_operation(config_path, operation).await?;
    let GatewayOperationResult::DefaultSelection(selection) = result else {
        unreachable!("default-selection operation returned a different result");
    };
    render_default_selection(&selection);
    Ok(())
}

async fn show_default(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let result = execute_gateway_operation(config_path, GatewayOperation::ShowDefault).await?;
    let GatewayOperationResult::DefaultSelection(selection) = result else {
        unreachable!("show-default operation returned a different result");
    };
    render_default_selection(&selection);
    Ok(())
}

async fn client_config(config_path: Option<PathBuf>, args: ClientConfigArgs) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_client_config_args(args);
    let result = execute_gateway_operation(config_path, operation).await?;
    let GatewayOperationResult::ClientConfig {
        rendered,
        written_path,
    } = result
    else {
        unreachable!("client-config operation returned a different result");
    };
    let _written_path = written_path;
    println!("{rendered}");
    Ok(())
}

async fn status(
    config_path: Option<PathBuf>,
    status: StatusArg,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    if status.all {
        if status.target.is_some() {
            anyhow::bail!("--all cannot be combined with a target");
        }
        if status.session_id.is_some() {
            anyhow::bail!("--all cannot be combined with --session-id");
        }
        let json = status.json;
        let result = execute_gateway_operation_with_context(
            config_path,
            GatewayOperation::StatusAll,
            context,
        )
        .await?;
        let GatewayOperationResult::StatusAll(entries) = result else {
            unreachable!("status-all operation returned a different result");
        };
        return render_status_all(entries, json);
    }
    let json = status.json;
    let operation = GatewayOperation::from_status_args(status);
    let result = execute_gateway_operation_with_context(config_path, operation, context).await?;
    let GatewayOperationResult::Status(result) = result else {
        unreachable!("status operation returned a different result");
    };
    render_status_result(result, json)
}

async fn targets(config_path: Option<PathBuf>, args: TargetsArgs) -> anyhow::Result<()> {
    let json = args.json;
    let operation = GatewayOperation::from_targets_args(args);
    let result = execute_gateway_operation(config_path, operation).await?;
    let GatewayOperationResult::Targets(entries) = result else {
        unreachable!("targets operation returned a different result");
    };
    render_targets(entries, json)
}

async fn launches(
    config_path: Option<PathBuf>,
    args: LaunchesArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let json = args.json;
    let operation = GatewayOperation::from_launches_args(args);
    let result = execute_gateway_operation_with_context(config_path, operation, context).await?;
    let GatewayOperationResult::Launches(entries) = result else {
        unreachable!("launches operation returned a different result");
    };
    render_launches(entries, json)
}

async fn launch(
    config_path: Option<PathBuf>,
    command: LaunchCommand,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    match command {
        LaunchCommand::Show(args) => launch_show(config_path, args, context).await,
        LaunchCommand::Run(raw) => {
            let parsed = parse_launch_run_args(raw)?;
            launch_execute(
                config_path,
                &parsed.name,
                parsed.session_id,
                parsed.vars,
                parsed.args,
                context,
            )
            .await
        }
    }
}

fn parse_launch_run_args(raw: Vec<std::ffi::OsString>) -> anyhow::Result<LaunchRunArgs> {
    let mut args = raw.into_iter();
    parse_launch_run_args_from(|role| {
        let Some(arg) = args.next() else {
            return Ok(None);
        };
        arg.into_string()
            .map(Some)
            .map_err(|_| anyhow::anyhow!(launch_arg_utf8_error(role)))
    })
}

fn launch_arg_utf8_error(role: LaunchRunArgRole) -> &'static str {
    match role {
        LaunchRunArgRole::Name => "launch name must be valid UTF-8",
        LaunchRunArgRole::Argument => "launch arguments must be valid UTF-8",
        LaunchRunArgRole::SessionId => "session id must be valid UTF-8",
        LaunchRunArgRole::Variable => "launch variable must be valid UTF-8",
        LaunchRunArgRole::Passthrough => "launch passthrough argument must be valid UTF-8",
    }
}

async fn launch_show(
    config_path: Option<PathBuf>,
    args: crate::cli::LaunchShowArgs,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let json = args.json;
    let operation = GatewayOperation::from_launch_show_args(args);
    let result = execute_gateway_operation_with_context(config_path, operation, context).await?;
    let GatewayOperationResult::LaunchShow(detail) = result else {
        unreachable!("launch-show operation returned a different result");
    };
    render_launch_detail(detail, json)
}

async fn launch_execute(
    config_path: Option<PathBuf>,
    name: &str,
    session_id: Option<String>,
    supplied: Vec<String>,
    args: Vec<String>,
    context: RuntimeContext,
) -> anyhow::Result<()> {
    let supplied = SuppliedLaunchVars::from_cli_pairs(supplied)?;
    let args = LaunchPassthroughArgs::from_strings(args)?;
    let result = execute_gateway_operation_with_context(
        config_path,
        GatewayOperation::launch_run(name.to_string(), session_id, supplied, args),
        context,
    )
    .await?;
    let GatewayOperationResult::Launch(outcome) = result else {
        unreachable!("launch operation returned a different result");
    };
    exit_with_execution_outcome(outcome)
}

async fn launch_execute_with_config(
    cfg: GatewayConfig,
    name: &str,
    session_id: Option<String>,
    supplied: SuppliedLaunchVars,
    args: LaunchPassthroughArgs,
    options: OperationExecutionOptions,
    context: RuntimeContext,
) -> OperationResult<ExecutionOutcome> {
    let launch = lookup_launch(&cfg, name)?;
    let resolved_vars = resolve_launch_vars(name, &launch, &supplied)?;
    validate_launch_passthrough_args(name, &launch, &args)?;
    let target = launch.target.clone();
    let runtime = Runtime::from_config(
        cfg,
        Some(&target),
        session_id,
        true,
        Some(name.to_string()),
        context,
    )
    .await?;
    OperationRunner::launch(runtime, options, launch, resolved_vars, args)
        .run()
        .await
        .map_err(OperationError::operation_failed)
}

#[allow(clippy::too_many_arguments)]
async fn launch_execute_with_config_cancelable(
    cfg: GatewayConfig,
    name: &str,
    session_id: Option<String>,
    supplied: SuppliedLaunchVars,
    args: LaunchPassthroughArgs,
    options: OperationExecutionOptions,
    cancel: CancellationToken,
    context: RuntimeContext,
) -> OperationResult<ExecutionOutcome> {
    let launch = lookup_launch(&cfg, name)?;
    let resolved_vars = resolve_launch_vars(name, &launch, &supplied)?;
    validate_launch_passthrough_args(name, &launch, &args)?;
    let target = launch.target.clone();
    let runtime = Runtime::from_config(
        cfg,
        Some(&target),
        session_id,
        true,
        Some(name.to_string()),
        context,
    )
    .await?;
    OperationRunner::launch(runtime, options, launch, resolved_vars, args)
        .run_cancelable(cancel)
        .await
        .map_err(OperationError::operation_failed)
}

async fn prepare_run_execution_with_config(
    cfg: GatewayConfig,
    target: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    command: Vec<String>,
    context: RuntimeContext,
) -> OperationResult<PreparedExecution> {
    let runtime =
        Runtime::from_config(cfg, target.as_deref(), session_id, true, None, context).await?;
    OperationRunner::run_command(runtime, OperationExecutionOptions::STREAM, cwd, command)
        .prepare()
        .await
        .map_err(OperationError::operation_failed)
}

async fn prepare_launch_execution_with_config(
    cfg: GatewayConfig,
    name: &str,
    session_id: Option<String>,
    supplied: SuppliedLaunchVars,
    args: LaunchPassthroughArgs,
    context: RuntimeContext,
) -> OperationResult<PreparedExecution> {
    let launch = lookup_launch(&cfg, name)?;
    let resolved_vars = resolve_launch_vars(name, &launch, &supplied)?;
    validate_launch_passthrough_args(name, &launch, &args)?;
    let target = launch.target.clone();
    let runtime = Runtime::from_config(
        cfg,
        Some(&target),
        session_id,
        true,
        Some(name.to_string()),
        context,
    )
    .await?;
    OperationRunner::launch(
        runtime,
        OperationExecutionOptions::STREAM,
        launch,
        resolved_vars,
        args,
    )
    .prepare()
    .await
    .map_err(OperationError::operation_failed)
}

fn validate_launch_passthrough_args(
    name: &str,
    launch: &LaunchConfig,
    args: &LaunchPassthroughArgs,
) -> OperationResult<()> {
    if args.is_empty() || launch.allow_args {
        return Ok(());
    }
    Err(OperationError::invalid_launch_args(format!(
        "launch {name:?} does not allow passthrough args"
    )))
}

fn launch_summaries(cfg: &GatewayConfig) -> anyhow::Result<Vec<LaunchSummary>> {
    Ok(cfg
        .effective_launches()?
        .iter()
        .map(|(name, launch)| LaunchSummary {
            name: name.clone(),
            target: launch.target.clone(),
            allow_args: launch.allow_args,
            description: launch.description.clone(),
            vars: launch_var_metadata(&launch.vars),
        })
        .collect())
}

fn launch_detail(
    cfg: &GatewayConfig,
    name: &str,
    launch: &LaunchConfig,
    context: &RuntimeContext,
) -> anyhow::Result<LaunchDetail> {
    let target = cfg.effective_target(&launch.target)?;
    Ok(LaunchDetail {
        name: name.to_string(),
        target: launch.target.clone(),
        target_mode: target_mode_name(target.mode).into(),
        allow_args: launch.allow_args,
        target_container: if target.mode == TargetMode::Fixed {
            Some(target_container_display(&target, context)?)
        } else {
            None
        },
        description: launch.description.clone(),
        vars: launch_var_metadata(&launch.vars),
        steps: launch.steps.iter().map(launch_step_detail).collect(),
        cwd: launch.cwd.clone(),
        env: launch.env.clone(),
        command: launch.command.clone(),
    })
}

fn launch_var_metadata(
    vars: &BTreeMap<String, LaunchVarConfig>,
) -> BTreeMap<String, LaunchVarMetadata> {
    vars.iter()
        .map(|(name, var)| {
            (
                name.clone(),
                LaunchVarMetadata {
                    var_type: launch_var_type_name(var.var_type),
                    required: var.required,
                    default: var.default.clone(),
                    values: var.values.clone(),
                    description: var.description.clone(),
                },
            )
        })
        .collect()
}

fn launch_step_detail(step: &LaunchStep) -> LaunchStepDetail {
    LaunchStepDetail {
        name: step.name.clone(),
        phase: "post_ready".into(),
        location: match step.location {
            LaunchStepLocation::Host => "host",
            LaunchStepLocation::Container => "container",
        }
        .into(),
        required: step.required,
        timeout: step.timeout.clone(),
        cwd: step.cwd.clone(),
        env: step.env.clone(),
        command: step.command.clone(),
    }
}

fn launch_var_type_name(var_type: LaunchVarType) -> &'static str {
    match var_type {
        LaunchVarType::String => "string",
        LaunchVarType::Enum => "enum",
        LaunchVarType::Boolean => "boolean",
        LaunchVarType::Number => "number",
    }
}

fn resolve_launch_vars(
    launch_name: &str,
    launch: &LaunchConfig,
    supplied: &SuppliedLaunchVars,
) -> OperationResult<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for key in supplied.keys() {
        if !launch.vars.contains_key(key) {
            return Err(OperationError::invalid_launch_variable(format!(
                "unknown launch variable {key:?}"
            )));
        }
    }
    for (name, var) in &launch.vars {
        let value = if let Some(value) = supplied.get(name) {
            value.coerce_for_config(name, var)?
        } else if let Some(default) = &var.default {
            CanonicalLaunchVarValue::from_config_default(default).coerce_for_config(name, var)?
        } else if var.required {
            return Err(OperationError::invalid_launch_variable(format!(
                "missing required launch variable {name:?}"
            )));
        } else {
            continue;
        };
        resolved.insert(name.clone(), value.rendered());
    }
    tracing::debug!(
        launch = launch_name,
        vars = resolved.len(),
        "resolved launch variables"
    );
    Ok(resolved)
}

async fn run_launch_steps(
    runtime: &Runtime,
    launch: &LaunchConfig,
    vars: &Vars,
    launch_env: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let session_env = runtime.session_env()?;
    for step in &launch.steps {
        let result = match step.location {
            LaunchStepLocation::Host => run_host_launch_step(step, vars, runtime).await,
            LaunchStepLocation::Container => {
                run_container_launch_step(step, launch_env, &session_env, vars, runtime).await
            }
        };
        if let Err(err) = result {
            if step.required {
                return Err(err).with_context(|| format!("launch step {:?}", step.name));
            }
            tracing::warn!(step = step.name, error = %err, "optional launch step failed");
        }
    }
    Ok(())
}

async fn run_host_launch_step(
    step: &LaunchStep,
    vars: &Vars,
    runtime: &Runtime,
) -> anyhow::Result<()> {
    let command = template::render_argv(&step.command, vars)?;
    let cwd = step.cwd.as_deref();
    let cwd = render_launch_cwd(cwd, vars, runtime.identity.user.home.as_path())?;
    let env = render_template_map(&step.env, vars)?;
    let timeout = host_hook_timeout(step.timeout.as_deref())?;
    run_argv_with_options(&command, timeout, cwd.as_deref(), &env).await
}

async fn run_container_launch_step(
    step: &LaunchStep,
    launch_env: &BTreeMap<String, String>,
    session_env: &BTreeMap<String, String>,
    vars: &Vars,
    runtime: &Runtime,
) -> anyhow::Result<()> {
    let env = launch_container_step_env(session_env, launch_env, &step.env, vars)?;
    let cwd = render_launch_cwd(
        step.cwd.as_deref(),
        vars,
        runtime.identity.container_home.as_path(),
    )?;
    let exec_spec = ContainerExecSpec {
        stdin_tty: false,
        stdout_tty: false,
        user: runtime.exec_identity(),
        cwd,
        env,
        container_name: runtime.identity.container_name.clone(),
        command: template::render_argv(&step.command, vars)?,
    };
    let timeout = host_hook_timeout(step.timeout.as_deref())?;
    let code = runtime
        .container_runtime
        .exec_with_timeout(&exec_spec, Some(timeout))
        .await?;
    if code != 0 {
        anyhow::bail!("container launch step exited with status {code}");
    }
    Ok(())
}

fn launch_container_step_env(
    session_env: &BTreeMap<String, String>,
    launch_env: &BTreeMap<String, String>,
    step_env: &BTreeMap<String, String>,
    vars: &Vars,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = session_env.clone();
    env.extend(launch_env.clone());
    env.extend(render_template_map(step_env, vars)?);
    Ok(env)
}

fn launch_final_env(
    session_env: &BTreeMap<String, String>,
    launch_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = session_env.clone();
    env.extend(launch_env.clone());
    env
}

fn render_template_map(
    values: &BTreeMap<String, String>,
    vars: &Vars,
) -> anyhow::Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|(key, value)| Ok((key.clone(), template::render(value, vars)?)))
        .collect()
}

fn ensure_launch_templates_supported(
    runtime: &Runtime,
    launch: &LaunchConfig,
    container_pid: Option<&str>,
) -> anyhow::Result<()> {
    runtime.ensure_runtime_template_values_supported(
        launch
            .command
            .iter()
            .filter(|arg| arg.as_str() != "{args}")
            .map(String::as_str),
        container_pid,
    )?;
    runtime.ensure_runtime_template_values_supported(
        launch.env.values().map(String::as_str),
        container_pid,
    )?;
    if let Some(cwd) = launch.cwd.as_deref() {
        runtime.ensure_runtime_template_values_supported([cwd], container_pid)?;
    }
    for step in &launch.steps {
        runtime.ensure_runtime_template_values_supported(
            step.command.iter().map(String::as_str),
            container_pid,
        )?;
        runtime.ensure_runtime_template_values_supported(
            step.env.values().map(String::as_str),
            container_pid,
        )?;
        if let Some(cwd) = step.cwd.as_deref() {
            runtime.ensure_runtime_template_values_supported([cwd], container_pid)?;
        }
    }
    Ok(())
}

fn render_launch_cwd(
    cwd: Option<&str>,
    vars: &Vars,
    home_base: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    cwd.map(|cwd| template::render(cwd, vars))
        .transpose()
        .map(|cwd| cwd.map(|cwd| paths::expand_home(home_base, &cwd)))
}

fn launch_template_vars(
    runtime: &Runtime,
    resolved_vars: &BTreeMap<String, String>,
    container_pid: Option<&str>,
) -> Vars {
    let mut vars = runtime.vars(container_pid);
    for (key, value) in resolved_vars {
        vars.insert(format!("var.{key}"), value.clone());
    }
    vars
}

fn target_entries(
    cfg: &GatewayConfig,
    context: &RuntimeContext,
) -> anyhow::Result<Vec<TargetEntry>> {
    let user = UserContext::current()?;
    let default_selection = client::read_default_selection(&user)
        .transpose()?
        .unwrap_or_else(|| cfg.default_target.clone());
    let default_target = client::resolve_target_selection(cfg, Some(&default_selection))
        .with_context(|| format!("validate default selection {default_selection:?}"))?;
    let effective_targets = cfg.effective_targets()?;
    let entries = effective_targets
        .iter()
        .map(|(name, target)| {
            Ok(TargetEntry {
                target: name.clone(),
                image: target.image.clone(),
                access: target.access.method.as_str().into(),
                mode: target_mode_name(target.mode).into(),
                container: target_container_display(target, context)?,
                default: name == &default_target,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(entries)
}

fn target_container_display(
    target: &TargetConfig,
    context: &RuntimeContext,
) -> anyhow::Result<String> {
    Ok(match target.mode {
        TargetMode::Fixed => {
            if context.is_empty()
                && target.name.as_deref().is_some_and(|name| {
                    template::referenced_keys(name)
                        .is_ok_and(|refs| refs.iter().any(|key| key.starts_with("context.")))
                })
            {
                target.name.clone().unwrap_or_else(|| "{image_slug}".into())
            } else {
                target.container_name_with_context(None, context)?
            }
        }
        TargetMode::Ephemeral => target
            .ephemeral_name
            .as_deref()
            .unwrap_or("{image_slug}-{session_id}")
            .to_string(),
    })
}

fn target_mode_name(mode: TargetMode) -> &'static str {
    match mode {
        TargetMode::Fixed => "fixed",
        TargetMode::Ephemeral => "ephemeral",
    }
}

#[derive(Debug)]
struct Runtime {
    cfg: GatewayConfig,
    context: RuntimeContext,
    target: TargetConfig,
    identity: RuntimeIdentity,
    paths: RuntimePaths,
    container_runtime: ContainerRuntime,
}

#[derive(Debug)]
struct RuntimeIdentity {
    target_name: String,
    launch_name: Option<String>,
    session_id: Option<String>,
    user: UserContext,
    bootstrap_user: String,
    session_uid: u32,
    session_gid: u32,
    session_shell: String,
    container_user: String,
    container_home: PathBuf,
    container_name: String,
}

#[derive(Debug)]
struct RuntimePaths {
    workspace: PathBuf,
    workspace_state_dir: PathBuf,
    workspace_container_path: PathBuf,
    workspace_state_dir_in_container: PathBuf,
    container_state_dir: PathBuf,
    container_state_dir_in_container: PathBuf,
    control_sockets: ControlSocketPaths,
}

#[derive(Debug, Clone)]
struct ControlSocketPaths {
    host_dir: PathBuf,
    container_dir: PathBuf,
    host_agent_socket: PathBuf,
    host_ssh_socket: PathBuf,
    container_agent_socket: PathBuf,
    container_ssh_socket: PathBuf,
    default_host_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    Success,
    Canceled,
    Failure,
}

impl SessionOutcome {
    fn is_clean(self) -> bool {
        matches!(self, Self::Success | Self::Canceled)
    }

    fn from_result<T>(result: &anyhow::Result<T>) -> Self {
        if result.is_ok() {
            Self::Success
        } else {
            Self::Failure
        }
    }

    #[cfg(test)]
    fn from_exit_code_result(result: &anyhow::Result<i32>) -> Self {
        match result {
            Ok(0) => Self::Success,
            Ok(_) | Err(_) => Self::Failure,
        }
    }

    fn from_execution_result(result: &anyhow::Result<ExecutionOutcome>) -> Self {
        match result {
            Ok(outcome) if outcome.is_canceled() => Self::Canceled,
            Ok(outcome) => outcome
                .exit_code()
                .map(|code| {
                    if code == 0 {
                        Self::Success
                    } else {
                        Self::Failure
                    }
                })
                .unwrap_or(Self::Success),
            Err(_) => Self::Failure,
        }
    }
}

impl RuntimeIdentity {
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        target_name: String,
        launch_name: Option<String>,
        session_id: Option<String>,
        user: UserContext,
        target: &TargetConfig,
        container_name: String,
        container_runtime: &ContainerRuntime,
        context: &RuntimeContext,
    ) -> anyhow::Result<Self> {
        let mut identity_vars = Vars::new();
        identity_vars.insert("user".into(), user.user.clone());
        identity_vars.insert("uid".into(), user.uid.to_string());
        identity_vars.insert("gid".into(), user.gid.to_string());
        identity_vars.insert("home".into(), user.home.display().to_string());
        context.insert_template_vars(&mut identity_vars);

        let default_container_user = target.container_user.clone().unwrap_or_else(|| {
            if container_runtime.kind() == ContainerRuntimeType::Podman {
                user.user.clone()
            } else {
                "root".into()
            }
        });
        let default_container_home = target
            .container_home
            .as_ref()
            .map(|home| {
                template::render(&home.display().to_string(), &identity_vars).map(PathBuf::from)
            })
            .transpose()?
            .unwrap_or_else(|| {
                if container_runtime.kind() == ContainerRuntimeType::Podman {
                    user.home.clone()
                } else {
                    PathBuf::from("/root")
                }
            });
        let identity_cfg = target.identity.as_ref();
        let bootstrap_user = identity_cfg
            .and_then(|cfg| cfg.bootstrap_user.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .unwrap_or_else(|| "root".into());
        let container_user = identity_cfg
            .and_then(|cfg| cfg.session_user.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .unwrap_or(default_container_user);
        validate_name("target identity session_user", &container_user)?;
        let session_uid = identity_cfg
            .and_then(|cfg| cfg.session_uid.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .map(|value| value.parse::<u32>())
            .transpose()
            .context("parse target identity session_uid")?
            .unwrap_or(user.uid);
        let session_gid = identity_cfg
            .and_then(|cfg| cfg.session_gid.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .map(|value| value.parse::<u32>())
            .transpose()
            .context("parse target identity session_gid")?
            .unwrap_or(user.gid);
        let container_home = identity_cfg
            .and_then(|cfg| cfg.session_home.as_deref())
            .map(|value| template::render(value, &identity_vars).map(PathBuf::from))
            .transpose()?
            .unwrap_or(default_container_home);
        if !container_home.is_absolute() {
            anyhow::bail!("target identity session_home must render to an absolute path");
        }
        validate_passwd_scalar(
            "target identity session_home",
            &container_home.display().to_string(),
        )?;
        let session_shell = identity_cfg
            .and_then(|cfg| cfg.session_shell.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .unwrap_or_else(|| "/bin/bash".into());
        validate_passwd_scalar("target identity session_shell", &session_shell)?;
        Ok(Self {
            target_name,
            launch_name,
            session_id,
            user,
            bootstrap_user,
            session_uid,
            session_gid,
            session_shell,
            container_user,
            container_home,
            container_name,
        })
    }
}

impl RuntimePaths {
    fn resolve(
        target: &TargetConfig,
        identity: &RuntimeIdentity,
        workspace: PathBuf,
        context: &RuntimeContext,
    ) -> anyhow::Result<Self> {
        let session_id = identity.session_id.as_deref();
        let session_id = || session_id.expect("ephemeral target has a session id");
        let (state_kind, state_id) = match target.mode {
            TargetMode::Fixed => ("containers", identity.container_name.as_str()),
            TargetMode::Ephemeral => ("sessions", session_id()),
        };
        let runtime_id = match target.mode {
            TargetMode::Fixed => identity.target_name.as_str(),
            TargetMode::Ephemeral => session_id(),
        };
        let mut vars = Vars::new();
        vars.insert("user".into(), identity.user.user.clone());
        vars.insert("uid".into(), identity.session_uid.to_string());
        vars.insert("gid".into(), identity.session_gid.to_string());
        vars.insert("home".into(), identity.user.home.display().to_string());
        vars.insert("container_user".into(), identity.container_user.clone());
        vars.insert(
            "container_home".into(),
            identity.container_home.display().to_string(),
        );
        vars.insert("workspace".into(), workspace.display().to_string());
        vars.insert("target".into(), identity.target_name.clone());
        vars.insert("image".into(), target.image.clone());
        vars.insert("image_slug".into(), template::image_slug(&target.image));
        vars.insert("container_name".into(), identity.container_name.clone());
        if let Some(session_id) = identity.session_id.as_deref() {
            vars.insert("session_id".into(), session_id.to_string());
        }
        context.insert_template_vars(&mut vars);
        let workspace_container_path = target
            .workspace
            .container_path
            .as_deref()
            .map(|path| template::render(path, &vars).map(PathBuf::from))
            .transpose()?
            .unwrap_or_else(|| identity.container_home.clone());
        if !workspace_container_path.is_absolute() {
            anyhow::bail!("target.workspace.container_path must render to an absolute path");
        }
        if workspace_container_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            anyhow::bail!("target.workspace.container_path must not render with '..' components");
        }
        let workspace_container_path: PathBuf = workspace_container_path.components().collect();
        let state_dir = template::render(&target.workspace.state_dir, &vars)?;
        let workspace_state_dir =
            control_sockets::resolve_workspace_state_path(&workspace, &state_dir)?;
        let container_state_dir = workspace_state_dir.join(state_kind).join(state_id);
        let workspace_state_dir_in_container =
            control_sockets::resolve_workspace_state_path(&workspace_container_path, &state_dir)?;
        let container_state_dir_in_container = workspace_state_dir_in_container
            .join(state_kind)
            .join(state_id);
        vars.insert("state".into(), workspace_state_dir.display().to_string());
        vars.insert(
            "state_dir".into(),
            identity.user.state_dir().display().to_string(),
        );
        let control_sockets = render_control_socket_paths(
            &target.control_sockets,
            target,
            &identity.target_name,
            &identity.container_name,
            identity.session_id.as_deref(),
            runtime_id,
            &identity.user,
            context,
        )?;
        Ok(Self {
            workspace,
            workspace_state_dir,
            workspace_container_path,
            workspace_state_dir_in_container,
            container_state_dir,
            container_state_dir_in_container,
            control_sockets,
        })
    }
}

impl Runtime {
    async fn load(
        config_path: Option<PathBuf>,
        target: Option<&str>,
        session_id: Option<String>,
        generate_session_id: bool,
    ) -> OperationResult<Runtime> {
        Self::load_with_context(
            config_path,
            target,
            session_id,
            generate_session_id,
            RuntimeContext::empty(),
        )
        .await
    }

    async fn load_with_context(
        config_path: Option<PathBuf>,
        target: Option<&str>,
        session_id: Option<String>,
        generate_session_id: bool,
        context: RuntimeContext,
    ) -> OperationResult<Runtime> {
        let cfg = load_config(config_path)?;
        Self::from_config(cfg, target, session_id, generate_session_id, None, context).await
    }

    async fn from_config(
        cfg: GatewayConfig,
        target: Option<&str>,
        session_id: Option<String>,
        generate_session_id: bool,
        launch_name: Option<String>,
        context: RuntimeContext,
    ) -> OperationResult<Runtime> {
        validate_runtime_context(&cfg.context_vars, &context)
            .map_err(|err| OperationError::invalid_request(err.to_string()))?;
        let user = UserContext::current()?;
        let target_name = match target {
            Some(target) => resolve_target_selection(&cfg, Some(target))?,
            None => match read_default_selection(&user).transpose()? {
                Some(selection) => resolve_target_selection(&cfg, Some(&selection))?,
                None => cfg.default_target.clone(),
            },
        };
        let target_cfg = cfg.effective_target(&target_name)?;
        let session_id = match target_cfg.mode {
            TargetMode::Fixed => {
                if session_id.is_some() {
                    return Err(OperationError::invalid_session(
                        "--session-id is only valid for ephemeral targets",
                    ));
                }
                None
            }
            TargetMode::Ephemeral => match session_id {
                Some(value) => {
                    validate_session_id(&value)
                        .map_err(|err| OperationError::invalid_session(err.to_string()))?;
                    Some(value)
                }
                None if generate_session_id => Some(generate_session_id_value()?),
                None => {
                    return Err(OperationError::invalid_session(format!(
                        "ephemeral target {target_name:?} requires --session-id"
                    )));
                }
            },
        };
        let container_name =
            target_cfg.container_name_with_context(session_id.as_deref(), &context)?;
        let workspace = resolve_target_workspace(
            &target_cfg,
            &target_name,
            &user,
            session_id.as_deref(),
            &context,
        )?;
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home)?;
        let identity = RuntimeIdentity::resolve(
            target_name,
            launch_name,
            session_id,
            user,
            &target_cfg,
            container_name,
            &container_runtime,
            &context,
        )?;
        let paths = RuntimePaths::resolve(&target_cfg, &identity, workspace, &context)?;
        let runtime = Runtime {
            cfg,
            context,
            target: target_cfg,
            identity,
            paths,
            container_runtime,
        };
        runtime.validate_workspace_cleanup_path().await?;
        runtime.validate_unix_socket_paths()?;
        Ok(runtime)
    }

    async fn ensure_ready(&self) -> anyhow::Result<ReadyStatus> {
        let _lock = self.acquire_lifecycle_lock().await?;
        self.ensure_ready_locked().await
    }

    async fn begin_ready_session(
        &self,
        kind: &str,
        launch_marker: bool,
    ) -> anyhow::Result<(session::SessionGuard, anyhow::Result<ReadyStatus>)> {
        let _lock = self.acquire_lifecycle_lock().await?;
        let session = self
            .create_session_marker_async(kind, launch_marker)
            .await?;
        let ready = self.ensure_ready_locked().await;
        Ok((session, ready))
    }

    async fn ensure_ready_locked(&self) -> anyhow::Result<ReadyStatus> {
        let mut failed_start_cleanup = FailedStartCleanup::default();
        let result = async {
            self.prepare_container_state_dir()?;
            if self.requires_control_socket_dir() {
                self.prepare_control_socket_dir()?;
            }
            if self.uses_ssh_access() {
                self.write_sshd_session_env_config()?;
                self.write_ssh_command_filter_policy()?;
            }
            if self.ssh_endpoint_configured() {
                self.ensure_inner_keypair(false).await?;
            }
            if self.agent_enabled() {
                self.ensure_control_token()?;
                self.write_container_agent_config()?;
                if self.target.container_bootstrap.enabled {
                    self.write_container_bootstrap_config()?;
                }
            }
            let inspect = self
                .container_runtime
                .inspect(&self.identity.container_name)
                .await?;
            let plan = readiness_plan(inspect);
            let inspect = self
                .ensure_container_for_readiness_plan(plan, &mut failed_start_cleanup)
                .await?;
            self.validate_labels(&inspect)?;
            self.sweep_stale_cancel_markers().await;
            let container_pid = inspect.state.pid.map(|pid| pid.to_string());
            self.run_lifecycle_phase(LifecyclePhase::PostStartHost, container_pid.as_deref())
                .await?;
            if self.requires_agent_control() {
                self.wait_agent_ready().await?;
            }
            self.run_host_steps(container_pid.as_deref()).await?;
            if self.requires_agent_control() {
                self.validate_agent_socket().await?;
            }
            if self.ssh_endpoint_configured() {
                match self.ssh_backend() {
                    LocalSshBackend::Socket => self.validate_ssh_socket().await?,
                    LocalSshBackend::PublishedPort => self.wait_published_ssh_ready().await?,
                }
            }
            let status = self.status().await?;
            if self.requires_agent_control() && !status.agent_ready {
                anyhow::bail!("container agent is not ready after host setup");
            }
            Ok((inspect, status))
        }
        .await;

        let (inspect, status) = match result {
            Ok(value) => value,
            Err(err) => {
                failed_start_cleanup.run_if_needed(self).await;
                return Err(err);
            }
        };
        Ok(ReadyStatus {
            target: self.identity.target_name.clone(),
            session_id: self.identity.session_id.clone(),
            mode: format!("{:?}", self.target.mode).to_lowercase(),
            user: self.identity.user.user.clone(),
            image: self.target.image.clone(),
            container: self.identity.container_name.clone(),
            access: self.access_name(),
            context: self.context.as_map().clone(),
            container_pid: inspect.state.pid,
            ssh_socket: self.ssh_socket_endpoint(),
            ssh_tcp: self.published_ssh_endpoint().await?,
            status: status.status,
            local_ssh: status.local_ssh,
            client_config: None,
        })
    }

    fn prepare_container_state_dir(&self) -> anyhow::Result<()> {
        paths::ensure_private_dir(&self.paths.container_state_dir)?;
        if self.agent_enabled() {
            paths::ensure_private_dir(&self.paths.container_state_dir.join("logs"))?;
        }
        Ok(())
    }

    async fn status(&self) -> anyhow::Result<GatewayStatus> {
        let inspect = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?;
        if let Some(inspect) = &inspect {
            self.validate_labels(inspect)?;
        }
        let agent = if self.agent_control_enabled() {
            self.agent_status().await.ok()
        } else {
            None
        };
        let sessions = self.active_session_markers_async().await?;
        let launch = status_launch(self.identity.session_id.as_deref(), &sessions);
        let agent_ready = agent.as_ref().is_some_and(|status| status.ready);
        let direct_endpoint = if self.direct_published_ssh_enabled() {
            self.direct_status_ssh_endpoint(inspect.as_ref()).await?
        } else {
            None
        };
        let local_ssh = direct_endpoint.clone().map(|endpoint| LocalSshReady {
            host: endpoint.host,
            port: endpoint.port,
        });
        let ssh_tcp = if self.direct_published_ssh_enabled() {
            inspect
                .as_ref()
                .is_some_and(|value| value.state.running)
                .then_some(direct_endpoint)
                .flatten()
        } else {
            self.published_ssh_endpoint().await?
        };
        Ok(GatewayStatus {
            target: self.identity.target_name.clone(),
            session_id: self.identity.session_id.clone(),
            launch,
            mode: format!("{:?}", self.target.mode).to_lowercase(),
            user: self.identity.user.user.clone(),
            image: self.target.image.clone(),
            access: self.access_name(),
            container: inspect
                .as_ref()
                .map(|_| self.identity.container_name.clone()),
            context: self.context.as_map().clone(),
            container_pid: inspect.as_ref().and_then(|value| value.state.pid),
            active_sessions: sessions.len(),
            sessions,
            agent_ready,
            ssh_socket: self.ssh_socket_endpoint(),
            ssh_tcp,
            local_ssh,
            status: gateway_status_name(
                inspect.as_ref().is_some_and(|value| value.state.running),
                self.requires_agent_control(),
                agent.is_some(),
                agent_ready,
            )
            .into(),
            agent: agent.map(Box::new),
        })
    }

    fn ssh_backend(&self) -> LocalSshBackend {
        self.target
            .local_ssh
            .as_ref()
            .map(|local_ssh| local_ssh.backend)
            .unwrap_or_default()
    }

    fn access_name(&self) -> String {
        self.target.access.method.as_str().into()
    }

    fn uses_ssh_access(&self) -> bool {
        self.target.access.method == TargetAccessMethod::Ssh
    }

    pub(super) fn ensure_ssh_operation_supported(&self, operation: &str) -> anyhow::Result<()> {
        if self.uses_ssh_access() {
            return Ok(());
        }
        anyhow::bail!(
            "gateway operation {operation:?} requires an SSH target, but target {:?} uses access.method = \"runtime_exec\"",
            self.identity.target_name
        )
    }

    fn requires_control_socket_dir(&self) -> bool {
        self.agent_control_enabled()
            || (self.ssh_endpoint_configured() && self.ssh_backend() == LocalSshBackend::Socket)
    }

    fn agent_enabled(&self) -> bool {
        self.target.container_agent.enabled
    }

    fn agent_control_enabled(&self) -> bool {
        self.agent_enabled()
            && self
                .target
                .container_agent
                .control_socket
                .as_ref()
                .is_none_or(ControlSocketConfig::is_enabled)
    }

    fn ssh_endpoint_configured(&self) -> bool {
        if !self.uses_ssh_access() {
            return false;
        }
        match self.ssh_backend() {
            LocalSshBackend::Socket => self
                .target
                .container_agent
                .ssh_bridge
                .as_ref()
                .is_some_and(|bridge| self.agent_enabled() && bridge.enabled),
            LocalSshBackend::PublishedPort => true,
        }
    }

    fn ssh_socket_endpoint(&self) -> Option<PathBuf> {
        (self.ssh_endpoint_configured() && self.ssh_backend() == LocalSshBackend::Socket)
            .then(|| self.ssh_socket())
    }

    fn ensure_ssh_endpoint_configured(&self) -> anyhow::Result<()> {
        if !self.uses_ssh_access() {
            anyhow::bail!(
                "target {:?} uses access.method = \"runtime_exec\" and does not expose an SSH endpoint; use run, launch, shell, status, stop, or remove instead",
                self.identity.target_name
            );
        }
        if self.ssh_endpoint_configured() {
            Ok(())
        } else {
            let backend = match self.ssh_backend() {
                LocalSshBackend::Socket => "socket",
                LocalSshBackend::PublishedPort => "published_port",
            };
            let bridge = match &self.target.container_agent.ssh_bridge {
                Some(bridge) if bridge.enabled => "enabled",
                Some(_) => "disabled",
                None => "not configured",
            };
            anyhow::bail!(
                "target {:?} does not configure an SSH endpoint (container_agent.enabled = {}, local_ssh.backend = {backend:?}, container_agent.ssh_bridge = {bridge}); set container_agent.ssh_bridge.enabled = true for socket backend or use local_ssh.backend = \"published_port\" with an image-managed SSH server",
                self.identity.target_name,
                self.agent_enabled()
            )
        }
    }

    fn requires_agent_control(&self) -> bool {
        self.agent_control_enabled()
            && self
                .target
                .local_ssh
                .as_ref()
                .map(|local_ssh| local_ssh.readiness == LocalSshReadiness::AgentControl)
                .unwrap_or(true)
    }

    async fn published_ssh_endpoint(&self) -> anyhow::Result<Option<TcpEndpoint>> {
        if !self.ssh_endpoint_configured() {
            return Ok(None);
        }
        if self.ssh_backend() != LocalSshBackend::PublishedPort {
            return Ok(None);
        }
        if self.container_runtime.kind() == ContainerRuntimeType::AppleContainer {
            return self.apple_published_ssh_endpoint().await;
        }
        Ok(self
            .container_runtime
            .published_port(&self.identity.container_name, 22)
            .await?
            .map(|endpoint| TcpEndpoint {
                host: endpoint.host,
                port: endpoint.port,
            }))
    }
}

fn load_config(config_path: Option<PathBuf>) -> anyhow::Result<GatewayConfig> {
    let path = paths::resolve_gateway_config(config_path)?.selected_path()?;
    GatewayConfig::load(&path)
}

struct OperationSessionGuard {
    session: Option<session::SessionGuard>,
    agent_session: Option<AgentSessionHold>,
}

#[cfg(test)]
mod tests;
