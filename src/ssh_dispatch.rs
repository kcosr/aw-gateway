use crate::config::SshDispatchConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    InteractiveShell,
    ContainerCommand(String),
    Gateway(GatewayAction),
    Reject(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayAction {
    Connect(TargetSessionAction),
    Up(Option<String>),
    Run(RunAction),
    Launches {
        json: bool,
    },
    LaunchShow {
        name: String,
        json: bool,
    },
    LaunchRun {
        name: String,
        session_id: Option<String>,
        vars: Vec<String>,
    },
    Status(StatusAction),
    Targets {
        json: bool,
    },
    Stop(Option<String>),
    Remove(Option<String>),
    SetDefault(String),
    ShowDefault,
    ResetDefault,
    AddKey(KeyAction),
    AddHostKey(KeySourceAction),
    AddContainerKey(KeyAction),
    ClientConfig(ClientConfigAction),
    ClientBundle(ClientBundleAction),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TargetSessionAction {
    pub target: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunAction {
    pub target: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusAction {
    pub target: Option<String>,
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyAction {
    pub target: Option<String>,
    pub public_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeySourceAction {
    pub public_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientConfigAction {
    pub target: Option<String>,
    pub identity_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientBundleAction {
    pub target: Option<String>,
    pub identity_file: Option<String>,
    pub rotate_key: bool,
}

pub fn dispatch(
    original_command: Option<&str>,
    has_pty: bool,
    cfg: &SshDispatchConfig,
) -> Dispatch {
    let Some(command) = original_command.filter(|value| !value.trim().is_empty()) else {
        return if has_pty && cfg.allow_interactive_shell {
            Dispatch::InteractiveShell
        } else {
            Dispatch::Reject("no command and no interactive PTY".into())
        };
    };

    match parse_gateway_action(command, cfg) {
        Ok(Some(action)) => Dispatch::Gateway(action),
        Ok(None) if cfg.allow_container_commands => Dispatch::ContainerCommand(command.to_string()),
        Ok(None) => Dispatch::Reject("container command passthrough is disabled".into()),
        Err(err) => Dispatch::Reject(err.to_string()),
    }
}

pub fn parse_gateway_action(
    command: &str,
    cfg: &SshDispatchConfig,
) -> anyhow::Result<Option<GatewayAction>> {
    let words = shell_words::split(command)?;
    if words.is_empty() {
        return Ok(None);
    }
    if words.first().is_some_and(|word| word == "--") {
        anyhow::bail!("malformed gateway command starts with --");
    }

    let mut words = words.as_slice();
    if words
        .first()
        .is_some_and(|word| is_gateway_program_word(word))
    {
        words = &words[1..];
    }
    parse_native(words, cfg)
}

fn is_gateway_program_word(word: &str) -> bool {
    if matches!(word, "aw-gateway" | "/opt/aw-gateway/bin/aw-gateway") {
        return true;
    }
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    if current == std::path::Path::new(word) {
        return true;
    }
    current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| word == name || word.ends_with(&format!("/{name}")))
}

fn parse_native(
    words: &[String],
    cfg: &SshDispatchConfig,
) -> anyhow::Result<Option<GatewayAction>> {
    let Some(action) = words.first().map(String::as_str) else {
        return Ok(None);
    };
    let parsed = match action {
        "connect" if action_enabled(cfg, "connect") => {
            GatewayAction::Connect(parse_target_session_action(words)?)
        }
        "up" if words.len() <= 2 && action_enabled(cfg, "up") => {
            GatewayAction::Up(words.get(1).cloned())
        }
        "run" if action_enabled(cfg, "run") => parse_run_action(words)?,
        "launches" if action_enabled(cfg, "launches") => GatewayAction::Launches {
            json: parse_json_flag(words, "launches")?,
        },
        "launch" if action_enabled(cfg, "launch") => parse_launch_action(words)?,
        "status" if action_enabled(cfg, "status") => parse_status_action(words)?,
        "targets" if action_enabled(cfg, "targets") => GatewayAction::Targets {
            json: parse_json_flag(words, "targets")?,
        },
        "stop" if words.len() <= 2 && action_enabled(cfg, "stop") => {
            GatewayAction::Stop(words.get(1).cloned())
        }
        "remove" | "rm" if words.len() <= 2 && action_enabled(cfg, "remove") => {
            GatewayAction::Remove(words.get(1).cloned())
        }
        "set-default" if words.len() == 2 && action_enabled(cfg, "set-default") => {
            GatewayAction::SetDefault(words[1].clone())
        }
        "show-default" if words.len() == 1 && action_enabled(cfg, "show-default") => {
            GatewayAction::ShowDefault
        }
        "reset-default" if words.len() == 1 && action_enabled(cfg, "reset-default") => {
            GatewayAction::ResetDefault
        }
        "add-key" if action_enabled(cfg, "add-key") => {
            GatewayAction::AddKey(parse_key_action(words)?)
        }
        "add-host-key" if action_enabled(cfg, "add-host-key") => {
            GatewayAction::AddHostKey(parse_key_source_action(words)?)
        }
        "add-container-key" if action_enabled(cfg, "add-container-key") => {
            GatewayAction::AddContainerKey(parse_key_action(words)?)
        }
        "client-config" if action_enabled(cfg, "client-config") => {
            parse_client_config_action(words)?
        }
        "client-bundle" if action_enabled(cfg, "client-bundle") => {
            parse_client_bundle_action(words)?
        }
        "help" if words.len() == 1 && action_enabled(cfg, "help") => GatewayAction::Help,
        "connect" | "up" | "run" | "launches" | "launch" | "status" | "targets" | "stop"
        | "remove" | "rm" | "set-default" | "show-default" | "reset-default" | "add-key"
        | "add-host-key" | "add-container-key" | "client-config" | "client-bundle" | "help" => {
            anyhow::bail!(
                "invalid or disabled gateway action shape: {}",
                words.join(" ")
            );
        }
        _ => return Ok(None),
    };
    Ok(Some(parsed))
}

fn parse_status_action(words: &[String]) -> anyhow::Result<GatewayAction> {
    let mut action = StatusAction::default();
    let mut index = 1;
    if let Some(value) = words.get(index)
        && !value.starts_with('-')
    {
        action.target = Some(value.clone());
        index += 1;
    }
    while let Some(flag) = words.get(index).map(String::as_str) {
        match flag {
            "--all" => {
                action.all = true;
                index += 1;
            }
            "--json" => {
                index += 1;
            }
            _ => anyhow::bail!("invalid status option {flag:?}"),
        }
    }
    if action.all && action.target.is_some() {
        anyhow::bail!("--all cannot be combined with a target");
    }
    Ok(GatewayAction::Status(action))
}

fn parse_target_session_action(words: &[String]) -> anyhow::Result<TargetSessionAction> {
    let mut action = TargetSessionAction::default();
    let mut index = 1;
    while let Some(arg) = words.get(index).map(String::as_str) {
        if let Some(value) = arg.strip_prefix("--session-id=") {
            set_session_id(&mut action.session_id, value.to_string())?;
            index += 1;
            continue;
        }
        match arg {
            "--session-id" => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--session-id requires a value"))?;
                set_session_id(&mut action.session_id, value.clone())?;
                index += 2;
            }
            value if !value.starts_with('-') => {
                if action.target.replace(value.to_string()).is_some() {
                    anyhow::bail!("target may only be specified once");
                }
                index += 1;
            }
            _ => anyhow::bail!("invalid connect option {arg:?}"),
        }
    }
    Ok(action)
}

fn set_session_id(slot: &mut Option<String>, value: String) -> anyhow::Result<()> {
    if slot.replace(value).is_some() {
        anyhow::bail!("--session-id may only be specified once");
    }
    Ok(())
}

fn parse_run_action(words: &[String]) -> anyhow::Result<GatewayAction> {
    let mut action = RunAction::default();
    let mut index = 1;

    while let Some(flag) = words.get(index).map(String::as_str) {
        if let Some(value) = flag.strip_prefix("--session-id=") {
            set_session_id(&mut action.session_id, value.to_string())?;
            index += 1;
            continue;
        }
        match flag {
            "--cwd" => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--cwd requires a value"))?;
                action.cwd = Some(value.clone());
                index += 2;
            }
            "--session-id" => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--session-id requires a value"))?;
                set_session_id(&mut action.session_id, value.clone())?;
                index += 2;
            }
            "--" => {
                action.command = words[index + 1..].to_vec();
                if action.command.is_empty() {
                    anyhow::bail!("run requires -- followed by a command");
                }
                return Ok(GatewayAction::Run(action));
            }
            value if !value.starts_with('-') => {
                if action.target.replace(value.to_string()).is_some() {
                    anyhow::bail!("target may only be specified once");
                }
                index += 1;
            }
            _ => anyhow::bail!("invalid run option {flag:?}"),
        }
    }

    anyhow::bail!("run requires -- followed by a command")
}

fn parse_launch_action(words: &[String]) -> anyhow::Result<GatewayAction> {
    let Some(name) = words.get(1) else {
        anyhow::bail!("launch requires a launch name");
    };
    if name == "show" {
        let Some(launch_name) = words.get(2) else {
            anyhow::bail!("launch show requires a launch name");
        };
        let json = match words {
            [command, subcommand, _] if command == "launch" && subcommand == "show" => false,
            [command, subcommand, _, flag]
                if command == "launch" && subcommand == "show" && flag == "--json" =>
            {
                true
            }
            _ => anyhow::bail!(
                "invalid or disabled gateway action shape: {}",
                words.join(" ")
            ),
        };
        return Ok(GatewayAction::LaunchShow {
            name: launch_name.clone(),
            json,
        });
    }

    let mut vars = Vec::new();
    let mut session_id = None;
    let mut index = 2;
    while let Some(arg) = words.get(index).map(String::as_str) {
        if arg == "--json" {
            anyhow::bail!("launch execution does not support --json");
        }
        if let Some(value) = arg.strip_prefix("--session-id=") {
            set_session_id(&mut session_id, value.to_string())?;
            index += 1;
            continue;
        }
        if arg == "--session-id" {
            let Some(value) = words.get(index + 1) else {
                anyhow::bail!("--session-id requires a value");
            };
            set_session_id(&mut session_id, value.clone())?;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--var=") {
            vars.push(value.to_string());
            index += 1;
            continue;
        }
        if arg == "--var" {
            let Some(value) = words.get(index + 1) else {
                anyhow::bail!("--var must be key=value");
            };
            vars.push(value.clone());
            index += 2;
            continue;
        }
        anyhow::bail!("unexpected extra launch argument {arg:?}");
    }

    Ok(GatewayAction::LaunchRun {
        name: name.clone(),
        session_id,
        vars,
    })
}

fn parse_json_flag(words: &[String], command: &str) -> anyhow::Result<bool> {
    match words {
        [value] if value == command => Ok(false),
        [value, flag] if value == command && flag == "--json" => Ok(true),
        _ => anyhow::bail!(
            "invalid or disabled gateway action shape: {}",
            words.join(" ")
        ),
    }
}

fn parse_key_action(words: &[String]) -> anyhow::Result<KeyAction> {
    let mut action = KeyAction::default();
    let mut index = 1;
    if let Some(value) = words.get(index)
        && !value.starts_with('-')
    {
        action.target = Some(value.clone());
        index += 1;
    }
    parse_public_key_flag(words, &mut index, |value| {
        action.public_key = Some(value);
    })?;
    Ok(action)
}

fn parse_key_source_action(words: &[String]) -> anyhow::Result<KeySourceAction> {
    let mut action = KeySourceAction::default();
    let mut index = 1;
    parse_public_key_flag(words, &mut index, |value| {
        action.public_key = Some(value);
    })?;
    Ok(action)
}

fn parse_public_key_flag<F>(
    words: &[String],
    index: &mut usize,
    mut set_public_key: F,
) -> anyhow::Result<()>
where
    F: FnMut(String),
{
    let mut seen_public_key = false;
    while let Some(flag) = words.get(*index).map(String::as_str) {
        match flag {
            "--public-key" => {
                if seen_public_key {
                    anyhow::bail!("--public-key may only be specified once");
                }
                let value = words
                    .get(*index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--public-key requires a value"))?;
                set_public_key(value.clone());
                seen_public_key = true;
                *index += 2;
            }
            _ => anyhow::bail!("invalid key option {flag:?}"),
        }
    }
    Ok(())
}

fn action_enabled(cfg: &SshDispatchConfig, name: &str) -> bool {
    cfg.enabled_actions.iter().any(|value| value == name)
}

fn parse_client_config_action(words: &[String]) -> anyhow::Result<GatewayAction> {
    let mut action = ClientConfigAction::default();
    let mut index = 1;
    if let Some(value) = words.get(index)
        && !value.starts_with('-')
    {
        action.target = Some(value.clone());
        index += 1;
    }
    while let Some(flag) = words.get(index).map(String::as_str) {
        match flag {
            "--identity-file" => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--identity-file requires a value"))?;
                action.identity_file = Some(value.clone());
                index += 2;
            }
            _ => anyhow::bail!("invalid client-config option {flag:?}"),
        }
    }
    Ok(GatewayAction::ClientConfig(action))
}

fn parse_client_bundle_action(words: &[String]) -> anyhow::Result<GatewayAction> {
    let mut action = ClientBundleAction::default();
    let mut index = 1;
    if let Some(value) = words.get(index)
        && !value.starts_with('-')
    {
        action.target = Some(value.clone());
        index += 1;
    }
    while let Some(flag) = words.get(index).map(String::as_str) {
        match flag {
            "--rotate-key" => {
                action.rotate_key = true;
                index += 1;
            }
            "--identity-file" => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| anyhow::anyhow!("--identity-file requires a value"))?;
                action.identity_file = Some(value.clone());
                index += 2;
            }
            _ => anyhow::bail!("invalid client-bundle option {flag:?}"),
        }
    }
    Ok(GatewayAction::ClientBundle(action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SshDispatchConfig;

    #[test]
    fn parses_proxy_command_action() {
        let cfg = SshDispatchConfig::default();
        let action =
            parse_gateway_action("/opt/aw-gateway/bin/aw-gateway connect default", &cfg).unwrap();
        assert_eq!(
            action,
            Some(GatewayAction::Connect(TargetSessionAction {
                target: Some("default".into()),
                session_id: None,
            }))
        );
        let action =
            parse_gateway_action("/opt/aw-gateway/bin/aw-gateway up default", &cfg).unwrap();
        assert_eq!(action, Some(GatewayAction::Up(Some("default".into()))));
        let action = parse_gateway_action(
            "/opt/aw-gateway/bin/aw-gateway run default -- bash -l",
            &cfg,
        )
        .unwrap();
        assert_eq!(
            action,
            Some(GatewayAction::Run(RunAction {
                target: Some("default".into()),
                session_id: None,
                cwd: None,
                command: vec!["bash".into(), "-l".into()],
            }))
        );
    }

    #[test]
    fn rejects_leading_dashdash() {
        let cfg = SshDispatchConfig::default();
        assert!(parse_gateway_action("-- connect default", &cfg).is_err());
    }

    #[test]
    fn parses_client_config_and_bundle_onboarding_flags() {
        let cfg = SshDispatchConfig::default();
        assert_eq!(
            parse_gateway_action(
                "/opt/aw-gateway/bin/aw-gateway client-bundle default --rotate-key",
                &cfg
            )
            .unwrap(),
            Some(GatewayAction::ClientBundle(ClientBundleAction {
                target: Some("default".into()),
                rotate_key: true,
                identity_file: None,
            }))
        );
        assert!(parse_gateway_action("client-config --identity-file", &cfg).is_err());
        assert!(parse_gateway_action("client-config --print", &cfg).is_err());
        assert_eq!(
            parse_gateway_action("add-key default --public-key -", &cfg).unwrap(),
            Some(GatewayAction::AddKey(KeyAction {
                target: Some("default".into()),
                public_key: Some("-".into()),
            }))
        );
        assert_eq!(
            parse_gateway_action("add-host-key --public-key -", &cfg).unwrap(),
            Some(GatewayAction::AddHostKey(KeySourceAction {
                public_key: Some("-".into()),
            }))
        );
        assert_eq!(
            parse_gateway_action("add-container-key default --public-key -", &cfg).unwrap(),
            Some(GatewayAction::AddContainerKey(KeyAction {
                target: Some("default".into()),
                public_key: Some("-".into()),
            }))
        );
        assert!(parse_gateway_action("add-host-key default --public-key -", &cfg).is_err());
        assert!(
            parse_gateway_action(
                "add-container-key default --public-key - --public-key -",
                &cfg
            )
            .is_err()
        );
    }

    #[test]
    fn parses_help_action() {
        let cfg = SshDispatchConfig::default();
        for (command, expected) in [
            (
                "connect default",
                Some(GatewayAction::Connect(TargetSessionAction {
                    target: Some("default".into()),
                    session_id: None,
                })),
            ),
            (
                "up default",
                Some(GatewayAction::Up(Some("default".into()))),
            ),
            (
                "status default",
                Some(GatewayAction::Status(StatusAction {
                    target: Some("default".into()),
                    all: false,
                })),
            ),
            (
                "stop default",
                Some(GatewayAction::Stop(Some("default".into()))),
            ),
            (
                "remove default",
                Some(GatewayAction::Remove(Some("default".into()))),
            ),
            ("help", Some(GatewayAction::Help)),
        ] {
            assert_eq!(parse_gateway_action(command, &cfg).unwrap(), expected);
        }
        assert_eq!(
            parse_gateway_action("targets", &cfg).unwrap(),
            Some(GatewayAction::Targets { json: false })
        );
        assert_eq!(
            parse_gateway_action("targets --json", &cfg).unwrap(),
            Some(GatewayAction::Targets { json: true })
        );
        assert_eq!(
            parse_gateway_action("run ubuntu-base --cwd /tmp -- pwd", &cfg).unwrap(),
            Some(GatewayAction::Run(RunAction {
                target: Some("ubuntu-base".into()),
                session_id: None,
                cwd: Some("/tmp".into()),
                command: vec!["pwd".into()],
            }))
        );
        assert_eq!(
            parse_gateway_action("run -- pwd", &cfg).unwrap(),
            Some(GatewayAction::Run(RunAction {
                target: None,
                session_id: None,
                cwd: None,
                command: vec!["pwd".into()],
            }))
        );
        assert!(parse_gateway_action("run", &cfg).is_err());
        assert!(parse_gateway_action("run default", &cfg).is_err());
        assert!(parse_gateway_action("run default --", &cfg).is_err());
        assert!(parse_gateway_action("run default --cwd /tmp", &cfg).is_err());
        assert!(parse_gateway_action("run ubuntu-base --bad -- pwd", &cfg).is_err());
        assert!(parse_gateway_action("targets extra", &cfg).is_err());
        assert_eq!(
            parse_gateway_action("status --all", &cfg).unwrap(),
            Some(GatewayAction::Status(StatusAction {
                target: None,
                all: true,
            }))
        );
        assert_eq!(
            parse_gateway_action("status --all --json", &cfg).unwrap(),
            Some(GatewayAction::Status(StatusAction {
                target: None,
                all: true,
            }))
        );
        assert!(parse_gateway_action("status default --all", &cfg).is_err());
        assert!(parse_gateway_action("status --bad", &cfg).is_err());
        assert_eq!(
            parse_gateway_action("remove default", &cfg).unwrap(),
            Some(GatewayAction::Remove(Some("default".into())))
        );
        assert_eq!(
            parse_gateway_action("rm default", &cfg).unwrap(),
            Some(GatewayAction::Remove(Some("default".into())))
        );
        assert!(
            parse_gateway_action("gateway-help", &cfg)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parses_connect_and_run_session_id_forms() {
        let cfg = SshDispatchConfig::default();
        for command in [
            "connect default --session-id abc123def456",
            "connect --session-id abc123def456 default",
            "connect default --session-id=abc123def456",
            "connect --session-id=abc123def456 default",
        ] {
            assert_eq!(
                parse_gateway_action(command, &cfg).unwrap(),
                Some(GatewayAction::Connect(TargetSessionAction {
                    target: Some("default".into()),
                    session_id: Some("abc123def456".into()),
                })),
                "command {command:?}"
            );
        }

        for command in [
            "run default --session-id abc123def456 -- bash -l",
            "run --session-id abc123def456 default -- bash -l",
            "run default --session-id=abc123def456 -- bash -l",
            "run --session-id=abc123def456 default -- bash -l",
        ] {
            assert_eq!(
                parse_gateway_action(command, &cfg).unwrap(),
                Some(GatewayAction::Run(RunAction {
                    target: Some("default".into()),
                    session_id: Some("abc123def456".into()),
                    cwd: None,
                    command: vec!["bash".into(), "-l".into()],
                })),
                "command {command:?}"
            );
        }
    }

    #[test]
    fn preserves_target_first_run_cwd_shape_with_session_id_forms() {
        let cfg = SshDispatchConfig::default();
        assert_eq!(
            parse_gateway_action("run default --cwd /workspace -- cargo test", &cfg).unwrap(),
            Some(GatewayAction::Run(RunAction {
                target: Some("default".into()),
                session_id: None,
                cwd: Some("/workspace".into()),
                command: vec!["cargo".into(), "test".into()],
            }))
        );
        assert_eq!(
            parse_gateway_action(
                "run default --session-id abc123def456 --cwd /workspace -- cargo test",
                &cfg,
            )
            .unwrap(),
            Some(GatewayAction::Run(RunAction {
                target: Some("default".into()),
                session_id: Some("abc123def456".into()),
                cwd: Some("/workspace".into()),
                command: vec!["cargo".into(), "test".into()],
            }))
        );
    }

    #[test]
    fn rejects_malformed_session_id_actions() {
        let cfg = SshDispatchConfig::default();
        for command in [
            "connect default --session-id",
            "connect --session-id abc --session-id def default",
            "connect first second --session-id abc",
            "run default --session-id",
            "run --session-id abc --session-id def default -- pwd",
            "run first second --session-id abc -- pwd",
            "launch show repo-shell --session-id abc",
            "launch repo-shell --session-id",
            "launch repo-shell --session-id abc --session-id def",
        ] {
            assert!(
                parse_gateway_action(command, &cfg).is_err(),
                "expected {command:?} to fail"
            );
        }
    }

    #[test]
    fn parses_launch_actions() {
        let cfg = SshDispatchConfig::default();
        for (command, expected) in [
            ("launches", Some(GatewayAction::Launches { json: false })),
            (
                "launches --json",
                Some(GatewayAction::Launches { json: true }),
            ),
            (
                "launch show repo-shell",
                Some(GatewayAction::LaunchShow {
                    name: "repo-shell".into(),
                    json: false,
                }),
            ),
            (
                "launch show repo-shell --json",
                Some(GatewayAction::LaunchShow {
                    name: "repo-shell".into(),
                    json: true,
                }),
            ),
            (
                "launch repo-shell --var repo=https://example.test/repo.git --var branch=main",
                Some(GatewayAction::LaunchRun {
                    name: "repo-shell".into(),
                    session_id: None,
                    vars: vec![
                        "repo=https://example.test/repo.git".into(),
                        "branch=main".into(),
                    ],
                }),
            ),
            (
                "launch repo-shell --var=repo=https://example.test/repo.git",
                Some(GatewayAction::LaunchRun {
                    name: "repo-shell".into(),
                    session_id: None,
                    vars: vec!["repo=https://example.test/repo.git".into()],
                }),
            ),
            (
                "launch repo-shell --session-id abc123def456 --var repo=https://example.test/repo.git",
                Some(GatewayAction::LaunchRun {
                    name: "repo-shell".into(),
                    session_id: Some("abc123def456".into()),
                    vars: vec!["repo=https://example.test/repo.git".into()],
                }),
            ),
            (
                "launch repo-shell --session-id=abc123def456 --var=repo=https://example.test/repo.git",
                Some(GatewayAction::LaunchRun {
                    name: "repo-shell".into(),
                    session_id: Some("abc123def456".into()),
                    vars: vec!["repo=https://example.test/repo.git".into()],
                }),
            ),
        ] {
            assert_eq!(parse_gateway_action(command, &cfg).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_malformed_launch_actions() {
        let cfg = SshDispatchConfig::default();
        for command in [
            "launches extra",
            "launch",
            "launch show",
            "launch show repo-shell --var repo=x",
            "launch show repo-shell extra",
            "launch repo-shell --json",
            "launch repo-shell --var",
            "launch repo-shell extra",
        ] {
            assert!(
                parse_gateway_action(command, &cfg).is_err(),
                "expected {command:?} to fail"
            );
        }
    }

    #[test]
    fn disabled_launch_actions_are_rejected() {
        let mut cfg = SshDispatchConfig::default();
        cfg.enabled_actions
            .retain(|action| action != "launch" && action != "launches");
        for command in [
            "launches",
            "launches --json",
            "launch show repo-shell",
            "launch repo-shell --var repo=x",
        ] {
            let err = parse_gateway_action(command, &cfg).unwrap_err();
            assert!(
                err.to_string()
                    .contains("invalid or disabled gateway action shape"),
                "{err}"
            );
        }
    }

    #[test]
    fn disabled_session_actions_are_rejected() {
        let mut cfg = SshDispatchConfig::default();
        cfg.enabled_actions
            .retain(|action| action != "connect" && action != "run" && action != "launch");
        for command in [
            "connect default --session-id abc123def456",
            "run default --session-id abc123def456 -- pwd",
            "launch repo-shell --session-id abc123def456 --var repo=x",
        ] {
            let err = parse_gateway_action(command, &cfg).unwrap_err();
            assert!(
                err.to_string()
                    .contains("invalid or disabled gateway action shape"),
                "{err}"
            );
        }
    }

    #[test]
    fn parses_default_selection_actions() {
        let cfg = SshDispatchConfig::default();
        assert_eq!(
            parse_gateway_action("show-default", &cfg).unwrap(),
            Some(GatewayAction::ShowDefault)
        );
        assert_eq!(
            parse_gateway_action("reset-default", &cfg).unwrap(),
            Some(GatewayAction::ResetDefault)
        );
        assert!(parse_gateway_action("show-default extra", &cfg).is_err());
        assert!(parse_gateway_action("reset-default extra", &cfg).is_err());
    }
}
