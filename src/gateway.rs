use crate::cli::{
    AddContainerKeyArgs, AddHostKeyArgs, AddKeyArgs, ClientBundleArgs, ClientConfigArgs,
    ConfigCommand, GatewayArgs, GatewayCommand, LaunchCommand, LaunchesArgs, RunArgs,
    SetDefaultArgs, StatusArg, StopArgs, TargetArg, TargetsArgs, UpArgs,
};
use crate::config::{
    AGENT_SCHEMA_VERSION, BootstrapIdentity, ContainerAgentConfig, ContainerAgentFile,
    ContainerBootstrapConfig, ContainerBootstrapFile, ContainerBootstrapStep, ContainerMountMode,
    ContainerRuntimeType, ContainerSshConfig, ControlSocketConfig, GatewayConfig, HostStep,
    IdleCleanupAction, IdleCleanupOwner, LaunchConfig, LaunchStep, LaunchStepLocation,
    LaunchVarConfig, LaunchVarType, LifecyclePhase, LifecycleStep, LocalSshBackend, LocalSshMode,
    LocalSshReadiness, LoggingConfig, RenderedContainerBootstrapStep, TargetConfig, TargetMode,
    validate_name, validate_passwd_scalar,
};
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
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::{Duration, Instant, sleep};

pub const DEFAULT_GATEWAY_CONFIG: &str = include_str!("../aw-gateway.sample.toml");
const MAX_SSH_ORIGINAL_COMMAND_BYTES: usize = 64 * 1024;
const DEFAULT_HOST_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

mod client;
mod fileutil;
mod health;
mod identity;
mod listener;
mod model;
mod session;

use client::{read_default_selection, resolve_target_selection};
use fileutil::{atomic_write_file, write_private_file};
use health::{render_command, run_argv_with_options, run_argv_with_timeout, run_health_check};
use model::{GatewayStatus, ReadyStatus, TcpEndpoint, gateway_status_name};
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
        Some(GatewayCommand::Connect(TargetArg { target })) => connect(args.config, target).await,
        Some(GatewayCommand::Up(status)) => up(args.config, status).await,
        Some(GatewayCommand::Run(run_args)) => run_container_command(args.config, run_args).await,
        Some(GatewayCommand::Launch(launch_command)) => launch(args.config, launch_command).await,
        Some(GatewayCommand::Launches(launches_args)) => launches(args.config, launches_args).await,
        Some(GatewayCommand::Stop(stop_args)) => stop(args.config, stop_args).await,
        Some(GatewayCommand::Remove(target_arg)) => remove(args.config, target_arg).await,
        Some(GatewayCommand::Status(status_args)) => status(args.config, status_args).await,
        Some(GatewayCommand::Targets(targets_args)) => targets(args.config, targets_args).await,
        Some(GatewayCommand::SetDefault(set_default_args)) => {
            client::set_default(args.config, set_default_args).await
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
            client::client_config(args.config, client_config_args).await
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
        if let Some(direction) = legacy_scp_server_direction(command)
            && !legacy_scp_mode_allows(cfg.container_ssh.transfer.legacy_scp, direction)
        {
            anyhow::bail!("blocked by policy: legacy scp is not allowed");
        }
        if !cfg.container_ssh.transfer.sftp.allows() && is_sftp_server_command(command) {
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
    match action {
        GatewayAction::Connect(target) => connect(config_path, target).await,
        GatewayAction::Up(target) => {
            let runtime = Runtime::load(config_path.clone(), target.as_deref(), None, true).await?;
            if runtime
                .target
                .local_ssh
                .as_ref()
                .is_some_and(|local_ssh| local_ssh.mode == LocalSshMode::Listen)
            {
                anyhow::bail!(
                    "gateway action \"up\" over SSH is not supported for local_ssh.mode = \"listen\" targets; use connect or run aw-gateway up locally"
                );
            }
            up(
                config_path,
                UpArgs {
                    target,
                    json: true,
                    session_id: None,
                },
            )
            .await
        }
        GatewayAction::Run(action) => {
            run_container_command(
                config_path,
                RunArgs {
                    target: action.target,
                    cwd: action.cwd,
                    command: action.command,
                },
            )
            .await
        }
        GatewayAction::Status(action) => {
            status(
                config_path,
                StatusArg {
                    target: action.target,
                    all: action.all,
                    json: true,
                    session_id: None,
                },
            )
            .await
        }
        GatewayAction::Targets { json } => targets(config_path, TargetsArgs { json }).await,
        GatewayAction::Stop(target) => {
            stop(
                config_path,
                StopArgs {
                    target,
                    session_id: None,
                },
            )
            .await
        }
        GatewayAction::Remove(target) => remove(config_path, TargetArg { target }).await,
        GatewayAction::SetDefault(target_or_image) => {
            client::set_default(
                config_path,
                SetDefaultArgs {
                    target_or_image: Some(target_or_image),
                    reset: false,
                },
            )
            .await
        }
        GatewayAction::ShowDefault => client::show_default(config_path).await,
        GatewayAction::ResetDefault => {
            client::set_default(
                config_path,
                SetDefaultArgs {
                    target_or_image: None,
                    reset: true,
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
        GatewayAction::ClientConfig(action) => {
            client::client_config(
                config_path,
                ClientConfigArgs {
                    target: action.target,
                    identity_file: action.identity_file.map(PathBuf::from),
                },
            )
            .await
        }
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
            "run [target] [--cwd DIR] -- <command>",
            "Run a command in the container",
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
        ("connect [target]", "Connect to the container SSH service"),
        ("help", "Show this help"),
    ];
    for (command, description) in commands {
        if let Some(action) = command.split_whitespace().next()
            && cfg
                .ssh_dispatch
                .enabled_gateway_actions
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

async fn connect(config_path: Option<PathBuf>, target: Option<String>) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, target.as_deref(), None, true).await?;
    runtime.ensure_ssh_endpoint_configured()?;
    let session = runtime.create_session_marker("connect")?;
    let ready = runtime.ensure_ready().await?;
    let proxy_result = listener::proxy_ready_to_stdio(&ready).await;
    drop(session);
    if let Err(err) = runtime.apply_gateway_idle_cleanup().await {
        tracing::warn!(error = %err, "gateway-owned idle cleanup failed");
    }
    proxy_result
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
        let mut ready = runtime.ensure_ready().await?;
        let up_result = {
            let bound = listener::bind_local_ssh(&runtime).await?;
            ready.local_ssh = Some(bound.ready.clone());
            let config = runtime.render_client_config(None)?;
            ready.client_config = Some(runtime.write_inner_config(&config)?);
            println!("{}", serde_json::to_string_pretty(&ready)?);
            let target = ready.ssh_target();
            listener::serve_local_ssh(bound, target).await
        };
        drop(session);
        if let Err(err) = runtime.apply_gateway_idle_cleanup().await {
            tracing::warn!(error = %err, "gateway-owned idle cleanup failed");
        }
        return up_result;
    }
    // Non-listen `up` is a warm-up operation: it starts or validates the
    // target and exits without holding an active session marker.
    let ready = runtime.ensure_ready().await?;
    println!("{}", serde_json::to_string_pretty(&ready)?);
    Ok(())
}

async fn run_container_command(config_path: Option<PathBuf>, args: RunArgs) -> anyhow::Result<()> {
    if args.command.is_empty() {
        anyhow::bail!("run requires -- followed by a command; use up to start or hold a target");
    }
    let runtime = Runtime::load(config_path, args.target.as_deref(), None, true).await?;
    let session_kind = "run-command";
    let session = runtime.create_session_marker(session_kind)?;
    let _ready = runtime.ensure_ready().await?;
    let _agent_session = runtime.agent_session_hold(session_kind).await?;
    let command = args.command;
    let cwd = args
        .cwd
        .as_deref()
        .map(|cwd| paths::expand_home(&runtime.container_home, cwd));
    let code = exec_final_container_command(&runtime, command, cwd, runtime.session_env()?).await?;
    drop(session);
    if let Err(err) = runtime.apply_gateway_idle_cleanup().await {
        tracing::warn!(error = %err, "gateway-owned idle cleanup failed");
    }
    std::process::exit(code);
}

async fn exec_final_container_command(
    runtime: &Runtime,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
) -> anyhow::Result<i32> {
    let exec_spec = ContainerExecSpec {
        stdin_tty: std::io::stdin().is_terminal(),
        stdout_tty: std::io::stdout().is_terminal(),
        user: runtime.exec_identity(),
        cwd,
        env,
        container_name: runtime.container_name.clone(),
        command,
    };
    runtime.container_runtime.exec(&exec_spec).await
}

async fn stop(config_path: Option<PathBuf>, args: StopArgs) -> anyhow::Result<()> {
    let runtime =
        Runtime::load(config_path, args.target.as_deref(), args.session_id, false).await?;
    let _lock = runtime.acquire_lifecycle_lock().await?;
    let Some(inspect) = runtime
        .container_runtime
        .inspect(&runtime.container_name)
        .await?
    else {
        println!("not running");
        return Ok(());
    };
    runtime.stop_inspected_container(&inspect).await?;
    println!("stopped {}", runtime.container_name);
    Ok(())
}

async fn remove(config_path: Option<PathBuf>, args: TargetArg) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, args.target.as_deref(), None, false).await?;
    let _lock = runtime.acquire_lifecycle_lock().await?;
    let Some(inspect) = runtime
        .container_runtime
        .inspect(&runtime.container_name)
        .await?
    else {
        println!("not found");
        return Ok(());
    };
    runtime.validate_labels(&inspect)?;
    if inspect.state.running {
        runtime.stop_inspected_container(&inspect).await?;
    }
    if let Some(current) = runtime
        .container_runtime
        .inspect(&runtime.container_name)
        .await?
    {
        runtime.validate_labels(&current)?;
        runtime
            .container_runtime
            .rm(&runtime.container_name)
            .await?;
    }
    println!("removed {}", runtime.container_name);
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
        return status_all(config_path, status.json).await;
    }
    let runtime = Runtime::load(
        config_path,
        status.target.as_deref(),
        status.session_id,
        false,
    )
    .await?;
    let result = runtime.status().await?;
    if status.json {
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

async fn status_all(config_path: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    let container_runtime = ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home)?;
    let containers = container_runtime
        .list_managed_containers(&user.user, user.uid)
        .await?;
    let summaries = status_all_entries(&cfg, containers);
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
    let cfg = load_config(config_path)?;
    let entries = target_entries(&cfg)?;
    if args.json {
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
    let cfg = load_config(config_path)?;
    let entries = launch_summaries(&cfg);
    if args.json {
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
        LaunchCommand::Show(args) => launch_show(config_path, &args.name, args.json).await,
        LaunchCommand::Run(raw) => {
            let (name, vars) = parse_launch_run_args(raw)?;
            launch_execute(config_path, &name, vars).await
        }
    }
}

fn parse_launch_run_args(raw: Vec<std::ffi::OsString>) -> anyhow::Result<(String, Vec<String>)> {
    let mut args = raw.into_iter();
    let Some(name) = args.next() else {
        anyhow::bail!("launch requires a launch name");
    };
    let name = name
        .into_string()
        .map_err(|_| anyhow::anyhow!("launch name must be valid UTF-8"))?;
    let mut vars = Vec::new();
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| anyhow::anyhow!("launch arguments must be valid UTF-8"))?;
        if arg == "--json" {
            anyhow::bail!("launch execution does not support --json");
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
    Ok((name, vars))
}

async fn launch_show(config_path: Option<PathBuf>, name: &str, json: bool) -> anyhow::Result<()> {
    let cfg = load_config(config_path)?;
    let launch = cfg
        .launches
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown launch {name:?}"))?;
    let detail = launch_detail(name, launch);
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
    supplied: Vec<String>,
) -> anyhow::Result<()> {
    let cfg = load_config(config_path.clone())?;
    launch_execute_with_config(cfg, name, supplied).await
}

async fn launch_execute_with_config(
    cfg: GatewayConfig,
    name: &str,
    supplied: Vec<String>,
) -> anyhow::Result<()> {
    let launch = cfg
        .launches
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown launch {name:?}"))?
        .clone();
    let resolved_vars = resolve_launch_vars(name, &launch, supplied)?;
    let runtime = Runtime::from_config(
        cfg,
        Some(&launch.target),
        None,
        true,
        Some(name.to_string()),
    )
    .await?;
    let session_kind = "launch";
    let session = runtime.create_launch_session_marker(session_kind)?;
    let result = async {
        let ready = runtime.ensure_ready().await?;
        let _agent_session = runtime.agent_session_hold(session_kind).await?;
        let container_pid = ready.container_pid.to_string();
        let vars = launch_template_vars(&runtime, &resolved_vars, Some(&container_pid));
        let launch_env = render_template_map(&launch.env, &vars)?;
        run_launch_steps(&runtime, &launch, &vars, &launch_env).await?;
        let env = launch_final_env(&runtime.session_env()?, &launch_env);
        let cwd = render_launch_cwd(
            launch.cwd.as_deref(),
            &vars,
            runtime.container_home.as_path(),
        )?;
        let command = render_command(&launch.command, &vars)?;
        exec_final_container_command(&runtime, command, cwd, env).await
    }
    .await;
    drop(session);
    if let Err(err) = runtime.apply_gateway_idle_cleanup().await {
        tracing::warn!(error = %err, "gateway-owned idle cleanup failed");
    }
    let code = result?;
    std::process::exit(code);
}

#[derive(Debug, Serialize)]
struct TargetEntry {
    target: String,
    image: String,
    mode: String,
    container: String,
    default: bool,
}

#[derive(Debug, Serialize)]
struct LaunchSummary {
    name: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    vars: BTreeMap<String, LaunchVarMetadata>,
}

#[derive(Debug, Serialize)]
struct LaunchDetail {
    name: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    vars: BTreeMap<String, LaunchVarMetadata>,
    steps: Vec<LaunchStepDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LaunchStepDetail {
    name: String,
    phase: String,
    location: String,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LaunchVarMetadata {
    #[serde(rename = "type")]
    var_type: &'static str,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<crate::config::LaunchVarValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct AllStatusEntry {
    target: String,
    session_id: Option<String>,
    launch: Option<String>,
    mode: String,
    user: String,
    uid: String,
    image: String,
    container: String,
    status: String,
}

fn launch_summaries(cfg: &GatewayConfig) -> Vec<LaunchSummary> {
    cfg.launches
        .iter()
        .map(|(name, launch)| LaunchSummary {
            name: name.clone(),
            target: launch.target.clone(),
            description: launch.description.clone(),
            vars: launch_var_metadata(&launch.vars),
        })
        .collect()
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
    supplied: Vec<String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    let mut supplied_map = BTreeMap::new();
    for raw in supplied {
        let Some((key, value)) = raw.split_once('=') else {
            anyhow::bail!("--var must be key=value");
        };
        if supplied_map
            .insert(key.to_string(), value.to_string())
            .is_some()
        {
            anyhow::bail!("duplicate launch variable {key:?}");
        }
    }
    for key in supplied_map.keys() {
        if !launch.vars.contains_key(key) {
            anyhow::bail!("unknown launch variable {key:?}");
        }
    }
    for (name, var) in &launch.vars {
        if let Some(value) = supplied_map.get(name) {
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
    value: &str,
) -> anyhow::Result<String> {
    match var.var_type {
        LaunchVarType::String => Ok(value.to_string()),
        LaunchVarType::Enum => {
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
            "true" | "false" => Ok(value.to_string()),
            _ => anyhow::bail!("invalid boolean launch variable {name:?}; expected true or false"),
        },
        LaunchVarType::Number => {
            let parsed = value
                .parse::<f64>()
                .with_context(|| format!("invalid number launch variable {name:?}"))?;
            if !parsed.is_finite() {
                anyhow::bail!("invalid number launch variable {name:?}; expected finite number");
            }
            Ok(canonical_cli_number(value, parsed))
        }
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
    let command = render_command(&step.command, vars)?;
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
        command: render_command(&step.command, vars)?,
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
    let Some(target_cfg) = cfg.targets.get(target) else {
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
    let entries = cfg
        .targets
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
    container_name: String,
    container_runtime: ContainerRuntime,
    effective_container_ssh: ContainerSshConfig,
    effective_lifecycle_steps: Vec<LifecycleStep>,
    effective_host_steps: Vec<HostStep>,
    effective_container_bootstrap: ContainerBootstrapConfig,
    effective_container_bootstrap_steps: Vec<ContainerBootstrapStep>,
    effective_container_agent: ContainerAgentConfig,
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
        let target_cfg = cfg
            .targets
            .get(&target_name)
            .ok_or_else(|| anyhow::anyhow!("unknown target {target_name:?}"))?;
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
        let target_cfg = target_cfg.clone();
        let effective_container_ssh = cfg.effective_container_ssh(&target_cfg)?;
        let effective_lifecycle_steps = cfg.effective_lifecycle_steps(&target_cfg)?;
        let effective_host_steps = cfg.effective_host_steps(&target_cfg)?;
        let effective_container_bootstrap = cfg.effective_container_bootstrap(&target_cfg)?;
        let effective_container_bootstrap_steps =
            cfg.effective_container_bootstrap_steps(&target_cfg)?;
        let effective_container_agent = cfg.effective_container_agent(&target_cfg)?;
        let workspace = resolve_target_workspace(
            &cfg,
            &target_cfg,
            &target_name,
            &user,
            session_id.as_deref(),
        )?;
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
            .join(&cfg.workspace.state_dir)
            .join(state_kind)
            .join(state_id);
        let container_state_dir_in_container = resolve_container_path(
            &container_home,
            &cfg.workspace.state_dir,
            [state_kind, state_id],
        );
        Ok(Runtime {
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
            container_name,
            container_runtime,
            effective_container_ssh,
            effective_lifecycle_steps,
            effective_host_steps,
            effective_container_bootstrap,
            effective_container_bootstrap_steps,
            effective_container_agent,
        })
    }

    async fn ensure_ready(&self) -> anyhow::Result<ReadyStatus> {
        let _lock = self.acquire_lifecycle_lock().await?;
        let mut started_container = false;
        let result = async {
            paths::ensure_private_dir(&self.container_state_dir)?;
            self.write_sshd_session_env_config()?;
            self.write_ssh_command_filter_policy()?;
            if self.ssh_endpoint_configured() {
                self.ensure_inner_keypair(false).await?;
            }
            if self.agent_enabled() {
                self.ensure_control_token()?;
                self.write_container_agent_config()?;
                if self.effective_container_bootstrap.enabled {
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
                    self.container_runtime.start(&self.container_name).await?;
                    started_container = true;
                    inspect = self.container_runtime.inspect(&self.container_name).await?;
                }
                ContainerReadinessPlan::CreateMissing => {
                    self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                        .await?;
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
                if started_container {
                    self.cleanup_failed_start().await;
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
            .effective_container_agent
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
            if self.effective_container_bootstrap.enabled {
                vec![
                    self.render_value(&self.effective_container_bootstrap.entrypoint)?,
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
            container_user: if self.effective_container_bootstrap.enabled {
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
            .effective_lifecycle_steps
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
        let command = render_command(&step.command, &vars)?;
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
        for step in &self.effective_host_steps {
            let vars = self.vars(Some(container_pid));
            let command = render_command(&step.command, &vars)?;
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
                .join(&self.cfg.workspace.state_dir)
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
        let mut container_agent = self.effective_container_agent.clone();
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
        let raw = toml::to_string_pretty(&cfg)?;
        write_private_file(&path, raw.as_bytes(), 0o600)
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
            sftp: self.effective_container_ssh.transfer.sftp,
            legacy_scp: self.effective_container_ssh.transfer.legacy_scp,
        };
        let path = self.ssh_command_filter_policy_host();
        let raw = toml::to_string_pretty(&cfg)?;
        atomic_write_file(&path, raw.as_bytes(), 0o600)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    fn write_container_bootstrap_config(&self) -> anyhow::Result<PathBuf> {
        let vars = self.vars(None);
        let steps = self
            .effective_container_bootstrap_steps
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
            agent_program: template::render(
                &self.effective_container_bootstrap.agent_program,
                &vars,
            )?,
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
        let raw = toml::to_string_pretty(&cfg)?;
        write_private_file(&path, raw.as_bytes(), 0o600)
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
        self.cfg
            .container_mounts
            .iter()
            .chain(self.target.container_mounts.iter())
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
            .collect()
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
        self.container_state_dir.join("agent.sock")
    }

    fn ssh_socket(&self) -> PathBuf {
        self.container_state_dir.join("ssh.sock")
    }

    fn ssh_backend(&self) -> LocalSshBackend {
        self.target
            .local_ssh
            .as_ref()
            .map(|local_ssh| local_ssh.backend)
            .unwrap_or_default()
    }

    fn agent_enabled(&self) -> bool {
        self.effective_container_agent.enabled
    }

    fn agent_control_enabled(&self) -> bool {
        self.agent_enabled()
            && self
                .effective_container_agent
                .control_socket
                .as_ref()
                .is_none_or(ControlSocketConfig::is_enabled)
    }

    fn ssh_endpoint_configured(&self) -> bool {
        match self.ssh_backend() {
            LocalSshBackend::Socket => self
                .effective_container_agent
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
            let bridge = match &self.effective_container_agent.ssh_bridge {
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

fn resolve_target_workspace(
    cfg: &GatewayConfig,
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
    let configured = target.workspace.as_deref().unwrap_or(&cfg.workspace.path);
    let rendered = template::render(configured, &vars)?;
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

    fn test_runtime(
        dir: &tempfile::TempDir,
        program: PathBuf,
        configure: impl FnOnce(&mut GatewayConfig),
    ) -> Runtime {
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.runtime.program = Some(program.display().to_string());
        cfg.container_agent.enabled = false;
        cfg.container_agent.services.clear();
        cfg.container_agent.ssh_bridge = None;
        cfg.container_agent.control_socket = None;
        cfg.container_agent.idle_cleanup = None;
        configure(&mut cfg);
        cfg.validate().unwrap();

        let target = cfg.targets.get("default").unwrap().clone();
        let user = UserContext::current().unwrap();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home).unwrap();
        Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
            container_name: "ubuntu-dev".into(),
            container_runtime,
            user,
        }
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
        cfg.targets.get_mut("default").unwrap().mode = TargetMode::Ephemeral;
        let mut first = managed_labels("default", "ubuntu-dev-x9k2p");
        first.insert("io.aw-gateway.image".into(), "scratch/dev".into());
        first.insert("io.aw-gateway.mode".into(), "ephemeral".into());
        first.insert("io.aw-gateway.session_id".into(), "x9k2p".into());
        let mut second = managed_labels("default", "ubuntu-dev-m4v8r");
        second.insert("io.aw-gateway.image".into(), "scratch/dev".into());
        second.insert("io.aw-gateway.mode".into(), "ephemeral".into());
        second.insert("io.aw-gateway.session_id".into(), "m4v8r".into());

        let entries = status_all_entries(
            &cfg,
            vec![
                managed_container("ubuntu-dev-x9k2p", "scratch/dev", false, first),
                managed_container("ubuntu-dev-m4v8r", "scratch/dev", true, second),
            ],
        );

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id.as_deref(), Some("x9k2p"));
        assert_eq!(entries[0].launch, None);
        assert_eq!(entries[0].status, "stopped");
        assert_eq!(entries[1].session_id.as_deref(), Some("m4v8r"));
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
        let mut labels = managed_labels("default", "ubuntu-dev-x9k2p");
        labels.insert("io.aw-gateway.mode".into(), "ephemeral".into());
        labels.insert("io.aw-gateway.session_id".into(), "x9k2p".into());
        labels.insert("io.aw-gateway.launch".into(), "agent-pack-codex".into());

        let entries = status_all_entries(
            &cfg,
            vec![managed_container(
                "ubuntu-dev-x9k2p",
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
        runtime.session_id = Some("x9k2p".into());

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

        let code = exec_final_container_command(
            &runtime,
            vec!["/bin/launch-final".into()],
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(code, 37);
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
            target.stop_when_idle = true;
            target.remove_on_stop = false;
            target.idle_cleanup = Some(crate::config::IdleCleanupConfig {
                owner: IdleCleanupOwner::Gateway,
                action: IdleCleanupAction::ExitContainer,
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
        let launch = cfg.launches.get("agent").unwrap();
        let vars = resolve_launch_vars(
            "agent",
            launch,
            vec![
                "repo=https://example.test/repo.git".into(),
                "count=2.0".into(),
                "debug=true".into(),
                "mode=safe".into(),
            ],
        )
        .unwrap();
        assert_eq!(vars["count"], "2");
        assert_eq!(vars["debug"], "true");
        assert_eq!(vars["mode"], "safe");

        let err = resolve_launch_vars("agent", launch, vec!["repo=a".into(), "repo=b".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate launch variable"), "{err}");
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

[workspace]
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
    fn podman_run_args_start_agent_as_root_with_workspace_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let user = UserContext {
            uid: 2450,
            gid: 2450,
            user: "alice".into(),
            home: PathBuf::from("/home/alice"),
        };
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
    fn target_workspace_override_resolves_relative_to_user_home() {
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        let target = cfg.targets.get_mut("default").unwrap();
        target.workspace = Some("{home}/workspace-internal".into());
        let user = UserContext {
            uid: 2450,
            gid: 2450,
            user: "alice".into(),
            home: PathBuf::from("/home/alice"),
        };

        let workspace = resolve_target_workspace(
            &cfg,
            cfg.targets.get("default").unwrap(),
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
            Some(crate::config::TargetContainerAgentConfig {
                services: vec![override_service],
            });
        cfg.validate().unwrap();
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let container_state_dir = dir
            .path()
            .join("workspace/.aw-gateway/containers/ubuntu-dev");
        std::fs::create_dir_all(&container_state_dir).unwrap();
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let agent_path = runtime.write_container_agent_config().unwrap();
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
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };
        let spec = runtime.container_run_spec(None, None).unwrap();
        assert_eq!(spec.env.get("START_ONLY"), Some(&"start".to_string()));
        assert!(!spec.env.contains_key("SESSION_ONLY"));

        let exec_env = runtime.session_env().unwrap();
        assert_eq!(exec_env.get("SESSION_ONLY"), Some(&"session".to_string()));
    }

    #[test]
    fn disabled_agent_run_spec_uses_plain_sleep_without_agent_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.container_agent.enabled = false;
        cfg.container_agent.services.clear();
        cfg.container_agent.ssh_bridge = None;
        cfg.container_agent.control_socket = None;
        cfg.container_agent.idle_cleanup = None;
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let container_state_dir = dir
            .path()
            .join("workspace/.aw-gateway/containers/ubuntu-dev");
        std::fs::create_dir_all(&container_state_dir).unwrap();
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
            container_name: "ubuntu-dev".into(),
            container_runtime,
        };

        let policy_path = runtime.write_ssh_command_filter_policy().unwrap();
        let policy = std::fs::read_to_string(policy_path).unwrap();
        assert!(policy.contains("sftp = \"deny\""));
        assert!(policy.contains("legacy_scp = \"deny\""));

        let agent_path = runtime.write_container_agent_config().unwrap();
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
    }

    #[test]
    fn bootstrap_enabled_run_spec_uses_bootstrap_entrypoint_and_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap_agent = dir.path().join("bootstrap/aw-container-agent");
        std::fs::create_dir_all(bootstrap_agent.parent().unwrap()).unwrap();
        std::fs::write(&bootstrap_agent, "").unwrap();
        let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.container_bootstrap_steps
            .push(crate::config::ContainerBootstrapStep {
                name: "global-bootstrap".into(),
                required: true,
                user: "root".into(),
                command: vec!["/bin/global".into()],
                timeout: None,
            });
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
        cfg.container_mounts
            .push(crate::config::ContainerMountConfig {
                source: bootstrap_agent.display().to_string(),
                target: "/opt/aw-gateway/bin/aw-container-agent".into(),
                mode: ContainerMountMode::Ro,
            });
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
        cfg.container_mounts
            .push(crate::config::ContainerMountConfig {
                source: missing.display().to_string(),
                target: "/opt/aw-gateway/bin/missing".into(),
                mode: ContainerMountMode::Ro,
            });
        let target = cfg.targets.get("default").unwrap().clone();
        let container_runtime =
            ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
        let runtime = Runtime {
            effective_container_ssh: cfg.effective_container_ssh(&target).unwrap(),
            effective_lifecycle_steps: cfg.effective_lifecycle_steps(&target).unwrap(),
            effective_host_steps: cfg.effective_host_steps(&target).unwrap(),
            effective_container_bootstrap: cfg.effective_container_bootstrap(&target).unwrap(),
            effective_container_bootstrap_steps: cfg
                .effective_container_bootstrap_steps(&target)
                .unwrap(),
            effective_container_agent: cfg.effective_container_agent(&target).unwrap(),
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
