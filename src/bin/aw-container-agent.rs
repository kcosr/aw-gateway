use aw_gateway::agent;
use aw_gateway::cli::AgentArgs;
use aw_gateway::logging;
use aw_gateway::paths;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = AgentArgs::parse();
    let config_path = paths::agent_config_path(args.config.clone());
    let _logging = logging::init_agent(Some(&config_path), args.log_level.as_deref())?;
    agent::run(args).await
}
