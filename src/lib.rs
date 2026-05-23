mod action;
pub mod agent;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod gateway;
pub mod logging;
pub mod paths;
pub mod runtime;
pub mod ssh_dispatch;
pub mod ssh_filter;
pub mod template;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
