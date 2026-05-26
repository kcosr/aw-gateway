use super::{GatewayOperation, OperationResult, SuppliedLaunchVars};
use crate::ssh_dispatch::{GatewayAction, RunAction, StatusAction};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub(in crate::gateway) struct SshGatewayOperation {
    pub(in crate::gateway) operation: GatewayOperation,
    pub(in crate::gateway) render: SshRenderOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::gateway) struct SshRenderOptions {
    pub(in crate::gateway) json: bool,
}

impl SshGatewayOperation {
    pub(in crate::gateway) fn from_action(action: &GatewayAction) -> OperationResult<Option<Self>> {
        Ok(match action {
            GatewayAction::Up(target) => Some(Self {
                operation: GatewayOperation::Up {
                    target: target.clone(),
                    session_id: None,
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Run(action) => Some(Self {
                operation: GatewayOperation::from_run_action(action),
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Launches { json } => Some(Self {
                operation: GatewayOperation::Launches,
                render: SshRenderOptions { json: *json },
            }),
            GatewayAction::LaunchShow { name, json } => Some(Self {
                operation: GatewayOperation::LaunchShow { name: name.clone() },
                render: SshRenderOptions { json: *json },
            }),
            GatewayAction::LaunchRun {
                name,
                session_id,
                vars,
            } => {
                let vars = SuppliedLaunchVars::from_cli_pairs(vars.clone())?;
                Some(Self {
                    operation: GatewayOperation::launch_run(name.clone(), session_id.clone(), vars),
                    render: SshRenderOptions::default(),
                })
            }
            GatewayAction::Status(action) => Some(Self {
                operation: GatewayOperation::from_status_action(action),
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Targets { json } => Some(Self {
                operation: GatewayOperation::Targets,
                render: SshRenderOptions { json: *json },
            }),
            GatewayAction::Stop(action) => Some(Self {
                operation: GatewayOperation::Stop {
                    target: action.target.clone(),
                    session_id: action.session_id.clone(),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Remove(action) => Some(Self {
                operation: GatewayOperation::Remove {
                    target: action.target.clone(),
                    session_id: action.session_id.clone(),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::SetDefault(target_or_image) => Some(Self {
                operation: GatewayOperation::SetDefault {
                    target_or_image: target_or_image.clone(),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::ShowDefault => Some(Self {
                operation: GatewayOperation::ShowDefault,
                render: SshRenderOptions::default(),
            }),
            GatewayAction::ResetDefault => Some(Self {
                operation: GatewayOperation::ResetDefault,
                render: SshRenderOptions::default(),
            }),
            GatewayAction::ClientConfig(action) => Some(Self {
                operation: GatewayOperation::ClientConfig {
                    target: action.target.clone(),
                    identity_file: action.identity_file.clone().map(PathBuf::from),
                },
                render: SshRenderOptions::default(),
            }),
            GatewayAction::Connect(_)
            | GatewayAction::AddKey(_)
            | GatewayAction::AddHostKey(_)
            | GatewayAction::AddContainerKey(_)
            | GatewayAction::ClientBundle(_)
            | GatewayAction::Help => None,
        })
    }
}

impl GatewayOperation {
    pub(super) fn from_run_action(action: &RunAction) -> Self {
        Self::Run {
            target: action.target.clone(),
            session_id: action.session_id.clone(),
            cwd: action.cwd.clone(),
            command: action.command.clone(),
            options: super::OperationExecutionOptions::STREAM,
        }
    }

    pub(super) fn from_status_action(action: &StatusAction) -> Self {
        if action.all {
            Self::StatusAll
        } else {
            Self::Status {
                target: action.target.clone(),
                session_id: None,
            }
        }
    }
}
