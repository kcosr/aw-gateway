use super::{
    OperationSessionGuard, Runtime, SessionOutcome, exec_container_command_with_options,
    final_container_exec_spec, launch_final_env, launch_template_vars, render_launch_cwd,
    render_template_map, run_launch_steps,
};
use crate::config::LaunchConfig;
use crate::gateway::model::ReadyStatus;
use crate::gateway::ops::{
    ExecutionOutcome, OperationExecutionOptions, OperationMode, OutputSelection,
};
use crate::paths;
use crate::runtime::{ContainerExecSpec, ContainerPtySession, ContainerPtySize};
use crate::template;
use anyhow::Context;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub(super) enum OperationSessionSpec {
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

    pub(super) async fn prepare(self) -> anyhow::Result<PreparedExecution> {
        let Self {
            runtime,
            session_spec,
            body,
            options: _,
        } = self;
        let mut session = runtime
            .begin_operation_session(session_spec.kind(), session_spec.uses_launch_marker())?;
        let prepared =
            prepare_operation_session_body(&runtime, session_spec, &mut session, body).await?;
        Ok(PreparedExecution {
            runtime,
            session,
            prepared,
        })
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
    let foreground = if options.mode == OperationMode::Stream {
        run_foreground_operation_session(&runtime, session_spec, &mut session, body, options)
            .await?
    } else {
        let result =
            execute_operation_session_body(&runtime, session_spec, &mut session, body, options)
                .await;
        let outcome = SessionOutcome::from_execution_result(&result);
        ForegroundOperationResult {
            result,
            outcome,
            cleanup_signals: None,
        }
    };
    let ForegroundOperationResult {
        result,
        outcome,
        cleanup_signals,
    } = foreground;
    if let Some(signals) = cleanup_signals {
        return finish_interrupted_operation_session(runtime, session, result, outcome, signals)
            .await;
    }
    runtime
        .finish_operation_session(session, result, outcome)
        .await
}

async fn run_foreground_operation_session(
    runtime: &Runtime,
    session_spec: OperationSessionSpec,
    session: &mut OperationSessionGuard,
    body: OperationBody,
    options: OperationExecutionOptions,
) -> anyhow::Result<ForegroundOperationResult> {
    let operation = execute_operation_session_body(runtime, session_spec, session, body, options);
    tokio::pin!(operation);
    let mut signals = match ForegroundSignalListener::new() {
        Ok(signals) => signals,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "foreground signal handling disabled; operation will use default teardown"
            );
            let result = operation.await;
            let outcome = SessionOutcome::from_execution_result(&result);
            return Ok(ForegroundOperationResult {
                result,
                outcome,
                cleanup_signals: None,
            });
        }
    };
    tokio::select! {
        biased;
        result = &mut operation => {
            let outcome = SessionOutcome::from_execution_result(&result);
            Ok(ForegroundOperationResult {
                result,
                outcome,
                cleanup_signals: None,
            })
        }
        signal = signals.recv() => {
            tracing::info!(
                signal = signal.name,
                exit_code = signal.exit_code(),
                "foreground operation canceled"
            );
            Ok(ForegroundOperationResult {
                result: Ok(ExecutionOutcome::new(signal.exit_code())),
                outcome: SessionOutcome::Canceled,
                cleanup_signals: Some(signals),
            })
        }
    }
}

struct ForegroundOperationResult {
    result: anyhow::Result<ExecutionOutcome>,
    outcome: SessionOutcome,
    cleanup_signals: Option<ForegroundSignalListener>,
}

async fn finish_interrupted_operation_session(
    runtime: Runtime,
    session: OperationSessionGuard,
    result: anyhow::Result<ExecutionOutcome>,
    outcome: SessionOutcome,
    mut signals: ForegroundSignalListener,
) -> anyhow::Result<ExecutionOutcome> {
    let cleanup = runtime.finish_operation_session(session, result, outcome);
    tokio::pin!(cleanup);
    tokio::select! {
        biased;
        result = &mut cleanup => result,
        signal = signals.recv() => {
            tracing::warn!(
                signal = signal.name,
                exit_code = signal.exit_code(),
                "foreground cleanup interrupted by a second signal"
            );
            std::process::exit(signal.exit_code());
        }
    }
}

async fn execute_operation_session_body(
    runtime: &Runtime,
    session_spec: OperationSessionSpec,
    session: &mut OperationSessionGuard,
    body: OperationBody,
    options: OperationExecutionOptions,
) -> anyhow::Result<ExecutionOutcome> {
    let prepared = prepare_operation_session_body(runtime, session_spec, session, body).await?;
    exec_container_command_with_options(runtime, &prepared.exec_spec, options).await
}

async fn prepare_operation_session_body(
    runtime: &Runtime,
    session_spec: OperationSessionSpec,
    session: &mut OperationSessionGuard,
    body: OperationBody,
) -> anyhow::Result<PreparedCommand> {
    let ready = runtime.ensure_ready().await?;
    runtime
        .hold_operation_agent_session(session, session_spec.kind())
        .await?;
    body.prepare(runtime, ready).await
}

#[derive(Debug, Clone, Copy)]
struct ForegroundCancelSignal {
    name: &'static str,
    number: i32,
}

impl ForegroundCancelSignal {
    fn exit_code(self) -> i32 {
        128 + self.number
    }
}

#[cfg(unix)]
struct ForegroundSignalListener {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ForegroundSignalListener {
    fn new() -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("register SIGINT handler")?,
            terminate: signal(SignalKind::terminate()).context("register SIGTERM handler")?,
            hangup: signal(SignalKind::hangup()).context("register SIGHUP handler")?,
        })
    }

    async fn recv(&mut self) -> ForegroundCancelSignal {
        tokio::select! {
            _ = self.interrupt.recv() => {
                ForegroundCancelSignal {
                    name: "SIGINT",
                    number: libc::SIGINT,
                }
            },
            _ = self.terminate.recv() => {
                ForegroundCancelSignal {
                    name: "SIGTERM",
                    number: libc::SIGTERM,
                }
            },
            _ = self.hangup.recv() => {
                ForegroundCancelSignal {
                    name: "SIGHUP",
                    number: libc::SIGHUP,
                }
            },
        }
    }
}

#[cfg(not(unix))]
struct ForegroundSignalListener;

#[cfg(not(unix))]
impl ForegroundSignalListener {
    fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> ForegroundCancelSignal {
        if let Err(err) = tokio::signal::ctrl_c().await.context("wait for Ctrl-C") {
            tracing::warn!(error = %err, "failed while waiting for Ctrl-C");
        }
        const SIGINT_EXIT_SIGNAL_NUMBER: i32 = 2;
        ForegroundCancelSignal {
            name: "SIGINT",
            number: SIGINT_EXIT_SIGNAL_NUMBER,
        }
    }
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
    async fn prepare(
        self,
        runtime: &Runtime,
        ready: ReadyStatus,
    ) -> anyhow::Result<PreparedCommand> {
        match self {
            Self::Run { cwd, command } => {
                let cwd = cwd
                    .as_deref()
                    .map(|cwd| paths::expand_home(&runtime.identity.container_home, cwd));
                let exec_spec =
                    final_container_exec_spec(runtime, command, cwd, runtime.session_env()?);
                Ok(PreparedCommand { ready, exec_spec })
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
                let exec_spec = final_container_exec_spec(runtime, command, cwd, env);
                Ok(PreparedCommand { ready, exec_spec })
            }
        }
    }
}

pub(super) struct PreparedExecution {
    runtime: Runtime,
    session: OperationSessionGuard,
    prepared: PreparedCommand,
}

impl PreparedExecution {
    pub(super) fn ready(&self) -> &ReadyStatus {
        &self.prepared.ready
    }

    pub(super) fn exec_spec(&self) -> &ContainerExecSpec {
        &self.prepared.exec_spec
    }

    pub(super) fn launch_name(&self) -> Option<&str> {
        self.runtime.identity.launch_name.as_deref()
    }

    pub(super) fn spawn_pty(&self, size: ContainerPtySize) -> anyhow::Result<ContainerPtySession> {
        self.runtime
            .container_runtime
            .exec_pty(self.exec_spec(), size)
    }

    pub(super) async fn finish<T>(
        self,
        result: anyhow::Result<T>,
        outcome: SessionOutcome,
    ) -> anyhow::Result<T> {
        let Self {
            runtime,
            session,
            prepared: _,
        } = self;
        runtime
            .finish_operation_session(session, result, outcome)
            .await
    }
}

struct PreparedCommand {
    ready: ReadyStatus,
    exec_spec: ContainerExecSpec,
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
