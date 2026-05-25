use anyhow::Context;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchRunArgs {
    pub(crate) name: String,
    pub(crate) session_id: Option<String>,
    pub(crate) vars: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchRunArgRole {
    Name,
    Argument,
    SessionId,
    Variable,
}

pub(crate) fn parse_launch_run_strings(
    args: impl IntoIterator<Item = String>,
) -> anyhow::Result<LaunchRunArgs> {
    let mut args = args.into_iter();
    parse_launch_run_args_from(|_| Ok(args.next()))
}

pub(crate) fn parse_launch_run_args_from(
    mut next_arg: impl FnMut(LaunchRunArgRole) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<LaunchRunArgs> {
    let Some(name) = next_arg(LaunchRunArgRole::Name)? else {
        anyhow::bail!("launch requires a launch name");
    };

    let mut vars = Vec::new();
    let mut session_id = None;
    while let Some(arg) = next_arg(LaunchRunArgRole::Argument)? {
        if arg == "--json" {
            anyhow::bail!("launch execution does not support --json");
        }
        if let Some(value) = arg.strip_prefix("--session-id=") {
            set_session_id(&mut session_id, value.to_string())?;
            continue;
        }
        if arg == "--session-id" {
            let Some(value) = next_arg(LaunchRunArgRole::SessionId)? else {
                anyhow::bail!("--session-id requires a value");
            };
            set_session_id(&mut session_id, value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--var=") {
            validate_launch_var_pair(value)?;
            vars.push(value.to_string());
            continue;
        }
        if arg == "--var" {
            let Some(value) = next_arg(LaunchRunArgRole::Variable)? else {
                anyhow::bail!("--var must be key=value");
            };
            validate_launch_var_pair(&value)?;
            vars.push(value);
            continue;
        }
        anyhow::bail!("unexpected extra launch argument {arg:?}");
    }

    Ok(LaunchRunArgs {
        name,
        session_id,
        vars,
    })
}

fn set_session_id(slot: &mut Option<String>, value: String) -> anyhow::Result<()> {
    if slot.replace(value).is_some() {
        anyhow::bail!("--session-id may only be specified once");
    }
    Ok(())
}

fn validate_launch_var_pair(value: &str) -> anyhow::Result<()> {
    value
        .split_once('=')
        .context("--var must be key=value")
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launch_run_flags() {
        let parsed = parse_launch_run_strings([
            "repo-shell".into(),
            "--session-id=abc123".into(),
            "--var".into(),
            "repo=https://example.test/repo.git".into(),
            "--var=branch=main".into(),
        ])
        .unwrap();

        assert_eq!(
            parsed,
            LaunchRunArgs {
                name: "repo-shell".into(),
                session_id: Some("abc123".into()),
                vars: vec![
                    "repo=https://example.test/repo.git".into(),
                    "branch=main".into()
                ],
            }
        );
    }

    #[test]
    fn rejects_invalid_launch_run_flags() {
        for args in [
            vec!["repo-shell", "--json"],
            vec!["repo-shell", "--session-id", "a", "--session-id=b"],
            vec!["repo-shell", "--var"],
            vec!["repo-shell", "--var", "repo"],
            vec!["repo-shell", "extra"],
        ] {
            let err = parse_launch_run_strings(args.into_iter().map(str::to_string)).unwrap_err();
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn reports_missing_launch_name() {
        let err = parse_launch_run_strings(Vec::new()).unwrap_err();
        assert_eq!(err.to_string(), "launch requires a launch name");
    }
}
