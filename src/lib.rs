mod action;
pub mod agent;
mod agent_control;
pub mod bootstrap;
pub mod cli;
pub mod config;
mod fileutil;
pub mod gateway;
mod health_probe;
pub mod logging;
pub mod paths;
mod rotating_log;
pub mod runtime;
pub mod ssh_dispatch;
pub mod ssh_filter;
pub mod template;
mod unix_priv;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
