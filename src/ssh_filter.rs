use crate::config::{LegacyScpTransferMode, SftpTransferMode};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::Path;

const MAX_DISPLAYED_SSH_COMMAND_CHARS: usize = 1_000;
const TRUNCATION_MARKER: &str = "... [truncated]";

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
    RejectComposedTransfer,
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
    if contains_denied_transfer_invocation(policy, command) {
        return SshCommandDecision::RejectComposedTransfer;
    }

    SshCommandDecision::RunCommand(command.to_string())
}

pub fn format_ssh_original_command(command: &str) -> String {
    let mut escaped = String::with_capacity(command.len().min(MAX_DISPLAYED_SSH_COMMAND_CHARS));
    let mut rendered_chars = 0;
    let mut boundaries = vec![(0, 0)];

    for character in command.chars() {
        let rendered = if matches!(character, ' '..='~') {
            character.to_string()
        } else {
            character.escape_default().to_string()
        };
        let character_count = rendered.chars().count();
        if rendered_chars + character_count > MAX_DISPLAYED_SSH_COMMAND_CHARS {
            let marker_chars = TRUNCATION_MARKER.chars().count();
            while rendered_chars + marker_chars > MAX_DISPLAYED_SSH_COMMAND_CHARS {
                boundaries.pop();
                let &(byte_length, character_length) = boundaries.last().unwrap();
                escaped.truncate(byte_length);
                rendered_chars = character_length;
            }
            escaped.push_str(TRUNCATION_MARKER);
            return escaped;
        }
        escaped.push_str(&rendered);
        rendered_chars += character_count;
        boundaries.push((escaped.len(), rendered_chars));
    }

    escaped
}

// Best-effort scan for recognizable transfer-server commands inside shell
// composition. This is intentionally lexical rather than a complete shell
// parser: transfer policy disables common SSH transfer workflows, but arbitrary
// command execution remains capable of moving files through other means.
fn contains_denied_transfer_invocation(policy: &SshCommandFilterPolicy, command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return false;
    };

    for window in words.windows(3) {
        if is_shell_interpreter(&window[0])
            && window[1].starts_with('-')
            && window[1][1..].contains('c')
            && contains_denied_transfer_invocation(policy, &window[2])
        {
            return true;
        }
    }

    let tokens = words
        .iter()
        .flat_map(|word| word.split(is_shell_control_character))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();

    for (index, program) in tokens.iter().enumerate() {
        match path_basename(program) {
            "scp" => {
                if let Some(direction) =
                    legacy_scp_direction_from_args(tokens[index + 1..].iter().copied())
                    && !legacy_scp_mode_allows(policy.legacy_scp, direction)
                {
                    return true;
                }
            }
            "internal-sftp" | "sftp-server" if !policy.sftp.allows() => return true,
            _ => {}
        }
    }

    false
}

fn is_shell_control_character(character: char) -> bool {
    matches!(character, ';' | '|' | '&' | '(' | ')' | '`' | '\n' | '\r')
}

fn is_shell_interpreter(program: &str) -> bool {
    matches!(
        path_basename(program),
        "bash" | "dash" | "fish" | "ksh" | "sh" | "zsh"
    )
}

fn legacy_scp_direction_from_args<'a>(
    args: impl IntoIterator<Item = &'a str>,
) -> Option<LegacyScpDirection> {
    for arg in args {
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

    legacy_scp_direction_from_args(words.iter().skip(1).map(String::as_str))
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
    fn rejects_recognizable_denied_transfer_commands_in_shell_composition() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Deny,
        };
        for command in [
            "true; scp -t /tmp/file",
            "true && scp -t /tmp/file",
            "printf hi | scp -t /tmp/file",
            "x=$(scp -t /tmp/file)",
            "(scp -t /tmp/file)",
            "cat <(scp -f /tmp/file)",
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
                SshCommandDecision::RejectComposedTransfer,
                "command {command:?}"
            );
        }
    }

    #[test]
    fn composed_transfer_checks_respect_protocol_policy() {
        let sftp_only = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Allow,
        };
        assert_eq!(
            decide_command(&sftp_only, Some("true; scp -t /tmp/file")),
            SshCommandDecision::RunCommand("true; scp -t /tmp/file".into())
        );
        assert_eq!(
            decide_command(&sftp_only, Some("true; internal-sftp")),
            SshCommandDecision::RejectComposedTransfer
        );

        let scp_only = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Deny,
        };
        assert_eq!(
            decide_command(&scp_only, Some("true; internal-sftp")),
            SshCommandDecision::RunCommand("true; internal-sftp".into())
        );
        assert_eq!(
            decide_command(&scp_only, Some("true; scp -t /tmp/file")),
            SshCommandDecision::RejectComposedTransfer
        );
    }

    #[test]
    fn allows_ordinary_shell_composition_when_policy_is_restrictive() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Deny,
        };
        for command in [
            "true; echo ok",
            "printf hi | sed s/hi/ok/",
            "env printf hello",
            "x=1 printf hello",
            "echo 'scp -t /tmp/file'",
            "sh -c 'if [ -z \"$SHELL\" ] || [ ! -x \"$SHELL\" ]; then exit 127; fi; CODEX_REMOTE_PAYLOAD=\"$1\"; export CODEX_REMOTE_PAYLOAD; exec \"$SHELL\" -l -c \"$CODEX_REMOTE_PAYLOAD\"' sh payload",
        ] {
            assert_eq!(
                decide_command(&policy, Some(command)),
                SshCommandDecision::RunCommand(command.into()),
                "command {command:?}"
            );
        }
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

    #[test]
    fn allows_redirection_and_expansion_under_restrictive_policy() {
        let policy = SshCommandFilterPolicy {
            sftp: SftpTransferMode::Deny,
            legacy_scp: LegacyScpTransferMode::Deny,
        };
        for command in [
            "echo \"$HOME\"",
            "printf '%s' $PATH",
            "ls -la > /tmp/out",
            "grep needle < /tmp/in",
        ] {
            assert_eq!(
                decide_command(&policy, Some(command)),
                SshCommandDecision::RunCommand(command.into()),
                "command {command:?}"
            );
        }
    }

    #[test]
    fn formats_rejected_command_with_control_escaping_and_bounded_length() {
        let command = format!("printf hello\n{}", "x".repeat(1_200));
        let formatted = format_ssh_original_command(&command);

        assert!(formatted.starts_with("printf hello\\n"));
        assert!(formatted.ends_with(TRUNCATION_MARKER));
        assert_eq!(formatted.chars().count(), MAX_DISPLAYED_SSH_COMMAND_CHARS);
        assert!(!formatted.contains('\n'));
    }

    #[test]
    fn escapes_non_ascii_and_truncates_only_between_escape_sequences() {
        assert_eq!(
            format_ssh_original_command("printf \u{202e}secret\u{200b}"),
            "printf \\u{202e}secret\\u{200b}"
        );

        let command = format!("{}\u{202e}{}", "x".repeat(980), "y".repeat(100));
        let formatted = format_ssh_original_command(&command);
        let displayed = formatted.strip_suffix(TRUNCATION_MARKER).unwrap();
        assert!(displayed.chars().count() <= MAX_DISPLAYED_SSH_COMMAND_CHARS);
        assert!(!displayed.ends_with('\\'));
        assert!(!displayed.ends_with("\\u"));
        assert!(!displayed.contains("\\u{202e"));
    }
}
