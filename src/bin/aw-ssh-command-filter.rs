use aw_gateway::ssh_filter::{
    SshCommandDecision, decide_command, format_ssh_original_command, load_policy, shell_basename,
};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Container-side SSH command policy filter")]
struct Args {
    #[arg(long)]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.config)?;
    let original = std::env::var("SSH_ORIGINAL_COMMAND").ok();

    match decide_command(&policy, original.as_deref()) {
        SshCommandDecision::LoginShell => exec_login_shell(),
        SshCommandDecision::RunCommand(command) => exec_shell_command(&command),
        SshCommandDecision::RejectLegacyScp => {
            eprintln!("blocked by policy: legacy scp is not allowed");
            std::process::exit(1);
        }
        SshCommandDecision::RejectSftp => {
            eprintln!("blocked by policy: sftp is not allowed");
            std::process::exit(1);
        }
        SshCommandDecision::RejectComposedTransfer => {
            eprintln!("blocked by policy: shell composition invokes a restricted transfer command");
            eprintln!(
                "rejected SSH_ORIGINAL_COMMAND: {}",
                format_ssh_original_command(original.as_deref().unwrap_or_default())
            );
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn exec_login_shell() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let argv0 = format!("-{}", shell_basename(&shell));
    let err = std::process::Command::new(&shell).arg0(argv0).exec();
    Err(anyhow::Error::new(err).context(format!("exec login shell {shell}")))
}

#[cfg(unix)]
fn exec_shell_command(command: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let err = std::process::Command::new(&shell)
        .arg("-c")
        .arg(command)
        .exec();
    Err(anyhow::Error::new(err).context(format!("exec shell command with {shell}")))
}

#[cfg(not(unix))]
fn exec_login_shell() -> anyhow::Result<()> {
    anyhow::bail!("aw-ssh-command-filter requires a Unix platform")
}

#[cfg(not(unix))]
fn exec_shell_command(_command: &str) -> anyhow::Result<()> {
    anyhow::bail!("aw-ssh-command-filter requires a Unix platform")
}
