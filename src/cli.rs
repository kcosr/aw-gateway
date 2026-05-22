use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "aw-gateway",
    version,
    about = "AW Gateway SSH/container gateway",
    disable_help_subcommand = true
)]
pub struct GatewayArgs {
    #[arg(long, env = "AW_GATEWAY_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, env = "AW_GATEWAY_LOG_LEVEL", global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<GatewayCommand>,
}

#[derive(Debug, Subcommand)]
pub enum GatewayCommand {
    #[command(subcommand)]
    Config(ConfigCommand),
    Connect(TargetArg),
    Up(UpArgs),
    Run(RunArgs),
    #[command(subcommand)]
    Launch(LaunchCommand),
    Launches(LaunchesArgs),
    Stop(StopArgs),
    Remove(TargetArg),
    Status(StatusArg),
    Targets(TargetsArgs),
    SetDefault(SetDefaultArgs),
    AddKey(AddKeyArgs),
    AddHostKey(AddHostKeyArgs),
    AddContainerKey(AddContainerKeyArgs),
    Help,
    ClientConfig(ClientConfigArgs),
    ClientBundle(ClientBundleArgs),
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate,
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    pub path: Option<PathBuf>,

    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args, Clone)]
pub struct TargetArg {
    pub target: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct StatusArg {
    pub target: Option<String>,

    #[arg(long)]
    pub all: bool,

    #[arg(long)]
    pub json: bool,

    #[arg(long)]
    pub session_id: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct UpArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub json: bool,

    #[arg(long)]
    pub session_id: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct TargetsArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub session_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct SetDefaultArgs {
    pub target_or_image: Option<String>,

    #[arg(long)]
    pub reset: bool,
}

#[derive(Debug, Args)]
pub struct AddKeyArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub public_key: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AddHostKeyArgs {
    #[arg(long)]
    pub public_key: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AddContainerKeyArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub public_key: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub cwd: Option<String>,

    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct LaunchesArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum LaunchCommand {
    Show(LaunchShowArgs),
    #[command(external_subcommand)]
    Run(Vec<OsString>),
}

#[derive(Debug, Args)]
pub struct LaunchShowArgs {
    pub name: String,

    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ClientConfigArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ClientBundleArgs {
    pub target: Option<String>,

    #[arg(long)]
    pub identity_file: Option<PathBuf>,

    #[arg(long)]
    pub rotate_key: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "aw-container-agent",
    version,
    about = "In-container AW Gateway supervisor"
)]
pub struct AgentArgs {
    #[arg(long, env = "AW_CONTAINER_AGENT_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, env = "AW_CONTAINER_AGENT_LOG_LEVEL", global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<AgentCommand>,
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    #[command(subcommand)]
    Config(ConfigCommand),
    Run,
}

#[derive(Debug, Parser)]
#[command(
    name = "aw-container-bootstrap",
    version,
    about = "In-container AW Gateway bootstrap entrypoint"
)]
pub struct BootstrapArgs {
    #[arg(long, env = "AW_CONTAINER_AGENT_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, env = "AW_CONTAINER_BOOTSTRAP_CONFIG", global = true)]
    pub bootstrap_config: Option<PathBuf>,

    #[arg(long, env = "AW_CONTAINER_AGENT_LOG_LEVEL", global = true)]
    pub log_level: Option<String>,
}
