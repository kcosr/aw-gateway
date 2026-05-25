use super::Runtime;
use super::failures::ContainerNotFound;
use crate::config::{IdleCleanupAction, IdleCleanupConfig, IdleCleanupOwner, LifecyclePhase};
use crate::runtime::ContainerInspect;
use tokio::net::TcpStream;
use tokio::time::{Duration, Instant, sleep};

#[derive(Debug)]
pub(super) enum ContainerReadinessPlan {
    ReuseRunning(ContainerInspect),
    StartStopped(ContainerInspect),
    CreateMissing,
}

#[derive(Default)]
pub(super) struct FailedStartCleanup {
    runtime_start_attempted: bool,
}

impl FailedStartCleanup {
    pub(super) fn mark_runtime_start_attempted(&mut self) {
        self.runtime_start_attempted = true;
    }

    pub(super) async fn run_if_needed(self, runtime: &Runtime) {
        if self.runtime_start_attempted {
            runtime.cleanup_failed_start().await;
            runtime.cleanup_control_socket_dir();
        }
    }
}

impl Runtime {
    pub(super) async fn ensure_container_for_readiness_plan(
        &self,
        plan: ContainerReadinessPlan,
        failed_start_cleanup: &mut FailedStartCleanup,
    ) -> anyhow::Result<ContainerInspect> {
        match plan {
            ContainerReadinessPlan::ReuseRunning(existing) => {
                self.validate_labels(&existing)?;
                Ok(existing)
            }
            ContainerReadinessPlan::StartStopped(existing) => {
                self.validate_labels(&existing)?;
                self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                    .await?;
                self.remove_stale_control_socket_files()?;
                failed_start_cleanup.mark_runtime_start_attempted();
                self.container_runtime
                    .start(&self.identity.container_name)
                    .await?;
                self.inspect_container_after_start().await
            }
            ContainerReadinessPlan::CreateMissing => {
                self.run_lifecycle_phase(LifecyclePhase::PreStart, None)
                    .await?;
                self.remove_stale_control_socket_files()?;
                failed_start_cleanup.mark_runtime_start_attempted();
                self.start_container().await?;
                self.inspect_container_after_start().await
            }
        }
    }

    async fn inspect_container_after_start(&self) -> anyhow::Result<ContainerInspect> {
        self.container_runtime
            .inspect(&self.identity.container_name)
            .await?
            .ok_or_else(|| ContainerNotFound::after_start().into())
    }

    pub(super) async fn apply_gateway_idle_cleanup(&self) -> anyhow::Result<()> {
        if !self.target.stop_when_idle {
            return Ok(());
        }
        let Some(cleanup) = self.target.idle_cleanup.as_ref() else {
            return Ok(());
        };
        if cleanup.owner != IdleCleanupOwner::Gateway || cleanup.action == IdleCleanupAction::None {
            return Ok(());
        }
        let idle_grace = cleanup
            .idle_grace
            .as_deref()
            .and_then(|value| crate::config::parse_duration(value).ok())
            .unwrap_or_default();
        if !idle_grace.is_zero() {
            sleep(idle_grace).await;
        }
        let _lock = self.acquire_lifecycle_lock().await?;
        if !self.active_session_markers()?.is_empty() {
            return Ok(());
        }
        if self.has_preserve_process(cleanup).await? {
            tracing::info!(
                container = self.identity.container_name,
                "gateway-owned cleanup preserving container because preserve process is running"
            );
            return Ok(());
        }
        self.stop_managed_container().await
    }

    async fn stop_managed_container(&self) -> anyhow::Result<()> {
        let Some(inspect) = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?
        else {
            return Ok(());
        };
        self.stop_inspected_container(&inspect).await
    }

    pub(super) async fn stop_inspected_container(
        &self,
        inspect: &ContainerInspect,
    ) -> anyhow::Result<()> {
        self.validate_labels(inspect)?;
        let container_pid = inspect.state.pid.to_string();
        self.run_lifecycle_phase(LifecyclePhase::PreStop, Some(&container_pid))
            .await?;
        if self.agent_control_enabled() {
            let _ = self.agent_shutdown().await;
            if let Some(current) = self.wait_for_container_exit().await? {
                self.validate_labels(&current)?;
                self.container_runtime
                    .stop(&self.identity.container_name)
                    .await?;
            }
        } else {
            self.container_runtime
                .stop(&self.identity.container_name)
                .await?;
        }
        if self.target.remove_on_stop
            && let Some(current) = self
                .container_runtime
                .inspect(&self.identity.container_name)
                .await?
        {
            self.validate_labels(&current)?;
            self.container_runtime
                .rm(&self.identity.container_name)
                .await?;
        }
        self.run_lifecycle_phase(LifecyclePhase::PostStop, Some(&container_pid))
            .await?;
        self.cleanup_control_socket_dir();
        Ok(())
    }

    async fn wait_for_container_exit(&self) -> anyhow::Result<Option<ContainerInspect>> {
        let timeout = self.shutdown_wait_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            let Some(inspect) = self
                .container_runtime
                .inspect(&self.identity.container_name)
                .await?
            else {
                return Ok(None);
            };
            self.validate_labels(&inspect)?;
            if !inspect.state.running {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Ok(Some(inspect));
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    fn shutdown_wait_timeout(&self) -> Duration {
        self.target
            .idle_cleanup
            .as_ref()
            .and_then(|cleanup| cleanup.shutdown_timeout.as_deref())
            .and_then(|value| crate::config::parse_duration(value).ok())
            .unwrap_or_else(|| Duration::from_secs(10))
    }

    async fn has_preserve_process(&self, cleanup: &IdleCleanupConfig) -> anyhow::Result<bool> {
        for process in &cleanup.preserve_processes {
            let code = self
                .container_runtime
                .exec_quiet(
                    &self.identity.container_name,
                    ["pgrep", "-x", process.as_str()],
                )
                .await?;
            if code == 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) async fn wait_published_ssh_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(endpoint) = self.published_ssh_endpoint().await?
                && TcpStream::connect((endpoint.host.as_str(), endpoint.port))
                    .await
                    .is_ok()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("published container SSH port did not become ready");
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    async fn cleanup_failed_start(&self) {
        match self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await
        {
            Ok(Some(inspect)) => {
                if let Err(err) = self.validate_labels(&inspect) {
                    tracing::warn!(
                        container = self.identity.container_name,
                        error = %err,
                        "not cleaning failed start because labels did not match"
                    );
                    return;
                }
                if let Err(err) = self
                    .container_runtime
                    .stop(&self.identity.container_name)
                    .await
                {
                    tracing::warn!(
                        container = self.identity.container_name,
                        error = %err,
                        "failed to stop container after startup failure"
                    );
                }
                if self.target.remove_on_stop
                    && let Err(err) = self
                        .container_runtime
                        .rm(&self.identity.container_name)
                        .await
                {
                    tracing::warn!(
                        container = self.identity.container_name,
                        error = %err,
                        "failed to remove container after startup failure"
                    );
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    container = self.identity.container_name,
                    error = %err,
                    "failed to inspect container after startup failure"
                );
            }
        }
    }
}

pub(super) fn readiness_plan(inspect: Option<ContainerInspect>) -> ContainerReadinessPlan {
    match inspect {
        Some(inspect) if inspect.state.running => ContainerReadinessPlan::ReuseRunning(inspect),
        Some(inspect) => ContainerReadinessPlan::StartStopped(inspect),
        None => ContainerReadinessPlan::CreateMissing,
    }
}
