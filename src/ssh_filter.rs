use crate::config::{LegacyScpTransferMode, SftpTransferMode};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshCommandFilterPolicy {
    #[serde(default)]
    pub sftp: SftpTransferMode,
    #[serde(default)]
    pub legacy_scp: LegacyScpTransferMode,
}

impl Default for SshCommandFilterPolicy {
    fn default() -> Self {
        Self {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Allow,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshCommandDecision {
    LoginShell,
    RunCommand(String),
    RejectLegacyScp,
    RejectSftp,
    RejectShellComposition,
}

pub fn load_policy(path: &Path) -> anyhow::Result<SshCommandFilterPolicy> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read SSH command filter policy {}", path.display()))?;
    let policy = toml::from_str(&raw)
        .with_context(|| format!("parse SSH command filter policy {}", path.display()))?;
    Ok(policy)
}

pub fn decide_command(
    policy: &SshCommandFilterPolicy,
    original_command: Option<&str>,
) -> SshCommandDecision {
    let Some(command) = original_command
        .map(str::trim)
        .filter(|command| !command.is_empty())
    else {
        return SshCommandDecision::LoginShell;
    };

    if let Some(direction) = legacy_scp_server_direction(command)
        && !legacy_scp_mode_allows(policy.legacy_scp, direction)
    {
        return SshCommandDecision::RejectLegacyScp;
    }
    if !policy.sftp.allows() && is_sftp_server_command(command) {
        return SshCommandDecision::RejectSftp;
    }
    if policy_is_restrictive(policy) && contains_restricted_shell_invocation(command) {
        return SshCommandDecision::RejectShellComposition;
    }

    SshCommandDecision::RunCommand(command.to_string())
}

pub fn policy_is_restrictive(policy: &SshCommandFilterPolicy) -> bool {
    !policy.sftp.allows() || policy.legacy_scp != LegacyScpTransferMode::Allow
}

fn contains_restricted_shell_invocation(command: &str) -> bool {
    contains_shell_control_syntax(command) || uses_shell_prefix_or_wrapper(command)
}

fn contains_shell_control_syntax(command: &str) -> bool {
    command.bytes().any(|byte| {
        matches!(
            byte,
            b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'`' | b'$' | b'\n' | b'\r'
        )
    })
}

fn uses_shell_prefix_or_wrapper(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    let Some(first) = words.first() else {
        return false;
    };
    is_assignment_prefix(first)
        || matches!(
            path_basename(first),
            "bash"
                | "command"
                | "dash"
                | "env"
                | "eval"
                | "exec"
                | "fish"
                | "ksh"
                | "nice"
                | "nohup"
                | "setsid"
                | "sh"
                | "stdbuf"
                | "time"
                | "timeout"
                | "zsh"
        )
}

fn is_assignment_prefix(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub fn is_legacy_scp_server_command(command: &str) -> bool {
    legacy_scp_server_direction(command).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyScpDirection {
    Inbound,
    Outbound,
    Ambiguous,
}

pub fn legacy_scp_server_direction(command: &str) -> Option<LegacyScpDirection> {
    let Ok(words) = shell_words::split(command) else {
        return None;
    };
    let program = words.first()?;
    if path_basename(program) != "scp" {
        return None;
    }

    for arg in words.iter().skip(1) {
        if arg == "--" {
            return None;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let inbound = arg[1..].contains('t');
        let outbound = arg[1..].contains('f');
        if inbound && outbound {
            return Some(LegacyScpDirection::Ambiguous);
        }
        if inbound {
            return Some(LegacyScpDirection::Inbound);
        }
        if outbound {
            return Some(LegacyScpDirection::Outbound);
        }
    }

    None
}

pub fn legacy_scp_mode_allows(mode: LegacyScpTransferMode, direction: LegacyScpDirection) -> bool {
    match direction {
        LegacyScpDirection::Inbound => mode.allows_inbound(),
        LegacyScpDirection::Outbound => mode.allows_outbound(),
        LegacyScpDirection::Ambiguous => false,
    }
}

pub fn is_sftp_server_command(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    let Some(program) = words.first() else {
        return false;
    };
    matches!(path_basename(program), "sftp-server" | "internal-sftp")
}

pub fn shell_basename(shell: &str) -> &str {
    path_basename(shell)
}

fn path_basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_legacy_scp_server_commands() {
        for (command, direction) in [
            "scp -t /tmp/file",
            "scp -v -t /tmp/file",
            "/usr/bin/scp -qrt /tmp/dir",
        ]
        .into_iter()
        .map(|command| (command, LegacyScpDirection::Inbound))
        .chain(
            ["scp -f /tmp/file", "scp -d -f /tmp/dir"]
                .into_iter()
                .map(|command| (command, LegacyScpDirection::Outbound)),
        ) {
            assert!(
                is_legacy_scp_server_command(command),
                "expected legacy scp server command: {command}"
            );
            assert_eq!(legacy_scp_server_direction(command), Some(direction));
        }
    }

    #[test]
    fn does_not_flag_normal_commands_as_legacy_scp() {
        for command in [
            "echo scp -t /tmp/file",
            "scp source dest",
            "scp -- -t literal",
            "git status",
            "scp 'unterminated",
        ] {
            assert!(
                !is_legacy_scp_server_command(command),
                "unexpected legacy scp match: {command}"
            );
        }
    }

    #[test]
    fn detects_sftp_server_commands() {
        for command in [
            "internal-sftp",
            "/usr/libexec/openssh/sftp-server",
            "/usr/lib/openssh/sftp-server -e",
        ] {
            assert!(
                is_sftp_server_command(command),
                "expected sftp server command: {command}"
            );
        }
        assert!(!is_sftp_server_command("sftp example.com"));
        assert!(!is_sftp_server_command("echo internal-sftp"));
    }

    #[test]
    fn rejects_ambiguous_legacy_scp_direction() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Allow,
        };
        assert_eq!(
            legacy_scp_server_direction("scp -tf /tmp/file"),
            Some(LegacyScpDirection::Ambiguous)
        );
        assert_eq!(
            decide_command(&policy, Some("scp -tf /tmp/file")),
            SshCommandDecision::RejectLegacyScp
        );
    }

    #[test]
    fn decides_empty_original_command_as_login_shell() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Deny,
        };
        assert_eq!(
            decide_command(&policy, None),
            SshCommandDecision::LoginShell
        );
        assert_eq!(
            decide_command(&policy, Some("  ")),
            SshCommandDecision::LoginShell
        );
    }

    #[test]
    fn rejects_legacy_scp_only_when_policy_disallows_it() {
        assert_eq!(
            decide_command(
                &SshCommandFilterPolicy {
                    sftp: SftpTransferMode::Allow,
                    legacy_scp: LegacyScpTransferMode::Deny,
                },
                Some("scp -t /tmp/file"),
            ),
            SshCommandDecision::RejectLegacyScp
        );
        assert_eq!(
            decide_command(&SshCommandFilterPolicy::default(), Some("scp -t /tmp/file")),
            SshCommandDecision::RunCommand("scp -t /tmp/file".into())
        );
    }

    #[test]
    fn allows_legacy_scp_by_direction() {
        let outbound_only = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Outbound,
        };
        assert_eq!(
            decide_command(&outbound_only, Some("scp -f /tmp/file")),
            SshCommandDecision::RunCommand("scp -f /tmp/file".into())
        );
        assert_eq!(
            decide_command(&outbound_only, Some("scp -t /tmp/file")),
            SshCommandDecision::RejectLegacyScp
        );

        let inbound_only = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Inbound,
        };
        assert_eq!(
            decide_command(&inbound_only, Some("scp -t /tmp/file")),
            SshCommandDecision::RunCommand("scp -t /tmp/file".into())
        );
        assert_eq!(
            decide_command(&inbound_only, Some("scp -f /tmp/file")),
            SshCommandDecision::RejectLegacyScp
        );
    }

    #[test]
    fn rejects_sftp_server_when_policy_denies_sftp() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Allow,
        };
        assert_eq!(
            decide_command(&policy, Some("/usr/libexec/openssh/sftp-server")),
            SshCommandDecision::RejectSftp
        );
        assert_eq!(
            decide_command(
                &SshCommandFilterPolicy::default(),
                Some("/usr/libexec/openssh/sftp-server"),
            ),
            SshCommandDecision::RunCommand("/usr/libexec/openssh/sftp-server".into())
        );
    }

    #[test]
    fn rejects_shell_composition_when_policy_is_restrictive() {
        for policy in [
            SshCommandFilterPolicy {
                sftp: SftpTransferMode::Deny,
                legacy_scp: LegacyScpTransferMode::Allow,
            },
            SshCommandFilterPolicy {
                sftp: SftpTransferMode::Allow,
                legacy_scp: LegacyScpTransferMode::Deny,
            },
        ] {
            for command in [
                "true; scp -t /tmp/file",
                "true && scp -t /tmp/file",
                "printf hi | scp -t /tmp/file",
                "x=$(scp -t /tmp/file)",
                "echo `scp -t /tmp/file`",
                "x=1 scp -t /tmp/file",
                "command scp -t /tmp/file",
                "exec scp -t /tmp/file",
                "env /usr/libexec/openssh/sftp-server",
                "nice scp -t /tmp/file",
                "eval scp -t /tmp/file",
                "sh -c 'scp -t /tmp/file'",
                "bash -c 'scp -t /tmp/file'",
                "dash -c 'scp -t /tmp/file'",
                "fish -c 'scp -t /tmp/file'",
                "ksh -c 'scp -t /tmp/file'",
                "zsh -c 'scp -t /tmp/file'",
                "nohup scp -t /tmp/file",
                "time scp -t /tmp/file",
                "timeout 60 scp -t /tmp/file",
                "setsid scp -t /tmp/file",
                "stdbuf -oL scp -t /tmp/file",
            ] {
                assert_eq!(
                    decide_command(&policy, Some(command)),
                    SshCommandDecision::RejectShellComposition,
                    "command {command:?}"
                );
            }
        }
    }

    #[test]
    fn allows_shell_composition_when_policy_is_fully_allow() {
        assert_eq!(
            decide_command(&SshCommandFilterPolicy::default(), Some("true; echo ok")),
            SshCommandDecision::RunCommand("true; echo ok".into())
        );
    }

    #[test]
    fn allows_normal_command_exec() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Deny,
        };
        assert_eq!(
            decide_command(&policy, Some("printf hello")),
            SshCommandDecision::RunCommand("printf hello".into())
        );
    }
}
