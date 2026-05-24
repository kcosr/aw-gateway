use crate::VERSION;
use crate::cli::{AgentArgs, AgentCommand, ConfigCommand};
use crate::config::{
    ContainerAgentFile, ControlSocketConfig, EnvValue, HealthCheck, IdleCleanupAction,
    IdleCleanupConfig, IdleCleanupOwner, LoggingConfig, RestartPolicy, ServiceConfig,
    parse_duration,
};
use crate::paths;
use crate::template::{self, Vars};
use anyhow::Context;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};

pub const DEFAULT_AGENT_CONFIG: &str = include_str!("../container-agent.sample.toml");
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONTROL_REQUEST_BYTES: usize = 64 * 1024;

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        Some(AgentCommand::Config(ConfigCommand::Validate)) => {
            let path = paths::agent_config_path(args.config);
            ContainerAgentFile::load(&path)?;
            println!("ok");
            Ok(())
        }
        Some(AgentCommand::Config(ConfigCommand::Init(init))) => {
            let path = init
                .path
                .unwrap_or_else(|| paths::agent_config_path(args.config));
            if path.exists() && !init.force {
                anyhow::bail!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                );
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_AGENT_CONFIG)?;
            println!("{}", path.display());
            Ok(())
        }
        Some(AgentCommand::Run) | None => run_agent(args.config).await,
    }
}

async fn run_agent(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = ContainerAgentFile::load(&paths::agent_config_path(config_path))?;
    let state_dir = std::env::var_os("AW_CONTAINER_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(paths::DEFAULT_AGENT_STATE_DIR));
    ensure_private_dir(&state_dir).await?;
    let bridge_enabled = cfg
        .container_agent
        .ssh_bridge
        .as_ref()
        .is_some_and(|bridge| bridge.enabled);
    let socket_owner = SocketOwner::from_env();
    let state = Arc::new(AgentState::new(
        state_dir.clone(),
        cfg.container_agent.idle_cleanup.clone(),
        bridge_enabled,
        std::env::var("AW_CONTAINER_CONTROL_TOKEN").ok(),
        socket_owner,
    ));

    let services: Vec<_> = cfg
        .container_agent
        .services
        .clone()
        .into_iter()
        .map(|service| {
            Arc::new(ManagedService::new(
                service,
                state_dir.clone(),
                cfg.logging.clone(),
            ))
        })
        .collect();
    *state.services.lock().await = services.clone();
    for service in services.clone() {
        tokio::spawn(service_supervisor(service, services.clone()));
    }

    if let Some(bridge) = cfg
        .container_agent
        .ssh_bridge
        .clone()
        .filter(|bridge| bridge.enabled)
    {
        let socket = bridge
            .socket
            .expect("validated enabled ssh_bridge must include socket");
        let bridge_state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = run_bridge(bridge_state, socket, bridge.target).await {
                tracing::error!(error = %err, "ssh bridge exited");
            }
        });
    }

    if state.idle_cleanup.is_some() {
        let cleanup_state = state.clone();
        tokio::spawn(async move {
            run_idle_cleanup(cleanup_state).await;
        });
    }

    if let Some(control_socket) = configured_control_socket(&cfg.container_agent.control_socket) {
        let mut vars = Vars::new();
        vars.insert(
            "container_state_dir".into(),
            state_dir.display().to_string(),
        );
        let control_socket = PathBuf::from(template::render(&control_socket, &vars)?);
        run_control_socket(state, &control_socket).await
    } else {
        wait_for_shutdown_signal(state).await
    }
}

fn configured_control_socket(config: &Option<ControlSocketConfig>) -> Option<String> {
    match config {
        Some(ControlSocketConfig::Path(path)) => Some(path.clone()),
        Some(ControlSocketConfig::Enabled(false)) => None,
        Some(ControlSocketConfig::Enabled(true)) | None => {
            Some("{container_state_dir}/agent.sock".into())
        }
    }
}

#[derive(Debug)]
struct AgentState {
    state_dir: PathBuf,
    services: Mutex<Vec<Arc<ManagedService>>>,
    idle_cleanup: Option<IdleCleanupConfig>,
    idle_state: Mutex<IdleRuntimeState>,
    bridge_enabled: bool,
    bridge_ready: AtomicBool,
    active_streams: AtomicUsize,
    active_sessions: AtomicUsize,
    accepting_bridge: AtomicBool,
    shutting_down: AtomicBool,
    control_token: Option<String>,
    socket_owner: Option<SocketOwner>,
}

impl AgentState {
    fn new(
        state_dir: PathBuf,
        idle_cleanup: Option<IdleCleanupConfig>,
        bridge_enabled: bool,
        control_token: Option<String>,
        socket_owner: Option<SocketOwner>,
    ) -> Self {
        let idle_cleanup = idle_cleanup.filter(|config| {
            config.owner == IdleCleanupOwner::Agent && config.action != IdleCleanupAction::None
        });
        Self {
            state_dir,
            services: Mutex::new(Vec::new()),
            idle_cleanup,
            idle_state: Mutex::new(IdleRuntimeState::default()),
            bridge_enabled,
            bridge_ready: AtomicBool::new(!bridge_enabled),
            active_streams: AtomicUsize::new(0),
            active_sessions: AtomicUsize::new(0),
            accepting_bridge: AtomicBool::new(true),
            shutting_down: AtomicBool::new(false),
            control_token,
            socket_owner,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SocketOwner {
    uid: u32,
    gid: u32,
}

impl SocketOwner {
    fn from_env() -> Option<Self> {
        let uid = std::env::var("AW_AUTHENTICATED_UID").ok()?.parse().ok()?;
        let gid = std::env::var("AW_AUTHENTICATED_GID").ok()?.parse().ok()?;
        Some(Self { uid, gid })
    }
}

#[derive(Debug, Default)]
struct IdleRuntimeState {
    state: IdleStateName,
    idle_since: Option<Instant>,
    preserve: bool,
    preserve_reason: Option<String>,
    matched_processes: Vec<ProcessMatch>,
    last_reap_result: Option<ReapResult>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum IdleStateName {
    #[default]
    IdlePending,
    Attached,
    Preserved,
    ShutdownContainer,
    ReapUnpreservedProcesses,
}

#[derive(Debug, Clone, Serialize)]
struct ProcessMatch {
    pid: u32,
    comm: String,
    #[serde(skip)]
    start_time: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ReapResult {
    dry_run: bool,
    would_terminate: Vec<ProcessMatch>,
    preserved: Vec<ProcessMatch>,
}

#[derive(Debug)]
struct ManagedService {
    config: ServiceConfig,
    state_dir: PathBuf,
    logging: LoggingConfig,
    child: Mutex<Option<Child>>,
    stopping: AtomicBool,
    restart_count: AtomicUsize,
    last_error: Mutex<Option<String>>,
}

impl ManagedService {
    fn new(config: ServiceConfig, state_dir: PathBuf, logging: LoggingConfig) -> Self {
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

    async fn status(&self) -> ServiceStatus {
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
}

async fn service_supervisor(service: Arc<ManagedService>, all_services: Vec<Arc<ManagedService>>) {
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

impl ManagedService {
    fn required_health_restart(&self) -> bool {
        self.config.required
            && !matches!(self.config.health_check, None | Some(HealthCheck::Process))
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

fn should_restart(policy: RestartPolicy, exit_success: bool) -> bool {
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => !exit_success,
        RestartPolicy::Always => true,
    }
}

async fn wait_for_dependencies(
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

fn health_check_interval(check: Option<&HealthCheck>) -> Duration {
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

fn health_check_timeout(check: Option<&HealthCheck>) -> Duration {
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
    let command_argv = render_command(&service.config.command, &vars)?;
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

struct RotatingServiceLog {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    file: tokio::fs::File,
    bytes_written: u64,
}

impl RotatingServiceLog {
    async fn new(path: PathBuf, max_bytes: u64, max_files: usize) -> anyhow::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let bytes_written = file.metadata().await?.len();
        Ok(Self {
            path,
            max_bytes: max_bytes.max(1),
            max_files,
            file,
            bytes_written,
        })
    }

    async fn write_all(&mut self, buffer: &[u8]) -> anyhow::Result<()> {
        self.rotate_if_needed(buffer.len()).await?;
        self.file.write_all(buffer).await?;
        self.bytes_written += buffer.len() as u64;
        Ok(())
    }

    async fn rotate_if_needed(&mut self, incoming: usize) -> anyhow::Result<()> {
        if self.max_files == 0 || self.bytes_written + incoming as u64 <= self.max_bytes {
            return Ok(());
        }
        self.file.flush().await?;
        for generation in (1..=self.max_files).rev() {
            let path = self.path_for_generation(generation);
            if generation == self.max_files {
                let _ = tokio::fs::remove_file(path).await;
            } else {
                let next = self.path_for_generation(generation + 1);
                if tokio::fs::try_exists(&path).await? {
                    tokio::fs::rename(path, next).await?;
                }
            }
        }
        if tokio::fs::try_exists(&self.path).await? {
            tokio::fs::rename(&self.path, self.path_for_generation(1)).await?;
        }
        self.file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)
            .await
            .with_context(|| format!("open {}", self.path.display()))?;
        self.bytes_written = 0;
        Ok(())
    }

    fn path_for_generation(&self, generation: usize) -> PathBuf {
        if generation == 0 {
            self.path.clone()
        } else {
            PathBuf::from(format!("{}.{}", self.path.display(), generation))
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ServiceUser {
    uid: u32,
    gid: u32,
}

fn resolve_service_user(name: &str) -> anyhow::Result<ServiceUser> {
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

fn render_command(command: &[String], vars: &Vars) -> anyhow::Result<Vec<String>> {
    command
        .iter()
        .map(|arg| template::render(arg, vars))
        .collect()
}

async fn http_health(
    url: &str,
    expect_status: u16,
    expect_json: &BTreeMap<String, String>,
) -> anyhow::Result<bool> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Ok(false);
    };
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match host_port.split_once(':') {
        Some((host, port)) => (host, port.parse::<u16>()?),
        None => (host_port, 80),
    };
    let mut stream = TcpStream::connect((host, port)).await?;
    let request = format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let response = String::from_utf8_lossy(&buf);
    let status_ok = response.starts_with(&format!("HTTP/1.1 {expect_status} "))
        || response.starts_with(&format!("HTTP/1.0 {expect_status} "));
    if !status_ok {
        return Ok(false);
    }
    if expect_json.is_empty() {
        return Ok(true);
    }
    let Some((_, body)) = response.split_once("\r\n\r\n") else {
        return Ok(false);
    };
    json_fields_match(body, expect_json)
}

fn json_fields_match(body: &str, expected: &BTreeMap<String, String>) -> anyhow::Result<bool> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    for (key, expected_value) in expected {
        let Some(actual) = value.get(key).and_then(serde_json::Value::as_str) else {
            return Ok(false);
        };
        if actual != expected_value {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn run_bridge(
    state: Arc<AgentState>,
    socket_template: String,
    target: String,
) -> anyhow::Result<()> {
    let mut vars = Vars::new();
    vars.insert(
        "container_state_dir".into(),
        state.state_dir.display().to_string(),
    );
    let socket = PathBuf::from(template::render(&socket_template, &vars)?);
    if let Some(parent) = socket.parent() {
        ensure_private_dir(parent).await?;
        apply_path_owner(parent, state.socket_owner)?;
    }
    unlink_socket_if_present(&socket).await?;
    let listener = UnixListener::bind(&socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).await?;
    }
    apply_path_owner(&socket, state.socket_owner)?;
    state.bridge_ready.store(true, Ordering::SeqCst);
    loop {
        let (client, _) = listener.accept().await?;
        if !state.accepting_bridge.load(Ordering::SeqCst) {
            continue;
        }
        let target = target.clone();
        let state = state.clone();
        tokio::spawn(async move {
            state.active_streams.fetch_add(1, Ordering::SeqCst);
            let result = proxy_to_tcp(client, &target).await;
            state.active_streams.fetch_sub(1, Ordering::SeqCst);
            if let Err(err) = result {
                tracing::warn!(error = %err, "ssh bridge stream failed");
            }
        });
    }
}

async fn proxy_to_tcp(mut client: UnixStream, target: &str) -> anyhow::Result<()> {
    let mut remote = TcpStream::connect(target).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

async fn run_idle_cleanup(state: Arc<AgentState>) {
    let Some(config) = state.idle_cleanup.clone() else {
        return;
    };
    let poll_interval = config
        .poll_interval
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(30));
    let idle_grace = config
        .idle_grace
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(300));

    loop {
        if state.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let transition = evaluate_idle_state(&state, &config, idle_grace).await;
        match transition {
            IdleTransition::None => {}
            IdleTransition::ShutdownContainer => {
                state.shutting_down.store(true, Ordering::SeqCst);
                state.accepting_bridge.store(false, Ordering::SeqCst);
                stop_services(&state).await;
                std::process::exit(0);
            }
            IdleTransition::ReapProcesses => {
                let managed = managed_service_pids(&state).await;
                let dry_run = std::env::var("AW_CONTAINER_AGENT_ALLOW_PROCESS_REAP")
                    .ok()
                    .as_deref()
                    != Some("1");
                let result = run_reap_processes(&config, &managed, dry_run).await;
                let mut idle = state.idle_state.lock().await;
                idle.state = IdleStateName::ReapUnpreservedProcesses;
                idle.last_reap_result = Some(result);
                idle.idle_since = Some(Instant::now());
            }
        }
        sleep(poll_interval).await;
    }
}

enum IdleTransition {
    None,
    ShutdownContainer,
    ReapProcesses,
}

async fn evaluate_idle_state(
    state: &AgentState,
    config: &IdleCleanupConfig,
    idle_grace: Duration,
) -> IdleTransition {
    let active_streams = state.active_streams.load(Ordering::SeqCst);
    let active_sessions = state.active_sessions.load(Ordering::SeqCst);
    let process_table = read_process_table(Path::new("/proc"));
    let matched_processes = find_preserve_processes(&process_table, &config.preserve_processes);
    let preserve = !matched_processes.is_empty();
    let now = Instant::now();
    let mut idle = state.idle_state.lock().await;

    if active_streams > 0 || active_sessions > 0 {
        idle.state = IdleStateName::Attached;
        idle.idle_since = None;
        idle.preserve = false;
        idle.preserve_reason = None;
        idle.matched_processes.clear();
        return IdleTransition::None;
    }

    if preserve {
        idle.state = IdleStateName::Preserved;
        idle.idle_since = None;
        idle.preserve = true;
        idle.preserve_reason = matched_processes
            .first()
            .map(|process| format!("process:{}", process.comm));
        idle.matched_processes = matched_processes;
        return IdleTransition::None;
    }

    let idle_since = *idle.idle_since.get_or_insert(now);
    idle.state = IdleStateName::IdlePending;
    idle.preserve = false;
    idle.preserve_reason = None;
    idle.matched_processes.clear();

    if now.duration_since(idle_since) < idle_grace {
        return IdleTransition::None;
    }

    match config.action {
        IdleCleanupAction::ExitContainer => {
            idle.state = IdleStateName::ShutdownContainer;
            IdleTransition::ShutdownContainer
        }
        IdleCleanupAction::ReapProcesses => IdleTransition::ReapProcesses,
        IdleCleanupAction::None => IdleTransition::None,
    }
}

fn find_preserve_processes(processes: &[ProcInfo], names: &[String]) -> Vec<ProcessMatch> {
    if names.is_empty() {
        return Vec::new();
    }
    let names: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    let mut matches: Vec<_> = processes
        .iter()
        .filter(|process| names.contains(process.comm.as_str()))
        .map(|process| ProcessMatch {
            pid: process.pid,
            comm: process.comm.clone(),
            start_time: process.start_time,
        })
        .collect();
    matches.sort_by_key(|process| process.pid);
    matches
}

fn reap_processes(
    config: &IdleCleanupConfig,
    managed_pids: &BTreeSet<u32>,
    dry_run: bool,
) -> ReapResult {
    let processes = read_process_table(Path::new("/proc"));
    let plan = build_reap_plan(
        &processes,
        config,
        managed_pids,
        current_uid(),
        std::process::id(),
    );
    if !dry_run {
        signal_processes(&plan.would_terminate, signal_number(&config.reap_signal));
    }
    ReapResult { dry_run, ..plan }
}

async fn run_reap_processes(
    config: &IdleCleanupConfig,
    managed_pids: &BTreeSet<u32>,
    dry_run: bool,
) -> ReapResult {
    let result = reap_processes(config, managed_pids, dry_run);
    if !dry_run
        && signal_number(&config.reap_signal) != libc::SIGKILL
        && let Some(delay) = config
            .reap_kill_after
            .as_deref()
            .and_then(|value| parse_duration(value).ok())
    {
        let candidates = result.would_terminate.clone();
        tokio::spawn(async move {
            sleep(delay).await;
            signal_matching_processes(&candidates, libc::SIGKILL);
        });
    }
    result
}

fn build_reap_plan(
    processes: &[ProcInfo],
    config: &IdleCleanupConfig,
    managed_pids: &BTreeSet<u32>,
    agent_uid: u32,
    agent_pid: u32,
) -> ReapResult {
    let preserve_roots = find_preserve_processes(processes, &config.preserve_processes);
    let mut preserved_pids: BTreeSet<u32> =
        preserve_roots.iter().map(|process| process.pid).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for process in processes {
            if preserved_pids.contains(&process.ppid) && preserved_pids.insert(process.pid) {
                changed = true;
            }
        }
    }

    let mut preserved: Vec<_> = processes
        .iter()
        .filter(|process| preserved_pids.contains(&process.pid))
        .map(ProcessMatch::from)
        .collect();
    preserved.sort_by_key(|process| process.pid);

    let mut would_terminate: Vec<_> = processes
        .iter()
        .filter(|process| process.pid != 1)
        .filter(|process| process.pid != agent_pid)
        .filter(|process| !managed_pids.contains(&process.pid))
        .filter(|process| !preserved_pids.contains(&process.pid))
        .filter(|process| process.uid != 0 || agent_uid != 0)
        .filter(|process| process.uid == agent_uid || agent_uid == 0)
        .map(ProcessMatch::from)
        .collect();
    would_terminate.sort_by_key(|process| process.pid);

    ReapResult {
        dry_run: true,
        would_terminate,
        preserved,
    }
}

fn read_process_table(proc_root: &Path) -> Vec<ProcInfo> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = file_name.parse::<u32>() else {
            continue;
        };
        if let Some(process) = read_proc_info(proc_root, pid) {
            processes.push(process);
        }
    }
    processes.sort_by_key(|process| process.pid);
    processes
}

fn read_proc_info(proc_root: &Path, pid: u32) -> Option<ProcInfo> {
    let dir = proc_root.join(pid.to_string());
    let comm = std::fs::read_to_string(dir.join("comm"))
        .ok()?
        .trim()
        .to_string();
    let start_time = read_proc_start_time(&dir);
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    let mut ppid = 0;
    let mut uid = 0;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("PPid:") {
            ppid = value.trim().parse().ok()?;
        } else if let Some(value) = line.strip_prefix("Uid:") {
            uid = value.split_whitespace().next()?.parse().ok()?;
        }
    }
    Some(ProcInfo {
        pid,
        ppid,
        uid,
        comm,
        start_time,
    })
}

fn read_proc_start_time(proc_dir: &Path) -> Option<u64> {
    let stat = std::fs::read_to_string(proc_dir.join("stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

async fn managed_service_pids(state: &AgentState) -> BTreeSet<u32> {
    let services = state.services.lock().await.clone();
    let mut pids = BTreeSet::new();
    for service in services {
        if let Some(child) = service.child.lock().await.as_ref()
            && let Some(pid) = child.id()
        {
            pids.insert(pid);
        }
    }
    pids
}

fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn signal_number(name: &str) -> i32 {
    match name {
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        _ => libc::SIGTERM,
    }
}

fn signal_processes(processes: &[ProcessMatch], signal: i32) {
    for process in processes {
        signal_process(process, signal);
    }
}

fn signal_matching_processes(processes: &[ProcessMatch], signal: i32) {
    for process in processes {
        match read_proc_info(Path::new("/proc"), process.pid) {
            Some(current) => {
                if current.comm == process.comm && current.start_time == process.start_time {
                    signal_process(process, signal);
                } else {
                    tracing::warn!(
                        pid = process.pid,
                        original_comm = process.comm,
                        current_comm = current.comm,
                        "skipping reap escalation because process identity changed"
                    );
                }
            }
            None => {
                tracing::debug!(
                    pid = process.pid,
                    comm = process.comm,
                    "skipping reap escalation because process exited"
                );
            }
        }
    }
}

fn signal_process(process: &ProcessMatch, signal: i32) {
    let rc = unsafe { libc::kill(process.pid as i32, signal) };
    if rc != 0 {
        tracing::warn!(
            pid = process.pid,
            comm = process.comm,
            error = %std::io::Error::last_os_error(),
            "failed to signal reap candidate"
        );
    } else {
        tracing::info!(
            pid = process.pid,
            comm = process.comm,
            signal,
            "signaled reap candidate"
        );
    }
}

fn process_exists(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

#[derive(Debug, Clone)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    uid: u32,
    comm: String,
    start_time: Option<u64>,
}

impl From<&ProcInfo> for ProcessMatch {
    fn from(value: &ProcInfo) -> Self {
        Self {
            pid: value.pid,
            comm: value.comm.clone(),
            start_time: value.start_time,
        }
    }
}

async fn unlink_socket_if_present(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    anyhow::bail!("refusing to unlink non-socket {}", path.display());
                }
            }
            tokio::fs::remove_file(path).await?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    }
    Ok(())
}

async fn run_control_socket(state: Arc<AgentState>, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).await?;
        apply_path_owner(parent, state.socket_owner)?;
    }
    unlink_socket_if_present(path).await?;
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    apply_path_owner(path, state.socket_owner)?;
    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                shutdown_agent(state).await;
                return Ok(());
            }
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_control_connection(state, stream).await {
                        tracing::warn!(error = %err, "control connection failed");
                    }
                });
            }
        }
    }
}

async fn wait_for_shutdown_signal(state: Arc<AgentState>) -> anyhow::Result<()> {
    shutdown_signal().await?;
    shutdown_agent(state).await;
    Ok(())
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("install SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("wait for Ctrl-C")?;
            }
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("wait for Ctrl-C")?;
    }
    Ok(())
}

async fn shutdown_agent(state: Arc<AgentState>) {
    state.shutting_down.store(true, Ordering::SeqCst);
    state.accepting_bridge.store(false, Ordering::SeqCst);
    stop_services(&state).await;
}

async fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

async fn handle_control_connection(
    state: Arc<AgentState>,
    stream: UnixStream,
) -> anyhow::Result<()> {
    validate_control_peer(&stream, state.socket_owner.map(|owner| owner.uid))?;
    let mut reader = BufReader::new(stream);
    let line = match tokio::time::timeout(CONTROL_READ_TIMEOUT, read_control_request(&mut reader))
        .await
    {
        Ok(Ok(line)) => line,
        Ok(Err(err)) if err.to_string().contains("exceeds") => {
            let response = serde_json::json!({"id": serde_json::Value::Null, "ok": false, "error": {"code": "request_too_large", "message": "control request is too large"}});
            write_control_response(reader.into_inner(), response).await?;
            return Ok(());
        }
        Ok(Err(err)) => return Err(err),
        Err(_) => anyhow::bail!("timed out reading control request"),
    };
    let Some(line) = line else {
        anyhow::bail!("empty control request");
    };
    let request: serde_json::Value = match serde_json::from_slice(&line) {
        Ok(request) => request,
        Err(err) => {
            let response = serde_json::json!({"id": serde_json::Value::Null, "ok": false, "error": {"code": "parse_error", "message": err.to_string()}});
            write_control_response(reader.into_inner(), response).await?;
            return Ok(());
        }
    };
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let (response, exit_after_response) = match method {
        "status" => (
            serde_json::json!({"id": id, "ok": true, "result": status_payload(&state).await}),
            false,
        ),
        "session_hold" => {
            if let Some(response) = unauthorized_if_needed(&state, &request, &id) {
                write_control_response(reader.into_inner(), response).await?;
                return Ok(());
            }
            let mut stream = reader.into_inner();
            write_control_response_ref(
                &mut stream,
                serde_json::json!({"id": id, "ok": true, "result": {"held": true}}),
            )
            .await?;
            hold_control_session(state, stream).await;
            return Ok(());
        }
        "shutdown" => {
            if let Some(response) = unauthorized_if_needed(&state, &request, &id) {
                write_control_response(reader.into_inner(), response).await?;
                return Ok(());
            }
            state.shutting_down.store(true, Ordering::SeqCst);
            state.accepting_bridge.store(false, Ordering::SeqCst);
            stop_services(&state).await;
            (
                serde_json::json!({"id": id, "ok": true, "result": {"shutting_down": true}}),
                true,
            )
        }
        "reap_now" => {
            if let Some(response) = unauthorized_if_needed(&state, &request, &id) {
                write_control_response(reader.into_inner(), response).await?;
                return Ok(());
            }
            let result = state
                .idle_cleanup
                .as_ref()
                .map(|config| reap_processes(config, &BTreeSet::new(), true))
                .unwrap_or(ReapResult {
                    dry_run: true,
                    would_terminate: Vec::new(),
                    preserved: Vec::new(),
                });
            state.idle_state.lock().await.last_reap_result = Some(result.clone());
            (
                serde_json::json!({"id": id, "ok": true, "result": result}),
                false,
            )
        }
        _ => (
            serde_json::json!({"id": id, "ok": false, "error": {"code": "unknown_method", "message": "unknown control method"}}),
            false,
        ),
    };
    write_control_response(reader.into_inner(), response).await?;
    if exit_after_response {
        tokio::spawn(async {
            sleep(Duration::from_millis(10)).await;
            std::process::exit(0);
        });
    }
    Ok(())
}

async fn read_control_request(
    reader: &mut BufReader<UnixStream>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if line.len() + end > MAX_CONTROL_REQUEST_BYTES {
            anyhow::bail!("control request exceeds {MAX_CONTROL_REQUEST_BYTES} bytes");
        }
        line.extend_from_slice(&available[..end]);
        reader.consume(end);
        if line.ends_with(b"\n") {
            return Ok(Some(line));
        }
    }
}

fn apply_path_owner(path: &Path, owner: Option<SocketOwner>) -> anyhow::Result<()> {
    let Some(owner) = owner else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path contains NUL byte: {}", path.display()))?;
        let rc = unsafe { libc::chown(c_path.as_ptr(), owner.uid, owner.gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown {}", path.display()));
        }
    }
    Ok(())
}

fn validate_control_peer(stream: &UnixStream, expected_uid: Option<u32>) -> anyhow::Result<()> {
    let Some(expected_uid) = expected_uid else {
        return Ok(());
    };
    let peer = unix_peer_credentials(stream)?;
    if peer.uid != expected_uid {
        anyhow::bail!(
            "control peer uid mismatch: expected {}, got {}",
            expected_uid,
            peer.uid
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct PeerCredentials {
    uid: u32,
}

#[cfg(target_os = "linux")]
fn unix_peer_credentials(stream: &UnixStream) -> anyhow::Result<PeerCredentials> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getsockopt SO_PEERCRED");
    }
    Ok(PeerCredentials { uid: cred.uid })
}

#[cfg(target_os = "macos")]
fn unix_peer_credentials(stream: &UnixStream) -> anyhow::Result<PeerCredentials> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getpeereid");
    }
    Ok(PeerCredentials { uid })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unix_peer_credentials(_stream: &UnixStream) -> anyhow::Result<PeerCredentials> {
    anyhow::bail!("Unix peer credential validation is not supported on this platform")
}

fn unauthorized_if_needed(
    state: &AgentState,
    request: &serde_json::Value,
    id: &serde_json::Value,
) -> Option<serde_json::Value> {
    let expected = state.control_token.as_deref()?;
    let actual = request
        .get("params")
        .and_then(|params| params.get("token"))
        .and_then(serde_json::Value::as_str);
    if actual == Some(expected) {
        return None;
    }
    Some(serde_json::json!({
        "id": id,
        "ok": false,
        "error": {
            "code": "unauthorized",
            "message": "control token is required"
        }
    }))
}

async fn write_control_response(
    mut stream: UnixStream,
    response: serde_json::Value,
) -> anyhow::Result<()> {
    write_control_response_ref(&mut stream, response).await
}

async fn write_control_response_ref(
    stream: &mut UnixStream,
    response: serde_json::Value,
) -> anyhow::Result<()> {
    stream
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn hold_control_session(state: Arc<AgentState>, mut stream: UnixStream) {
    state.active_sessions.fetch_add(1, Ordering::SeqCst);
    let mut buffer = [0_u8; 1024];
    loop {
        tokio::select! {
            read = stream.read(&mut buffer) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            _ = sleep(Duration::from_secs(1)), if state.shutting_down.load(Ordering::SeqCst) => {
                break;
            }
        }
    }
    state.active_sessions.fetch_sub(1, Ordering::SeqCst);
}

async fn stop_services(state: &AgentState) {
    let services = state.services.lock().await.clone();
    for service in service_stop_order(&services) {
        service.stopping.store(true, Ordering::SeqCst);
        if let Some(child) = service.child.lock().await.take() {
            stop_child_gracefully(&service, child).await;
        }
    }
}

fn service_stop_order(services: &[Arc<ManagedService>]) -> Vec<Arc<ManagedService>> {
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

async fn status_payload(state: &AgentState) -> AgentStatus {
    let services = state.services.lock().await.clone();
    let mut service_status = Vec::new();
    let mut ready = true;
    for service in services {
        let status = service.status().await;
        if service.config.required && !status.healthy {
            ready = false;
        }
        service_status.push(status);
    }
    if state.shutting_down.load(Ordering::SeqCst) {
        ready = false;
    }
    if state.bridge_enabled
        && (!state.bridge_ready.load(Ordering::SeqCst)
            || !state.accepting_bridge.load(Ordering::SeqCst))
    {
        ready = false;
    }
    AgentStatus {
        ready,
        version: VERSION.to_string(),
        services: service_status,
        ssh_bridge: BridgeStatus {
            enabled: state.bridge_enabled,
            ready: !state.shutting_down.load(Ordering::SeqCst)
                && state.accepting_bridge.load(Ordering::SeqCst)
                && state.bridge_ready.load(Ordering::SeqCst),
            active_streams: state.active_streams.load(Ordering::SeqCst),
            active_sessions: state.active_sessions.load(Ordering::SeqCst),
        },
        idle_cleanup: idle_cleanup_status(state).await,
        shutting_down: state.shutting_down.load(Ordering::SeqCst),
    }
}

async fn idle_cleanup_status(state: &AgentState) -> Option<IdleCleanupStatus> {
    let config = state.idle_cleanup.as_ref()?;
    let idle = state.idle_state.lock().await;
    Some(IdleCleanupStatus {
        owner: "agent".to_string(),
        action: idle_action_name(config.action).to_string(),
        state: idle.state,
        idle_for_ms: idle.idle_since.map(|since| since.elapsed().as_millis()),
        preserve: idle.preserve,
        preserve_reason: idle.preserve_reason.clone(),
        matched_processes: idle.matched_processes.clone(),
        last_reap_result: idle.last_reap_result.clone(),
    })
}

fn idle_action_name(action: IdleCleanupAction) -> &'static str {
    match action {
        IdleCleanupAction::None => "none",
        IdleCleanupAction::ExitContainer => "exit_container",
        IdleCleanupAction::ReapProcesses => "reap_processes",
    }
}

#[derive(Debug, Serialize)]
struct AgentStatus {
    ready: bool,
    version: String,
    services: Vec<ServiceStatus>,
    ssh_bridge: BridgeStatus,
    idle_cleanup: Option<IdleCleanupStatus>,
    shutting_down: bool,
}

#[derive(Debug, Serialize)]
struct IdleCleanupStatus {
    owner: String,
    action: String,
    state: IdleStateName,
    idle_for_ms: Option<u128>,
    preserve: bool,
    preserve_reason: Option<String>,
    matched_processes: Vec<ProcessMatch>,
    last_reap_result: Option<ReapResult>,
}

#[derive(Debug, Serialize)]
struct ServiceStatus {
    name: String,
    required: bool,
    state: String,
    pid: Option<u32>,
    healthy: bool,
    restart_count: usize,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BridgeStatus {
    enabled: bool,
    ready: bool,
    active_streams: usize,
    active_sessions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn sample_agent_config_validates() {
        let cfg: ContainerAgentFile = toml::from_str(DEFAULT_AGENT_CONFIG).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn reap_plan_preserves_named_process_tree_and_managed_services() {
        let config = IdleCleanupConfig {
            action: IdleCleanupAction::ReapProcesses,
            preserve_processes: vec!["tmux".to_string()],
            ..IdleCleanupConfig::default()
        };
        let processes = vec![
            proc_info(1, 0, 0, "init"),
            proc_info(10, 1, 0, "aw-container-agent"),
            proc_info(20, 1, 0, "sshd"),
            proc_info(30, 1, 1000, "bash"),
            proc_info(40, 1, 1000, "tmux"),
            proc_info(41, 40, 1000, "codex"),
            proc_info(50, 30, 1000, "node"),
        ];
        let managed = BTreeSet::from([20]);
        let plan = build_reap_plan(&processes, &config, &managed, 0, 10);
        let terminate: Vec<_> = plan
            .would_terminate
            .iter()
            .map(|process| process.pid)
            .collect();
        let preserved: Vec<_> = plan.preserved.iter().map(|process| process.pid).collect();
        assert_eq!(terminate, vec![30, 50]);
        assert_eq!(preserved, vec![40, 41]);
    }

    #[test]
    fn reap_plan_for_non_root_agent_only_targets_same_uid() {
        let config = IdleCleanupConfig {
            action: IdleCleanupAction::ReapProcesses,
            preserve_processes: Vec::new(),
            ..IdleCleanupConfig::default()
        };
        let processes = vec![
            proc_info(1, 0, 0, "init"),
            proc_info(10, 1, 1000, "aw-container-agent"),
            proc_info(20, 1, 0, "root-service"),
            proc_info(30, 1, 1000, "bash"),
            proc_info(40, 1, 1001, "other-user"),
        ];
        let plan = build_reap_plan(&processes, &config, &BTreeSet::new(), 1000, 10);
        let terminate: Vec<_> = plan
            .would_terminate
            .iter()
            .map(|process| process.pid)
            .collect();
        assert_eq!(terminate, vec![30]);
    }

    #[test]
    fn resolves_root_service_user() {
        let root = resolve_service_user("root").unwrap();
        assert_eq!(root.uid, 0);
        assert_eq!(root.gid, 0);
    }

    #[test]
    fn restart_policy_only_restarts_on_failure_when_configured() {
        assert!(!should_restart(RestartPolicy::Never, false));
        assert!(!should_restart(RestartPolicy::Never, true));
        assert!(should_restart(RestartPolicy::Always, false));
        assert!(should_restart(RestartPolicy::Always, true));
        assert!(should_restart(RestartPolicy::OnFailure, false));
        assert!(!should_restart(RestartPolicy::OnFailure, true));
    }

    #[test]
    fn required_health_restart_only_applies_to_required_non_process_checks() {
        let service = ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                    interval: None,
                    timeout: None,
                }),
                ..test_service("proxy", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        assert!(service.required_health_restart());

        let optional = ManagedService::new(
            ServiceConfig {
                required: false,
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                    interval: None,
                    timeout: None,
                }),
                ..test_service("metrics", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        assert!(!optional.required_health_restart());

        let process = ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Process),
                ..test_service("worker", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        assert!(!process.required_health_restart());
    }

    #[test]
    fn health_check_timing_uses_configured_interval_and_timeout() {
        let check = HealthCheck::Tcp {
            host: "127.0.0.1".into(),
            port: 1,
            interval: Some("3s".into()),
            timeout: Some("75ms".into()),
        };
        assert_eq!(health_check_interval(Some(&check)), Duration::from_secs(3));
        assert_eq!(
            health_check_timeout(Some(&check)),
            Duration::from_millis(75)
        );
        assert_eq!(health_check_interval(None), Duration::from_millis(250));
        assert_eq!(health_check_timeout(None), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn dependency_wait_can_exceed_startup_timeout_until_dependency_is_healthy() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);

        let proxy = Arc::new(ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port,
                    interval: Some("10ms".into()),
                    timeout: Some("10ms".into()),
                }),
                ..test_service("proxy", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let sshd = ManagedService::new(
            ServiceConfig {
                startup_timeout: Some("25ms".into()),
                ..test_service("container-sshd", vec!["proxy"])
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        let listener = tokio::spawn(async move {
            sleep(Duration::from_millis(150)).await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            let _ = listener.accept().await;
        });

        let started_at = Instant::now();
        let ready = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_dependencies(&sshd, &[proxy]),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(ready);
        assert!(started_at.elapsed() >= Duration::from_millis(100));
        assert!(sshd.last_error.lock().await.is_none());
        listener.abort();
    }

    #[tokio::test]
    async fn dependency_wait_exits_when_service_is_stopping() {
        let proxy = Arc::new(ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                    interval: Some("1s".into()),
                    timeout: Some("10ms".into()),
                }),
                ..test_service("proxy", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let sshd = ManagedService::new(
            test_service("container-sshd", vec!["proxy"]),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        sshd.stopping.store(true, Ordering::SeqCst);

        let ready = wait_for_dependencies(&sshd, &[proxy]).await.unwrap();

        assert!(!ready);
    }

    #[test]
    fn json_health_expectation_matches_top_level_fields() {
        let expected = BTreeMap::from([("status".to_string(), "ready".to_string())]);
        assert!(json_fields_match(r#"{"status":"ready"}"#, &expected).unwrap());
        assert!(!json_fields_match(r#"{"status":"starting"}"#, &expected).unwrap());
        assert!(!json_fields_match(r#"{"state":"ready"}"#, &expected).unwrap());
    }

    #[test]
    fn service_command_templates_render_container_state_dir() {
        let vars = BTreeMap::from([(
            "container_state_dir".to_string(),
            "/tmp/agent-state".to_string(),
        )]);
        let command = vec![
            "/bin/echo".to_string(),
            "{container_state_dir}/ready".to_string(),
        ];

        assert_eq!(
            render_command(&command, &vars).unwrap(),
            vec![
                "/bin/echo".to_string(),
                "/tmp/agent-state/ready".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn status_is_not_ready_when_agent_is_shutting_down() {
        let state = AgentState::new(PathBuf::from("/tmp"), None, true, None, None);
        state.bridge_ready.store(true, Ordering::SeqCst);
        state.shutting_down.store(true, Ordering::SeqCst);

        let status = status_payload(&state).await;

        assert!(!status.ready);
        assert!(!status.ssh_bridge.ready);
    }

    #[tokio::test]
    async fn shutdown_agent_disables_bridge_accepts() {
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            true,
            None,
            None,
        ));
        state.accepting_bridge.store(true, Ordering::SeqCst);

        shutdown_agent(state.clone()).await;

        assert!(state.shutting_down.load(Ordering::SeqCst));
        assert!(!state.accepting_bridge.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn ensure_private_dir_sets_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        ensure_private_dir(&path).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[tokio::test]
    async fn rotating_service_log_rotates_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.log");
        let mut log = RotatingServiceLog::new(path.clone(), 8, 2).await.unwrap();
        log.write_all(b"12345678").await.unwrap();
        log.write_all(b"abcdef").await.unwrap();
        log.file.flush().await.unwrap();

        assert!(path.exists());
        assert!(dir.path().join("service.log.1").exists());
        assert!(!dir.path().join("service.log.3").exists());
    }

    #[tokio::test]
    async fn rotating_service_log_can_disable_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.log");
        let mut log = RotatingServiceLog::new(path.clone(), 4, 0).await.unwrap();
        log.write_all(b"1234").await.unwrap();
        log.write_all(b"5678").await.unwrap();
        log.file.flush().await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"12345678");
        assert!(!dir.path().join("service.log.1").exists());
    }

    #[tokio::test]
    async fn control_peer_validation_checks_uid() {
        let (client, _server) = UnixStream::pair().unwrap();
        validate_control_peer(&client, Some(current_uid())).unwrap();
        assert!(validate_control_peer(&client, Some(current_uid().wrapping_add(1))).is_err());
    }

    #[test]
    fn process_exists_detects_current_process() {
        assert!(process_exists(std::process::id()));
    }

    #[test]
    fn service_stop_order_stops_dependents_before_dependencies() {
        let sshd = Arc::new(ManagedService::new(
            test_service("sshd", Vec::new()),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let proxy = Arc::new(ManagedService::new(
            test_service("proxy", vec!["sshd"]),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let metrics = Arc::new(ManagedService::new(
            test_service("metrics", Vec::new()),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let ordered_services = service_stop_order(&[sshd, proxy, metrics]);
        let ordered: Vec<_> = ordered_services
            .iter()
            .map(|service| service.config.name.clone())
            .collect();
        assert_eq!(ordered, vec!["proxy", "sshd", "metrics"]);
    }

    fn test_service(name: &str, depends_on: Vec<&str>) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            required: true,
            user: "root".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            cwd: None,
            restart: RestartPolicy::Always,
            restart_backoff: None,
            restart_backoff_max: None,
            startup_timeout: None,
            shutdown_timeout: None,
            depends_on: depends_on.into_iter().map(str::to_string).collect(),
            env: BTreeMap::new(),
            health_check: None,
        }
    }

    fn proc_info(pid: u32, ppid: u32, uid: u32, comm: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            uid,
            comm: comm.to_string(),
            start_time: Some(pid as u64 * 10),
        }
    }
}
