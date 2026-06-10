use aw_gateway::cli::{GatewayArgs, GatewayCommand, GatewayConfigCommand};
use aw_gateway::context::parse_context_sources;
use aw_gateway::paths;
use aw_gateway::{gateway, logging};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = GatewayArgs::parse();
    let context = parse_context_sources(&args.context_files, &args.context)?;
    let protocol_mode = matches!(args.command, Some(GatewayCommand::Connect(_)));
    let config_path = match &args.command {
        Some(GatewayCommand::Config(GatewayConfigCommand::Paths(_))) => None,
        _ => paths::resolve_gateway_config(args.config.clone())?
            .selected_path()
            .ok(),
    };
    let _logging = logging::init_gateway(
        config_path.as_deref(),
        args.log_level.as_deref(),
        protocol_mode,
        &context,
    )?;
    gateway::run_with_context(args, context).await
}
