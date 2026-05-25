use super::Runtime;
use crate::config::LifecyclePhase;
use crate::runtime::ContainerInspect;

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
            .ok_or_else(|| anyhow::anyhow!("container did not exist after start"))
    }
}

pub(super) fn readiness_plan(inspect: Option<ContainerInspect>) -> ContainerReadinessPlan {
    match inspect {
        Some(inspect) if inspect.state.running => ContainerReadinessPlan::ReuseRunning(inspect),
        Some(inspect) => ContainerReadinessPlan::StartStopped(inspect),
        None => ContainerReadinessPlan::CreateMissing,
    }
}
