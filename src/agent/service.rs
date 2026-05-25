use crate::agent_control::ServiceStatus;
use crate::config::{
    EnvValue, HealthCheck, LoggingConfig, RestartPolicy, ServiceConfig, parse_duration,
};
use crate::health_probe::{JsonFieldCheck, check_json_fields, http_get};
use crate::rotating_log::{RotationState, RotationStep};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};

use super::process::process_exists;

#[derive(Debug)]
pub(super) struct ManagedService {
    pub(super) config: ServiceConfig,
    state_dir: PathBuf,
    logging: LoggingConfig,
    pub(super) child: Mutex<Option<Child>>,
    pub(super) stopping: AtomicBool,
    restart_count: AtomicUsize,
    pub(super) last_error: Mutex<Option<String>>,
}

impl ManagedService {
    pub(super) fn new(config: ServiceConfig, state_dir: PathBuf, logging: LoggingConfig) -> Self {
        Self {
            config,
            state_dir,
            logging,
            child: Mutex::new(None),
            stopping: AtomicBool::new(false),
            restart_count: AtomicUsize::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub(super) async fn status(&self) -> ServiceStatus {
        let child_guard = self.child.lock().await;
        let (state, pid) = match child_guard.as_ref().and_then(|child| child.id()) {
            Some(pid) if process_exists(pid) => ("running".to_string(), Some(pid)),
            Some(_) => ("exited".to_string(), None),
            None => ("stopped".to_string(), None),
        };
        drop(child_guard);
        let healthy = self.health_check().await.unwrap_or(false);
        ServiceStatus {
            name: self.config.name.clone(),
            required: self.config.required,
            state,
            pid,
            healthy,
            restart_count: self.restart_count.load(Ordering::SeqCst),
            last_error: self.last_error.lock().await.clone(),
        }
    }

    async fn health_check(&self) -> anyhow::Result<bool> {
        let vars = self.vars();
        match &self.config.health_check {
            None | Some(HealthCheck::Process) => {
                let child = self.child.lock().await;
                Ok(child
                    .as_ref()
                    .and_then(|child| child.id())
                    .is_some_and(process_exists))
            }
            Some(HealthCheck::Tcp { host, port, .. }) => Ok(tokio::time::timeout(
                health_check_timeout(self.config.health_check.as_ref()),
                TcpStream::connect((host.as_str(), *port)),
            )
            .await
            .is_ok_and(|result| result.is_ok())),
            Some(HealthCheck::Http {
                url,
                expect_status,
                expect_json,
                ..
            }) => {
                let url = template::render(url, &vars)?;
                Ok(tokio::time::timeout(
                    health_check_timeout(self.config.health_check.as_ref()),
                    http_health(&url, expect_status.unwrap_or(200), expect_json),
                )
                .await
                .is_ok_and(|result| result.unwrap_or(false)))
            }
            Some(HealthCheck::Command { .. }) => Ok(false),
        }
    }

    fn vars(&self) -> Vars {
        BTreeMap::from([(
            "container_state_dir".to_string(),
            self.state_dir.display().to_string(),
        )])
    }

    pub(super) fn required_health_restart(&self) -> bool {
        self.config.required
            && !matches!(self.config.health_check, None | Some(HealthCheck::Process))
    }

    pub(super) async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().await.take() {
            stop_child_gracefully(self, child).await;
        }
    }
}

pub(super) async fn service_supervisor(
    service: Arc<ManagedService>,
    all_services: Vec<Arc<ManagedService>>,
) {
    let base_backoff = service
        .config
        .restart_backoff
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(2));
    let max_backoff = service
        .config
        .restart_backoff_max
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(30));
    let mut current_backoff = base_backoff;
    loop {
        if service.stopping.load(Ordering::SeqCst) {
            break;
        }
        match wait_for_dependencies(&service, &all_services).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) => {
                *service.last_error.lock().await = Some(err.to_string());
                tracing::error!(service = service.config.name, error = %err, "service dependency configuration is invalid");
                sleep(current_backoff).await;
                current_backoff = std::cmp::min(current_backoff.saturating_mul(2), max_backoff);
                continue;
            }
        }
        let exit_success = if let Err(err) = start_service(&service).await {
            *service.last_error.lock().await = Some(err.to_string());
            tracing::error!(service = service.config.name, error = %err, "service start failed");
            false
        } else {
            current_backoff = base_backoff;
            wait_for_service_exit_or_unhealthy(&service).await
        };

        if service.stopping.load(Ordering::SeqCst)
            || !should_restart(service.config.restart, exit_success)
        {
            break;
        }

        service.restart_count.fetch_add(1, Ordering::SeqCst);
        sleep(current_backoff).await;
        current_backoff = std::cmp::min(current_backoff.saturating_mul(2), max_backoff);
    }
}

async fn wait_for_service_exit_or_unhealthy(service: &ManagedService) -> bool {
    let health_grace = service_startup_timeout(service);
    let started_at = Instant::now();
    loop {
        sleep(health_check_interval(service.config.health_check.as_ref())).await;
        let exit_status = {
            let mut child_guard = service.child.lock().await;
            match child_guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => {
                        *child_guard = None;
                        Some(Ok(status.success()))
                    }
                    Ok(None) => None,
                    Err(err) => {
                        *child_guard = None;
                        Some(Err(err))
                    }
                },
                None => Some(Ok(true)),
            }
        };
        match exit_status {
            Some(Ok(success)) => {
                if success {
                    *service.last_error.lock().await = None;
                } else {
                    *service.last_error.lock().await = Some("service exited unsuccessfully".into());
                }
                return success;
            }
            Some(Err(err)) => {
                *service.last_error.lock().await = Some(err.to_string());
                return false;
            }
            None => {
                if service.required_health_restart()
                    && started_at.elapsed() >= health_grace
                    && !service.health_check().await.unwrap_or(false)
                {
                    *service.last_error.lock().await =
                        Some("required service health check failed".into());
                    if let Some(child) = service.child.lock().await.take() {
                        stop_child_gracefully(service, child).await;
                    }
                    return false;
                }
            }
        }
    }
}

fn service_startup_timeout(service: &ManagedService) -> Duration {
    service
        .config
        .startup_timeout
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(10))
}

pub(super) fn should_restart(policy: RestartPolicy, exit_success: bool) -> bool {
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => !exit_success,
        RestartPolicy::Always => true,
    }
}

pub(super) async fn wait_for_dependencies(
    service: &ManagedService,
    all_services: &[Arc<ManagedService>],
) -> anyhow::Result<bool> {
    if service.config.depends_on.is_empty() {
        return Ok(true);
    }
    // Dependency readiness gates process start. It is intentionally not bounded
    // by startup_timeout, which applies after this service has started.
    loop {
        if service.stopping.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut missing = Vec::new();
        for dep in &service.config.depends_on {
            let Some(dep_service) = all_services
                .iter()
                .find(|candidate| candidate.config.name == *dep)
            else {
                anyhow::bail!("dependency {dep:?} is not configured");
            };
            if !dep_service.health_check().await.unwrap_or(false) {
                missing.push(dep.clone());
            }
        }
        if missing.is_empty() {
            let mut last_error = service.last_error.lock().await;
            if last_error
                .as_deref()
                .is_some_and(|error| error.starts_with("waiting for dependencies: "))
            {
                *last_error = None;
            }
            return Ok(true);
        }
        *service.last_error.lock().await =
            Some(format!("waiting for dependencies: {}", missing.join(", ")));
        sleep(dependency_health_interval(&missing, all_services)).await;
    }
}

fn dependency_health_interval(
    missing: &[String],
    all_services: &[Arc<ManagedService>],
) -> Duration {
    missing
        .iter()
        .filter_map(|name| {
            all_services
                .iter()
                .find(|service| service.config.name == *name)
        })
        .map(|service| health_check_interval(service.config.health_check.as_ref()))
        .min()
        .unwrap_or(Duration::from_millis(250))
}

pub(super) fn health_check_interval(check: Option<&HealthCheck>) -> Duration {
    match check {
        Some(HealthCheck::Tcp {
            interval: Some(value),
            ..
        })
        | Some(HealthCheck::Http {
            interval: Some(value),
            ..
        }) => parse_duration(value).unwrap_or(Duration::from_secs(2)),
        _ => Duration::from_millis(250),
    }
}

pub(super) fn health_check_timeout(check: Option<&HealthCheck>) -> Duration {
    match check {
        Some(HealthCheck::Tcp {
            timeout: Some(value),
            ..
        })
        | Some(HealthCheck::Http {
            timeout: Some(value),
            ..
        }) => parse_duration(value).unwrap_or(Duration::from_secs(1)),
        _ => Duration::from_secs(1),
    }
}

async fn start_service(service: &ManagedService) -> anyhow::Result<()> {
    let logs = service.state_dir.join("logs");
    tokio::fs::create_dir_all(&logs).await?;
    let log_path = logs.join(format!("{}.log", service.config.name));
    let service_log = Arc::new(Mutex::new(
        RotatingServiceLog::new(
            log_path,
            service.logging.max_bytes.unwrap_or(100 * 1024 * 1024),
            service.logging.max_files.unwrap_or(5),
        )
        .await?,
    ));
    let vars = service.vars();
    let command_argv = template::render_argv(&service.config.command, &vars)?;
    let mut command = Command::new(&command_argv[0]);
    command.args(&command_argv[1..]);
    command.env_clear();
    command.env(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string()),
    );
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    if let Some(cwd) = &service.config.cwd {
        command.current_dir(template::render(cwd, &vars)?);
    }
    for (key, value) in &service.config.env {
        if let Some(value) = resolve_env(value, &vars)? {
            command.env(key, value);
        }
    }
    let mut child_slot = service.child.lock().await;
    if service.stopping.load(Ordering::SeqCst) {
        anyhow::bail!("service {:?} is stopping", service.config.name);
    }
    if service.config.user != "root" {
        let identity = resolve_service_user(&service.config.user)?;
        let user = CString::new(service.config.user.clone())
            .with_context(|| format!("service user {:?} contains NUL", service.config.user))?;
        unsafe {
            command.pre_exec(move || {
                crate::unix_priv::drop_to_user_pre_exec(&user, identity.uid, identity.gid)
            });
        }
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn service {:?}", service.config.name))?;
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(copy_service_output(
            service.config.name.clone(),
            "stdout",
            stdout,
            service_log.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(copy_service_output(
            service.config.name.clone(),
            "stderr",
            stderr,
            service_log.clone(),
        ));
    }
    *child_slot = Some(child);
    Ok(())
}

async fn copy_service_output<R>(
    service: String,
    stream_name: &'static str,
    mut reader: R,
    log: Arc<Mutex<RotatingServiceLog>>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(count) => {
                if let Err(err) = log.lock().await.write_all(&buffer[..count]).await {
                    tracing::warn!(
                        service,
                        stream = stream_name,
                        error = %err,
                        "failed to write service output"
                    );
                    return;
                }
            }
            Err(err) => {
                tracing::warn!(
                    service,
                    stream = stream_name,
                    error = %err,
                    "failed to read service output"
                );
                return;
            }
        }
    }
}

pub(super) struct RotatingServiceLog {
    rotation: RotationState,
    pub(super) file: tokio::fs::File,
}

impl RotatingServiceLog {
    pub(super) async fn new(
        path: PathBuf,
        max_bytes: u64,
        max_files: usize,
    ) -> anyhow::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let bytes_written = file.metadata().await?.len();
        Ok(Self {
            rotation: RotationState::new(path, max_bytes, max_files, bytes_written),
            file,
        })
    }

    pub(super) async fn write_all(&mut self, buffer: &[u8]) -> anyhow::Result<()> {
        self.rotate_if_needed(buffer.len()).await?;
        self.file.write_all(buffer).await?;
        self.rotation.record_write(buffer.len());
        Ok(())
    }

    async fn rotate_if_needed(&mut self, incoming: usize) -> anyhow::Result<()> {
        if !self.rotation.should_rotate(incoming) {
            return Ok(());
        }
        self.file.flush().await?;
        let plan = self.rotation.rotation_plan();
        for step in plan.steps() {
            match step {
                RotationStep::Remove { path } => {
                    let _ = tokio::fs::remove_file(path).await;
                }
                RotationStep::Rename { from, to } => {
                    if tokio::fs::try_exists(from).await? {
                        tokio::fs::rename(from, to).await?;
                    }
                }
            }
        }
        self.file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(plan.active_path())
            .await
            .with_context(|| format!("open {}", plan.active_path().display()))?;
        self.rotation.reset_after_rotation();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ServiceUser {
    pub(super) uid: u32,
    pub(super) gid: u32,
}

pub(super) fn resolve_service_user(name: &str) -> anyhow::Result<ServiceUser> {
    let c_name = CString::new(name).context("service user contains NUL byte")?;
    unsafe {
        let pw = libc::getpwnam(c_name.as_ptr());
        if pw.is_null() {
            anyhow::bail!("service user {name:?} does not exist");
        }
        Ok(ServiceUser {
            uid: (*pw).pw_uid,
            gid: (*pw).pw_gid,
        })
    }
}

fn resolve_env(value: &EnvValue, vars: &Vars) -> anyhow::Result<Option<String>> {
    value.resolve(vars)
}

async fn http_health(
    url: &str,
    expect_status: u16,
    expect_json: &BTreeMap<String, String>,
) -> anyhow::Result<bool> {
    let response = http_get(url).await?;
    if !response.status_matches(expect_status) {
        return Ok(false);
    }
    if expect_json.is_empty() {
        return Ok(true);
    }
    let Some(body) = response.body() else {
        return Ok(false);
    };
    Ok(matches!(
        check_json_fields(body, expect_json)?,
        JsonFieldCheck::Match
    ))
}

pub(super) fn service_stop_order(services: &[Arc<ManagedService>]) -> Vec<Arc<ManagedService>> {
    fn visit(
        name: &str,
        services: &[Arc<ManagedService>],
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<Arc<ManagedService>>,
    ) {
        if !visited.insert(name.to_string()) {
            return;
        }
        for dependent in services
            .iter()
            .filter(|candidate| candidate.config.depends_on.iter().any(|dep| dep == name))
        {
            visit(&dependent.config.name, services, visited, ordered);
        }
        if let Some(service) = services
            .iter()
            .find(|candidate| candidate.config.name == name)
        {
            ordered.push(service.clone());
        }
    }

    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();
    for service in services {
        visit(&service.config.name, services, &mut visited, &mut ordered);
    }
    ordered
}

async fn stop_child_gracefully(service: &ManagedService, mut child: Child) {
    let timeout = service
        .config
        .shutdown_timeout
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(10));
    if let Some(pid) = child.id() {
        let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                tracing::warn!(
                    service = service.config.name,
                    pid,
                    error = %err,
                    "failed to send SIGTERM to service"
                );
            }
        }
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            tracing::info!(
                service = service.config.name,
                status = %status,
                "service stopped"
            );
        }
        Ok(Err(err)) => {
            tracing::warn!(
                service = service.config.name,
                error = %err,
                "failed while waiting for service shutdown"
            );
        }
        Err(_) => {
            tracing::warn!(
                service = service.config.name,
                timeout_ms = timeout.as_millis(),
                "service did not stop before timeout; sending SIGKILL"
            );
            let _ = child.kill().await;
        }
    }
}
