use crate::agent_control::{
    AgentStatus, ControlEnvelope, ControlFailure, ControlSuccess, SessionHoldParams,
    SessionHoldResult, ShutdownParams, ShutdownResult,
};
use crate::cli::{
    AddContainerKeyArgs, AddHostKeyArgs, AddKeyArgs, ClientBundleArgs, ClientConfigArgs,
    ConfigCommand, ConnectArgs, GatewayArgs, GatewayCommand, LaunchCommand, LaunchesArgs, RunArgs,
    SetDefaultArgs, StatusArg, StopArgs, TargetArg, TargetsArgs, UpArgs,
};
use crate::config::{
    AGENT_SCHEMA_VERSION, BootstrapIdentity, ContainerAgentFile, ContainerBootstrapFile,
    ContainerMountMode, ContainerRuntimeType, ControlSocketConfig, ControlSocketsConfig,
    GatewayConfig, IdleCleanupAction, IdleCleanupOwner, LaunchConfig, LaunchStep,
    LaunchStepLocation, LaunchVarConfig, LaunchVarType, LifecyclePhase, LifecycleStep,
    LocalSshBackend, LocalSshMode, LocalSshReadiness, LoggingConfig,
    RenderedContainerBootstrapStep, TargetConfig, TargetMode, WorkspaceCleanup, validate_name,
    validate_passwd_scalar,
};
use crate::fileutil::{AtomicWritePolicy, atomic_write_toml, write_private_file};
use crate::paths::{self, UserContext};
use crate::runtime::{
    self, ContainerExecSpec, ContainerInspect, ContainerMountSpec, ContainerRunSpec,
    ContainerRuntime,
};
use crate::ssh_dispatch::{self, Dispatch, GatewayAction};
use crate::ssh_filter::{
    SshCommandFilterPolicy, is_sftp_server_command, legacy_scp_mode_allows,
    legacy_scp_server_direction,
};
use crate::template::{self, Vars};
use anyhow::Context;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::{Duration, Instant, sleep};

pub const DEFAULT_GATEWAY_CONFIG: &str = include_str!("../aw-gateway.sample.toml");
const MAX_SSH_ORIGINAL_COMMAND_BYTES: usize = 64 * 1024;
const DEFAULT_HOST_HOOK_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_SESSION_SHELL_ENV: &str = "/usr/bin/bash";

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

mod client;
mod health;
mod http;
mod identity;
mod listener;
mod model;
mod ops;
mod render;
mod session;
mod status_view;
mod token;

use client::{read_default_selection, resolve_target_selection};
use health::{run_argv_with_options, run_argv_with_timeout, run_health_check};
use model::{
    GatewayStatus, LaunchDetail, LaunchStepDetail, LaunchSummary, LaunchVarMetadata, ReadyStatus,
    TargetEntry, TcpEndpoint, gateway_status_name,
};
use ops::{
    CanonicalLaunchVarValue, ExecutionOutcome, GatewayOperation, GatewayOperationResult,
    OperationError, OperationExecutionOptions, OperationMode, OperationResult, OutputSelection,
    SshGatewayOperation, SshRenderOptions, SuppliedLaunchVars, execute_gateway_operation,
    lookup_launch, operation_up_with_runtime,
};
use render::{
    render_default_selection, render_launch_detail, render_launches, render_remove_result,
    render_status_all, render_status_result, render_stop_result, render_targets, render_up_result,
};
use session::{generate_session_id_value, validate_session_id};
use status_view::{status_all_entries, status_launch};

#[cfg(test)]
use crate::config::HealthCheck;
#[cfg(test)]
use crate::runtime::ManagedContainer;
#[cfg(test)]
use client::{configured_default_display, normalize_image_selection};
#[cfg(test)]
use identity::{
    ensure_identity_token_file, is_plausible_public_key, validate_identity_token_content,
    validate_public_key_content,
};
#[cfg(test)]
use model::{AllStatusEntry, LocalListenerStatus, SessionMarker};
#[cfg(test)]
use ops::{RemoveResult, StopResult};
#[cfg(test)]
use render::{remove_result_text, stop_result_text};
#[cfg(test)]
use session::{
    local_listener_is_active, parse_process_start_time, process_start_time,
    session_marker_is_active,
};

pub async fn run(args: GatewayArgs) -> anyhow::Result<()> {
    match args.command {
        Some(GatewayCommand::Config(command)) => run_config(command, args.config).await,
        Some(GatewayCommand::Connect(connect_args)) => connect(args.config, connect_args).await,
        Some(GatewayCommand::Up(status)) => up(args.config, status).await,
        Some(GatewayCommand::Run(run_args)) => run_container_command(args.config, run_args).await,
        Some(GatewayCommand::Launch(launch_command)) => launch(args.config, launch_command).await,
        Some(GatewayCommand::Launches(launches_args)) => launches(args.config, launches_args).await,
        Some(GatewayCommand::Stop(stop_args)) => stop(args.config, stop_args).await,
        Some(GatewayCommand::Remove(target_arg)) => remove(args.config, target_arg).await,
        Some(GatewayCommand::Status(status_args)) => status(args.config, status_args).await,
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

async fn run_config(command: ConfigCommand, config: Option<PathBuf>) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Validate => {
            let path = paths::gateway_config_path(config);
            GatewayConfig::load(&path)?;
            println!("ok");
            Ok(())
        }
        ConfigCommand::Init(init) => {
            let path = init
                .path
                .unwrap_or_else(|| paths::gateway_config_path(config));
            if path.exists() && !init.force {
                anyhow::bail!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                );
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_GATEWAY_CONFIG)?;
            println!("{}", path.display());
            Ok(())
        }
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
        if let Some(direction) = legacy_scp_server_direction(command)
            && !legacy_scp_mode_allows(transfer.legacy_scp, direction)
        {
            anyhow::bail!("blocked by policy: legacy scp is not allowed");
        }
        if !transfer.sftp.allows() && is_sftp_server_command(command) {
            anyhow::bail!("blocked by policy: sftp is not allowed");
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
            client::client_bundle(
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

async fn connect(config_path: Option<PathBuf>, args: ConnectArgs) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, args.target.as_deref(), args.session_id, true).await?;
    runtime.ensure_ssh_endpoint_configured()?;
    let session = runtime.create_session_marker("connect")?;
    let proxy_result = async {
        let ready = runtime.ensure_ready().await?;
        listener::proxy_ready_to_stdio(&ready).await
    }
    .await;
    let outcome = SessionOutcome::from_result(&proxy_result);
    runtime
        .finish_post_session(session, proxy_result, outcome)
        .await
}

async fn up(config_path: Option<PathBuf>, status: UpArgs) -> anyhow::Result<()> {
    let runtime = Runtime::load(
        config_path,
        status.target.as_deref(),
        status.session_id,
        true,
    )
    .await?;
    if let Some(local_ssh) = &runtime.target.local_ssh
        && local_ssh.mode == LocalSshMode::Listen
    {
        runtime.ensure_ssh_endpoint_configured()?;
        let session = runtime.create_session_marker("local-listen")?;
        let up_result = async {
            let mut ready = runtime.ensure_ready().await?;
            let bound = listener::bind_local_ssh(&runtime).await?;
            ready.local_ssh = Some(bound.ready.clone());
            let config = runtime.render_client_config(None)?;
            ready.client_config = Some(runtime.write_inner_config(&config)?);
            println!("{}", serde_json::to_string_pretty(&ready)?);
            let target = ready.ssh_target();
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

async fn run_container_command(config_path: Option<PathBuf>, args: RunArgs) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_run_args(args)?;
    let result = execute_gateway_operation(config_path, operation).await?;
    let GatewayOperationResult::Run(outcome) = result else {
        unreachable!("run operation returned a different result");
    };
    exit_with_execution_outcome(outcome)
}

fn exit_with_execution_outcome(outcome: ExecutionOutcome) -> ! {
    let code = outcome
        .exit_code()
        .expect("CLI/SSH command execution must return a completed outcome");
    std::process::exit(code);
}

async fn run_container_command_with_runtime(
    runtime: Runtime,
    cwd: Option<String>,
    command: Vec<String>,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    OperationRunner::run_command(runtime, options, cwd, command)
        .run()
        .await
}

#[derive(Debug, Clone, Copy)]
enum OperationSessionSpec {
    RunCommand,
    Launch,
}

impl OperationSessionSpec {
    fn kind(self) -> &'static str {
        match self {
            Self::RunCommand => "run-command",
            Self::Launch => "launch",
        }
    }

    fn uses_launch_marker(self) -> bool {
        matches!(self, Self::Launch)
    }

    fn warn_fire_and_forget_failure(self, operation_id: &str, err: &anyhow::Error) {
        match self {
            Self::RunCommand => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %err,
                    "detached run operation failed"
                );
            }
            Self::Launch => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %err,
                    "detached launch operation failed"
                );
            }
        }
    }
}

struct OperationRunner {
    runtime: Runtime,
    options: OperationExecutionOptions,
    session_spec: OperationSessionSpec,
    body: OperationBody,
}

impl OperationRunner {
    fn run_command(
        runtime: Runtime,
        options: OperationExecutionOptions,
        cwd: Option<String>,
        command: Vec<String>,
    ) -> Self {
        Self {
            runtime,
            options,
            session_spec: OperationSessionSpec::RunCommand,
            body: OperationBody::Run { cwd, command },
        }
    }

    fn launch(
        runtime: Runtime,
        options: OperationExecutionOptions,
        launch: LaunchConfig,
        resolved_vars: BTreeMap<String, String>,
    ) -> Self {
        debug_assert!(
            runtime.identity.launch_name.is_some(),
            "launch operations require a launch name on the runtime"
        );
        Self {
            runtime,
            options,
            session_spec: OperationSessionSpec::Launch,
            body: OperationBody::Launch {
                launch,
                resolved_vars,
            },
        }
    }

    async fn run(self) -> anyhow::Result<ExecutionOutcome> {
        if self.options.mode == OperationMode::Detach {
            return self.spawn_detached().await;
        }
        let Self {
            runtime,
            options,
            session_spec,
            body,
        } = self;
        let session = runtime
            .begin_operation_session(session_spec.kind(), session_spec.uses_launch_marker())?;
        run_operation_session(runtime, session_spec, session, body, options).await
    }

    async fn spawn_detached(self) -> anyhow::Result<ExecutionOutcome> {
        // Detach is explicitly fire-and-forget. This id is emitted in the
        // response and logs so operators can correlate failures, not look up
        // operation status later.
        let operation_id = generate_session_id_value()?;
        let Self {
            runtime,
            session_spec,
            body,
            options: _,
        } = self;
        let session = runtime
            .begin_operation_session(session_spec.kind(), session_spec.uses_launch_marker())?;
        let background_id = operation_id.clone();
        tokio::spawn(async move {
            let result = run_operation_session(
                runtime,
                session_spec,
                session,
                body,
                detach_discard_options(),
            )
            .await;
            if let Err(err) = result {
                session_spec.warn_fire_and_forget_failure(&background_id, &err);
            }
        });
        Ok(ExecutionOutcome::detached(operation_id))
    }
}

async fn run_operation_session(
    runtime: Runtime,
    session_spec: OperationSessionSpec,
    mut session: OperationSessionGuard,
    body: OperationBody,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    let result = async {
        let ready = runtime.ensure_ready().await?;
        runtime
            .hold_operation_agent_session(&mut session, session_spec.kind())
            .await?;
        body.execute(&runtime, ready, options).await
    }
    .await;
    let outcome = SessionOutcome::from_execution_result(&result);
    runtime
        .finish_operation_session(session, result, outcome)
        .await
}

enum OperationBody {
    Run {
        cwd: Option<String>,
        command: Vec<String>,
    },
    Launch {
        launch: LaunchConfig,
        resolved_vars: BTreeMap<String, String>,
    },
}

impl OperationBody {
    async fn execute(
        self,
        runtime: &Runtime,
        ready: ReadyStatus,
        options: OperationExecutionOptions,
    ) -> anyhow::Result<ExecutionOutcome> {
        match self {
            Self::Run { cwd, command } => {
                let cwd = cwd
                    .as_deref()
                    .map(|cwd| paths::expand_home(&runtime.identity.container_home, cwd));
                exec_final_container_command_with_options(
                    runtime,
                    command,
                    cwd,
                    runtime.session_env()?,
                    options,
                )
                .await
            }
            Self::Launch {
                launch,
                resolved_vars,
            } => {
                let container_pid = ready.container_pid.to_string();
                let vars = launch_template_vars(runtime, &resolved_vars, Some(&container_pid));
                let launch_env = render_template_map(&launch.env, &vars)?;
                run_launch_steps(runtime, &launch, &vars, &launch_env).await?;
                let env = launch_final_env(&runtime.session_env()?, &launch_env);
                let cwd = render_launch_cwd(
                    launch.cwd.as_deref(),
                    &vars,
                    runtime.identity.container_home.as_path(),
                )?;
                let command = template::render_argv(&launch.command, &vars)?;
                exec_final_container_command_with_options(runtime, command, cwd, env, options).await
            }
        }
    }
}

fn detach_discard_options() -> OperationExecutionOptions {
    OperationExecutionOptions {
        mode: OperationMode::Detach,
        output: OutputSelection {
            stdout: false,
            stderr: false,
        },
    }
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

async fn exec_final_container_command_with_options(
    runtime: &Runtime,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    let exec_spec = ContainerExecSpec {
        stdin_tty: std::io::stdin().is_terminal(),
        stdout_tty: std::io::stdout().is_terminal(),
        user: runtime.exec_identity(),
        cwd,
        env,
        container_name: runtime.identity.container_name.clone(),
        command,
    };
    match options.mode {
        OperationMode::Stream => Ok(ExecutionOutcome::new(
            runtime.container_runtime.exec(&exec_spec).await?,
        )),
        OperationMode::Wait => {
            let output = runtime.container_runtime.exec_capture(&exec_spec).await?;
            Ok(ExecutionOutcome::captured(
                output.exit_code,
                options.output.stdout.then_some(output.stdout),
                options.output.stderr.then_some(output.stderr),
            ))
        }
        OperationMode::Detach => Ok(ExecutionOutcome::new(
            runtime.container_runtime.exec_discard(&exec_spec).await?,
        )),
    }
}

async fn stop(config_path: Option<PathBuf>, args: StopArgs) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_stop_args(args);
    let result = execute_gateway_operation(config_path, operation).await?;
    let GatewayOperationResult::Stop(result) = result else {
        unreachable!("stop operation returned a different result");
    };
    render_stop_result(&result);
    Ok(())
}

async fn remove(config_path: Option<PathBuf>, args: TargetArg) -> anyhow::Result<()> {
    let operation = GatewayOperation::from_remove_args(args);
    let result = execute_gateway_operation(config_path, operation).await?;
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

async fn status(config_path: Option<PathBuf>, status: StatusArg) -> anyhow::Result<()> {
    if status.all {
        if status.target.is_some() {
            anyhow::bail!("--all cannot be combined with a target");
        }
        if status.session_id.is_some() {
            anyhow::bail!("--all cannot be combined with --session-id");
        }
        let json = status.json;
        let result = execute_gateway_operation(config_path, GatewayOperation::StatusAll).await?;
        let GatewayOperationResult::StatusAll(entries) = result else {
            unreachable!("status-all operation returned a different result");
        };
        return render_status_all(entries, json);
    }
    let json = status.json;
    let operation = GatewayOperation::from_status_args(status);
    let result = execute_gateway_operation(config_path, operation).await?;
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

async fn launches(config_path: Option<PathBuf>, args: LaunchesArgs) -> anyhow::Result<()> {
    let json = args.json;
    let operation = GatewayOperation::from_launches_args(args);
    let result = execute_gateway_operation(config_path, operation).await?;
    let GatewayOperationResult::Launches(entries) = result else {
        unreachable!("launches operation returned a different result");
    };
    render_launches(entries, json)
}

async fn launch(config_path: Option<PathBuf>, command: LaunchCommand) -> anyhow::Result<()> {
    match command {
        LaunchCommand::Show(args) => launch_show(config_path, args).await,
        LaunchCommand::Run(raw) => {
            let (name, session_id, vars) = parse_launch_run_args(raw)?;
            launch_execute(config_path, &name, session_id, vars).await
        }
    }
}

fn parse_launch_run_args(
    raw: Vec<std::ffi::OsString>,
) -> anyhow::Result<(String, Option<String>, Vec<String>)> {
    let mut args = raw.into_iter();
    let Some(name) = args.next() else {
        anyhow::bail!("launch requires a launch name");
    };
    let name = name
        .into_string()
        .map_err(|_| anyhow::anyhow!("launch name must be valid UTF-8"))?;
    let mut vars = Vec::new();
    let mut session_id = None;
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| anyhow::anyhow!("launch arguments must be valid UTF-8"))?;
        if arg == "--json" {
            anyhow::bail!("launch execution does not support --json");
        }
        if let Some(value) = arg.strip_prefix("--session-id=") {
            if session_id.replace(value.to_string()).is_some() {
                anyhow::bail!("--session-id may only be specified once");
            }
            continue;
        }
        if arg == "--session-id" {
            if session_id.is_some() {
                anyhow::bail!("--session-id may only be specified once");
            }
            let Some(value) = args.next() else {
                anyhow::bail!("--session-id requires a value");
            };
            session_id = Some(
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("session id must be valid UTF-8"))?,
            );
            continue;
        }
        if let Some(value) = arg.strip_prefix("--var=") {
            vars.push(value.to_string());
            continue;
        }
        if arg == "--var" {
            let Some(value) = args.next() else {
                anyhow::bail!("--var must be key=value");
            };
            vars.push(
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("launch variable must be valid UTF-8"))?,
            );
            continue;
        }
        anyhow::bail!("unexpected extra launch argument {arg:?}");
    }
    Ok((name, session_id, vars))
}

async fn launch_show(
    config_path: Option<PathBuf>,
    args: crate::cli::LaunchShowArgs,
) -> anyhow::Result<()> {
    let json = args.json;
    let operation = GatewayOperation::from_launch_show_args(args);
    let result = execute_gateway_operation(config_path, operation).await?;
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
) -> anyhow::Result<()> {
    let supplied = SuppliedLaunchVars::from_cli_pairs(supplied)?;
    let result = execute_gateway_operation(
        config_path,
        GatewayOperation::launch_run(name.to_string(), session_id, supplied),
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
    options: OperationExecutionOptions,
) -> OperationResult<ExecutionOutcome> {
    let launch = lookup_launch(&cfg, name)?;
    let resolved_vars = resolve_launch_vars(name, &launch, &supplied)?;
    let target = launch.target.clone();
    let runtime =
        Runtime::from_config(cfg, Some(&target), session_id, true, Some(name.to_string())).await?;
    OperationRunner::launch(runtime, options, launch, resolved_vars)
        .run()
        .await
        .map_err(OperationError::operation_failed)
}

fn launch_summaries(cfg: &GatewayConfig) -> anyhow::Result<Vec<LaunchSummary>> {
    Ok(cfg
        .effective_launches()?
        .iter()
        .map(|(name, launch)| LaunchSummary {
            name: name.clone(),
            target: launch.target.clone(),
            description: launch.description.clone(),
            vars: launch_var_metadata(&launch.vars),
        })
        .collect())
}

fn launch_detail(name: &str, launch: &LaunchConfig) -> LaunchDetail {
    LaunchDetail {
        name: name.to_string(),
        target: launch.target.clone(),
        description: launch.description.clone(),
        vars: launch_var_metadata(&launch.vars),
        steps: launch.steps.iter().map(launch_step_detail).collect(),
        cwd: launch.cwd.clone(),
        env: launch.env.clone(),
        command: launch.command.clone(),
    }
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

#[cfg(unix)]
fn unix_socket_path_bytes(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn unix_socket_path_bytes(path: &Path) -> usize {
    path.as_os_str().to_string_lossy().len()
}

fn target_entries(cfg: &GatewayConfig) -> anyhow::Result<Vec<TargetEntry>> {
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
                mode: format!("{:?}", target.mode).to_lowercase(),
                container: target_container_display(target)?,
                default: name == &default_target,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(entries)
}

fn target_container_display(target: &TargetConfig) -> anyhow::Result<String> {
    Ok(match target.mode {
        TargetMode::Fixed => target.container_name(None)?,
        TargetMode::Ephemeral => target
            .ephemeral_name
            .as_deref()
            .unwrap_or("{image_slug}-{session_id}")
            .to_string(),
    })
}

#[derive(Debug)]
struct Runtime {
    cfg: GatewayConfig,
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
    Failure,
}

impl SessionOutcome {
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
    fn resolve(
        target_name: String,
        launch_name: Option<String>,
        session_id: Option<String>,
        user: UserContext,
        target: &TargetConfig,
        container_name: String,
        container_runtime: &ContainerRuntime,
    ) -> anyhow::Result<Self> {
        let default_container_user = target.container_user.clone().unwrap_or_else(|| {
            if container_runtime.kind() == ContainerRuntimeType::Podman {
                user.user.clone()
            } else {
                "root".into()
            }
        });
        let default_container_home = target.container_home.clone().unwrap_or_else(|| {
            if container_runtime.kind() == ContainerRuntimeType::Podman {
                user.home.clone()
            } else {
                PathBuf::from("/root")
            }
        });
        let mut identity_vars = Vars::new();
        identity_vars.insert("user".into(), user.user.clone());
        identity_vars.insert("uid".into(), user.uid.to_string());
        identity_vars.insert("gid".into(), user.gid.to_string());
        identity_vars.insert("home".into(), user.home.display().to_string());
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
        let container_state_dir = workspace
            .join(&target.workspace.state_dir)
            .join(state_kind)
            .join(state_id);
        let container_state_dir_in_container = resolve_container_path(
            &identity.container_home,
            &target.workspace.state_dir,
            [state_kind, state_id],
        );
        let control_sockets = render_control_socket_paths(
            &target.control_sockets,
            target,
            &identity.target_name,
            &identity.container_name,
            identity.session_id.as_deref(),
            runtime_id,
            &identity.user,
        )?;
        Ok(Self {
            workspace,
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
        let cfg = load_config(config_path)?;
        Self::from_config(cfg, target, session_id, generate_session_id, None).await
    }

    async fn from_config(
        cfg: GatewayConfig,
        target: Option<&str>,
        session_id: Option<String>,
        generate_session_id: bool,
        launch_name: Option<String>,
    ) -> OperationResult<Runtime> {
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
        let container_name = target_cfg.container_name(session_id.as_deref())?;
        let workspace =
            resolve_target_workspace(&target_cfg, &target_name, &user, session_id.as_deref())?;
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
        )?;
        let paths = RuntimePaths::resolve(&target_cfg, &identity, workspace)?;
        let runtime = Runtime {
            cfg,
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
        let mut failed_start_cleanup = FailedStartCleanup::default();
        let result = async {
            paths::ensure_private_dir(&self.paths.container_state_dir)?;
            self.prepare_control_socket_dir()?;
            self.write_sshd_session_env_config()?;
            self.write_ssh_command_filter_policy()?;
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
            let inspect = self
                .ensure_container_for_readiness_plan(inspect, &mut failed_start_cleanup)
                .await?;
            self.validate_labels(&inspect)?;
            let container_pid = inspect.state.pid.to_string();
            self.run_lifecycle_phase(LifecyclePhase::PostStartHost, Some(&container_pid))
                .await?;
            if self.requires_agent_control() {
                self.wait_agent_ready().await?;
            }
            self.run_host_steps(&container_pid).await?;
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
            container_pid: inspect.state.pid,
            ssh_socket: self.ssh_socket(),
            ssh_tcp: self.published_ssh_endpoint().await?,
            status: status.status,
            local_ssh: None,
            client_config: None,
        })
    }

    async fn ensure_container_for_readiness_plan(
        &self,
        inspect: Option<ContainerInspect>,
        failed_start_cleanup: &mut FailedStartCleanup,
    ) -> anyhow::Result<ContainerInspect> {
        match (readiness_plan(inspect.as_ref()), inspect) {
            (ContainerReadinessPlan::ReuseRunning, Some(existing)) => {
                self.validate_labels(&existing)?;
                Ok(existing)
            }
            (ContainerReadinessPlan::StartStopped, Some(existing)) => {
                self.validate_labels(&existing)?;
                self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                    .await?;
                self.remove_stale_control_socket_files()?;
                failed_start_cleanup.mark_runtime_start_attempted();
                self.container_runtime
                    .start(&self.identity.container_name)
                    .await?;
                self.inspect_container_after_start().await
            }
            (ContainerReadinessPlan::CreateMissing, None) => {
                self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                    .await?;
                self.remove_stale_control_socket_files()?;
                failed_start_cleanup.mark_runtime_start_attempted();
                self.start_container().await?;
                self.inspect_container_after_start().await
            }
            _ => unreachable!("readiness plan and inspect state should agree"),
        }
    }

    async fn inspect_container_after_start(&self) -> anyhow::Result<ContainerInspect> {
        self.container_runtime
            .inspect(&self.identity.container_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("container did not exist after start"))
    }

    async fn status(&self) -> anyhow::Result<GatewayStatus> {
        let inspect = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?;
        let agent = if self.agent_control_enabled() {
            self.agent_status().await.ok()
        } else {
            None
        };
        let sessions = self.active_session_markers()?;
        let launch = status_launch(self.identity.session_id.as_deref(), &sessions);
        let agent_ready = agent.as_ref().is_some_and(|status| status.ready);
        Ok(GatewayStatus {
            target: self.identity.target_name.clone(),
            session_id: self.identity.session_id.clone(),
            launch,
            mode: format!("{:?}", self.target.mode).to_lowercase(),
            user: self.identity.user.user.clone(),
            image: self.target.image.clone(),
            container: inspect
                .as_ref()
                .map(|_| self.identity.container_name.clone()),
            container_pid: inspect.as_ref().map(|value| value.state.pid),
            active_sessions: sessions.len(),
            sessions,
            agent_ready,
            ssh_socket: self.ssh_socket(),
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

    async fn apply_gateway_idle_cleanup(&self) -> anyhow::Result<()> {
        if !self.target.stop_when_idle {
            return Ok(());
        }
        let Some(cleanup) = self.target.idle_cleanup.as_ref() else {
            return Ok(());
        };
        if cleanup.owner != IdleCleanupOwner::Gateway || cleanup.action == IdleCleanupAction::None {
            return Ok(());
        }
        let idle_grace = cleanup
            .idle_grace
            .as_deref()
            .and_then(|value| crate::config::parse_duration(value).ok())
            .unwrap_or_default();
        if !idle_grace.is_zero() {
            sleep(idle_grace).await;
        }
        let _lock = self.acquire_lifecycle_lock().await?;
        if !self.active_session_markers()?.is_empty() {
            return Ok(());
        }
        if self.has_preserve_process(cleanup).await? {
            tracing::info!(
                container = self.identity.container_name,
                "gateway-owned cleanup preserving container because preserve process is running"
            );
            return Ok(());
        }
        self.stop_managed_container().await
    }

    async fn finish_post_session<T>(
        &self,
        session: session::SessionGuard,
        result: anyhow::Result<T>,
        outcome: SessionOutcome,
    ) -> anyhow::Result<T> {
        drop(session);
        self.apply_post_session_cleanup(outcome).await;
        result
    }

    fn begin_operation_session(
        &self,
        kind: &str,
        launch_marker: bool,
    ) -> anyhow::Result<OperationSessionGuard> {
        let session = if launch_marker {
            self.create_launch_session_marker(kind)?
        } else {
            self.create_session_marker(kind)?
        };
        Ok(OperationSessionGuard {
            session: Some(session),
            agent_session: None,
        })
    }

    async fn hold_operation_agent_session(
        &self,
        session: &mut OperationSessionGuard,
        kind: &str,
    ) -> anyhow::Result<()> {
        session.agent_session = self.agent_session_hold(kind).await?;
        Ok(())
    }

    async fn finish_operation_session<T>(
        &self,
        mut session: OperationSessionGuard,
        result: anyhow::Result<T>,
        outcome: SessionOutcome,
    ) -> anyhow::Result<T> {
        drop(session.agent_session.take());
        let marker = session
            .session
            .take()
            .expect("operation session marker must be present");
        self.finish_post_session(marker, result, outcome).await
    }

    async fn apply_post_session_cleanup(&self, outcome: SessionOutcome) {
        if let Err(err) = self.apply_gateway_idle_cleanup().await {
            tracing::warn!(error = %err, "gateway-owned idle cleanup failed");
        }
        if !self.should_cleanup_workspace(outcome) {
            return;
        }
        let _lock = match self.acquire_lifecycle_lock().await {
            Ok(lock) => lock,
            Err(err) => {
                tracing::warn!(error = %err, "workspace cleanup skipped because lifecycle lock failed");
                return;
            }
        };
        match self.active_session_markers() {
            Ok(sessions) if sessions.is_empty() => {}
            Ok(_) => {
                tracing::warn!(
                    target = %self.identity.target_name,
                    workspace = %self.paths.workspace.display(),
                    "workspace cleanup skipped because active sessions remain"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    target = %self.identity.target_name,
                    workspace = %self.paths.workspace.display(),
                    error = %err,
                    "workspace cleanup skipped because active sessions could not be checked"
                );
                return;
            }
        }
        if let Err(err) = self.remove_session_workspace().await {
            tracing::warn!(
                target = %self.identity.target_name,
                workspace = %self.paths.workspace.display(),
                error = %err,
                "workspace cleanup failed"
            );
        }
    }

    fn should_cleanup_workspace(&self, outcome: SessionOutcome) -> bool {
        match self.target.workspace.cleanup {
            WorkspaceCleanup::Never => false,
            WorkspaceCleanup::Success => outcome == SessionOutcome::Success,
            WorkspaceCleanup::Always => true,
        }
    }

    async fn stop_managed_container(&self) -> anyhow::Result<()> {
        let Some(inspect) = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?
        else {
            return Ok(());
        };
        self.stop_inspected_container(&inspect).await
    }

    async fn stop_inspected_container(&self, inspect: &ContainerInspect) -> anyhow::Result<()> {
        self.validate_labels(inspect)?;
        let container_pid = inspect.state.pid.to_string();
        self.run_lifecycle_phase(LifecyclePhase::PreStop, Some(&container_pid))
            .await?;
        if self.agent_control_enabled() {
            let _ = self.agent_shutdown().await;
            if let Some(current) = self.wait_for_container_exit().await? {
                self.validate_labels(&current)?;
                self.container_runtime
                    .stop(&self.identity.container_name)
                    .await?;
            }
        } else {
            self.container_runtime
                .stop(&self.identity.container_name)
                .await?;
        }
        if self.target.remove_on_stop
            && let Some(current) = self
                .container_runtime
                .inspect(&self.identity.container_name)
                .await?
        {
            self.validate_labels(&current)?;
            self.container_runtime
                .rm(&self.identity.container_name)
                .await?;
        }
        self.run_lifecycle_phase(LifecyclePhase::PostStop, Some(&container_pid))
            .await?;
        self.cleanup_control_socket_dir();
        Ok(())
    }

    async fn wait_for_container_exit(&self) -> anyhow::Result<Option<ContainerInspect>> {
        let timeout = self.shutdown_wait_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            let Some(inspect) = self
                .container_runtime
                .inspect(&self.identity.container_name)
                .await?
            else {
                return Ok(None);
            };
            self.validate_labels(&inspect)?;
            if !inspect.state.running {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Ok(Some(inspect));
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    fn shutdown_wait_timeout(&self) -> Duration {
        self.target
            .idle_cleanup
            .as_ref()
            .and_then(|cleanup| cleanup.shutdown_timeout.as_deref())
            .and_then(|value| crate::config::parse_duration(value).ok())
            .unwrap_or_else(|| Duration::from_secs(10))
    }

    async fn has_preserve_process(
        &self,
        cleanup: &crate::config::IdleCleanupConfig,
    ) -> anyhow::Result<bool> {
        for process in &cleanup.preserve_processes {
            let code = self
                .container_runtime
                .exec_quiet(
                    &self.identity.container_name,
                    ["pgrep", "-x", process.as_str()],
                )
                .await?;
            if code == 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.validation_labels();
        labels.extend([
            ("io.aw-gateway.image".into(), self.target.image.clone()),
            (
                "io.aw-gateway.mode".into(),
                format!("{:?}", self.target.mode).to_lowercase(),
            ),
        ]);
        if let Some(session_id) = &self.identity.session_id {
            labels.insert("io.aw-gateway.session_id".into(), session_id.clone());
        }
        if self.target.mode == TargetMode::Ephemeral
            && let Some(launch_name) = &self.identity.launch_name
        {
            labels.insert("io.aw-gateway.launch".into(), launch_name.clone());
        }
        labels
    }

    fn validation_labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("io.aw-gateway.gateway".into(), "true".into()),
            ("io.aw-gateway.user".into(), self.identity.user.user.clone()),
            (
                "io.aw-gateway.uid".into(),
                self.identity.user.uid.to_string(),
            ),
            (
                "io.aw-gateway.target".into(),
                self.identity.target_name.clone(),
            ),
            (
                "io.aw-gateway.container_id".into(),
                self.identity.container_name.clone(),
            ),
        ])
    }

    fn validate_labels(&self, inspect: &ContainerInspect) -> anyhow::Result<()> {
        runtime::validate_gateway_labels(inspect, &self.validation_labels())
    }

    async fn start_container(&self) -> anyhow::Result<()> {
        let identity_token = self
            .target
            .container_agent
            .needs_identity_token()
            .then(|| self.ensure_identity_token())
            .transpose()?;
        let control_token = self
            .agent_control_enabled()
            .then(|| self.ensure_control_token())
            .transpose()?;
        self.warn_about_unsafe_container_mounts()?;
        let run_spec =
            self.container_run_spec(identity_token.as_deref(), control_token.as_deref())?;
        self.container_runtime.run_detached(&run_spec).await
    }

    fn container_run_spec(
        &self,
        identity_token: Option<&str>,
        control_token: Option<&str>,
    ) -> anyhow::Result<ContainerRunSpec> {
        let mut env = BTreeMap::new();
        if let Some(identity_token) = identity_token {
            env.insert("AW_IDENTITY_TOKEN".into(), identity_token.to_string());
        }
        if let Some(control_token) = control_token {
            env.insert(
                "AW_CONTAINER_CONTROL_TOKEN".into(),
                control_token.to_string(),
            );
        }
        if self.agent_enabled() {
            env.insert(
                "AW_AUTHENTICATED_UID".into(),
                self.identity.user.uid.to_string(),
            );
            env.insert(
                "AW_AUTHENTICATED_GID".into(),
                self.identity.user.gid.to_string(),
            );
        }
        env.extend(self.render_env_map(&self.target.container_env)?);
        let command = if self.agent_enabled() {
            if self.target.container_bootstrap.enabled {
                vec![
                    self.render_value(&self.target.container_bootstrap.entrypoint)?,
                    "--config".into(),
                    self.container_agent_config_in_container()
                        .display()
                        .to_string(),
                    "--bootstrap-config".into(),
                    self.container_bootstrap_config_in_container()
                        .display()
                        .to_string(),
                ]
            } else {
                vec![
                    "aw-container-agent".into(),
                    "--config".into(),
                    self.container_agent_config_in_container()
                        .display()
                        .to_string(),
                    "run".into(),
                ]
            }
        } else {
            vec!["sleep".into(), "infinity".into()]
        };
        Ok(ContainerRunSpec {
            name: self.identity.container_name.clone(),
            hostname: self.identity.container_name.clone(),
            image: self.target.image.clone(),
            workspace: self.paths.workspace.clone(),
            container_home: self.identity.container_home.clone(),
            container_user: if self.target.container_bootstrap.enabled {
                self.bootstrap_identity()
            } else {
                self.identity.container_user.clone()
            },
            passwd_entry: self
                .container_runtime
                .is_podman()
                .then(|| self.passwd_entry()),
            state_dir_in_container: self.paths.container_state_dir_in_container.clone(),
            mounts: self.container_mounts()?,
            env,
            labels: self.labels(),
            publish_ssh: self.ssh_endpoint_configured()
                && self.ssh_backend() == LocalSshBackend::PublishedPort,
            extra_run_args: self
                .target
                .runtime
                .extra_run_args
                .iter()
                .map(|arg| self.render_value(arg))
                .collect::<anyhow::Result<Vec<_>>>()?,
            command,
        })
    }

    async fn run_lifecycle_phase(
        &self,
        phase: LifecyclePhase,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        for step in self
            .target
            .lifecycle_steps
            .iter()
            .filter(|step| step.phase == phase)
        {
            self.run_step(step, container_pid).await?;
        }
        Ok(())
    }

    async fn run_step(
        &self,
        step: &LifecycleStep,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        let vars = self.vars(container_pid);
        let command = template::render_argv(&step.command, &vars)?;
        let timeout = host_hook_timeout(step.timeout.as_deref())?;
        match run_argv_with_timeout(&command, timeout).await {
            Ok(()) => Ok(()),
            Err(err) if !step.required => {
                tracing::warn!(step = step.name, error = %err, "optional lifecycle step failed");
                Ok(())
            }
            Err(err) => Err(err).with_context(|| format!("run lifecycle step {:?}", step.name)),
        }
    }

    async fn run_host_steps(&self, container_pid: &str) -> anyhow::Result<()> {
        for step in &self.target.host_steps {
            let vars = self.vars(Some(container_pid));
            let command = template::render_argv(&step.command, &vars)?;
            let timeout = host_hook_timeout(step.timeout.as_deref())?;
            let command_result = run_argv_with_timeout(&command, timeout).await;
            if let Err(err) = command_result {
                if step.required {
                    return Err(err).with_context(|| format!("host step {:?}", step.name));
                }
                tracing::warn!(step = step.name, error = %err, "optional host step failed");
                continue;
            }
            if let Some(health_check) = &step.health_check {
                let health_result = run_health_check(health_check, &vars).await;
                if let Err(err) = health_result {
                    if step.required {
                        return Err(err)
                            .with_context(|| format!("host step {:?} health check", step.name));
                    }
                    tracing::warn!(step = step.name, error = %err, "optional host step health check failed");
                }
            }
        }
        Ok(())
    }

    async fn wait_agent_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(status) = self.agent_status().await
                && status.ready
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("container agent did not become ready");
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_published_ssh_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(endpoint) = self.published_ssh_endpoint().await?
                && TcpStream::connect((endpoint.host.as_str(), endpoint.port))
                    .await
                    .is_ok()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("published container SSH port did not become ready");
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    async fn agent_status(&self) -> anyhow::Result<AgentStatus> {
        let response = self
            .agent_request::<AgentStatus>(&ControlEnvelope::status(serde_json::json!("status")))
            .await?;
        Ok(response.result)
    }

    async fn agent_shutdown(&self) -> anyhow::Result<()> {
        let token = self.control_token()?;
        let _response = self
            .agent_request::<ShutdownResult>(&ControlEnvelope::shutdown(
                serde_json::json!("shutdown"),
                ShutdownParams {
                    token: Some(token),
                    reason: Some("gateway-stop".into()),
                },
            ))
            .await?;
        Ok(())
    }

    async fn agent_request<T: DeserializeOwned>(
        &self,
        request: &ControlEnvelope,
    ) -> anyhow::Result<ControlSuccess<T>> {
        let (response, _reader) = self
            .send_typed_agent_request(
                request,
                "timed out waiting for container agent control response",
            )
            .await?;
        Ok(response)
    }

    async fn send_typed_agent_request<T: DeserializeOwned>(
        &self,
        request: &ControlEnvelope,
        timeout_message: &'static str,
    ) -> anyhow::Result<(ControlSuccess<T>, BufReader<UnixStream>)> {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.validate_agent_socket().await?;
            let mut stream = UnixStream::connect(self.agent_socket()).await?;
            let mut payload = serde_json::to_vec(&request)?;
            payload.push(b'\n');
            stream.write_all(&payload).await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let response = Self::parse_agent_control_success(&line)?;
            Ok((response, reader))
        })
        .await
        .context(timeout_message)?
    }

    fn parse_agent_control_success<T: DeserializeOwned>(
        line: &str,
    ) -> anyhow::Result<ControlSuccess<T>> {
        let value: serde_json::Value = serde_json::from_str(line)?;
        match value.get("ok").and_then(serde_json::Value::as_bool) {
            Some(true) => Ok(serde_json::from_value::<ControlSuccess<T>>(value)?),
            Some(false) if value.get("error").is_some() => {
                let failure = serde_json::from_value::<ControlFailure>(value)?;
                Err(Self::agent_control_failure(failure))
            }
            Some(false) => {
                let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                anyhow::bail!("agent control request returned ok=false without error: {id:?}")
            }
            None => Ok(serde_json::from_value::<ControlSuccess<T>>(value)?),
        }
    }

    fn agent_control_failure(failure: ControlFailure) -> anyhow::Error {
        anyhow::anyhow!(
            "agent control request failed: {}: {}",
            failure.error.code,
            failure.error.message
        )
    }

    async fn agent_session_hold(&self, kind: &str) -> anyhow::Result<Option<AgentSessionHold>> {
        if !self.uses_agent_idle_cleanup() {
            return Ok(None);
        }
        let token = self.control_token()?;
        let request = ControlEnvelope::session_hold(
            serde_json::json!("session_hold"),
            SessionHoldParams {
                token: Some(token),
                kind: Some(kind.to_string()),
            },
        );
        let (response, reader) = self
            .send_typed_agent_request::<SessionHoldResult>(
                &request,
                "timed out opening container agent session hold",
            )
            .await?;
        if !response.result.held {
            anyhow::bail!("agent session hold response did not confirm hold");
        }
        Ok(Some(AgentSessionHold { _reader: reader }))
    }

    fn uses_agent_idle_cleanup(&self) -> bool {
        self.agent_control_enabled()
            && self.target.idle_cleanup.as_ref().is_some_and(|cleanup| {
                cleanup.owner == IdleCleanupOwner::Agent
                    && cleanup.action != IdleCleanupAction::None
            })
    }

    fn vars(&self, container_pid: Option<&str>) -> Vars {
        let mut vars = Vars::new();
        vars.insert("user".into(), self.identity.user.user.clone());
        vars.insert("uid".into(), self.identity.session_uid.to_string());
        vars.insert("gid".into(), self.identity.session_gid.to_string());
        vars.insert("home".into(), self.identity.user.home.display().to_string());
        vars.insert(
            "container_user".into(),
            self.identity.container_user.clone(),
        );
        vars.insert(
            "container_home".into(),
            self.identity.container_home.display().to_string(),
        );
        vars.insert(
            "workspace".into(),
            self.paths.workspace.display().to_string(),
        );
        vars.insert(
            "state".into(),
            self.paths
                .workspace
                .join(&self.target.workspace.state_dir)
                .display()
                .to_string(),
        );
        vars.insert(
            "state_dir".into(),
            self.identity.user.state_dir().display().to_string(),
        );
        vars.insert("target".into(), self.identity.target_name.clone());
        if let Some(session_id) = &self.identity.session_id {
            vars.insert("session_id".into(), session_id.clone());
        }
        vars.insert("image".into(), self.target.image.clone());
        vars.insert(
            "image_slug".into(),
            template::image_slug(&self.target.image),
        );
        vars.insert(
            "container_name".into(),
            self.identity.container_name.clone(),
        );
        vars.insert(
            "container_state_dir".into(),
            self.paths.container_state_dir.display().to_string(),
        );
        vars.insert(
            "container_state_dir_in_container".into(),
            self.paths
                .container_state_dir_in_container
                .display()
                .to_string(),
        );
        if let Some(container_pid) = container_pid {
            vars.insert("container_pid".into(), container_pid.to_string());
        }
        vars
    }

    fn write_container_agent_config(&self) -> anyhow::Result<PathBuf> {
        let mut container_agent = self.target.container_agent.clone();
        if self.agent_control_enabled() {
            container_agent.control_socket = Some(ControlSocketConfig::Path(
                self.paths
                    .control_sockets
                    .container_agent_socket
                    .display()
                    .to_string(),
            ));
        }
        if let Some(bridge) = &mut container_agent.ssh_bridge
            && self.agent_enabled()
            && bridge.enabled
        {
            bridge.socket = Some(
                self.paths
                    .control_sockets
                    .container_ssh_socket
                    .display()
                    .to_string(),
            );
        }
        self.inject_container_sshd_env(&mut container_agent);
        if let Some(idle_cleanup) = &self.target.idle_cleanup {
            container_agent.idle_cleanup = match (idle_cleanup.owner, idle_cleanup.action) {
                (IdleCleanupOwner::Agent, action) if action != IdleCleanupAction::None => {
                    Some(idle_cleanup.clone())
                }
                _ => None,
            };
        }
        let cfg = ContainerAgentFile {
            schema_version: AGENT_SCHEMA_VERSION.to_string(),
            logging: LoggingConfig::default(),
            container_agent,
        };
        let path = self.container_agent_config_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn inject_container_sshd_env(&self, container_agent: &mut crate::config::ContainerAgentConfig) {
        for service in &mut container_agent.services {
            if service.name != "container-sshd" {
                continue;
            }
            service.env.insert(
                "AW_SSHD_POLICY_CONFIG".into(),
                crate::config::EnvValue {
                    value: Some(
                        self.ssh_command_filter_policy_in_container()
                            .display()
                            .to_string(),
                    ),
                    file: None,
                    inherit: None,
                    interpolate: false,
                    required: true,
                },
            );
            service
                .env
                .entry("AW_SSHD_SETENV_CONFIG".into())
                .or_insert(crate::config::EnvValue {
                    value: Some(
                        self.sshd_session_env_config_in_container()
                            .display()
                            .to_string(),
                    ),
                    file: None,
                    inherit: None,
                    interpolate: false,
                    required: true,
                });
        }
    }

    fn write_ssh_command_filter_policy(&self) -> anyhow::Result<PathBuf> {
        let cfg = SshCommandFilterPolicy {
            sftp: self.target.container_ssh.transfer.sftp,
            legacy_scp: self.target.container_ssh.transfer.legacy_scp,
        };
        let path = self.ssh_command_filter_policy_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn write_container_bootstrap_config(&self) -> anyhow::Result<PathBuf> {
        let vars = self.vars(None);
        let steps = self
            .target
            .container_bootstrap_steps
            .iter()
            .map(|step| {
                Ok(RenderedContainerBootstrapStep {
                    name: step.name.clone(),
                    required: step.required,
                    user: template::render(&step.user, &vars)?,
                    command: step
                        .command
                        .iter()
                        .map(|arg| template::render(arg, &vars))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                    timeout: step.timeout.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let cfg = ContainerBootstrapFile {
            schema_version: AGENT_SCHEMA_VERSION.to_string(),
            agent_program: template::render(&self.target.container_bootstrap.agent_program, &vars)?,
            agent_config: self
                .container_agent_config_in_container()
                .display()
                .to_string(),
            skip_identity_prepare: self.container_runtime.is_podman(),
            identity: BootstrapIdentity {
                session_user: self.identity.container_user.clone(),
                session_uid: self.identity.session_uid,
                session_gid: self.identity.session_gid,
                session_home: self.identity.container_home.display().to_string(),
                session_shell: self.identity.session_shell.clone(),
                state_dir: self
                    .paths
                    .container_state_dir_in_container
                    .display()
                    .to_string(),
            },
            steps,
        };
        let path = self.container_bootstrap_config_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn container_agent_config_host(&self) -> PathBuf {
        self.paths.container_state_dir.join("container-agent.toml")
    }

    fn container_agent_config_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("container-agent.toml")
    }

    fn container_bootstrap_config_host(&self) -> PathBuf {
        self.paths
            .container_state_dir
            .join("container-bootstrap.toml")
    }

    fn container_bootstrap_config_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("container-bootstrap.toml")
    }

    fn sshd_session_env_config_host(&self) -> PathBuf {
        self.paths.container_state_dir.join("sshd-session-env.conf")
    }

    fn sshd_session_env_config_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("sshd-session-env.conf")
    }

    fn ssh_command_filter_policy_host(&self) -> PathBuf {
        self.paths
            .container_state_dir
            .join("ssh-command-filter.toml")
    }

    fn ssh_command_filter_policy_in_container(&self) -> PathBuf {
        self.paths
            .container_state_dir_in_container
            .join("ssh-command-filter.toml")
    }

    fn write_sshd_session_env_config(&self) -> anyhow::Result<PathBuf> {
        let path = self.sshd_session_env_config_host();
        let env = self.render_env_map(&self.target.session_env)?;
        let mut raw = String::from(
            "# Generated by aw-gateway. Included by container SSHD helpers when configured.\n",
        );
        if !env.is_empty() {
            raw.push_str("SetEnv");
            for (key, value) in env {
                if key.contains(char::is_whitespace) {
                    anyhow::bail!(
                        "target session_env key {key:?} contains whitespace and cannot be rendered into sshd SetEnv"
                    );
                }
                if value.contains(char::is_whitespace) {
                    anyhow::bail!(
                        "target session_env value for {key:?} contains whitespace and cannot be rendered into sshd SetEnv"
                    );
                }
                raw.push(' ');
                raw.push_str(&key);
                raw.push('=');
                raw.push_str(&value);
            }
            raw.push('\n');
        }
        write_private_file(&path, raw.as_bytes(), 0o600)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn prepare_control_socket_dir(&self) -> anyhow::Result<()> {
        if self.paths.control_sockets.default_host_dir {
            let run_user_dir = PathBuf::from(format!("/run/user/{}", self.identity.user.uid));
            let metadata = std::fs::metadata(&run_user_dir).with_context(|| {
                format!(
                    "{} is required by the default control_sockets.host_dir; configure control_sockets.host_dir to a writable short absolute path if this host does not provide per-user runtime directories",
                    run_user_dir.display()
                )
            })?;
            if !metadata.is_dir() {
                anyhow::bail!(
                    "{} is not a directory; configure control_sockets.host_dir to a writable short absolute path",
                    run_user_dir.display()
                );
            }
            let probe = run_user_dir.join(format!(".aw-gateway-write-test-{}", std::process::id()));
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&probe)
            {
                Ok(_) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(err) => {
                    anyhow::bail!(
                        "{} is not writable: {err}; configure control_sockets.host_dir to a writable short absolute path",
                        run_user_dir.display()
                    );
                }
            }
        }
        ensure_control_socket_dir(&self.paths.control_sockets.host_dir)?;
        Ok(())
    }

    fn remove_stale_control_socket_files(&self) -> anyhow::Result<()> {
        for socket in [
            &self.paths.control_sockets.host_agent_socket,
            &self.paths.control_sockets.host_ssh_socket,
        ] {
            match std::fs::symlink_metadata(socket) {
                Ok(metadata) if metadata.is_dir() => {
                    anyhow::bail!(
                        "control socket path {} exists as a directory; remove it or configure a different control_sockets.host_dir",
                        socket.display()
                    );
                }
                Ok(_) => {
                    std::fs::remove_file(socket).with_context(|| {
                        format!("remove stale control socket {}", socket.display())
                    })?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("inspect control socket {}", socket.display()));
                }
            }
        }
        Ok(())
    }

    fn passwd_entry(&self) -> String {
        format!(
            "{}:x:{}:{}:{}:{}:{}",
            self.identity.container_user,
            self.identity.session_uid,
            self.identity.session_gid,
            self.identity.container_user,
            self.identity.container_home.display(),
            self.identity.session_shell,
        )
    }

    fn container_mounts(&self) -> anyhow::Result<Vec<ContainerMountSpec>> {
        let mut mounts = self
            .target
            .container_mounts
            .iter()
            .enumerate()
            .map(|(index, mount)| {
                let vars = self.vars(None);
                let source = PathBuf::from(template::render(&mount.source, &vars)?);
                let source = source.canonicalize().with_context(|| {
                    format!("container mount source #{index} {}", source.display())
                })?;
                Ok(ContainerMountSpec {
                    source,
                    target: PathBuf::from(template::render(&mount.target, &vars)?),
                    readonly: mount.mode == ContainerMountMode::Ro,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        mounts.push(ContainerMountSpec {
            source: self.paths.control_sockets.host_dir.clone(),
            target: self.paths.control_sockets.container_dir.clone(),
            readonly: false,
        });
        Ok(mounts)
    }

    fn warn_about_unsafe_container_mounts(&self) -> anyhow::Result<()> {
        for (index, mount) in self.container_mounts()?.into_iter().enumerate() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let metadata = std::fs::metadata(&mount.source).with_context(|| {
                    format!(
                        "stat container mount source #{} {}",
                        index,
                        mount.source.display()
                    )
                })?;
                if metadata.permissions().mode() & 0o002 != 0 {
                    tracing::warn!(
                        mount = index,
                        source = %mount.source.display(),
                        "container mount source is world-writable"
                    );
                }
            }
        }
        Ok(())
    }

    fn render_value(&self, value: &str) -> anyhow::Result<String> {
        template::render(value, &self.vars(None))
    }

    fn render_env_map(
        &self,
        env: &BTreeMap<String, String>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        env.iter()
            .map(|(key, value)| Ok((key.clone(), self.render_value(value)?)))
            .collect()
    }

    fn exec_identity(&self) -> String {
        if self.container_runtime.is_podman() {
            format!(
                "{}:{}",
                self.identity.session_uid, self.identity.session_gid
            )
        } else {
            self.identity.container_user.clone()
        }
    }

    fn bootstrap_identity(&self) -> String {
        if self.container_runtime.is_podman() {
            "0:0".into()
        } else {
            self.identity.bootstrap_user.clone()
        }
    }

    fn session_env(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::from([
            ("SHELL".into(), DEFAULT_SESSION_SHELL_ENV.to_string()),
            (
                "PATH".into(),
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
            ),
        ]);
        env.extend(self.render_env_map(&self.target.session_env)?);
        Ok(env)
    }

    fn agent_socket(&self) -> PathBuf {
        self.paths.control_sockets.host_agent_socket.clone()
    }

    fn ssh_socket(&self) -> PathBuf {
        self.paths.control_sockets.host_ssh_socket.clone()
    }

    fn container_agent_socket(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.agent_control_enabled() {
            return Ok(None);
        }
        Ok(Some(
            self.paths.control_sockets.container_agent_socket.clone(),
        ))
    }

    fn container_ssh_socket(&self) -> anyhow::Result<Option<PathBuf>> {
        let Some(_) = self
            .target
            .container_agent
            .ssh_bridge
            .as_ref()
            .filter(|bridge| self.agent_enabled() && bridge.enabled)
        else {
            return Ok(None);
        };
        Ok(Some(
            self.paths.control_sockets.container_ssh_socket.clone(),
        ))
    }

    fn effective_unix_socket_paths(&self) -> anyhow::Result<Vec<(&'static str, PathBuf)>> {
        let mut paths = Vec::new();
        if self.agent_control_enabled() {
            paths.push(("host agent socket path", self.agent_socket()));
            if let Some(path) = self.container_agent_socket()? {
                paths.push(("container agent socket path", path));
            }
        }
        if self.ssh_endpoint_configured() && self.ssh_backend() == LocalSshBackend::Socket {
            paths.push(("host ssh socket path", self.ssh_socket()));
        }
        if let Some(path) = self.container_ssh_socket()? {
            paths.push(("container ssh socket path", path));
        }
        Ok(paths)
    }

    fn validate_unix_socket_paths(&self) -> anyhow::Result<()> {
        for (label, path) in self.effective_unix_socket_paths()? {
            let bytes = unix_socket_path_bytes(&path);
            if bytes > UNIX_SOCKET_PATH_MAX_BYTES {
                anyhow::bail!(
                    "{label} is too long for a Unix domain socket ({bytes} bytes, limit {UNIX_SOCKET_PATH_MAX_BYTES}): {}. Configure control_sockets.host_dir or control_sockets.container_dir to a shorter absolute path. For fixed targets, runtime_id is the target id {:?}; for ephemeral targets, runtime_id is the session id.",
                    path.display(),
                    self.identity.target_name
                );
            }
        }
        Ok(())
    }

    fn ssh_backend(&self) -> LocalSshBackend {
        self.target
            .local_ssh
            .as_ref()
            .map(|local_ssh| local_ssh.backend)
            .unwrap_or_default()
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

    fn ensure_ssh_endpoint_configured(&self) -> anyhow::Result<()> {
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
        if self.ssh_backend() != LocalSshBackend::PublishedPort {
            return Ok(None);
        }
        Ok(self
            .container_runtime
            .published_port(&self.identity.container_name, 22)
            .await?
            .map(|port| TcpEndpoint {
                host: "127.0.0.1".into(),
                port,
            }))
    }

    async fn validate_agent_socket(&self) -> anyhow::Result<()> {
        runtime::socket_is_safe_for(
            &self.agent_socket(),
            self.identity.user.uid,
            self.identity.user.gid,
        )
    }

    async fn validate_ssh_socket(&self) -> anyhow::Result<()> {
        self.validate_socket_path(&self.ssh_socket()).await
    }

    async fn validate_socket_path(&self, socket: &Path) -> anyhow::Result<()> {
        runtime::socket_is_safe_for(socket, self.identity.user.uid, self.identity.user.gid)?;
        let _ = UnixStream::connect(socket)
            .await
            .with_context(|| format!("test-connect {}", socket.display()))?;
        Ok(())
    }

    async fn cleanup_failed_start(&self) {
        match self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await
        {
            Ok(Some(inspect)) => {
                if let Err(err) = self.validate_labels(&inspect) {
                    tracing::warn!(
                        container = self.identity.container_name,
                        error = %err,
                        "not cleaning failed start because labels did not match"
                    );
                    return;
                }
                if let Err(err) = self
                    .container_runtime
                    .stop(&self.identity.container_name)
                    .await
                {
                    tracing::warn!(
                        container = self.identity.container_name,
                        error = %err,
                        "failed to stop container after startup failure"
                    );
                }
                if self.target.remove_on_stop
                    && let Err(err) = self
                        .container_runtime
                        .rm(&self.identity.container_name)
                        .await
                {
                    tracing::warn!(
                        container = self.identity.container_name,
                        error = %err,
                        "failed to remove container after startup failure"
                    );
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    container = self.identity.container_name,
                    error = %err,
                    "failed to inspect container after startup failure"
                );
            }
        }
    }

    fn cleanup_control_socket_dir(&self) {
        match std::fs::symlink_metadata(&self.paths.control_sockets.host_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    "not removing symlink control socket runtime directory"
                );
                return;
            }
            Ok(metadata) if !metadata.is_dir() => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    "not removing non-directory control socket runtime path"
                );
                return;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    error = %err,
                    "failed to inspect control socket runtime directory before cleanup"
                );
                return;
            }
        }

        for socket in [
            &self.paths.control_sockets.host_agent_socket,
            &self.paths.control_sockets.host_ssh_socket,
        ] {
            match std::fs::remove_file(socket) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(
                        path = %socket.display(),
                        error = %err,
                        "failed to remove control socket file"
                    );
                }
            }
        }

        match std::fs::remove_dir(&self.paths.control_sockets.host_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    path = %self.paths.control_sockets.host_dir.display(),
                    error = %err,
                    "failed to remove control socket runtime directory"
                );
            }
        }
    }

    async fn validate_workspace_cleanup_path(&self) -> anyhow::Result<()> {
        if self.target.workspace.cleanup == WorkspaceCleanup::Never {
            return Ok(());
        }
        let session_id =
            self.identity.session_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!("workspace_cleanup requires an ephemeral session id")
            })?;
        validate_workspace_cleanup_path(
            &self.paths.workspace,
            &self.identity.user.home,
            session_id,
            Some(self.target.workspace.path.as_str()),
        )?;
        match tokio::fs::symlink_metadata(&self.paths.workspace).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "workspace_cleanup path {} must not be a symlink",
                    self.paths.workspace.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect workspace cleanup path {}",
                        self.paths.workspace.display()
                    )
                });
            }
        }
        Ok(())
    }

    async fn remove_session_workspace(&self) -> anyhow::Result<()> {
        self.validate_workspace_cleanup_path().await?;
        let metadata = match tokio::fs::symlink_metadata(&self.paths.workspace).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect workspace cleanup path {}",
                        self.paths.workspace.display()
                    )
                });
            }
        };
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "workspace cleanup path {} must not be a symlink",
                self.paths.workspace.display()
            );
        }
        if !metadata.is_dir() {
            anyhow::bail!(
                "workspace cleanup path {} exists but is not a directory",
                self.paths.workspace.display()
            );
        }
        self.container_runtime
            .remove_host_dir_all(&self.paths.workspace)
            .await
    }
}

fn validate_workspace_cleanup_path(
    workspace: &Path,
    home: &Path,
    session_id: &str,
    configured_workspace: Option<&str>,
) -> anyhow::Result<()> {
    if workspace.as_os_str().is_empty() {
        anyhow::bail!("workspace_cleanup resolved workspace must not be empty");
    }
    if workspace
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        anyhow::bail!(
            "workspace_cleanup path must not contain '.' or '..' components: {}",
            workspace.display()
        );
    }
    if workspace == Path::new("/") {
        anyhow::bail!("workspace_cleanup refuses to delete /");
    }
    if workspace == home {
        anyhow::bail!(
            "workspace_cleanup refuses to delete user home directory {}",
            workspace.display()
        );
    }
    if session_id.len() < 3 {
        anyhow::bail!("workspace_cleanup session_id {session_id:?} must be at least 3 characters");
    }
    let leaf = workspace
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !leaf.contains(session_id) {
        anyhow::bail!(
            "workspace_cleanup path {} leaf must contain session_id {session_id:?}",
            workspace.display()
        );
    }
    if let Some(configured_workspace) = configured_workspace
        && Path::new(configured_workspace)
            .components()
            .any(|component| component.as_os_str() == "aw-gateway")
        && !workspace
            .components()
            .any(|component| component.as_os_str() == "aw-gateway")
    {
        anyhow::bail!(
            "workspace_cleanup path {} is outside the configured aw-gateway workspace root",
            workspace.display()
        );
    }
    Ok(())
}

fn ensure_control_socket_dir(path: &Path) -> anyhow::Result<()> {
    let created = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "control_sockets.host_dir {} must not be a symlink",
                path.display()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!(
                "control_sockets.host_dir {} exists but is not a directory",
                path.display()
            );
        }
        Ok(metadata) => {
            validate_control_socket_dir_permissions(path, &metadata)?;
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .with_context(|| format!("create control_sockets.host_dir {}", path.display()))?;
            true
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("inspect control_sockets.host_dir {}", path.display()));
        }
    };

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect control_sockets.host_dir {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "control_sockets.host_dir {} must not be a symlink",
            path.display()
        );
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "control_sockets.host_dir {} exists but is not a directory",
            path.display()
        );
    }
    if created {
        set_control_socket_dir_permissions(path)?;
    } else {
        validate_control_socket_dir_permissions(path, &metadata)?;
    }
    Ok(())
}

fn set_control_socket_dir_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 control_sockets.host_dir {}", path.display()))?;
    }
    Ok(())
}

fn validate_control_socket_dir_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "control_sockets.host_dir {} exists with permissions {mode:o}; use a private 0700 directory or remove it so aw-gateway can create it",
                path.display()
            );
        }
    }
    Ok(())
}

fn resolve_container_path(home: &Path, configured: &str, suffix: [&str; 2]) -> PathBuf {
    let base = paths::expand_home(home, configured);
    let mut path = if base.is_absolute() {
        base
    } else {
        home.join(base)
    };
    for part in suffix {
        path.push(part);
    }
    path
}

fn render_control_socket_paths(
    cfg: &ControlSocketsConfig,
    target: &TargetConfig,
    target_name: &str,
    container_name: &str,
    session_id: Option<&str>,
    runtime_id: &str,
    user: &UserContext,
) -> anyhow::Result<ControlSocketPaths> {
    validate_name("control_sockets runtime_id", runtime_id)?;
    let mut vars = Vars::new();
    vars.insert("user".into(), user.user.clone());
    vars.insert("uid".into(), user.uid.to_string());
    vars.insert("gid".into(), user.gid.to_string());
    vars.insert("home".into(), user.home.display().to_string());
    vars.insert("target".into(), target_name.to_string());
    vars.insert("image".into(), target.image.clone());
    vars.insert("image_slug".into(), template::image_slug(&target.image));
    vars.insert("container_name".into(), container_name.to_string());
    vars.insert("runtime_id".into(), runtime_id.to_string());
    if let Some(session_id) = session_id {
        vars.insert("session_id".into(), session_id.to_string());
    }

    let host_dir = PathBuf::from(template::render(&cfg.host_dir, &vars)?);
    if !host_dir.is_absolute() {
        anyhow::bail!(
            "control_sockets.host_dir must render to an absolute path, got {}",
            host_dir.display()
        );
    }
    validate_control_socket_host_dir(&host_dir, runtime_id, user)?;
    let container_dir = PathBuf::from(template::render(&cfg.container_dir, &vars)?);
    if !container_dir.is_absolute() {
        anyhow::bail!(
            "control_sockets.container_dir must render to an absolute path, got {}",
            container_dir.display()
        );
    }

    Ok(ControlSocketPaths {
        host_agent_socket: host_dir.join("agent.sock"),
        host_ssh_socket: host_dir.join("ssh.sock"),
        container_agent_socket: container_dir.join("agent.sock"),
        container_ssh_socket: container_dir.join("ssh.sock"),
        default_host_dir: cfg.host_dir == "/run/user/{uid}/aw-gateway/{runtime_id}",
        host_dir,
        container_dir,
    })
}

fn validate_control_socket_host_dir(
    host_dir: &Path,
    runtime_id: &str,
    user: &UserContext,
) -> anyhow::Result<()> {
    if host_dir
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        anyhow::bail!(
            "control_sockets.host_dir must not contain '.' or '..' path components: {}",
            host_dir.display()
        );
    }

    let dangerous = [
        PathBuf::from("/"),
        user.home.clone(),
        PathBuf::from("/tmp"),
        PathBuf::from("/run"),
        PathBuf::from("/run/user"),
        PathBuf::from(format!("/run/user/{}", user.uid)),
    ];
    if dangerous.iter().any(|path| path == host_dir) {
        anyhow::bail!(
            "control_sockets.host_dir must be an isolated runtime-specific leaf directory, got dangerous shared path {}",
            host_dir.display()
        );
    }

    let Some(leaf) = host_dir.file_name().and_then(|value| value.to_str()) else {
        anyhow::bail!(
            "control_sockets.host_dir must end with runtime_id {runtime_id:?}, got {}",
            host_dir.display()
        );
    };
    if leaf != runtime_id {
        anyhow::bail!(
            "control_sockets.host_dir must end with runtime_id {runtime_id:?} so cleanup only removes the runtime leaf, got {}",
            host_dir.display()
        );
    }

    Ok(())
}

fn resolve_target_workspace(
    target: &TargetConfig,
    target_name: &str,
    user: &UserContext,
    session_id: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let mut vars = Vars::new();
    vars.insert("user".into(), user.user.clone());
    vars.insert("uid".into(), user.uid.to_string());
    vars.insert("gid".into(), user.gid.to_string());
    vars.insert("home".into(), user.home.display().to_string());
    vars.insert("target".into(), target_name.to_string());
    vars.insert("image".into(), target.image.clone());
    vars.insert("image_slug".into(), template::image_slug(&target.image));
    if let Some(session_id) = session_id {
        vars.insert("session_id".into(), session_id.to_string());
    }
    let rendered = template::render(&target.workspace.path, &vars)?;
    Ok(paths::resolve_workspace(&user.home, &rendered))
}

fn host_hook_timeout(configured: Option<&str>) -> anyhow::Result<Duration> {
    configured
        .map(crate::config::parse_duration)
        .transpose()
        .map(|timeout| timeout.unwrap_or(DEFAULT_HOST_HOOK_TIMEOUT))
}

fn load_config(config_path: Option<PathBuf>) -> anyhow::Result<GatewayConfig> {
    let path = paths::gateway_config_path(config_path);
    GatewayConfig::load(&path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerReadinessPlan {
    ReuseRunning,
    StartStopped,
    CreateMissing,
}

#[derive(Debug, Default)]
struct FailedStartCleanup {
    runtime_start_attempted: bool,
}

impl FailedStartCleanup {
    fn mark_runtime_start_attempted(&mut self) {
        self.runtime_start_attempted = true;
    }

    async fn run_if_needed(self, runtime: &Runtime) {
        if self.runtime_start_attempted {
            runtime.cleanup_failed_start().await;
            runtime.cleanup_control_socket_dir();
        }
    }
}

fn readiness_plan(inspect: Option<&ContainerInspect>) -> ContainerReadinessPlan {
    match inspect {
        Some(inspect) if inspect.state.running => ContainerReadinessPlan::ReuseRunning,
        Some(_) => ContainerReadinessPlan::StartStopped,
        None => ContainerReadinessPlan::CreateMissing,
    }
}

struct AgentSessionHold {
    _reader: BufReader<UnixStream>,
}

struct OperationSessionGuard {
    session: Option<session::SessionGuard>,
    agent_session: Option<AgentSessionHold>,
}

#[cfg(test)]
mod tests;
