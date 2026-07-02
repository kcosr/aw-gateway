use super::Runtime;
use super::health::{run_argv_with_timeout, run_health_check};
use crate::config::{HealthCheck, LifecyclePhase, LifecycleStep};
use anyhow::Context;
use tokio::time::Duration;

const DEFAULT_HOST_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

impl Runtime {
    pub(super) async fn run_lifecycle_phase(
        &self,
        phase: LifecyclePhase,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        for step in self
            .target
            .lifecycle_steps
            .iter()
            .filter(|step| step.phase == phase)
        {
            self.run_step(step, container_pid).await?;
        }
        Ok(())
    }

    async fn run_step(
        &self,
        step: &LifecycleStep,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        let vars = self.vars(container_pid);
        let command = self.render_runtime_argv(&step.command, &vars, container_pid)?;
        let timeout = host_hook_timeout(step.timeout.as_deref())?;
        match run_argv_with_timeout(&command, timeout).await {
            Ok(()) => Ok(()),
            Err(err) if !step.required => {
                tracing::warn!(step = step.name, error = %err, "optional lifecycle step failed");
                Ok(())
            }
            Err(err) => Err(err).with_context(|| format!("run lifecycle step {:?}", step.name)),
        }
    }

    pub(super) async fn run_host_steps(&self, container_pid: Option<&str>) -> anyhow::Result<()> {
        for step in &self.target.host_steps {
            let vars = self.vars(container_pid);
            let command = self.render_runtime_argv(&step.command, &vars, container_pid)?;
            let timeout = host_hook_timeout(step.timeout.as_deref())?;
            let command_result = run_argv_with_timeout(&command, timeout).await;
            if let Err(err) = command_result {
                if step.required {
                    return Err(err).with_context(|| format!("host step {:?}", step.name));
                }
                tracing::warn!(step = step.name, error = %err, "optional host step failed");
                continue;
            }
            if let Some(health_check) = &step.health_check {
                self.ensure_host_health_check_templates_supported(health_check, container_pid)?;
                let health_result = run_health_check(health_check, &vars).await;
                if let Err(err) = health_result {
                    if step.required {
                        return Err(err)
                            .with_context(|| format!("host step {:?} health check", step.name));
                    }
                    tracing::warn!(step = step.name, error = %err, "optional host step health check failed");
                }
            }
        }
        Ok(())
    }

    fn ensure_host_health_check_templates_supported(
        &self,
        health_check: &HealthCheck,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        match health_check {
            HealthCheck::Command { command, .. } => self.ensure_runtime_template_values_supported(
                command.iter().map(String::as_str),
                container_pid,
            ),
            HealthCheck::Http { url, .. } => {
                self.ensure_runtime_template_values_supported([url.as_str()], container_pid)
            }
            HealthCheck::Process | HealthCheck::Tcp { .. } => Ok(()),
        }
    }
}

pub(super) fn host_hook_timeout(configured: Option<&str>) -> anyhow::Result<Duration> {
    configured
        .map(crate::config::parse_duration)
        .transpose()
        .map(|timeout| timeout.unwrap_or(DEFAULT_HOST_HOOK_TIMEOUT))
}
