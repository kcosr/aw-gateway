use super::{
    OperationSessionGuard, Runtime, SessionOutcome, exec_final_container_command_with_options,
    launch_final_env, launch_template_vars, render_launch_cwd, render_template_map,
    run_launch_steps,
};
use crate::config::LaunchConfig;
use crate::gateway::model::ReadyStatus;
use crate::gateway::ops::{
    ExecutionOutcome, OperationExecutionOptions, OperationMode, OutputSelection,
};
use crate::paths;
use crate::template;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
enum OperationSessionSpec {
    RunCommand,
    Launch,
}

impl OperationSessionSpec {
    fn kind(self) -> &'static str {
        match self {
            Self::RunCommand => "run-command",
            Self::Launch => "launch",
        }
    }

    fn uses_launch_marker(self) -> bool {
        matches!(self, Self::Launch)
    }

    fn warn_fire_and_forget_failure(self, operation_id: &str, err: &anyhow::Error) {
        match self {
            Self::RunCommand => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %err,
                    "detached run operation failed"
                );
            }
            Self::Launch => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %err,
                    "detached launch operation failed"
                );
            }
        }
    }
}

pub(super) struct OperationRunner {
    runtime: Runtime,
    options: OperationExecutionOptions,
    session_spec: OperationSessionSpec,
    body: OperationBody,
}

impl OperationRunner {
    pub(super) fn run_command(
        runtime: Runtime,
        options: OperationExecutionOptions,
        cwd: Option<String>,
        command: Vec<String>,
    ) -> Self {
        Self {
            runtime,
            options,
            session_spec: OperationSessionSpec::RunCommand,
            body: OperationBody::Run { cwd, command },
        }
    }

    pub(super) fn launch(
        runtime: Runtime,
        options: OperationExecutionOptions,
        launch: LaunchConfig,
        resolved_vars: BTreeMap<String, String>,
    ) -> Self {
        debug_assert!(
            runtime.identity.launch_name.is_some(),
            "launch operations require a launch name on the runtime"
        );
        Self {
            runtime,
            options,
            session_spec: OperationSessionSpec::Launch,
            body: OperationBody::Launch {
                launch,
                resolved_vars,
            },
        }
    }

    pub(super) async fn run(self) -> anyhow::Result<ExecutionOutcome> {
        if self.options.mode == OperationMode::Detach {
            return self.spawn_detached().await;
        }
        let Self {
            runtime,
            options,
            session_spec,
            body,
        } = self;
        let session = runtime
            .begin_operation_session(session_spec.kind(), session_spec.uses_launch_marker())?;
        run_operation_session(runtime, session_spec, session, body, options).await
    }

    async fn spawn_detached(self) -> anyhow::Result<ExecutionOutcome> {
        // Detach is explicitly fire-and-forget. This id is emitted in the
        // response and logs so operators can correlate failures, not look up
        // operation status later.
        let operation_id = super::session::generate_session_id_value()?;
        let Self {
            runtime,
            session_spec,
            body,
            options: _,
        } = self;
        let session = runtime
            .begin_operation_session(session_spec.kind(), session_spec.uses_launch_marker())?;
        let background_id = operation_id.clone();
        tokio::spawn(async move {
            let result = run_operation_session(
                runtime,
                session_spec,
                session,
                body,
                detach_discard_options(),
            )
            .await;
            if let Err(err) = result {
                session_spec.warn_fire_and_forget_failure(&background_id, &err);
            }
        });
        Ok(ExecutionOutcome::detached(operation_id))
    }
}

pub(super) async fn run_container_command_with_runtime(
    runtime: Runtime,
    cwd: Option<String>,
    command: Vec<String>,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    OperationRunner::run_command(runtime, options, cwd, command)
        .run()
        .await
}

async fn run_operation_session(
    runtime: Runtime,
    session_spec: OperationSessionSpec,
    mut session: OperationSessionGuard,
    body: OperationBody,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    let result = async {
        let ready = runtime.ensure_ready().await?;
        runtime
            .hold_operation_agent_session(&mut session, session_spec.kind())
            .await?;
        body.execute(&runtime, ready, options).await
    }
    .await;
    let outcome = SessionOutcome::from_execution_result(&result);
    runtime
        .finish_operation_session(session, result, outcome)
        .await
}

enum OperationBody {
    Run {
        cwd: Option<String>,
        command: Vec<String>,
    },
    Launch {
        launch: LaunchConfig,
        resolved_vars: BTreeMap<String, String>,
    },
}

impl OperationBody {
    async fn execute(
        self,
        runtime: &Runtime,
        ready: ReadyStatus,
        options: OperationExecutionOptions,
    ) -> anyhow::Result<ExecutionOutcome> {
        match self {
            Self::Run { cwd, command } => {
                let cwd = cwd
                    .as_deref()
                    .map(|cwd| paths::expand_home(&runtime.identity.container_home, cwd));
                exec_final_container_command_with_options(
                    runtime,
                    command,
                    cwd,
                    runtime.session_env()?,
                    options,
                )
                .await
            }
            Self::Launch {
                launch,
                resolved_vars,
            } => {
                let container_pid = ready.container_pid.to_string();
                let vars = launch_template_vars(runtime, &resolved_vars, Some(&container_pid));
                let launch_env = render_template_map(&launch.env, &vars)?;
                run_launch_steps(runtime, &launch, &vars, &launch_env).await?;
                let env = launch_final_env(&runtime.session_env()?, &launch_env);
                let cwd = render_launch_cwd(
                    launch.cwd.as_deref(),
                    &vars,
                    runtime.identity.container_home.as_path(),
                )?;
                let command = template::render_argv(&launch.command, &vars)?;
                exec_final_container_command_with_options(runtime, command, cwd, env, options).await
            }
        }
    }
}

pub(super) fn detach_discard_options() -> OperationExecutionOptions {
    OperationExecutionOptions {
        mode: OperationMode::Detach,
        output: OutputSelection {
            stdout: false,
            stderr: false,
        },
    }
}

impl Runtime {
    fn begin_operation_session(
        &self,
        kind: &str,
        launch_marker: bool,
    ) -> anyhow::Result<OperationSessionGuard> {
        let session = if launch_marker {
            self.create_launch_session_marker(kind)?
        } else {
            self.create_session_marker(kind)?
        };
        Ok(OperationSessionGuard {
            session: Some(session),
            agent_session: None,
        })
    }

    async fn hold_operation_agent_session(
        &self,
        session: &mut OperationSessionGuard,
        kind: &str,
    ) -> anyhow::Result<()> {
        session.agent_session = self.agent_session_hold(kind).await?;
        Ok(())
    }

    async fn finish_operation_session<T>(
        &self,
        mut session: OperationSessionGuard,
        result: anyhow::Result<T>,
        outcome: SessionOutcome,
    ) -> anyhow::Result<T> {
        drop(session.agent_session.take());
        let marker = session
            .session
            .take()
            .expect("operation session marker must be present");
        self.finish_post_session(marker, result, outcome).await
    }
}
