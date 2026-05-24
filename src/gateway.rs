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
    ContainerRuntime, ManagedContainer,
};
use crate::ssh_dispatch::{self, Dispatch, GatewayAction};
use crate::ssh_filter::{
    SshCommandFilterPolicy, is_sftp_server_command, legacy_scp_mode_allows,
    legacy_scp_server_direction,
};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::{Duration, Instant, sleep};

pub const DEFAULT_GATEWAY_CONFIG: &str = include_str!("../aw-gateway.sample.toml");
const MAX_SSH_ORIGINAL_COMMAND_BYTES: usize = 64 * 1024;
const DEFAULT_HOST_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

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
mod session;
mod token;

use client::{read_default_selection, resolve_target_selection};
use health::{run_argv_with_options, run_argv_with_timeout, run_health_check};
use model::{
    AllStatusEntry, GatewayStatus, LaunchDetail, LaunchStepDetail, LaunchSummary,
    LaunchVarMetadata, ReadyStatus, TargetEntry, TcpEndpoint, gateway_status_name,
};
use ops::{
    ExecutionOutcome, GatewayOperation, GatewayOperationResult, OperationExecutionOptions,
    OperationMode, OutputSelection, RemoveResult, StopResult, SuppliedLaunchVarValue,
    SuppliedLaunchVars, execute_gateway_operation, operation_up_with_runtime,
};
use session::{generate_session_id_value, validate_session_id};

#[cfg(test)]
use crate::config::HealthCheck;
#[cfg(test)]
use client::{configured_default_display, normalize_image_selection};
#[cfg(test)]
use identity::{
    ensure_identity_token_file, is_plausible_public_key, validate_identity_token_content,
    validate_public_key_content,
};
#[cfg(test)]
use model::{LocalListenerStatus, SessionMarker};
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
    if let Some(render) = SshOperationRender::from_action(&action) {
        let operation = GatewayOperation::from_ssh_action(action)?
            .expect("ssh operation render must match operation conversion");
        let result = execute_gateway_operation(config_path, operation).await?;
        return render_operation_result(result, render);
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

#[derive(Debug, Clone, Copy)]
enum SshOperationRender {
    Up,
    Run,
    Launches { json: bool },
    LaunchShow { json: bool },
    Launch,
    Status,
    Targets { json: bool },
    Stop,
    Remove,
    DefaultSelection,
    ClientConfig,
}

impl SshOperationRender {
    fn from_action(action: &GatewayAction) -> Option<Self> {
        match action {
            GatewayAction::Up(_) => Some(Self::Up),
            GatewayAction::Run(_) => Some(Self::Run),
            GatewayAction::Launches { json } => Some(Self::Launches { json: *json }),
            GatewayAction::LaunchShow { json, .. } => Some(Self::LaunchShow { json: *json }),
            GatewayAction::LaunchRun { .. } => Some(Self::Launch),
            GatewayAction::Status(_) => Some(Self::Status),
            GatewayAction::Targets { json } => Some(Self::Targets { json: *json }),
            GatewayAction::Stop(_) => Some(Self::Stop),
            GatewayAction::Remove(_) => Some(Self::Remove),
            GatewayAction::SetDefault(_)
            | GatewayAction::ShowDefault
            | GatewayAction::ResetDefault => Some(Self::DefaultSelection),
            GatewayAction::ClientConfig(_) => Some(Self::ClientConfig),
            GatewayAction::Connect(_)
            | GatewayAction::AddKey(_)
            | GatewayAction::AddHostKey(_)
            | GatewayAction::AddContainerKey(_)
            | GatewayAction::ClientBundle(_)
            | GatewayAction::Help => None,
        }
    }
}

fn render_operation_result(
    result: GatewayOperationResult,
    render: SshOperationRender,
) -> anyhow::Result<()> {
    match (result, render) {
        (GatewayOperationResult::Up(ready), SshOperationRender::Up) => render_up_result(ready),
        (GatewayOperationResult::Run(outcome), SshOperationRender::Run)
        | (GatewayOperationResult::Launch(outcome), SshOperationRender::Launch) => {
            exit_with_execution_outcome(outcome)
        }
        (GatewayOperationResult::Launches(entries), SshOperationRender::Launches { json }) => {
            render_launches(entries, json)
        }
        (GatewayOperationResult::LaunchShow(detail), SshOperationRender::LaunchShow { json }) => {
            render_launch_detail(detail, json)
        }
        (GatewayOperationResult::Status(status), SshOperationRender::Status) => {
            render_status_result(status, true)
        }
        (GatewayOperationResult::StatusAll(entries), SshOperationRender::Status) => {
            render_status_all(entries, true)
        }
        (GatewayOperationResult::Targets(entries), SshOperationRender::Targets { json }) => {
            render_targets(entries, json)
        }
        (GatewayOperationResult::Stop(result), SshOperationRender::Stop) => {
            render_stop_result(&result);
            Ok(())
        }
        (GatewayOperationResult::Remove(result), SshOperationRender::Remove) => {
            render_remove_result(&result);
            Ok(())
        }
        (
            GatewayOperationResult::DefaultSelection(selection),
            SshOperationRender::DefaultSelection,
        ) => {
            render_default_selection(&selection);
            Ok(())
        }
        (
            GatewayOperationResult::ClientConfig {
                rendered,
                written_path,
            },
            SshOperationRender::ClientConfig,
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

fn render_up_result(ready: ReadyStatus) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&ready)?);
    Ok(())
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

    fn warn_detached_failure(self, operation_id: &str, err: &anyhow::Error) {
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
        let operation_id = generate_session_id_value()?;
        let Self {
            runtime,
            session_spec,
            body,
            ..
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
                session_spec.warn_detached_failure(&background_id, &err);
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
                    .map(|cwd| paths::expand_home(&runtime.container_home, cwd));
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
                    runtime.container_home.as_path(),
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
        container_name: runtime.container_name.clone(),
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

fn render_stop_result(result: &StopResult) {
    if result.stopped {
        println!("{}", stop_result_text(result));
    } else {
        println!("not running");
    }
}

fn stop_result_text(result: &StopResult) -> String {
    format!("stopped {}", result.container)
}

fn render_remove_result(result: &RemoveResult) {
    if result.removed {
        println!("{}", remove_result_text(result));
    } else {
        println!("not found");
    }
}

fn remove_result_text(result: &RemoveResult) -> String {
    format!("removed {}", result.container)
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

fn render_default_selection(selection: &str) {
    println!("{selection}");
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

fn render_status_result(result: GatewayStatus, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{}: {} ({})",
            result.target,
            result.status,
            result.container.unwrap_or_else(|| "not-created".into())
        );
        if let Some(launch) = &result.launch {
            println!("launch: {launch}");
        }
    }
    Ok(())
}

fn render_status_all(summaries: Vec<AllStatusEntry>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
    } else if summaries.is_empty() {
        println!("No aw-gateway-managed containers found for this user.");
    } else {
        println!(
            "{:<15} {:<11} {:<16} {:<11} {:<22} STATUS",
            "TARGET", "SESSION", "LAUNCH", "MODE", "CONTAINER"
        );
        for entry in summaries {
            println!(
                "{:<15} {:<11} {:<16} {:<11} {:<22} {}",
                entry.target,
                entry.session_id.as_deref().unwrap_or("-"),
                entry.launch.as_deref().unwrap_or("-"),
                entry.mode,
                entry.container,
                entry.status
            );
        }
    }
    Ok(())
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

fn render_targets(entries: Vec<TargetEntry>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!("{:<24} {:<24} {:<10} CONTAINER", "TARGET", "IMAGE", "MODE");
        for entry in entries {
            let default_marker = if entry.default { " *" } else { "" };
            println!(
                "{:<24} {:<24} {:<10} {}{}",
                entry.target, entry.image, entry.mode, entry.container, default_marker
            );
        }
    }
    Ok(())
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

fn render_launches(entries: Vec<LaunchSummary>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        println!("No launches configured.");
    } else {
        println!(
            "{:<24} {:<24} {:<25} DESCRIPTION",
            "LAUNCH", "TARGET", "REQUIRED VARS"
        );
        for entry in entries {
            let required = entry
                .vars
                .iter()
                .filter_map(|(name, var)| var.required.then_some(name.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "{:<24} {:<24} {:<25} {}",
                entry.name,
                entry.target,
                required,
                entry.description.unwrap_or_default()
            );
        }
    }
    Ok(())
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

fn render_launch_detail(detail: LaunchDetail, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        print_launch_detail(&detail);
    }
    Ok(())
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
) -> anyhow::Result<ExecutionOutcome> {
    let launch = cfg.effective_launch(name)?;
    let resolved_vars = resolve_launch_vars(name, &launch, &supplied)?;
    let target = launch.target.clone();
    let runtime =
        Runtime::from_config(cfg, Some(&target), session_id, true, Some(name.to_string())).await?;
    OperationRunner::launch(runtime, options, launch, resolved_vars)
        .run()
        .await
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

fn print_launch_detail(detail: &LaunchDetail) {
    println!("Launch: {}", detail.name);
    println!("Target: {}", detail.target);
    if let Some(description) = &detail.description {
        println!("Description: {description}");
    }
    if !detail.vars.is_empty() {
        println!("\nVariables:");
        for (name, var) in &detail.vars {
            println!(
                "  {name} ({}){}",
                launch_var_text(var),
                launch_var_description(var)
            );
        }
    }
    if !detail.steps.is_empty() {
        println!("\nSteps:");
        for (index, step) in detail.steps.iter().enumerate() {
            let required = if step.required {
                "required"
            } else {
                "optional"
            };
            let timeout = step
                .timeout
                .as_deref()
                .map(|value| format!(", timeout: {value}"))
                .unwrap_or_default();
            println!(
                "  {}. {} [{}/{}, {}{}]",
                index + 1,
                step.name,
                step.phase,
                step.location,
                required,
                timeout
            );
            if let Some(cwd) = &step.cwd {
                println!("     cwd: {cwd}");
            }
            if !step.env.is_empty() {
                println!("     env: {}", env_summary(&step.env));
            }
            println!("     argv: {}", step.command.join(" "));
        }
    }
    println!("\nCommand:");
    if let Some(cwd) = &detail.cwd {
        println!("  cwd: {cwd}");
    }
    if !detail.env.is_empty() {
        println!("  env: {}", env_summary(&detail.env));
    }
    println!("  argv: {}", detail.command.join(" "));
}

fn launch_var_text(var: &LaunchVarMetadata) -> String {
    let mut parts = Vec::new();
    match (var.var_type, &var.values) {
        ("enum", Some(values)) => parts.push(format!("enum: {}", values.join(", "))),
        (var_type, _) => parts.push(var_type.to_string()),
    }
    if var.required {
        parts.push("required".into());
    } else if let Some(default) = &var.default {
        parts.push(format!("default: {}", default.rendered()));
    }
    parts.join(", ")
}

fn launch_var_description(var: &LaunchVarMetadata) -> String {
    var.description
        .as_deref()
        .map(|description| format!(" - {description}"))
        .unwrap_or_default()
}

fn env_summary(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
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
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for key in supplied.keys() {
        if !launch.vars.contains_key(key) {
            anyhow::bail!("unknown launch variable {key:?}");
        }
    }
    for (name, var) in &launch.vars {
        if let Some(value) = supplied.get(name) {
            resolved.insert(name.clone(), validate_launch_var_value(name, var, value)?);
        } else if let Some(default) = var.default_rendered() {
            resolved.insert(name.clone(), default);
        } else if var.required {
            anyhow::bail!("missing required launch variable {name:?}");
        }
    }
    tracing::debug!(
        launch = launch_name,
        vars = resolved.len(),
        "resolved launch variables"
    );
    Ok(resolved)
}

fn validate_launch_var_value(
    name: &str,
    var: &LaunchVarConfig,
    value: &SuppliedLaunchVarValue,
) -> anyhow::Result<String> {
    match var.var_type {
        LaunchVarType::String => match value {
            SuppliedLaunchVarValue::String(value) => Ok(value.to_string()),
            _ => anyhow::bail!("invalid string launch variable {name:?}; expected string"),
        },
        LaunchVarType::Enum => {
            let SuppliedLaunchVarValue::String(value) = value else {
                anyhow::bail!("invalid enum launch variable {name:?}; expected string");
            };
            let values = var.values.as_deref().unwrap_or(&[]);
            if values.iter().any(|allowed| allowed == value) {
                Ok(value.to_string())
            } else {
                anyhow::bail!(
                    "invalid enum launch variable {name:?}; expected one of {}",
                    values.join(", ")
                );
            }
        }
        LaunchVarType::Boolean => match value {
            SuppliedLaunchVarValue::Boolean(value) => Ok(value.to_string()),
            SuppliedLaunchVarValue::String(value) if value == "true" || value == "false" => {
                Ok(value.to_string())
            }
            _ => anyhow::bail!("invalid boolean launch variable {name:?}; expected true or false"),
        },
        LaunchVarType::Number => match value {
            SuppliedLaunchVarValue::Integer(value) => Ok(value.to_string()),
            SuppliedLaunchVarValue::Float(value) => {
                if !value.is_finite() {
                    anyhow::bail!(
                        "invalid number launch variable {name:?}; expected finite number"
                    );
                }
                Ok(canonical_cli_number(&value.to_string(), *value))
            }
            SuppliedLaunchVarValue::String(value) => {
                let parsed = value
                    .parse::<f64>()
                    .with_context(|| format!("invalid number launch variable {name:?}"))?;
                if !parsed.is_finite() {
                    anyhow::bail!(
                        "invalid number launch variable {name:?}; expected finite number"
                    );
                }
                Ok(canonical_cli_number(value, parsed))
            }
            SuppliedLaunchVarValue::Boolean(_) => {
                anyhow::bail!("invalid number launch variable {name:?}")
            }
        },
    }
}

fn canonical_cli_number(raw: &str, parsed: f64) -> String {
    if raw.parse::<i64>().is_ok() {
        raw.trim_start_matches('+').to_string()
    } else {
        let text = parsed.to_string();
        text.strip_suffix(".0").unwrap_or(&text).to_string()
    }
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
    let cwd = render_launch_cwd(cwd, vars, runtime.user.home.as_path())?;
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
    let cwd = render_launch_cwd(step.cwd.as_deref(), vars, runtime.container_home.as_path())?;
    let exec_spec = ContainerExecSpec {
        stdin_tty: false,
        stdout_tty: false,
        user: runtime.exec_identity(),
        cwd,
        env,
        container_name: runtime.container_name.clone(),
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

fn status_launch(session_id: Option<&str>, sessions: &[model::SessionStatus]) -> Option<String> {
    match session_id {
        Some(session_id) => sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.launch.clone()),
        None => sessions.iter().find_map(|session| session.launch.clone()),
    }
}

fn status_all_entries(
    cfg: &GatewayConfig,
    containers: Vec<ManagedContainer>,
) -> Vec<AllStatusEntry> {
    containers
        .into_iter()
        .map(|container| status_all_entry(cfg, container))
        .collect()
}

fn status_all_entry(cfg: &GatewayConfig, container: ManagedContainer) -> AllStatusEntry {
    let target = container
        .labels
        .get("io.aw-gateway.target")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let session_id = container.labels.get("io.aw-gateway.session_id").cloned();
    let mode = container
        .labels
        .get("io.aw-gateway.mode")
        .cloned()
        .unwrap_or_else(|| {
            infer_status_all_mode(cfg, &target, &container.name, session_id.as_deref())
        });
    // Launch labels are only persisted on ephemeral session containers. Fixed
    // targets can be reused across launches, so their live provenance comes
    // from per-session markers in `status <target>`.
    let launch = (mode == "ephemeral")
        .then(|| container.labels.get("io.aw-gateway.launch").cloned())
        .flatten();
    let user = container
        .labels
        .get("io.aw-gateway.user")
        .cloned()
        .unwrap_or_default();
    let uid = container
        .labels
        .get("io.aw-gateway.uid")
        .cloned()
        .unwrap_or_default();
    let image = container
        .labels
        .get("io.aw-gateway.image")
        .cloned()
        .unwrap_or(container.image);
    let container_name = container
        .labels
        .get("io.aw-gateway.container_id")
        .cloned()
        .unwrap_or(container.name);
    let status = if container.running {
        "running"
    } else {
        "stopped"
    }
    .to_string();
    AllStatusEntry {
        target,
        session_id,
        launch,
        mode,
        user,
        uid,
        image,
        container: container_name,
        status,
    }
}

fn infer_status_all_mode(
    cfg: &GatewayConfig,
    target: &str,
    container_name: &str,
    session_id: Option<&str>,
) -> String {
    let Ok(target_cfg) = cfg.effective_target(target) else {
        return "unknown".into();
    };
    match target_cfg.mode {
        TargetMode::Fixed => match target_cfg.container_name(None) {
            Ok(expected) if expected == container_name => "fixed".into(),
            _ => "unknown".into(),
        },
        TargetMode::Ephemeral if session_id.is_some() => "ephemeral".into(),
        TargetMode::Ephemeral => "unknown".into(),
    }
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
    target_name: String,
    target: TargetConfig,
    launch_name: Option<String>,
    session_id: Option<String>,
    user: UserContext,
    bootstrap_user: String,
    session_uid: u32,
    session_gid: u32,
    session_shell: String,
    container_user: String,
    container_home: PathBuf,
    workspace: PathBuf,
    container_state_dir: PathBuf,
    container_state_dir_in_container: PathBuf,
    control_sockets: ControlSocketPaths,
    container_name: String,
    container_runtime: ContainerRuntime,
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

impl Runtime {
    async fn load(
        config_path: Option<PathBuf>,
        target: Option<&str>,
        session_id: Option<String>,
        generate_session_id: bool,
    ) -> anyhow::Result<Runtime> {
        let cfg = load_config(config_path)?;
        Self::from_config(cfg, target, session_id, generate_session_id, None).await
    }

    async fn from_config(
        cfg: GatewayConfig,
        target: Option<&str>,
        session_id: Option<String>,
        generate_session_id: bool,
        launch_name: Option<String>,
    ) -> anyhow::Result<Runtime> {
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
                    anyhow::bail!("--session-id is only valid for ephemeral targets");
                }
                None
            }
            TargetMode::Ephemeral => match session_id {
                Some(value) => {
                    validate_session_id(&value)?;
                    Some(value)
                }
                None if generate_session_id => Some(generate_session_id_value()?),
                None => anyhow::bail!("ephemeral target {target_name:?} requires --session-id"),
            },
        };
        let container_name = target_cfg.container_name(session_id.as_deref())?;
        let workspace =
            resolve_target_workspace(&target_cfg, &target_name, &user, session_id.as_deref())?;
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home)?;
        let default_container_user = target_cfg.container_user.clone().unwrap_or_else(|| {
            if container_runtime.kind() == ContainerRuntimeType::Podman {
                user.user.clone()
            } else {
                "root".into()
            }
        });
        let default_container_home = target_cfg.container_home.clone().unwrap_or_else(|| {
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
        let identity = target_cfg.identity.as_ref();
        let bootstrap_user = identity
            .and_then(|identity| identity.bootstrap_user.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .unwrap_or_else(|| "root".into());
        let container_user = identity
            .and_then(|identity| identity.session_user.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .unwrap_or(default_container_user);
        validate_name("target identity session_user", &container_user)?;
        let session_uid = identity
            .and_then(|identity| identity.session_uid.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .map(|value| value.parse::<u32>())
            .transpose()
            .context("parse target identity session_uid")?
            .unwrap_or(user.uid);
        let session_gid = identity
            .and_then(|identity| identity.session_gid.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .map(|value| value.parse::<u32>())
            .transpose()
            .context("parse target identity session_gid")?
            .unwrap_or(user.gid);
        let container_home = identity
            .and_then(|identity| identity.session_home.as_deref())
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
        let session_shell = identity
            .and_then(|identity| identity.session_shell.as_deref())
            .map(|value| template::render(value, &identity_vars))
            .transpose()?
            .unwrap_or_else(|| "/bin/bash".into());
        validate_passwd_scalar("target identity session_shell", &session_shell)?;
        let (state_kind, state_id) = match target_cfg.mode {
            TargetMode::Fixed => ("containers", container_name.as_str()),
            TargetMode::Ephemeral => (
                "sessions",
                session_id
                    .as_deref()
                    .expect("ephemeral target has a session id"),
            ),
        };
        let container_state_dir = workspace
            .join(&target_cfg.workspace.state_dir)
            .join(state_kind)
            .join(state_id);
        let container_state_dir_in_container = resolve_container_path(
            &container_home,
            &target_cfg.workspace.state_dir,
            [state_kind, state_id],
        );
        let runtime_id = match target_cfg.mode {
            TargetMode::Fixed => target_name.clone(),
            TargetMode::Ephemeral => session_id
                .clone()
                .expect("ephemeral target has a session id"),
        };
        let control_sockets = render_control_socket_paths(
            &target_cfg.control_sockets,
            &target_cfg,
            &target_name,
            &container_name,
            session_id.as_deref(),
            &runtime_id,
            &user,
        )?;
        let runtime = Runtime {
            cfg,
            target_name,
            target: target_cfg,
            launch_name,
            session_id,
            user,
            bootstrap_user,
            session_uid,
            session_gid,
            session_shell,
            container_user,
            container_home,
            workspace,
            container_state_dir,
            container_state_dir_in_container,
            control_sockets,
            container_name,
            container_runtime,
        };
        runtime.validate_workspace_cleanup_path().await?;
        runtime.validate_unix_socket_paths()?;
        Ok(runtime)
    }

    async fn ensure_ready(&self) -> anyhow::Result<ReadyStatus> {
        let _lock = self.acquire_lifecycle_lock().await?;
        let mut started_container = false;
        let mut attempted_container_start = false;
        let result = async {
            paths::ensure_private_dir(&self.container_state_dir)?;
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
            let mut inspect = self.container_runtime.inspect(&self.container_name).await?;
            match readiness_plan(inspect.as_ref()) {
                ContainerReadinessPlan::ReuseRunning => {
                    let existing = inspect
                        .as_ref()
                        .expect("reuse plan requires existing inspect");
                    self.validate_labels(existing)?;
                }
                ContainerReadinessPlan::StartStopped => {
                    let existing = inspect
                        .as_ref()
                        .expect("start plan requires existing inspect");
                    self.validate_labels(existing)?;
                    self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                        .await?;
                    self.remove_stale_control_socket_files()?;
                    attempted_container_start = true;
                    self.container_runtime.start(&self.container_name).await?;
                    started_container = true;
                    inspect = self.container_runtime.inspect(&self.container_name).await?;
                }
                ContainerReadinessPlan::CreateMissing => {
                    self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                        .await?;
                    self.remove_stale_control_socket_files()?;
                    attempted_container_start = true;
                    self.start_container().await?;
                    started_container = true;
                    inspect = self.container_runtime.inspect(&self.container_name).await?;
                }
            }
            let inspect =
                inspect.ok_or_else(|| anyhow::anyhow!("container did not exist after start"))?;
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
                if started_container || attempted_container_start {
                    self.cleanup_failed_start().await;
                    self.cleanup_control_socket_dir();
                }
                return Err(err);
            }
        };
        Ok(ReadyStatus {
            target: self.target_name.clone(),
            session_id: self.session_id.clone(),
            mode: format!("{:?}", self.target.mode).to_lowercase(),
            user: self.user.user.clone(),
            image: self.target.image.clone(),
            container: self.container_name.clone(),
            container_pid: inspect.state.pid,
            ssh_socket: self.ssh_socket(),
            ssh_tcp: self.published_ssh_endpoint().await?,
            status: status.status,
            local_ssh: None,
            client_config: None,
        })
    }

    async fn status(&self) -> anyhow::Result<GatewayStatus> {
        let inspect = self.container_runtime.inspect(&self.container_name).await?;
        let agent = if self.agent_control_enabled() {
            self.agent_status().await.ok()
        } else {
            None
        };
        let sessions = self.active_session_markers()?;
        let launch = status_launch(self.session_id.as_deref(), &sessions);
        let agent_ready = agent
            .as_ref()
            .and_then(|value| value.get("ready"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(GatewayStatus {
            target: self.target_name.clone(),
            session_id: self.session_id.clone(),
            launch,
            mode: format!("{:?}", self.target.mode).to_lowercase(),
            user: self.user.user.clone(),
            image: self.target.image.clone(),
            container: inspect.as_ref().map(|_| self.container_name.clone()),
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
            agent,
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
                container = self.container_name,
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
                    target = %self.target_name,
                    workspace = %self.workspace.display(),
                    "workspace cleanup skipped because active sessions remain"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    target = %self.target_name,
                    workspace = %self.workspace.display(),
                    error = %err,
                    "workspace cleanup skipped because active sessions could not be checked"
                );
                return;
            }
        }
        if let Err(err) = self.remove_session_workspace().await {
            tracing::warn!(
                target = %self.target_name,
                workspace = %self.workspace.display(),
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
        let Some(inspect) = self.container_runtime.inspect(&self.container_name).await? else {
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
                self.container_runtime.stop(&self.container_name).await?;
            }
        } else {
            self.container_runtime.stop(&self.container_name).await?;
        }
        if self.target.remove_on_stop
            && let Some(current) = self.container_runtime.inspect(&self.container_name).await?
        {
            self.validate_labels(&current)?;
            self.container_runtime.rm(&self.container_name).await?;
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
            let Some(inspect) = self.container_runtime.inspect(&self.container_name).await? else {
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
                .exec_quiet(&self.container_name, ["pgrep", "-x", process.as_str()])
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
        if let Some(session_id) = &self.session_id {
            labels.insert("io.aw-gateway.session_id".into(), session_id.clone());
        }
        if self.target.mode == TargetMode::Ephemeral
            && let Some(launch_name) = &self.launch_name
        {
            labels.insert("io.aw-gateway.launch".into(), launch_name.clone());
        }
        labels
    }

    fn validation_labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("io.aw-gateway.gateway".into(), "true".into()),
            ("io.aw-gateway.user".into(), self.user.user.clone()),
            ("io.aw-gateway.uid".into(), self.user.uid.to_string()),
            ("io.aw-gateway.target".into(), self.target_name.clone()),
            (
                "io.aw-gateway.container_id".into(),
                self.container_name.clone(),
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
            env.insert("AW_AUTHENTICATED_UID".into(), self.user.uid.to_string());
            env.insert("AW_AUTHENTICATED_GID".into(), self.user.gid.to_string());
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
            name: self.container_name.clone(),
            hostname: self.container_name.clone(),
            image: self.target.image.clone(),
            workspace: self.workspace.clone(),
            container_home: self.container_home.clone(),
            container_user: if self.target.container_bootstrap.enabled {
                self.bootstrap_identity()
            } else {
                self.container_user.clone()
            },
            passwd_entry: self
                .container_runtime
                .is_podman()
                .then(|| self.passwd_entry()),
            state_dir_in_container: self.container_state_dir_in_container.clone(),
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
                && status
                    .get("ready")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
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

    async fn agent_status(&self) -> anyhow::Result<serde_json::Value> {
        let response = self
            .agent_request(serde_json::json!({"id":"status","method":"status"}))
            .await?;
        if !response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            anyhow::bail!("agent status failed: {response}");
        }
        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    async fn agent_shutdown(&self) -> anyhow::Result<()> {
        let token = self.control_token()?;
        let _ = self
            .agent_request(serde_json::json!({
                "id": "shutdown",
                "method": "shutdown",
                "params": {
                    "reason": "gateway-stop",
                    "token": token,
                },
            }))
            .await?;
        Ok(())
    }

    async fn agent_request(&self, request: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.validate_agent_socket().await?;
            let mut stream = UnixStream::connect(self.agent_socket()).await?;
            let mut payload = serde_json::to_vec(&request)?;
            payload.push(b'\n');
            stream.write_all(&payload).await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            Ok(serde_json::from_str(&line)?)
        })
        .await
        .context("timed out waiting for container agent control response")?
    }

    async fn agent_session_hold(&self, kind: &str) -> anyhow::Result<Option<AgentSessionHold>> {
        if !self.uses_agent_idle_cleanup() {
            return Ok(None);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            self.validate_agent_socket().await?;
            let mut stream = UnixStream::connect(self.agent_socket()).await?;
            let token = self.control_token()?;
            let request = serde_json::json!({
                "id": "session_hold",
                "method": "session_hold",
                "params": {
                    "token": token,
                    "kind": kind,
                },
            });
            let mut payload = serde_json::to_vec(&request)?;
            payload.push(b'\n');
            stream.write_all(&payload).await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let response: serde_json::Value = serde_json::from_str(&line)?;
            if !response
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                anyhow::bail!("agent session hold failed: {response}");
            }
            Ok(Some(AgentSessionHold { _reader: reader }))
        })
        .await
        .context("timed out opening container agent session hold")?
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
        vars.insert("user".into(), self.user.user.clone());
        vars.insert("uid".into(), self.session_uid.to_string());
        vars.insert("gid".into(), self.session_gid.to_string());
        vars.insert("home".into(), self.user.home.display().to_string());
        vars.insert("container_user".into(), self.container_user.clone());
        vars.insert(
            "container_home".into(),
            self.container_home.display().to_string(),
        );
        vars.insert("workspace".into(), self.workspace.display().to_string());
        vars.insert(
            "state".into(),
            self.workspace
                .join(&self.target.workspace.state_dir)
                .display()
                .to_string(),
        );
        vars.insert(
            "state_dir".into(),
            self.user.state_dir().display().to_string(),
        );
        vars.insert("target".into(), self.target_name.clone());
        if let Some(session_id) = &self.session_id {
            vars.insert("session_id".into(), session_id.clone());
        }
        vars.insert("image".into(), self.target.image.clone());
        vars.insert(
            "image_slug".into(),
            template::image_slug(&self.target.image),
        );
        vars.insert("container_name".into(), self.container_name.clone());
        vars.insert(
            "container_state_dir".into(),
            self.container_state_dir.display().to_string(),
        );
        vars.insert(
            "container_state_dir_in_container".into(),
            self.container_state_dir_in_container.display().to_string(),
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
                self.control_sockets
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
                self.control_sockets
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
                session_user: self.container_user.clone(),
                session_uid: self.session_uid,
                session_gid: self.session_gid,
                session_home: self.container_home.display().to_string(),
                session_shell: self.session_shell.clone(),
                state_dir: self.container_state_dir_in_container.display().to_string(),
            },
            steps,
        };
        let path = self.container_bootstrap_config_host();
        atomic_write_toml(&path, &cfg, AtomicWritePolicy::fixed_no_fsync(0o600))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn container_agent_config_host(&self) -> PathBuf {
        self.container_state_dir.join("container-agent.toml")
    }

    fn container_agent_config_in_container(&self) -> PathBuf {
        self.container_state_dir_in_container
            .join("container-agent.toml")
    }

    fn container_bootstrap_config_host(&self) -> PathBuf {
        self.container_state_dir.join("container-bootstrap.toml")
    }

    fn container_bootstrap_config_in_container(&self) -> PathBuf {
        self.container_state_dir_in_container
            .join("container-bootstrap.toml")
    }

    fn sshd_session_env_config_host(&self) -> PathBuf {
        self.container_state_dir.join("sshd-session-env.conf")
    }

    fn sshd_session_env_config_in_container(&self) -> PathBuf {
        self.container_state_dir_in_container
            .join("sshd-session-env.conf")
    }

    fn ssh_command_filter_policy_host(&self) -> PathBuf {
        self.container_state_dir.join("ssh-command-filter.toml")
    }

    fn ssh_command_filter_policy_in_container(&self) -> PathBuf {
        self.container_state_dir_in_container
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
        if self.control_sockets.default_host_dir {
            let run_user_dir = PathBuf::from(format!("/run/user/{}", self.user.uid));
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
        ensure_control_socket_dir(&self.control_sockets.host_dir)?;
        Ok(())
    }

    fn remove_stale_control_socket_files(&self) -> anyhow::Result<()> {
        for socket in [
            &self.control_sockets.host_agent_socket,
            &self.control_sockets.host_ssh_socket,
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
            self.container_user,
            self.session_uid,
            self.session_gid,
            self.container_user,
            self.container_home.display(),
            self.session_shell,
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
            source: self.control_sockets.host_dir.clone(),
            target: self.control_sockets.container_dir.clone(),
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
            format!("{}:{}", self.session_uid, self.session_gid)
        } else {
            self.container_user.clone()
        }
    }

    fn bootstrap_identity(&self) -> String {
        if self.container_runtime.is_podman() {
            "0:0".into()
        } else {
            self.bootstrap_user.clone()
        }
    }

    fn session_env(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::from([
            ("SHELL".into(), "/usr/bin/bash".to_string()),
            (
                "PATH".into(),
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
            ),
        ]);
        env.extend(self.render_env_map(&self.target.session_env)?);
        Ok(env)
    }

    fn agent_socket(&self) -> PathBuf {
        self.control_sockets.host_agent_socket.clone()
    }

    fn ssh_socket(&self) -> PathBuf {
        self.control_sockets.host_ssh_socket.clone()
    }

    fn container_agent_socket(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.agent_control_enabled() {
            return Ok(None);
        }
        Ok(Some(self.control_sockets.container_agent_socket.clone()))
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
        Ok(Some(self.control_sockets.container_ssh_socket.clone()))
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
                    self.target_name
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
                self.target_name,
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
            .published_port(&self.container_name, 22)
            .await?
            .map(|port| TcpEndpoint {
                host: "127.0.0.1".into(),
                port,
            }))
    }

    async fn validate_agent_socket(&self) -> anyhow::Result<()> {
        runtime::socket_is_safe_for(&self.agent_socket(), self.user.uid, self.user.gid)
    }

    async fn validate_ssh_socket(&self) -> anyhow::Result<()> {
        self.validate_socket_path(&self.ssh_socket()).await
    }

    async fn validate_socket_path(&self, socket: &Path) -> anyhow::Result<()> {
        runtime::socket_is_safe_for(socket, self.user.uid, self.user.gid)?;
        let _ = UnixStream::connect(socket)
            .await
            .with_context(|| format!("test-connect {}", socket.display()))?;
        Ok(())
    }

    async fn cleanup_failed_start(&self) {
        match self.container_runtime.inspect(&self.container_name).await {
            Ok(Some(inspect)) => {
                if let Err(err) = self.validate_labels(&inspect) {
                    tracing::warn!(
                        container = self.container_name,
                        error = %err,
                        "not cleaning failed start because labels did not match"
                    );
                    return;
                }
                if let Err(err) = self.container_runtime.stop(&self.container_name).await {
                    tracing::warn!(
                        container = self.container_name,
                        error = %err,
                        "failed to stop container after startup failure"
                    );
                }
                if self.target.remove_on_stop
                    && let Err(err) = self.container_runtime.rm(&self.container_name).await
                {
                    tracing::warn!(
                        container = self.container_name,
                        error = %err,
                        "failed to remove container after startup failure"
                    );
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    container = self.container_name,
                    error = %err,
                    "failed to inspect container after startup failure"
                );
            }
        }
    }

    fn cleanup_control_socket_dir(&self) {
        match std::fs::symlink_metadata(&self.control_sockets.host_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tracing::warn!(
                    path = %self.control_sockets.host_dir.display(),
                    "not removing symlink control socket runtime directory"
                );
                return;
            }
            Ok(metadata) if !metadata.is_dir() => {
                tracing::warn!(
                    path = %self.control_sockets.host_dir.display(),
                    "not removing non-directory control socket runtime path"
                );
                return;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::warn!(
                    path = %self.control_sockets.host_dir.display(),
                    error = %err,
                    "failed to inspect control socket runtime directory before cleanup"
                );
                return;
            }
        }

        for socket in [
            &self.control_sockets.host_agent_socket,
            &self.control_sockets.host_ssh_socket,
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

        match std::fs::remove_dir(&self.control_sockets.host_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    path = %self.control_sockets.host_dir.display(),
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
        let session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("workspace_cleanup requires an ephemeral session id"))?;
        validate_workspace_cleanup_path(
            &self.workspace,
            &self.user.home,
            session_id,
            Some(self.target.workspace.path.as_str()),
        )?;
        match tokio::fs::symlink_metadata(&self.workspace).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "workspace_cleanup path {} must not be a symlink",
                    self.workspace.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect workspace cleanup path {}",
                        self.workspace.display()
                    )
                });
            }
        }
        Ok(())
    }

    async fn remove_session_workspace(&self) -> anyhow::Result<()> {
        self.validate_workspace_cleanup_path().await?;
        let metadata = match tokio::fs::symlink_metadata(&self.workspace).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect workspace cleanup path {}",
                        self.workspace.display()
                    )
                });
            }
        };
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "workspace cleanup path {} must not be a symlink",
                self.workspace.display()
            );
        }
        if !metadata.is_dir() {
            anyhow::bail!(
                "workspace cleanup path {} exists but is not a directory",
                self.workspace.display()
            );
        }
        self.container_runtime
            .remove_host_dir_all(&self.workspace)
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
mod tests {
    use super::*;

    fn write_fake_runtime(path: &Path, script: &str) {
        std::fs::write(path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[cfg(unix)]
    fn assert_file_mode(path: &Path, expected: u32) {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, expected, "unexpected mode for {}", path.display());
    }

    #[cfg(not(unix))]
    fn assert_file_mode(_path: &Path, _expected: u32) {}

    fn fake_running_runtime_script(exit_code: i32) -> String {
        let user = UserContext::current().unwrap();
        format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    exit {exit_code}
    ;;
esac
exit 0
"#,
            user = user.user,
            uid = user.uid,
        )
    }

    fn fake_background_runtime_script(log: &Path) -> String {
        let user = UserContext::current().unwrap();
        format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    echo started > "{log}"
    sleep 0.2
    echo done >> "{log}"
    ;;
esac
exit 0
"#,
            user = user.user,
            uid = user.uid,
            log = log.display()
        )
    }

    fn session_marker_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry.path().extension().and_then(|value| value.to_str()) == Some("json")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    async fn wait_for_background_marker_clear(log: &Path, marker_dir: &Path, panic_message: &str) {
        for _ in 0..20 {
            let done = std::fs::read_to_string(log)
                .map(|value| value.contains("done"))
                .unwrap_or(false);
            if done && session_marker_count(marker_dir) == 0 {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("{panic_message}");
    }

    fn test_control_socket_paths(base: &Path) -> ControlSocketPaths {
        let host_dir = base.join("runtime-sockets");
        let container_dir = PathBuf::from("/run/aw-gateway");
        ControlSocketPaths {
            host_agent_socket: host_dir.join("agent.sock"),
            host_ssh_socket: host_dir.join("ssh.sock"),
            container_agent_socket: container_dir.join("agent.sock"),
            container_ssh_socket: container_dir.join("ssh.sock"),
            host_dir,
            container_dir,
            default_host_dir: false,
        }
    }

    fn disable_default_container_agent(cfg: &mut GatewayConfig) {
        cfg.target_defaults.container_agent = Some(crate::config::ContainerAgentConfigInput {
            enabled: Some(false),
            services: Vec::new(),
            ssh_bridge: None,
            control_socket: None,
            idle_cleanup: None,
        });
    }

    fn enable_default_ssh_bridge(cfg: &mut GatewayConfig) {
        cfg.target_defaults.container_agent = Some(crate::config::ContainerAgentConfigInput {
            enabled: Some(true),
            services: Vec::new(),
            ssh_bridge: Some(crate::config::SshBridgeConfigInput {
                enabled: Some(true),
                socket: None,
                target: Some("127.0.0.1:22".into()),
                mode: Some("0600".into()),
            }),
            control_socket: None,
            idle_cleanup: None,
        });
    }

    fn test_runtime(
        dir: &tempfile::TempDir,
        program: PathBuf,
        configure: impl FnOnce(&mut GatewayConfig),
    ) -> Runtime {
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.runtime.program = Some(program.display().to_string());
        disable_default_container_agent(&mut cfg);
        configure(&mut cfg);
        cfg.validate().unwrap();

        let target = cfg.effective_target("default").unwrap();
        let user = UserContext::current().unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home).unwrap();
        Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: Some("agent-pack-codex".into()),
            session_id: None,
            bootstrap_user: "root".into(),
            session_uid: user.uid,
            session_gid: user.gid,
            session_shell: "/bin/bash".into(),
            container_user: user.user.clone(),
            container_home: user.home.clone(),
            workspace: dir.path().join("workspace"),
            container_state_dir: dir
                .path()
                .join("workspace/.aw-gateway/containers/ubuntu-dev"),
            container_state_dir_in_container: user.home.join(".aw-gateway/containers/ubuntu-dev"),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
            user,
        }
    }

    fn launch_test_config(
        dir: &tempfile::TempDir,
        fake_runtime: &Path,
        launch_name: &str,
        command: Vec<String>,
    ) -> GatewayConfig {
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.runtime.program = Some(fake_runtime.display().to_string());
        disable_default_container_agent(&mut cfg);
        cfg.target_defaults.host_steps.clear();
        cfg.target_defaults.workspace = Some(crate::config::WorkspaceConfigInput {
            path: Some(dir.path().join("workspace").display().to_string()),
            state_dir: Some(".aw-gateway".into()),
            cleanup: None,
        });
        cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
        cfg.launches.insert(
            launch_name.into(),
            crate::config::LaunchConfigInput {
                target: Some("default".into()),
                command: Some(command),
                ..Default::default()
            },
        );
        cfg.validate().unwrap();
        cfg
    }

    fn configure_workspace_cleanup_runtime(
        runtime: &mut Runtime,
        cleanup: WorkspaceCleanup,
        workspace: PathBuf,
        home: PathBuf,
        session_id: &str,
    ) {
        runtime.target.mode = TargetMode::Ephemeral;
        runtime.target.ephemeral_name = Some("ubuntu-dev-{session_id}".into());
        runtime.target.stop_when_idle = true;
        runtime.target.workspace.path =
            "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}".into();
        runtime.target.workspace.cleanup = cleanup;
        runtime.session_id = Some(session_id.into());
        runtime.workspace = workspace;
        runtime.container_state_dir = runtime
            .workspace
            .join(&runtime.target.workspace.state_dir)
            .join("sessions")
            .join(session_id);
        runtime.user.home = home;
    }

    #[test]
    fn workspace_cleanup_policy_matches_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});

        runtime.target.workspace.cleanup = WorkspaceCleanup::Never;
        assert!(!runtime.should_cleanup_workspace(SessionOutcome::Success));
        assert!(!runtime.should_cleanup_workspace(SessionOutcome::Failure));

        runtime.target.workspace.cleanup = WorkspaceCleanup::Success;
        assert!(runtime.should_cleanup_workspace(SessionOutcome::Success));
        assert!(!runtime.should_cleanup_workspace(SessionOutcome::Failure));

        runtime.target.workspace.cleanup = WorkspaceCleanup::Always;
        assert!(runtime.should_cleanup_workspace(SessionOutcome::Success));
        assert!(runtime.should_cleanup_workspace(SessionOutcome::Failure));
    }

    #[test]
    fn session_outcome_maps_exit_code_results() {
        assert_eq!(
            SessionOutcome::from_exit_code_result(&Ok(0)),
            SessionOutcome::Success
        );
        assert_eq!(
            SessionOutcome::from_exit_code_result(&Ok(7)),
            SessionOutcome::Failure
        );
        assert_eq!(
            SessionOutcome::from_exit_code_result(&Err(anyhow::anyhow!("setup failed"))),
            SessionOutcome::Failure
        );
    }

    #[test]
    fn workspace_cleanup_path_allows_missing_session_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456");

        validate_workspace_cleanup_path(
            &workspace,
            dir.path(),
            session_id,
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap();
    }

    #[test]
    fn workspace_cleanup_path_allows_three_character_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join(".cache/aw-gateway/workspaces/default-abc");

        validate_workspace_cleanup_path(
            &workspace,
            dir.path(),
            "abc",
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap();

        let err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                &workspace,
                dir.path(),
                "ab",
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );
        assert!(err.contains("must be at least 3 characters"), "{err}");
    }

    #[test]
    fn workspace_cleanup_path_rejects_unsafe_roots() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";

        let root_err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                Path::new("/"),
                dir.path(),
                session_id,
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );
        assert!(root_err.contains("refuses to delete /"), "{root_err}");

        let home_err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                dir.path(),
                dir.path(),
                session_id,
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );
        assert!(
            home_err.contains("refuses to delete user home directory"),
            "{home_err}"
        );
    }

    #[test]
    fn workspace_cleanup_path_rejects_paths_without_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join(".cache/aw-gateway/workspaces/default");

        let err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                &workspace,
                dir.path(),
                "abc123def456",
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );

        assert!(err.contains("must contain session_id"), "{err}");
    }

    #[test]
    fn workspace_cleanup_path_rejects_empty_and_dot_components() {
        let dir = tempfile::tempdir().unwrap();

        let empty_err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                Path::new(""),
                dir.path(),
                "abc123def456",
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );
        assert!(
            empty_err.contains("resolved workspace must not be empty"),
            "{empty_err}"
        );

        let dot_err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                Path::new("/tmp/aw-gateway/../default-abc123def456"),
                dir.path(),
                "abc123def456",
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );
        assert!(
            dot_err.contains("must not contain '.' or '..' components"),
            "{dot_err}"
        );
    }

    #[test]
    fn workspace_cleanup_path_rejects_aw_gateway_template_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspaces/default-abc123def456");

        let err = format!(
            "{:#}",
            validate_workspace_cleanup_path(
                &workspace,
                dir.path(),
                "abc123def456",
                Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
            )
            .unwrap_err()
        );

        assert!(
            err.contains("outside the configured aw-gateway workspace root"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_cleanup_path_rejects_symlink_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let real_workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456-real");
        let symlink_workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456");
        std::fs::create_dir_all(&real_workspace).unwrap();
        symlink(&real_workspace, &symlink_workspace).unwrap();
        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            symlink_workspace,
            dir.path().into(),
            session_id,
        );

        let err = format!(
            "{:#}",
            runtime.validate_workspace_cleanup_path().await.unwrap_err()
        );

        assert!(err.contains("must not be a symlink"), "{err}");
        assert!(real_workspace.exists());
    }

    #[tokio::test]
    async fn remove_session_workspace_treats_missing_workspace_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456");
        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            workspace,
            dir.path().into(),
            session_id,
        );

        runtime.remove_session_workspace().await.unwrap();
    }

    #[tokio::test]
    async fn remove_session_workspace_removes_only_resolved_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace_root = dir.path().join(".cache/aw-gateway/workspaces");
        let workspace = workspace_root.join("default-abc123def456");
        let sibling = workspace_root.join("sibling-abc123def456");
        std::fs::create_dir_all(workspace.join("nested")).unwrap();
        std::fs::write(workspace.join("nested/file.txt"), "data").unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
            cfg.runtime.runtime_type = crate::config::ContainerRuntimeType::Docker;
        });
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            workspace.clone(),
            dir.path().into(),
            session_id,
        );

        runtime.remove_session_workspace().await.unwrap();

        assert!(!workspace.exists());
        assert!(workspace_root.exists());
        assert!(sibling.exists());
    }

    #[tokio::test]
    async fn remove_session_workspace_uses_podman_unshare_for_podman() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace_root = dir.path().join(".cache/aw-gateway/workspaces");
        let workspace = workspace_root.join("default-abc123def456");
        std::fs::create_dir_all(workspace.join("nested")).unwrap();
        std::fs::write(workspace.join("nested/file.txt"), "data").unwrap();
        let args_log = dir.path().join("podman-args.txt");
        let fake_podman = dir.path().join("podman");
        write_fake_runtime(
            &fake_podman,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > "{}"
if [ "$1" = "unshare" ]; then
  shift
  exec "$@"
fi
exit 1
"#,
                args_log.display()
            ),
        );

        let mut runtime = test_runtime(&dir, fake_podman, |cfg| {
            cfg.runtime.runtime_type = crate::config::ContainerRuntimeType::Podman;
        });
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            workspace.clone(),
            dir.path().into(),
            session_id,
        );

        runtime.remove_session_workspace().await.unwrap();

        assert!(!workspace.exists());
        let args = std::fs::read_to_string(args_log).unwrap();
        assert!(args.contains("unshare\nrm\n-rf\n--\n"), "{args}");
        assert!(args.contains(&workspace.display().to_string()), "{args}");
    }

    #[tokio::test]
    async fn remove_session_workspace_rejects_non_directory_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456");
        std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        std::fs::write(&workspace, "not a directory").unwrap();

        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            workspace,
            dir.path().into(),
            session_id,
        );

        let err = format!(
            "{:#}",
            runtime.remove_session_workspace().await.unwrap_err()
        );
        assert!(err.contains("exists but is not a directory"), "{err}");
    }

    #[tokio::test]
    async fn finish_post_session_removes_workspace_for_failure_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
            cfg.runtime.runtime_type = crate::config::ContainerRuntimeType::Docker;
        });
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            workspace.clone(),
            dir.path().into(),
            session_id,
        );
        runtime.target.idle_cleanup = None;
        let session = runtime.create_session_marker("test").unwrap();

        let result = runtime
            .finish_post_session::<()>(
                session,
                Err(anyhow::anyhow!("simulated readiness failure")),
                SessionOutcome::Failure,
            )
            .await;

        assert!(result.is_err());
        assert!(!workspace.exists());
    }

    #[tokio::test]
    async fn finish_post_session_preserves_success_when_workspace_cleanup_fails() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "abc123def456";
        let workspace = dir
            .path()
            .join(".cache/aw-gateway/workspaces/default-abc123def456");
        std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        std::fs::write(&workspace, "not a directory").unwrap();

        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        configure_workspace_cleanup_runtime(
            &mut runtime,
            WorkspaceCleanup::Always,
            workspace.clone(),
            dir.path().into(),
            session_id,
        );
        runtime.target.idle_cleanup = None;
        runtime.container_state_dir = dir.path().join("state");
        let session = runtime.create_session_marker("test").unwrap();

        let code = runtime
            .finish_post_session(session, Ok(0), SessionOutcome::Success)
            .await
            .unwrap();

        assert_eq!(code, 0);
        assert!(workspace.is_file());
    }

    fn inspect_with_running(running: bool) -> ContainerInspect {
        ContainerInspect {
            id: "id".into(),
            name: "container".into(),
            state: runtime::ContainerState { running, pid: 123 },
            config: runtime::ContainerConfig {
                labels: BTreeMap::new(),
            },
        }
    }

    fn managed_container(
        name: &str,
        image: &str,
        running: bool,
        labels: BTreeMap<String, String>,
    ) -> ManagedContainer {
        ManagedContainer {
            name: name.into(),
            image: image.into(),
            running,
            labels,
        }
    }

    fn managed_labels(target: &str, container: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("io.aw-gateway.gateway".into(), "true".into()),
            ("io.aw-gateway.user".into(), "alice".into()),
            ("io.aw-gateway.uid".into(), "2450".into()),
            ("io.aw-gateway.target".into(), target.into()),
            ("io.aw-gateway.image".into(), "ubuntu/dev".into()),
            ("io.aw-gateway.container_id".into(), container.into()),
        ])
    }

    #[test]
    fn status_all_entries_empty_when_runtime_has_no_matches() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();

        let entries = status_all_entries(&cfg, Vec::new());

        assert!(entries.is_empty());
    }

    #[test]
    fn status_all_entry_projects_fixed_container_from_labels() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let mut labels = managed_labels("default", "ubuntu-dev");
        labels.insert("io.aw-gateway.mode".into(), "fixed".into());

        let entries = status_all_entries(
            &cfg,
            vec![managed_container(
                "ubuntu-dev",
                "runtime-image",
                true,
                labels,
            )],
        );

        assert_eq!(
            entries,
            vec![AllStatusEntry {
                target: "default".into(),
                session_id: None,
                launch: None,
                mode: "fixed".into(),
                user: "alice".into(),
                uid: "2450".into(),
                image: "ubuntu/dev".into(),
                container: "ubuntu-dev".into(),
                status: "running".into(),
            }]
        );
    }

    #[test]
    fn status_all_entries_project_multiple_ephemeral_containers() {
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.targets.get_mut("default").unwrap().mode = Some(TargetMode::Ephemeral);
        let mut first = managed_labels("default", "ubuntu-dev-1a2b3c4d5e6f");
        first.insert("io.aw-gateway.image".into(), "scratch/dev".into());
        first.insert("io.aw-gateway.mode".into(), "ephemeral".into());
        first.insert("io.aw-gateway.session_id".into(), "1a2b3c4d5e6f".into());
        let mut second = managed_labels("default", "ubuntu-dev-0f1e2d3c4b5a");
        second.insert("io.aw-gateway.image".into(), "scratch/dev".into());
        second.insert("io.aw-gateway.mode".into(), "ephemeral".into());
        second.insert("io.aw-gateway.session_id".into(), "0f1e2d3c4b5a".into());

        let entries = status_all_entries(
            &cfg,
            vec![
                managed_container("ubuntu-dev-1a2b3c4d5e6f", "scratch/dev", false, first),
                managed_container("ubuntu-dev-0f1e2d3c4b5a", "scratch/dev", true, second),
            ],
        );

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id.as_deref(), Some("1a2b3c4d5e6f"));
        assert_eq!(entries[0].launch, None);
        assert_eq!(entries[0].status, "stopped");
        assert_eq!(entries[1].session_id.as_deref(), Some("0f1e2d3c4b5a"));
        assert_eq!(entries[1].status, "running");
    }

    #[test]
    fn status_all_entry_keeps_stale_labeled_container_without_config_match() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let labels = managed_labels("old-target", "aw-old");

        let entries = status_all_entries(
            &cfg,
            vec![managed_container("aw-old", "runtime/old", true, labels)],
        );

        assert_eq!(entries[0].target, "old-target");
        assert_eq!(entries[0].mode, "unknown");
        assert_eq!(entries[0].session_id, None);
        assert_eq!(entries[0].launch, None);
        assert_eq!(entries[0].status, "running");
    }

    #[test]
    fn status_all_entry_projects_ephemeral_launch_label() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let mut labels = managed_labels("default", "ubuntu-dev-1a2b3c4d5e6f");
        labels.insert("io.aw-gateway.mode".into(), "ephemeral".into());
        labels.insert("io.aw-gateway.session_id".into(), "1a2b3c4d5e6f".into());
        labels.insert("io.aw-gateway.launch".into(), "agent-pack-codex".into());

        let entries = status_all_entries(
            &cfg,
            vec![managed_container(
                "ubuntu-dev-1a2b3c4d5e6f",
                "runtime-image",
                true,
                labels,
            )],
        );

        assert_eq!(entries[0].launch.as_deref(), Some("agent-pack-codex"));
        let serialized = serde_json::to_string(&entries).unwrap();
        assert!(serialized.contains("agent-pack-codex"));
        assert!(!serialized.contains("repo"));
        assert!(!serialized.contains("pack_id"));
        assert!(!serialized.contains("AGENT_PACK_ID"));
    }

    #[test]
    fn status_all_entry_ignores_stale_fixed_launch_label() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let mut labels = managed_labels("default", "ubuntu-dev");
        labels.insert("io.aw-gateway.mode".into(), "fixed".into());
        labels.insert("io.aw-gateway.launch".into(), "stale-launch".into());

        let entries = status_all_entries(
            &cfg,
            vec![managed_container(
                "ubuntu-dev",
                "runtime-image",
                true,
                labels,
            )],
        );

        assert_eq!(entries[0].launch, None);
        assert!(
            !serde_json::to_string(&entries)
                .unwrap()
                .contains("stale-launch")
        );
    }

    #[test]
    fn runtime_labels_only_persist_launch_for_ephemeral_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});

        assert_eq!(runtime.target.mode, TargetMode::Fixed);
        assert!(!runtime.labels().contains_key("io.aw-gateway.launch"));

        runtime.target.mode = TargetMode::Ephemeral;
        runtime.session_id = Some("1a2b3c4d5e6f".into());

        assert_eq!(
            runtime
                .labels()
                .get("io.aw-gateway.launch")
                .map(String::as_str),
            Some("agent-pack-codex")
        );
    }

    #[test]
    fn status_launch_prefers_selected_session() {
        let sessions = vec![
            model::SessionStatus {
                id: "s1".into(),
                kind: "launch".into(),
                gateway_pid: 1,
                container: "ubuntu-dev".into(),
                target: "default".into(),
                launch: Some("first".into()),
                created_at_ms: 1,
            },
            model::SessionStatus {
                id: "s2".into(),
                kind: "launch".into(),
                gateway_pid: 1,
                container: "ubuntu-dev".into(),
                target: "default".into(),
                launch: Some("second".into()),
                created_at_ms: 2,
            },
        ];

        assert_eq!(
            status_launch(Some("s2"), &sessions).as_deref(),
            Some("second")
        );
        assert_eq!(status_launch(Some("missing"), &sessions), None);
        assert_eq!(status_launch(None, &sessions).as_deref(), Some("first"));
    }

    #[test]
    fn launch_env_precedence_is_session_then_launch_then_step() {
        let mut vars = Vars::new();
        vars.insert("var.step".into(), "step-rendered".into());
        let session_env = BTreeMap::from([
            ("KEEP".into(), "session".into()),
            ("OVERRIDE".into(), "session".into()),
            ("STEP".into(), "session".into()),
        ]);
        let launch_env = BTreeMap::from([("OVERRIDE".into(), "launch".into())]);
        let step_env = BTreeMap::from([("STEP".into(), "{var.step}".into())]);

        let container_env =
            launch_container_step_env(&session_env, &launch_env, &step_env, &vars).unwrap();
        assert_eq!(container_env["KEEP"], "session");
        assert_eq!(container_env["OVERRIDE"], "launch");
        assert_eq!(container_env["STEP"], "step-rendered");

        let final_env = launch_final_env(&session_env, &launch_env);
        assert_eq!(final_env["KEEP"], "session");
        assert_eq!(final_env["OVERRIDE"], "launch");
        assert_eq!(final_env["STEP"], "session");
    }

    #[tokio::test]
    async fn final_container_command_returns_runtime_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        write_fake_runtime(
            &fake_runtime,
            r#"#!/bin/sh
if [ "$1" = "exec" ]; then
  exit 37
fi
exit 0
"#,
        );
        let runtime = test_runtime(&dir, fake_runtime, |_| {});

        let outcome = exec_final_container_command(
            &runtime,
            vec!["/bin/launch-final".into()],
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ExecutionOutcome::new(37));
    }

    #[tokio::test]
    async fn wait_capture_final_container_command_returns_selected_output() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        write_fake_runtime(
            &fake_runtime,
            r#"#!/bin/sh
if [ "$1" = "exec" ]; then
  echo "captured stdout"
  echo "captured stderr" >&2
  exit 23
fi
exit 0
"#,
        );
        let runtime = test_runtime(&dir, fake_runtime, |_| {});

        let both = exec_final_container_command_with_options(
            &runtime,
            vec!["/bin/capture".into()],
            None,
            BTreeMap::new(),
            OperationExecutionOptions::WAIT,
        )
        .await
        .unwrap();

        assert_eq!(
            both,
            ExecutionOutcome::captured(
                23,
                Some(b"captured stdout\n".to_vec()),
                Some(b"captured stderr\n".to_vec()),
            )
        );

        let stdout_only = exec_final_container_command_with_options(
            &runtime,
            vec!["/bin/capture".into()],
            None,
            BTreeMap::new(),
            OperationExecutionOptions {
                mode: OperationMode::Wait,
                output: OutputSelection {
                    stdout: true,
                    stderr: false,
                },
            },
        )
        .await
        .unwrap();

        assert_eq!(
            stdout_only,
            ExecutionOutcome::captured(23, Some(b"captured stdout\n".to_vec()), None)
        );
    }

    #[test]
    fn detached_runner_uses_detach_mode_without_selected_output() {
        assert_eq!(
            detach_discard_options(),
            OperationExecutionOptions {
                mode: OperationMode::Detach,
                output: OutputSelection {
                    stdout: false,
                    stderr: false,
                },
            }
        );
    }

    #[tokio::test]
    async fn run_operation_core_returns_nonzero_exit_without_exiting_process() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(37));
        let runtime = test_runtime(&dir, fake_runtime, |cfg| {
            cfg.target_defaults.host_steps.clear();
            cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
        });

        let outcome = run_container_command_with_runtime(
            runtime,
            None,
            vec!["/bin/command-that-returns-37".into()],
            OperationExecutionOptions::STREAM,
        )
        .await
        .unwrap();

        assert_eq!(outcome, ExecutionOutcome::new(37));
    }

    #[tokio::test]
    async fn detached_run_keeps_session_marker_until_background_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        write_fake_runtime(&fake_runtime, &fake_background_runtime_script(&log));
        let runtime = test_runtime(&dir, fake_runtime, |cfg| {
            cfg.target_defaults.host_steps.clear();
            cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
        });
        let marker_dir = runtime.session_marker_dir();

        let outcome = run_container_command_with_runtime(
            runtime,
            None,
            vec!["/bin/background".into()],
            OperationExecutionOptions::DETACH,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ExecutionOutcome::Detached { .. }));
        assert_eq!(session_marker_count(&marker_dir), 1);

        wait_for_background_marker_clear(
            &log,
            &marker_dir,
            "detached background operation did not finish and clear marker",
        )
        .await;
    }

    #[tokio::test]
    async fn launch_operation_core_returns_nonzero_exit_without_exiting_process() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(42));
        let cfg = launch_test_config(
            &dir,
            &fake_runtime,
            "returns-nonzero",
            vec!["/bin/command-that-returns-42".into()],
        );

        let outcome = launch_execute_with_config(
            cfg,
            "returns-nonzero",
            None,
            SuppliedLaunchVars::default(),
            OperationExecutionOptions::STREAM,
        )
        .await
        .unwrap();

        assert_eq!(outcome, ExecutionOutcome::new(42));
    }

    #[tokio::test]
    async fn detached_launch_keeps_launch_marker_until_background_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        write_fake_runtime(&fake_runtime, &fake_background_runtime_script(&log));
        let cfg = launch_test_config(
            &dir,
            &fake_runtime,
            "detached-launch",
            vec!["/bin/background-launch".into()],
        );
        let marker_runtime = Runtime::from_config(
            cfg.clone(),
            Some("default"),
            None,
            true,
            Some("detached-launch".into()),
        )
        .await
        .unwrap();
        let marker_dir = marker_runtime.session_marker_dir();

        let outcome = launch_execute_with_config(
            cfg,
            "detached-launch",
            None,
            SuppliedLaunchVars::default(),
            OperationExecutionOptions::DETACH,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ExecutionOutcome::Detached { .. }));
        assert_eq!(session_marker_count(&marker_dir), 1);
        let sessions = marker_runtime.active_session_markers().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].kind, "launch");
        assert_eq!(sessions[0].launch.as_deref(), Some("detached-launch"));

        wait_for_background_marker_clear(
            &log,
            &marker_dir,
            "detached launch background operation did not finish and clear marker",
        )
        .await;
    }

    #[tokio::test]
    async fn gateway_idle_cleanup_runs_after_launch_session_marker_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        let user = UserContext::current().unwrap();
        write_fake_runtime(
            &fake_runtime,
            &format!(
                r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  stop)
    echo "stop $2" >> "{log}"
    ;;
esac
exit 0
"#,
                user = user.user,
                uid = user.uid,
                log = log.display()
            ),
        );
        let runtime = test_runtime(&dir, fake_runtime, |cfg| {
            let target = cfg.targets.get_mut("default").unwrap();
            target.stop_when_idle = Some(true);
            target.remove_on_stop = Some(false);
            target.idle_cleanup = Some(crate::config::IdleCleanupConfigInput {
                owner: Some(IdleCleanupOwner::Gateway),
                action: Some(IdleCleanupAction::ExitContainer),
                ..Default::default()
            });
        });
        std::fs::create_dir_all(&runtime.container_state_dir).unwrap();
        let session = runtime.create_launch_session_marker("launch").unwrap();

        runtime.apply_gateway_idle_cleanup().await.unwrap();
        assert!(!log.exists());

        drop(session);
        runtime.apply_gateway_idle_cleanup().await.unwrap();

        assert_eq!(std::fs::read_to_string(log).unwrap(), "stop ubuntu-dev\n");
    }

    #[test]
    fn launch_var_resolution_rejects_duplicates_and_normalizes_values() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
target = "default"
command = ["true", "{var.count}", "{var.debug}", "{var.mode}"]

[launches.agent.vars]
repo = { type = "string", required = true }
count = { type = "number", default = 1 }
debug = { type = "boolean", default = false }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }
"#,
        )
        .unwrap();
        let launch = cfg.effective_launch("agent").unwrap();
        let supplied = SuppliedLaunchVars::from_cli_pairs(vec![
            "repo=https://example.test/repo.git".into(),
            "count=2.0".into(),
            "debug=true".into(),
            "mode=safe".into(),
        ])
        .unwrap();
        let vars = resolve_launch_vars("agent", &launch, &supplied).unwrap();
        assert_eq!(vars["count"], "2");
        assert_eq!(vars["debug"], "true");
        assert_eq!(vars["mode"], "safe");

        let err = SuppliedLaunchVars::from_cli_pairs(vec!["repo=a".into(), "repo=b".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate launch variable"), "{err}");

        let unknown = SuppliedLaunchVars::from_cli_pairs(vec![
            "repo=https://example.test/repo.git".into(),
            "extra=value".into(),
        ])
        .unwrap();
        let err = resolve_launch_vars("agent", &launch, &unknown)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown launch variable"), "{err}");

        let missing = SuppliedLaunchVars::default();
        let err = resolve_launch_vars("agent", &launch, &missing)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing required launch variable"), "{err}");

        let invalid_bool = SuppliedLaunchVars::from_cli_pairs(vec![
            "repo=https://example.test/repo.git".into(),
            "debug=yes".into(),
        ])
        .unwrap();
        let err = resolve_launch_vars("agent", &launch, &invalid_bool)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid boolean launch variable"), "{err}");

        let mut typed = SuppliedLaunchVars::default();
        typed
            .insert(
                "repo".into(),
                SuppliedLaunchVarValue::String("https://example.test/repo.git".into()),
            )
            .unwrap();
        typed
            .insert("count".into(), SuppliedLaunchVarValue::Integer(3))
            .unwrap();
        typed
            .insert("debug".into(), SuppliedLaunchVarValue::Boolean(true))
            .unwrap();
        typed
            .insert("mode".into(), SuppliedLaunchVarValue::String("safe".into()))
            .unwrap();
        let vars = resolve_launch_vars("agent", &launch, &typed).unwrap();
        assert_eq!(vars["count"], "3");
        assert_eq!(vars["debug"], "true");
    }

    #[test]
    fn status_json_serializes_nullable_launch_fields() {
        let status = GatewayStatus {
            target: "default".into(),
            session_id: None,
            launch: None,
            mode: "fixed".into(),
            user: "alice".into(),
            image: "ubuntu/dev".into(),
            container: Some("ubuntu-dev".into()),
            container_pid: Some(123),
            active_sessions: 1,
            sessions: vec![model::SessionStatus {
                id: "s1".into(),
                kind: "run-command".into(),
                gateway_pid: 1234,
                container: "ubuntu-dev".into(),
                target: "default".into(),
                launch: None,
                created_at_ms: 10,
            }],
            agent_ready: false,
            ssh_socket: PathBuf::from("/tmp/ssh.sock"),
            status: "container-running".into(),
            agent: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert!(value.get("launch").unwrap().is_null());
        assert!(value["sessions"][0].get("launch").unwrap().is_null());

        let all = AllStatusEntry {
            target: "default".into(),
            session_id: None,
            launch: None,
            mode: "fixed".into(),
            user: "alice".into(),
            uid: "2450".into(),
            image: "ubuntu/dev".into(),
            container: "ubuntu-dev".into(),
            status: "running".into(),
        };
        let value = serde_json::to_value(&all).unwrap();
        assert!(value.get("launch").unwrap().is_null());
    }

    #[test]
    fn lifecycle_result_text_preserves_stop_and_remove_messages() {
        assert_eq!(
            stop_result_text(&StopResult {
                container: "ubuntu-dev".into(),
                stopped: true,
            }),
            "stopped ubuntu-dev"
        );
        assert_eq!(
            remove_result_text(&RemoveResult {
                container: "ubuntu-dev".into(),
                removed: true,
            }),
            "removed ubuntu-dev"
        );
    }

    #[test]
    fn readiness_plan_skips_pre_start_for_running_container() {
        let running = inspect_with_running(true);
        let stopped = inspect_with_running(false);

        assert_eq!(
            readiness_plan(Some(&running)),
            ContainerReadinessPlan::ReuseRunning
        );
        assert_eq!(
            readiness_plan(Some(&stopped)),
            ContainerReadinessPlan::StartStopped
        );
        assert_eq!(readiness_plan(None), ContainerReadinessPlan::CreateMissing);
    }

    #[tokio::test]
    async fn ensure_ready_reuse_running_preserves_control_socket_files() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(0));
        let runtime = test_runtime(&dir, fake_runtime, |cfg| {
            cfg.target_defaults.host_steps.clear();
        });
        runtime.prepare_control_socket_dir().unwrap();
        std::fs::write(&runtime.control_sockets.host_agent_socket, "").unwrap();
        std::fs::write(&runtime.control_sockets.host_ssh_socket, "").unwrap();

        runtime.ensure_ready().await.unwrap();

        assert!(runtime.control_sockets.host_agent_socket.exists());
        assert!(runtime.control_sockets.host_ssh_socket.exists());
    }

    #[tokio::test]
    async fn ensure_ready_start_stopped_removes_stale_control_socket_files_before_start() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let state = dir.path().join("running");
        let log = dir.path().join("start.log");
        let user = UserContext::current().unwrap();
        write_fake_runtime(
            &fake_runtime,
            &format!(
                r#"#!/bin/sh
case "$1" in
  inspect)
    if [ -f "{state}" ]; then
      running=true
    else
      running=false
    fi
    cat <<JSON
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":$running,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  start)
    if [ -e "{agent_socket}" ] || [ -e "{ssh_socket}" ]; then
      echo stale-present > "{log}"
      exit 9
    fi
    echo clean > "{log}"
    touch "{state}"
    ;;
esac
exit 0
"#,
                state = state.display(),
                log = log.display(),
                agent_socket = dir.path().join("runtime-sockets/agent.sock").display(),
                ssh_socket = dir.path().join("runtime-sockets/ssh.sock").display(),
                user = user.user,
                uid = user.uid,
            ),
        );
        let runtime = test_runtime(&dir, fake_runtime, |cfg| {
            cfg.target_defaults.lifecycle_steps.clear();
            cfg.target_defaults.host_steps.clear();
        });
        runtime.prepare_control_socket_dir().unwrap();
        std::fs::write(&runtime.control_sockets.host_agent_socket, "").unwrap();
        std::fs::write(&runtime.control_sockets.host_ssh_socket, "").unwrap();

        runtime.ensure_ready().await.unwrap();

        assert_eq!(std::fs::read_to_string(log).unwrap(), "clean\n");
        assert!(!runtime.control_sockets.host_agent_socket.exists());
        assert!(!runtime.control_sockets.host_ssh_socket.exists());
    }

    #[tokio::test]
    async fn ensure_ready_create_missing_removes_stale_control_socket_files_before_run() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let state = dir.path().join("running");
        let log = dir.path().join("run.log");
        let user = UserContext::current().unwrap();
        write_fake_runtime(
            &fake_runtime,
            &format!(
                r#"#!/bin/sh
case "$1" in
  inspect)
    if [ ! -f "{state}" ]; then
      echo "container not found" >&2
      exit 1
    fi
    cat <<JSON
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  run)
    if [ -e "{agent_socket}" ] || [ -e "{ssh_socket}" ]; then
      echo stale-present > "{log}"
      exit 9
    fi
    echo clean > "{log}"
    touch "{state}"
    ;;
esac
exit 0
"#,
                state = state.display(),
                log = log.display(),
                agent_socket = dir.path().join("runtime-sockets/agent.sock").display(),
                ssh_socket = dir.path().join("runtime-sockets/ssh.sock").display(),
                user = user.user,
                uid = user.uid,
            ),
        );
        let runtime = test_runtime(&dir, fake_runtime, |cfg| {
            cfg.target_defaults.lifecycle_steps.clear();
            cfg.target_defaults.host_steps.clear();
        });
        runtime.prepare_control_socket_dir().unwrap();
        std::fs::write(&runtime.control_sockets.host_agent_socket, "").unwrap();
        std::fs::write(&runtime.control_sockets.host_ssh_socket, "").unwrap();

        runtime.ensure_ready().await.unwrap();

        assert_eq!(std::fs::read_to_string(log).unwrap(), "clean\n");
        assert!(!runtime.control_sockets.host_agent_socket.exists());
        assert!(!runtime.control_sockets.host_ssh_socket.exists());
    }

    #[tokio::test]
    async fn runtime_load_rejects_rendered_passwd_delimiters() {
        for (field, identity_line) in [
            ("session_user", r#"session_user = "bad:user""#),
            ("session_home", r#"session_home = "/home/bad:user""#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("gateway.toml");
            std::fs::write(
                &config,
                format!(
                    r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"

[targets.default.identity]
{identity_line}
"#,
                    dir.path().join("workspace").display(),
                ),
            )
            .unwrap();

            let err = Runtime::load(Some(config), Some("default"), None, false)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains(field),
                "expected {field} in error, got {err}"
            );
        }
    }

    #[test]
    fn unix_socket_path_inventory_includes_host_and_container_paths() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
            enable_default_ssh_bridge(cfg);
        });

        let labels = runtime
            .effective_unix_socket_paths()
            .unwrap()
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>();

        assert!(labels.contains(&"host agent socket path"));
        assert!(labels.contains(&"host ssh socket path"));
        assert!(labels.contains(&"container agent socket path"));
        assert!(labels.contains(&"container ssh socket path"));
    }

    #[tokio::test]
    async fn runtime_load_rejects_overlong_generated_host_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let host_dir = dir.path().join("h".repeat(120)).join("{runtime_id}");
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
host_dir = "{}"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{{image_slug}}-{{session_id}}"
stop_when_idle = true
"#,
                workspace.display(),
                host_dir.display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(Some(config), Some("default"), None, true)
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("host agent socket path"), "{err}");
        assert!(err.contains("too long for a Unix domain socket"), "{err}");
        assert!(err.contains("control_sockets.host_dir"), "{err}");
    }

    #[tokio::test]
    async fn runtime_load_uses_explicit_ephemeral_session_id_for_names() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{{image_slug}}-{{session_id}}"
stop_when_idle = true
"#,
                workspace.display(),
            ),
        )
        .unwrap();

        let runtime = Runtime::load(
            Some(config),
            Some("default"),
            Some("abc123def456".into()),
            true,
        )
        .await
        .unwrap();

        assert_eq!(runtime.session_id.as_deref(), Some("abc123def456"));
        assert_eq!(runtime.container_name, "ubuntu-dev-abc123def456");
        assert!(
            runtime
                .container_state_dir
                .ends_with(".aw-gateway/sessions/abc123def456")
        );
        assert_eq!(
            runtime.control_sockets.host_agent_socket,
            PathBuf::from(format!(
                "/run/user/{}/aw-gateway/abc123def456/agent.sock",
                runtime.user.uid
            ))
        );
        assert_eq!(
            runtime.control_sockets.host_ssh_socket,
            PathBuf::from(format!(
                "/run/user/{}/aw-gateway/abc123def456/ssh.sock",
                runtime.user.uid
            ))
        );
        assert_eq!(
            runtime.control_sockets.container_agent_socket,
            PathBuf::from("/run/aw-gateway/agent.sock")
        );
        assert_eq!(
            runtime.control_sockets.container_ssh_socket,
            PathBuf::from("/run/aw-gateway/ssh.sock")
        );
    }

    #[tokio::test]
    async fn runtime_load_uses_fixed_target_id_for_default_control_socket_runtime_id() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"
default_target = "dev-shell"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.dev-shell]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                workspace.display(),
            ),
        )
        .unwrap();

        let runtime = Runtime::load(Some(config), Some("dev-shell"), None, true)
            .await
            .unwrap();

        assert_eq!(runtime.session_id, None);
        assert_eq!(
            runtime.control_sockets.host_agent_socket,
            PathBuf::from(format!(
                "/run/user/{}/aw-gateway/dev-shell/agent.sock",
                runtime.user.uid
            ))
        );
        assert_eq!(
            runtime.control_sockets.host_ssh_socket,
            PathBuf::from(format!(
                "/run/user/{}/aw-gateway/dev-shell/ssh.sock",
                runtime.user.uid
            ))
        );
        assert_eq!(
            runtime.control_sockets.container_agent_socket,
            PathBuf::from("/run/aw-gateway/agent.sock")
        );
        assert_eq!(
            runtime.control_sockets.container_ssh_socket,
            PathBuf::from("/run/aw-gateway/ssh.sock")
        );
    }

    #[tokio::test]
    async fn runtime_load_applies_global_and_target_control_socket_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let global_host = dir.path().join("global/{runtime_id}");
        let target_host = dir.path().join("target/{runtime_id}");
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"
default_target = "global"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
host_dir = "{}"
container_dir = "/run/global-aw"

[targets.global]
image = "ubuntu/global"
mode = "fixed"
name = "{{image_slug}}"

[targets.targeted]
image = "ubuntu/targeted"
mode = "fixed"
name = "{{image_slug}}"

[targets.targeted.control_sockets]
host_dir = "{}"
container_dir = "/tmp/aw-gateway"
"#,
                workspace.display(),
                global_host.display(),
                target_host.display(),
            ),
        )
        .unwrap();

        let global = Runtime::load(Some(config.clone()), Some("global"), None, true)
            .await
            .unwrap();
        assert_eq!(
            global.control_sockets.host_agent_socket,
            dir.path().join("global/global/agent.sock")
        );
        assert_eq!(
            global.control_sockets.container_ssh_socket,
            PathBuf::from("/run/global-aw/ssh.sock")
        );

        let targeted = Runtime::load(Some(config), Some("targeted"), None, true)
            .await
            .unwrap();
        assert_eq!(
            targeted.control_sockets.host_agent_socket,
            dir.path().join("target/targeted/agent.sock")
        );
        assert_eq!(
            targeted.control_sockets.container_ssh_socket,
            PathBuf::from("/tmp/aw-gateway/ssh.sock")
        );
    }

    #[tokio::test]
    async fn runtime_load_rejects_relative_control_socket_dirs() {
        for (field, config_fragment) in [
            (
                "control_sockets.host_dir",
                r#"[target_defaults.control_sockets]
host_dir = "relative/{runtime_id}"
"#,
            ),
            (
                "control_sockets.container_dir",
                r#"[target_defaults.control_sockets]
container_dir = "relative-container"
"#,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("gateway.toml");
            std::fs::write(
                &config,
                format!(
                    r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

{config_fragment}
[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                    dir.path().join("workspace").display(),
                ),
            )
            .unwrap();

            let err = Runtime::load(Some(config), Some("default"), None, true)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(field), "{err}");
            assert!(err.contains("absolute path"), "{err}");
        }
    }

    #[tokio::test]
    async fn runtime_load_rejects_unsafe_control_socket_runtime_ids_and_host_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{{image_slug}}-{{session_id}}"
stop_when_idle = true
"#,
                dir.path().join("workspace").display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(Some(config), Some("default"), Some("..".into()), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid session id"), "{err}");

        for (host_dir, expected) in [
            ("/tmp", "dangerous shared path"),
            ("/run/user/{uid}", "dangerous shared path"),
            ("/run/user/{uid}/aw-gateway", "must end with runtime_id"),
            (
                "/run/user/{uid}/aw-gateway/../{runtime_id}",
                "must not contain '.' or '..'",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("gateway.toml");
            std::fs::write(
                &config,
                format!(
                    r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
host_dir = "{host_dir}"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                    dir.path().join("workspace").display(),
                ),
            )
            .unwrap();

            let err = Runtime::load(Some(config), Some("default"), None, true)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[tokio::test]
    async fn runtime_load_rejects_explicit_session_id_for_fixed_target() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                workspace.display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(
            Some(config),
            Some("default"),
            Some("abc123def456".into()),
            true,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("--session-id is only valid for ephemeral targets"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn runtime_load_rejects_overlong_explicit_container_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let container_dir = format!("/home/{}/aw-gateway", "b".repeat(100));
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
container_dir = "{container_dir}"

[target_defaults.container_agent]
control_socket = false

[target_defaults.container_agent.ssh_bridge]
enabled = true
target = "127.0.0.1:22"
mode = "0600"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                workspace.display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(Some(config), Some("default"), None, false)
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("container ssh socket path"), "{err}");
        assert!(err.contains("too long for a Unix domain socket"), "{err}");
        assert!(err.contains(&container_dir), "{err}");
        assert!(err.contains("control_sockets.container_dir"), "{err}");
    }

    #[test]
    fn podman_run_args_start_agent_as_root_with_workspace_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let user = UserContext {
            uid: 2450,
            gid: 2450,
            user: "alice".into(),
            home: PathBuf::from("/home/alice"),
        };
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user,
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir: dir
                .path()
                .join("workspace/.aw-gateway/containers/ubuntu-dev"),
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let old_labels = runtime.validation_labels();
        assert!(!old_labels.contains_key("io.aw-gateway.mode"));
        assert!(!old_labels.contains_key("io.aw-gateway.session_id"));
        runtime
            .validate_labels(&ContainerInspect {
                id: "old-id".into(),
                name: "ubuntu-dev".into(),
                state: runtime::ContainerState {
                    running: true,
                    pid: 123,
                },
                config: runtime::ContainerConfig { labels: old_labels },
            })
            .unwrap();

        let args = runtime.container_runtime.run_args(
            &runtime
                .container_run_spec(Some("identity-token"), Some("control-token"))
                .unwrap(),
        );
        let arg = |value: &str| args.iter().position(|item| item == value);

        assert!(args.contains(&"--userns=keep-id".to_string()));
        assert_eq!(arg("--user").map(|idx| args[idx + 1].as_str()), Some("0:0"));
        assert!(args.contains(&"--init".to_string()));
        assert!(args.contains(&"--passwd-entry".to_string()));
        assert!(args.contains(&"alice:x:2450:2450:alice:/home/alice:/bin/bash".to_string()));
        assert!(args.contains(&format!("{}:/home/alice:Z", runtime.workspace.display())));
        assert!(args.contains(&"AW_IDENTITY_TOKEN=identity-token".to_string()));
        assert!(args.contains(&"AW_CONTAINER_CONTROL_TOKEN=control-token".to_string()));
        assert!(args.contains(&"AW_AUTHENTICATED_UID=2450".to_string()));
        assert!(args.contains(&"AW_AUTHENTICATED_GID=2450".to_string()));
        assert!(args.contains(&"io.aw-gateway.gateway=true".to_string()));
        assert!(args.contains(&"io.aw-gateway.target=default".to_string()));
        assert!(args.contains(&"io.aw-gateway.mode=fixed".to_string()));
        assert!(args.contains(&format!(
            "{}:/run/aw-gateway:Z",
            dir.path().join("runtime-sockets").display()
        )));
        assert!(args.contains(&"localhost/ubuntu/dev:latest".to_string()));
        assert_eq!(
            &args[args.len() - 3..],
            [
                "--config",
                "/home/alice/.aw-gateway/containers/ubuntu-dev/container-agent.toml",
                "run"
            ]
        );
        assert!(args.iter().any(|arg| arg == "aw-container-agent"));
    }

    #[test]
    fn prepare_control_socket_dir_is_private_and_preserves_socket_files() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
            enable_default_ssh_bridge(cfg);
        });

        std::fs::create_dir_all(&runtime.control_sockets.host_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &runtime.control_sockets.host_dir,
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        std::fs::write(&runtime.control_sockets.host_agent_socket, "").unwrap();
        std::fs::write(&runtime.control_sockets.host_ssh_socket, "").unwrap();

        runtime.prepare_control_socket_dir().unwrap();

        assert!(runtime.control_sockets.host_dir.is_dir());
        assert!(runtime.control_sockets.host_agent_socket.exists());
        assert!(runtime.control_sockets.host_ssh_socket.exists());
    }

    #[test]
    fn remove_stale_control_socket_files_removes_socket_paths() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
            enable_default_ssh_bridge(cfg);
        });

        std::fs::create_dir_all(&runtime.control_sockets.host_dir).unwrap();
        std::fs::write(&runtime.control_sockets.host_agent_socket, "").unwrap();
        std::fs::write(&runtime.control_sockets.host_ssh_socket, "").unwrap();

        runtime.remove_stale_control_socket_files().unwrap();

        assert!(!runtime.control_sockets.host_agent_socket.exists());
        assert!(!runtime.control_sockets.host_ssh_socket.exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_control_socket_dir_rejects_symlink_and_non_private_existing_dir() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        let target = dir.path().join("real-runtime-dir");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &runtime.control_sockets.host_dir).unwrap();
        let err = runtime
            .prepare_control_socket_dir()
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be a symlink"), "{err}");
        assert!(target.is_dir());

        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        std::fs::create_dir_all(&runtime.control_sockets.host_dir).unwrap();
        std::fs::set_permissions(
            &runtime.control_sockets.host_dir,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let err = runtime
            .prepare_control_socket_dir()
            .unwrap_err()
            .to_string();
        assert!(err.contains("exists with permissions 755"), "{err}");
        let mode = std::fs::symlink_metadata(&runtime.control_sockets.host_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn cleanup_control_socket_dir_removes_only_runtime_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
            enable_default_ssh_bridge(cfg);
        });
        let parent = runtime
            .control_sockets
            .host_dir
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&runtime.control_sockets.host_dir).unwrap();
        std::fs::write(parent.join("parent-marker"), "").unwrap();
        std::fs::write(&runtime.control_sockets.host_agent_socket, "").unwrap();
        std::fs::write(&runtime.control_sockets.host_ssh_socket, "").unwrap();

        runtime.cleanup_control_socket_dir();

        assert!(!runtime.control_sockets.host_dir.exists());
        assert!(parent.join("parent-marker").exists());
        assert!(parent.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_control_socket_dir_refuses_symlink_deletion_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
        let target = dir.path().join("real-runtime-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("marker"), "").unwrap();
        symlink(&target, &runtime.control_sockets.host_dir).unwrap();

        runtime.cleanup_control_socket_dir();

        assert!(target.join("marker").exists());
        assert!(runtime.control_sockets.host_dir.exists());
    }

    #[test]
    fn target_workspace_override_resolves_relative_to_user_home() {
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let target = cfg.targets.get_mut("default").unwrap();
        target.workspace = Some(crate::config::WorkspaceConfigInput {
            path: Some("{home}/workspace-internal".into()),
            ..Default::default()
        });
        let user = UserContext {
            uid: 2450,
            gid: 2450,
            user: "alice".into(),
            home: PathBuf::from("/home/alice"),
        };

        let workspace = resolve_target_workspace(
            &cfg.effective_target("default").unwrap(),
            "default",
            &user,
            None,
        )
        .unwrap();

        assert_eq!(workspace, PathBuf::from("/home/alice/workspace-internal"));
    }

    #[test]
    fn target_service_override_is_written_to_container_agent_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let mut override_service = cfg
            .effective_target("default")
            .unwrap()
            .container_agent
            .services
            .iter()
            .find(|service| service.name == "acl-proxy")
            .unwrap()
            .clone();
        override_service.command = vec![
            "acl-proxy".into(),
            "--config".into(),
            "/etc/acl-proxy/internal-acl-proxy.toml".into(),
        ];
        cfg.targets.get_mut("default").unwrap().container_agent =
            Some(crate::config::ContainerAgentConfigInput {
                services: vec![override_service],
                ..Default::default()
            });
        cfg.validate().unwrap();
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let container_state_dir = dir
            .path()
            .join("workspace/.aw-gateway/containers/ubuntu-dev");
        std::fs::create_dir_all(&container_state_dir).unwrap();
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user: UserContext {
                uid: 2450,
                gid: 2450,
                user: "alice".into(),
                home: PathBuf::from("/home/alice"),
            },
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir,
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let agent_path = runtime.write_container_agent_config().unwrap();
        assert_file_mode(&agent_path, 0o600);
        let agent_config = std::fs::read_to_string(agent_path).unwrap();
        assert!(agent_config.contains("/etc/acl-proxy/internal-acl-proxy.toml"));
        assert!(!agent_config.contains("/etc/acl-proxy/acl-proxy.toml"));
        assert_eq!(agent_config.matches("name = \"acl-proxy\"").count(), 1);
    }

    #[test]
    fn gateway_status_distinguishes_agent_unready_from_ready() {
        assert_eq!(
            gateway_status_name(false, true, false, false),
            "not-running"
        );
        assert_eq!(
            gateway_status_name(true, false, false, false),
            "container-running"
        );
        assert_eq!(
            gateway_status_name(true, true, false, false),
            "container-running-agent-unavailable"
        );
        assert_eq!(
            gateway_status_name(true, true, true, false),
            "container-running-agent-not-ready"
        );
        assert_eq!(gateway_status_name(true, true, true, true), "ready");
    }

    #[test]
    fn container_run_env_does_not_include_target_session_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.targets
            .get_mut("default")
            .unwrap()
            .container_env
            .insert("START_ONLY".into(), "start".into());
        cfg.targets
            .get_mut("default")
            .unwrap()
            .session_env
            .insert("SESSION_ONLY".into(), "session".into());
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user: UserContext {
                uid: 2450,
                gid: 2450,
                user: "alice".into(),
                home: PathBuf::from("/home/alice"),
            },
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir: dir
                .path()
                .join("workspace/.aw-gateway/containers/ubuntu-dev"),
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };
        let spec = runtime.container_run_spec(None, None).unwrap();
        assert_eq!(spec.env.get("START_ONLY"), Some(&"start".to_string()));
        assert!(!spec.env.contains_key("SESSION_ONLY"));

        let exec_env = runtime.session_env().unwrap();
        assert_eq!(exec_env.get("SESSION_ONLY"), Some(&"session".to_string()));

        std::fs::create_dir_all(&runtime.container_state_dir).unwrap();
        let env_path = runtime.write_sshd_session_env_config().unwrap();
        assert_file_mode(&env_path, 0o600);
        let env_config = std::fs::read_to_string(env_path).unwrap();
        assert!(env_config.contains("SESSION_ONLY=session"));
    }

    #[test]
    fn disabled_agent_run_spec_uses_plain_sleep_without_agent_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        disable_default_container_agent(&mut cfg);
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user: UserContext {
                uid: 2450,
                gid: 2450,
                user: "alice".into(),
                home: PathBuf::from("/home/alice"),
            },
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir: dir
                .path()
                .join("workspace/.aw-gateway/containers/ubuntu-dev"),
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let args = runtime
            .container_runtime
            .run_args(&runtime.container_run_spec(None, None).unwrap());

        assert_eq!(&args[args.len() - 2..], ["sleep", "infinity"]);
        assert!(!args.iter().any(|arg| arg == "aw-container-agent"));
        assert!(!args.iter().any(|arg| arg.starts_with("AW_IDENTITY_TOKEN=")));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("AW_CONTAINER_CONTROL_TOKEN="))
        );
    }

    #[test]
    fn writes_container_ssh_policy_and_injects_sshd_policy_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.targets.get_mut("default").unwrap().container_ssh =
            Some(crate::config::TargetContainerSshConfig {
                transfer: Some(crate::config::TargetContainerSshTransferConfig {
                    sftp: Some(crate::config::SftpTransferMode::Deny),
                    legacy_scp: Some(crate::config::LegacyScpTransferMode::Deny),
                }),
            });
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let container_state_dir = dir
            .path()
            .join("workspace/.aw-gateway/containers/ubuntu-dev");
        std::fs::create_dir_all(&container_state_dir).unwrap();
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user: UserContext {
                uid: 2450,
                gid: 2450,
                user: "alice".into(),
                home: PathBuf::from("/home/alice"),
            },
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir,
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let policy_path = runtime.write_ssh_command_filter_policy().unwrap();
        assert_file_mode(&policy_path, 0o600);
        let policy = std::fs::read_to_string(policy_path).unwrap();
        assert!(policy.contains("sftp = \"deny\""));
        assert!(policy.contains("legacy_scp = \"deny\""));

        let agent_path = runtime.write_container_agent_config().unwrap();
        assert_file_mode(&agent_path, 0o600);
        let agent_config = std::fs::read_to_string(agent_path).unwrap();
        assert!(agent_config.contains("AW_SSHD_POLICY_CONFIG"));
        assert!(
            agent_config
                .contains("/home/alice/.aw-gateway/containers/ubuntu-dev/ssh-command-filter.toml")
        );
        assert!(agent_config.contains("AW_SSHD_SETENV_CONFIG"));
        assert!(
            agent_config
                .contains("/home/alice/.aw-gateway/containers/ubuntu-dev/sshd-session-env.conf")
        );
        assert!(agent_config.contains("control_socket = \"/run/aw-gateway/agent.sock\""));
        assert!(agent_config.contains("socket = \"/run/aw-gateway/ssh.sock\""));
        assert!(!agent_config.contains("/home/alice/.aw-gateway/containers/ubuntu-dev/agent.sock"));
        assert!(!agent_config.contains("/home/alice/.aw-gateway/containers/ubuntu-dev/ssh.sock"));
    }

    #[test]
    fn bootstrap_enabled_run_spec_uses_bootstrap_entrypoint_and_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap_agent = dir.path().join("bootstrap/aw-container-agent");
        std::fs::create_dir_all(bootstrap_agent.parent().unwrap()).unwrap();
        std::fs::write(&bootstrap_agent, "").unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.target_defaults.container_bootstrap_steps.push(
            crate::config::RawContainerBootstrapStep {
                name: "global-bootstrap".into(),
                enabled: true,
                before: None,
                after: None,
                required: Some(true),
                user: Some("root".into()),
                command: Some(vec!["/bin/global".into()]),
                timeout: None,
            },
        );
        let target_cfg = cfg.targets.get_mut("default").unwrap();
        target_cfg.container_bootstrap = Some(crate::config::TargetContainerBootstrapConfig {
            enabled: Some(true),
            entrypoint: Some("/opt/aw-gateway/bin/target-bootstrap".into()),
            agent_program: Some("/opt/aw-gateway/bin/target-agent".into()),
        });
        target_cfg.container_bootstrap_steps = vec![
            crate::config::RawContainerBootstrapStep {
                name: "global-bootstrap".into(),
                enabled: false,
                before: None,
                after: None,
                required: None,
                user: None,
                command: None,
                timeout: None,
            },
            crate::config::RawContainerBootstrapStep {
                name: "target-bootstrap".into(),
                enabled: true,
                before: None,
                after: None,
                required: Some(false),
                user: Some("root".into()),
                command: Some(vec!["/bin/target".into()]),
                timeout: Some("5s".into()),
            },
        ];
        cfg.target_defaults
            .container_mounts
            .push(crate::config::ContainerMountConfig {
                source: bootstrap_agent.display().to_string(),
                target: "/opt/aw-gateway/bin/aw-container-agent".into(),
                mode: ContainerMountMode::Ro,
            });
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user: UserContext {
                uid: 2450,
                gid: 2450,
                user: "alice".into(),
                home: PathBuf::from("/home/alice"),
            },
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir: dir
                .path()
                .join("workspace/.aw-gateway/containers/ubuntu-dev"),
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let args = runtime.container_runtime.run_args(
            &runtime
                .container_run_spec(Some("identity-token"), Some("control-token"))
                .unwrap(),
        );

        let expected_mount = format!(
            "{}:/opt/aw-gateway/bin/aw-container-agent:ro,Z",
            bootstrap_agent.canonicalize().unwrap().display()
        );
        assert!(args.iter().any(|arg| arg == &expected_mount));
        assert_eq!(
            &args[args.len() - 5..],
            [
                "/opt/aw-gateway/bin/target-bootstrap",
                "--config",
                "/home/alice/.aw-gateway/containers/ubuntu-dev/container-agent.toml",
                "--bootstrap-config",
                "/home/alice/.aw-gateway/containers/ubuntu-dev/container-bootstrap.toml",
            ]
        );

        std::fs::create_dir_all(&runtime.container_state_dir).unwrap();
        let bootstrap_path = runtime.write_container_bootstrap_config().unwrap();
        assert_file_mode(&bootstrap_path, 0o600);
        let bootstrap_config = std::fs::read_to_string(bootstrap_path).unwrap();
        assert!(bootstrap_config.contains("agent_program = \"/opt/aw-gateway/bin/target-agent\""));
        assert!(bootstrap_config.contains("name = \"target-bootstrap\""));
        assert!(bootstrap_config.contains("command = [\"/bin/target\"]"));
        assert!(!bootstrap_config.contains("global-bootstrap"));
        assert!(!bootstrap_config.contains("enabled"));
        assert!(!bootstrap_config.contains("before"));
        assert!(!bootstrap_config.contains("after"));
    }

    #[test]
    fn container_mounts_error_when_source_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-bootstrap-file");
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.target_defaults
            .container_mounts
            .push(crate::config::ContainerMountConfig {
                source: missing.display().to_string(),
                target: "/opt/aw-gateway/bin/missing".into(),
                mode: ContainerMountMode::Ro,
            });
        let target = cfg.effective_target("default").unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            cfg,
            target_name: "default".into(),
            target,
            launch_name: None,
            session_id: None,
            user: UserContext {
                uid: 2450,
                gid: 2450,
                user: "alice".into(),
                home: PathBuf::from("/home/alice"),
            },
            bootstrap_user: "root".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_shell: "/bin/bash".into(),
            container_user: "alice".into(),
            container_home: PathBuf::from("/home/alice"),
            workspace: dir.path().join("workspace"),
            container_state_dir: dir
                .path()
                .join("workspace/.aw-gateway/containers/ubuntu-dev"),
            container_state_dir_in_container: PathBuf::from(
                "/home/alice/.aw-gateway/containers/ubuntu-dev",
            ),
            control_sockets: test_control_socket_paths(dir.path()),
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let err = runtime.container_mounts().unwrap_err();
        assert!(
            err.to_string().contains("container mount source #0"),
            "{err:#}"
        );
    }

    #[test]
    fn target_selection_accepts_configured_target_or_image() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        assert_eq!(
            resolve_target_selection(&cfg, Some("default")).unwrap(),
            "default"
        );
        assert_eq!(
            resolve_target_selection(&cfg, Some("ubuntu/dev")).unwrap(),
            "default"
        );
        assert_eq!(
            resolve_target_selection(&cfg, Some("localhost/ubuntu/dev:latest")).unwrap(),
            "default"
        );
        assert!(resolve_target_selection(&cfg, Some("fedora/dev")).is_err());
    }

    #[test]
    fn configured_default_display_uses_configured_default_target() {
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        assert_eq!(configured_default_display(&cfg), "default");
    }

    #[test]
    fn image_selection_normalizes_localhost_latest() {
        assert_eq!(normalize_image_selection("ubuntu/dev"), "ubuntu/dev");
        assert_eq!(
            normalize_image_selection("localhost/ubuntu/dev:latest"),
            "ubuntu/dev"
        );
    }

    #[test]
    fn host_hook_timeout_defaults_to_sixty_seconds() {
        assert_eq!(host_hook_timeout(None).unwrap(), Duration::from_secs(60));
        assert_eq!(
            host_hook_timeout(Some("250ms")).unwrap(),
            Duration::from_millis(250)
        );
        assert!(host_hook_timeout(Some("5")).is_err());
    }

    #[test]
    fn parses_proc_stat_start_time() {
        let stat = "123 (bash) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20";
        assert_eq!(parse_process_start_time(stat).unwrap(), "987654");
    }

    #[test]
    fn detects_current_process_session_marker_as_active() {
        let marker = SessionMarker {
            id: "test".into(),
            kind: "connect".into(),
            gateway_pid: std::process::id(),
            gateway_start_time: process_start_time(std::process::id()).unwrap(),
            container: "ubuntu-dev".into(),
            target: "default".into(),
            launch: None,
            created_at_ms: 0,
        };
        assert!(session_marker_is_active(&marker));
    }

    #[test]
    fn old_shape_session_marker_deserializes_without_launch() {
        let raw = r#"
{
  "id": "test",
  "kind": "run-command",
  "gateway_pid": 123,
  "gateway_start_time": "456",
  "container": "ubuntu-dev",
  "target": "default",
  "created_at_ms": 789
}
"#;
        let marker: SessionMarker = serde_json::from_str(raw).unwrap();
        assert_eq!(marker.launch, None);
    }

    #[test]
    fn session_marker_launch_round_trips_and_none_is_omitted() {
        let marker = SessionMarker {
            id: "test".into(),
            kind: "launch".into(),
            gateway_pid: 123,
            gateway_start_time: "456".into(),
            container: "ubuntu-dev".into(),
            target: "default".into(),
            launch: Some("agent-pack-codex".into()),
            created_at_ms: 789,
        };
        let raw = serde_json::to_string(&marker).unwrap();
        let parsed: SessionMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.launch.as_deref(), Some("agent-pack-codex"));

        let without_launch = SessionMarker {
            launch: None,
            ..marker
        };
        let raw = serde_json::to_string(&without_launch).unwrap();
        assert!(!raw.contains("\"launch\":"));
    }

    #[test]
    fn detects_current_process_local_listener_status_as_active() {
        let status = LocalListenerStatus {
            gateway_pid: std::process::id(),
            gateway_start_time: process_start_time(std::process::id()).unwrap(),
            host: "127.0.0.1".into(),
            port: 40222,
            created_at_ms: 0,
        };
        assert!(local_listener_is_active(&status));
    }

    #[test]
    fn public_key_validation_accepts_known_types() {
        assert!(is_plausible_public_key("ssh-ed25519 AAAAC3Nza comment"));
        assert!(!is_plausible_public_key("not-a-key"));
        assert!(validate_public_key_content("ssh-ed25519 AAAAC3Nza comment\n").is_ok());
        assert!(
            validate_public_key_content("ssh-ed25519 AAAAC3Nza one\nssh-ed25519 AAAAC3Nza two")
                .is_err()
        );
        assert!(validate_public_key_content(" ssh-ed25519 AAAAC3Nza").is_err());
    }

    #[test]
    fn identity_token_validation_requires_single_non_empty_line() {
        let path = PathBuf::from("/tmp/token");
        assert_eq!(
            validate_identity_token_content("abc\n", &path).unwrap(),
            "abc"
        );
        assert!(validate_identity_token_content("", &path).is_err());
        assert!(validate_identity_token_content("a\nb", &path).is_err());
        assert!(validate_identity_token_content(&"x".repeat(4097), &path).is_err());
    }

    #[test]
    fn identity_token_is_generated_once_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/identity-token");

        let first = ensure_identity_token_file(&path).unwrap();
        let second = ensure_identity_token_file(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(&first[14..15], "4");
        assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn command_health_check_uses_exit_status() {
        let vars = Vars::new();
        let ok = HealthCheck::Command {
            command: vec!["/usr/bin/true".into()],
        };
        let fail = HealthCheck::Command {
            command: vec!["/usr/bin/false".into()],
        };
        assert!(run_health_check(&ok, &vars).await.is_ok());
        assert!(run_health_check(&fail, &vars).await.is_err());
    }

    #[tokio::test]
    async fn command_health_check_renders_variables() {
        let mut vars = Vars::new();
        vars.insert("value".into(), "expected".into());
        let check = HealthCheck::Command {
            command: vec![
                "/bin/test".into(),
                "{value}".into(),
                "=".into(),
                "expected".into(),
            ],
        };
        assert!(run_health_check(&check, &vars).await.is_ok());
    }
}
