use super::model::{
    AllStatusEntry, GatewayStatus, LaunchDetail, LaunchSummary, ReadyStatus, TargetEntry,
};
use crate::cli::{
    ClientConfigArgs, LaunchShowArgs, LaunchesArgs, RunArgs, SetDefaultArgs, StatusArg, StopArgs,
    TargetArg, TargetsArgs, UpArgs,
};
use crate::ssh_dispatch::{GatewayAction, RunAction, StatusAction};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GatewayOperation {
    Targets,
    Status {
        target: Option<String>,
        session_id: Option<String>,
    },
    StatusAll,
    Up {
        target: Option<String>,
        session_id: Option<String>,
    },
    Run {
        target: Option<String>,
        session_id: Option<String>,
        cwd: Option<String>,
        command: Vec<String>,
    },
    Launches,
    LaunchShow {
        name: String,
    },
    Launch {
        name: String,
        session_id: Option<String>,
        vars: Vec<String>,
    },
    Stop {
        target: Option<String>,
        session_id: Option<String>,
    },
    Remove {
        target: Option<String>,
    },
    SetDefault {
        target_or_image: String,
    },
    ShowDefault,
    ResetDefault,
    ClientConfig {
        target: Option<String>,
        identity_file: Option<PathBuf>,
    },
}

#[derive(Debug)]
pub(super) enum GatewayOperationResult {
    Targets(Vec<TargetEntry>),
    Status(GatewayStatus),
    StatusAll(Vec<AllStatusEntry>),
    Up(ReadyStatus),
    Run(ExecutionOutcome),
    Launches(Vec<LaunchSummary>),
    LaunchShow(LaunchDetail),
    Launch(ExecutionOutcome),
    Stop(StopResult),
    Remove(RemoveResult),
    DefaultSelection(String),
    ClientConfig {
        rendered: String,
        written_path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExecutionOutcome {
    exit_code: i32,
}

impl ExecutionOutcome {
    pub(super) fn new(exit_code: i32) -> Self {
        Self { exit_code }
    }

    pub(super) fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StopResult {
    pub(super) container: String,
    pub(super) stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoveResult {
    pub(super) container: String,
    pub(super) removed: bool,
}

impl GatewayOperation {
    pub(super) fn from_targets_args(_args: TargetsArgs) -> Self {
        Self::Targets
    }

    pub(super) fn from_status_args(args: StatusArg) -> Self {
        if args.all {
            Self::StatusAll
        } else {
            Self::Status {
                target: args.target,
                session_id: args.session_id,
            }
        }
    }

    pub(super) fn from_up_args(args: UpArgs) -> Self {
        Self::Up {
            target: args.target,
            session_id: args.session_id,
        }
    }

    pub(super) fn from_run_args(args: RunArgs) -> anyhow::Result<Self> {
        if args.command.is_empty() {
            anyhow::bail!(
                "run requires -- followed by a command; use up to start or hold a target"
            );
        }
        Ok(Self::Run {
            target: args.target,
            session_id: args.session_id,
            cwd: args.cwd,
            command: args.command,
        })
    }

    pub(super) fn from_launches_args(_args: LaunchesArgs) -> Self {
        Self::Launches
    }

    pub(super) fn from_launch_show_args(args: LaunchShowArgs) -> Self {
        Self::LaunchShow { name: args.name }
    }

    pub(super) fn launch_run(name: String, session_id: Option<String>, vars: Vec<String>) -> Self {
        Self::Launch {
            name,
            session_id,
            vars,
        }
    }

    pub(super) fn from_stop_args(args: StopArgs) -> Self {
        Self::Stop {
            target: args.target,
            session_id: args.session_id,
        }
    }

    pub(super) fn from_remove_args(args: TargetArg) -> Self {
        Self::Remove {
            target: args.target,
        }
    }

    pub(super) fn from_set_default_args(args: SetDefaultArgs) -> anyhow::Result<Self> {
        if args.reset {
            return Ok(Self::ResetDefault);
        }
        let target_or_image = args
            .target_or_image
            .ok_or_else(|| anyhow::anyhow!("target or image is required unless --reset is used"))?;
        Ok(Self::SetDefault { target_or_image })
    }

    pub(super) fn from_client_config_args(args: ClientConfigArgs) -> Self {
        Self::ClientConfig {
            target: args.target,
            identity_file: args.identity_file,
        }
    }

    pub(super) fn from_ssh_action(action: GatewayAction) -> Option<Self> {
        match action {
            GatewayAction::Up(target) => Some(Self::Up {
                target,
                session_id: None,
            }),
            GatewayAction::Run(action) => Some(Self::from_run_action(action)),
            GatewayAction::Launches { .. } => Some(Self::Launches),
            GatewayAction::LaunchShow { name, .. } => Some(Self::LaunchShow { name }),
            GatewayAction::LaunchRun {
                name,
                session_id,
                vars,
            } => Some(Self::launch_run(name, session_id, vars)),
            GatewayAction::Status(action) => Some(Self::from_status_action(action)),
            GatewayAction::Targets { .. } => Some(Self::Targets),
            GatewayAction::Stop(target) => Some(Self::Stop {
                target,
                session_id: None,
            }),
            GatewayAction::Remove(target) => Some(Self::Remove { target }),
            GatewayAction::SetDefault(target_or_image) => {
                Some(Self::SetDefault { target_or_image })
            }
            GatewayAction::ShowDefault => Some(Self::ShowDefault),
            GatewayAction::ResetDefault => Some(Self::ResetDefault),
            GatewayAction::ClientConfig(action) => Some(Self::ClientConfig {
                target: action.target,
                identity_file: action.identity_file.map(PathBuf::from),
            }),
            GatewayAction::Connect(_)
            | GatewayAction::AddKey(_)
            | GatewayAction::AddHostKey(_)
            | GatewayAction::AddContainerKey(_)
            | GatewayAction::ClientBundle(_)
            | GatewayAction::Help => None,
        }
    }

    fn from_run_action(action: RunAction) -> Self {
        Self::Run {
            target: action.target,
            session_id: action.session_id,
            cwd: action.cwd,
            command: action.command,
        }
    }

    fn from_status_action(action: StatusAction) -> Self {
        if action.all {
            Self::StatusAll
        } else {
            Self::Status {
                target: action.target,
                session_id: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LaunchShowArgs, LaunchesArgs, RunArgs, StatusArg, TargetsArgs};

    #[test]
    fn constructs_targets_request_without_rendering_flags() {
        assert_eq!(
            GatewayOperation::from_targets_args(TargetsArgs { json: true }),
            GatewayOperation::Targets
        );
    }

    #[test]
    fn constructs_status_and_status_all_requests_without_json() {
        assert_eq!(
            GatewayOperation::from_status_args(StatusArg {
                target: Some("dev".into()),
                all: false,
                json: true,
                session_id: Some("abc123".into()),
            }),
            GatewayOperation::Status {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
            }
        );
        assert_eq!(
            GatewayOperation::from_status_args(StatusArg {
                target: None,
                all: true,
                json: true,
                session_id: None,
            }),
            GatewayOperation::StatusAll
        );
    }

    #[test]
    fn constructs_run_request() {
        let operation = GatewayOperation::from_run_args(RunArgs {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
            cwd: Some("/work".into()),
            command: vec!["cargo".into(), "test".into()],
        })
        .unwrap();
        assert_eq!(
            operation,
            GatewayOperation::Run {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
                cwd: Some("/work".into()),
                command: vec!["cargo".into(), "test".into()],
            }
        );
    }

    #[test]
    fn constructs_launch_discovery_requests_without_json() {
        assert_eq!(
            GatewayOperation::from_launches_args(LaunchesArgs { json: true }),
            GatewayOperation::Launches
        );
        assert_eq!(
            GatewayOperation::from_launch_show_args(LaunchShowArgs {
                name: "repo-shell".into(),
                json: true,
            }),
            GatewayOperation::LaunchShow {
                name: "repo-shell".into(),
            }
        );
    }

    #[test]
    fn constructs_launch_run_request() {
        assert_eq!(
            GatewayOperation::launch_run(
                "repo-shell".into(),
                Some("abc123".into()),
                vec!["repo=https://example.test/repo.git".into()],
            ),
            GatewayOperation::Launch {
                name: "repo-shell".into(),
                session_id: Some("abc123".into()),
                vars: vec!["repo=https://example.test/repo.git".into()],
            }
        );
    }

    #[test]
    fn constructs_requests_from_ssh_actions_without_transport_flags() {
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Run(RunAction {
                target: Some("dev".into()),
                session_id: Some("abc123".into()),
                cwd: Some("/work".into()),
                command: vec!["cargo".into(), "test".into()],
            })),
            Some(
                GatewayOperation::from_run_args(RunArgs {
                    target: Some("dev".into()),
                    session_id: Some("abc123".into()),
                    cwd: Some("/work".into()),
                    command: vec!["cargo".into(), "test".into()],
                })
                .unwrap()
            )
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::LaunchRun {
                name: "repo-shell".into(),
                session_id: Some("abc123".into()),
                vars: vec!["repo=https://example.test/repo.git".into()],
            }),
            Some(GatewayOperation::launch_run(
                "repo-shell".into(),
                Some("abc123".into()),
                vec!["repo=https://example.test/repo.git".into()],
            ))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Launches { json: true }),
            Some(GatewayOperation::from_launches_args(LaunchesArgs {
                json: false
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Targets { json: true }),
            Some(GatewayOperation::from_targets_args(TargetsArgs {
                json: false
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::LaunchShow {
                name: "repo-shell".into(),
                json: true,
            }),
            Some(GatewayOperation::from_launch_show_args(LaunchShowArgs {
                name: "repo-shell".into(),
                json: false,
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Status(StatusAction {
                target: Some("dev".into()),
                all: false,
            })),
            Some(GatewayOperation::from_status_args(StatusArg {
                target: Some("dev".into()),
                all: false,
                json: false,
                session_id: None,
            }))
        );
        assert_eq!(
            GatewayOperation::from_ssh_action(GatewayAction::Status(StatusAction {
                target: None,
                all: true,
            })),
            Some(GatewayOperation::from_status_args(StatusArg {
                target: None,
                all: true,
                json: false,
                session_id: None,
            }))
        );
    }
}
