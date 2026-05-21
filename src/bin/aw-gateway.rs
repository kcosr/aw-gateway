use aw_gateway::cli::{GatewayArgs, GatewayCommand};
use aw_gateway::paths;
use aw_gateway::{gateway, logging};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = GatewayArgs::parse();
    let protocol_mode = matches!(args.command, Some(GatewayCommand::Connect(_)));
    let config_path = (!matches!(args.command, Some(GatewayCommand::Config(_))))
        .then(|| paths::gateway_config_path(args.config.clone()));
    let _logging = logging::init_gateway(
        config_path.as_deref(),
        args.log_level.as_deref(),
        protocol_mode,
    )?;
    gateway::run(args).await
}
