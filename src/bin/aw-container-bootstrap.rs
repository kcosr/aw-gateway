use aw_gateway::bootstrap;
use aw_gateway::cli::BootstrapArgs;
use aw_gateway::logging;
use aw_gateway::paths;
use clap::Parser;

fn main() -> anyhow::Result<()> {
    let args = BootstrapArgs::parse();
    let config_path = paths::agent_config_path(args.config.clone());
    let _logging = logging::init_agent(Some(&config_path), args.log_level.as_deref())?;
    bootstrap::run(args)
}
