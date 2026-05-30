use crate::config::{ContainerRuntimeType, RuntimeConfig};
use crate::template::{self, Vars};
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

mod parse;

pub use parse::parse_managed_containers;

#[derive(Debug, Clone)]
pub struct ContainerRuntime {
    kind: ContainerRuntimeType,
    program: String,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ContainerRunSpec {
    pub name: String,
    pub hostname: String,
    pub image: String,
    pub workspace: PathBuf,
    pub container_home: PathBuf,
    pub container_user: String,
    pub passwd_entry: Option<String>,
    pub state_dir_in_container: PathBuf,
    pub mounts: Vec<ContainerMountSpec>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub publish_ssh: bool,
    pub extra_run_args: Vec<String>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ContainerMountSpec {
    pub source: PathBuf,
    pub target: PathBuf,
    pub readonly: bool,
}

#[derive(Debug, Clone)]
pub struct ContainerExecSpec {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub user: String,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub container_name: String,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerPtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Debug)]
pub struct ContainerPtySession {
    pub output: mpsc::Receiver<Vec<u8>>,
    pub input: mpsc::Sender<Vec<u8>>,
    pub resize: mpsc::Sender<ContainerPtySize>,
    pub exit: JoinHandle<anyhow::Result<i32>>,
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    child_pid: Option<u32>,
    cleanup_runtime: ContainerRuntime,
    cleanup_spec: ContainerExecSpec,
    cleanup_marker_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerExecCaptureResult {
    Completed(ContainerExecOutput),
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerExecStatusResult {
    Completed(i32),
    Canceled,
}

const CANCEL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const CANCEL_HOST_TERMINATE_DELAY: Duration = Duration::from_millis(100);
const CANCEL_INITIAL_CLEANUP_JOIN_TIMEOUT: Duration = Duration::from_millis(2500);
const CANCEL_HOST_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

impl ContainerPtySession {
    pub async fn terminate(&self) -> anyhow::Result<()> {
        tracing::info!(
            container = %self.cleanup_spec.container_name,
            marker = %self.cleanup_marker_path,
            child_pid = ?self.child_pid,
            "terminating container pty session"
        );
        let cleanup_runtime = self.cleanup_runtime.clone();
        let cleanup_spec = self.cleanup_spec.clone();
        let cleanup_marker_path = self.cleanup_marker_path.clone();
        let cleanup_container_name = self.cleanup_spec.container_name.clone();
        let initial_cleanup_runtime = cleanup_runtime.clone();
        let initial_cleanup_spec = cleanup_spec.clone();
        let initial_cleanup_marker_path = cleanup_marker_path.clone();
        let initial_cleanup_container_name = cleanup_container_name.clone();
        let mut cleanup = tokio::spawn(async move {
            run_container_cancel_cleanup(
                initial_cleanup_runtime,
                initial_cleanup_spec,
                initial_cleanup_marker_path,
                initial_cleanup_container_name,
                "initial",
            )
            .await
        });
        let killer = self.killer.clone();
        let child_pid = self.child_pid;
        let host_terminate = async move {
            tokio::time::sleep(CANCEL_HOST_TERMINATE_DELAY).await;
            terminate_host_pty_child(killer, child_pid).await
        };

        let host_result = host_terminate.await;
        let initial_cleanup_finished =
            match tokio::time::timeout(CANCEL_INITIAL_CLEANUP_JOIN_TIMEOUT, &mut cleanup).await {
                Ok(Ok(ok)) => ok,
                Ok(Err(err)) => {
                    tracing::warn!(
                        error = %err,
                        container = %cleanup_container_name,
                        marker = %cleanup_marker_path,
                        "initial container pty cleanup task failed"
                    );
                    false
                }
                Err(_) => {
                    cleanup.abort();
                    tracing::warn!(
                        container = %cleanup_container_name,
                        marker = %cleanup_marker_path,
                        "initial container pty cleanup task timed out"
                    );
                    false
                }
            };
        if !initial_cleanup_finished {
            run_container_cancel_cleanup(
                cleanup_runtime,
                cleanup_spec,
                cleanup_marker_path,
                cleanup_container_name,
                "post-host-terminate",
            )
            .await;
        }
        host_result
    }
}

async fn run_container_cancel_cleanup(
    runtime: ContainerRuntime,
    spec: ContainerExecSpec,
    marker_path: String,
    container_name: String,
    phase: &'static str,
) -> bool {
    match runtime
        .exec_discard_with_timeout(&spec, Some(CANCEL_CLEANUP_TIMEOUT))
        .await
    {
        Ok(exit_code) => {
            tracing::info!(
                container = %container_name,
                marker = %marker_path,
                phase,
                exit_code,
                "container cancel cleanup exec finished"
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                container = %container_name,
                marker = %marker_path,
                phase,
                "container cancel cleanup exec failed"
            );
            false
        }
    }
}

async fn run_container_marker_remove(
    runtime: ContainerRuntime,
    spec: ContainerExecSpec,
    marker_path: String,
    container_name: String,
    phase: &'static str,
) -> bool {
    match runtime
        .exec_discard_with_timeout(&spec, Some(CANCEL_CLEANUP_TIMEOUT))
        .await
    {
        Ok(exit_code) => {
            tracing::debug!(
                container = %container_name,
                marker = %marker_path,
                phase,
                exit_code,
                "container cancel marker remove exec finished"
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                container = %container_name,
                marker = %marker_path,
                phase,
                "container cancel marker remove exec failed"
            );
            false
        }
    }
}

async fn terminate_host_pty_child(
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    child_pid: Option<u32>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        if let Some(child_pid) = child_pid {
            terminate_process_group(child_pid);
        }
        killer
            .lock()
            .map_err(|_| anyhow::anyhow!("pty child killer lock poisoned"))?
            .kill()
            .map_err(anyhow::Error::from)
    })
    .await
    .context("join pty child terminate task")?
}

async fn read_pipe_to_end<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn cancel_container_exec_child(
    runtime: ContainerRuntime,
    cleanup_spec: ContainerExecSpec,
    marker_path: String,
    container_name: String,
    child_pid: Option<u32>,
    mut wait_task: JoinHandle<std::io::Result<ExitStatus>>,
) {
    tracing::info!(
        container = %container_name,
        marker = %marker_path,
        child_pid = ?child_pid,
        "canceling container exec"
    );
    let initial_cleanup_runtime = runtime.clone();
    let initial_cleanup_spec = cleanup_spec.clone();
    let initial_marker_path = marker_path.clone();
    let initial_container_name = container_name.clone();
    let mut cleanup = tokio::spawn(async move {
        run_container_cancel_cleanup(
            initial_cleanup_runtime,
            initial_cleanup_spec,
            initial_marker_path,
            initial_container_name,
            "initial",
        )
        .await
    });

    tokio::time::sleep(CANCEL_HOST_TERMINATE_DELAY).await;
    terminate_host_exec_child(child_pid).await;

    let initial_cleanup_finished =
        match tokio::time::timeout(CANCEL_INITIAL_CLEANUP_JOIN_TIMEOUT, &mut cleanup).await {
            Ok(Ok(ok)) => ok,
            Ok(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    container = %container_name,
                    marker = %marker_path,
                    "initial container exec cleanup task failed"
                );
                false
            }
            Err(_) => {
                cleanup.abort();
                tracing::warn!(
                    container = %container_name,
                    marker = %marker_path,
                    "initial container exec cleanup task timed out"
                );
                false
            }
        };
    if !initial_cleanup_finished {
        run_container_cancel_cleanup(
            runtime,
            cleanup_spec,
            marker_path.clone(),
            container_name.clone(),
            "post-host-terminate",
        )
        .await;
    }

    match tokio::time::timeout(CANCEL_HOST_WAIT_TIMEOUT, &mut wait_task).await {
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(err))) => {
            tracing::warn!(
                error = %err,
                container = %container_name,
                marker = %marker_path,
                "host runtime exec wait failed after cancellation"
            );
        }
        Ok(Err(err)) => {
            tracing::warn!(
                error = %err,
                container = %container_name,
                marker = %marker_path,
                "host runtime exec wait task failed after cancellation"
            );
        }
        Err(_) => {
            wait_task.abort();
            tracing::warn!(
                container = %container_name,
                marker = %marker_path,
                "host runtime exec did not exit after cancellation"
            );
        }
    }
}

async fn terminate_host_exec_child(child_pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(child_pid) = child_pid {
        terminate_process_group(child_pid);
    }
}

impl Drop for ContainerPtySession {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(child_pid) = self.child_pid {
            terminate_process_group_nonblocking(child_pid);
        }
        match self.killer.lock() {
            Ok(mut killer) => {
                if let Err(err) = killer.kill() {
                    tracing::debug!(error = %err, "pty child terminate on drop failed");
                }
            }
            Err(_) => {
                tracing::debug!("pty child killer lock poisoned during drop");
            }
        }
    }
}

static NEXT_CANCEL_MARKER_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
fn terminate_process_group(child_pid: u32) {
    terminate_process_group_nonblocking(child_pid);
    std::thread::sleep(Duration::from_millis(100));
    signal_process_group(child_pid, libc::SIGKILL);
    signal_process(child_pid, libc::SIGKILL);
}

#[cfg(unix)]
fn terminate_process_group_nonblocking(child_pid: u32) {
    signal_process_group(child_pid, libc::SIGHUP);
    signal_process_group(child_pid, libc::SIGTERM);
    signal_process(child_pid, libc::SIGHUP);
    signal_process(child_pid, libc::SIGTERM);
}

#[cfg(unix)]
fn signal_process_group(child_pid: u32, signal: libc::c_int) {
    let pid = child_pid as libc::pid_t;
    if pid > 1 {
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

#[cfg(unix)]
fn signal_process(child_pid: u32, signal: libc::c_int) {
    let pid = child_pid as libc::pid_t;
    if pid > 1 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

fn run_pty_master_pump(
    master: Box<dyn portable_pty::MasterPty + Send>,
    output_tx: mpsc::Sender<Vec<u8>>,
    mut resize_rx: mpsc::Receiver<ContainerPtySize>,
) {
    #[cfg(unix)]
    {
        run_unix_pty_master_pump(master, output_tx, &mut resize_rx);
    }
    #[cfg(not(unix))]
    {
        let mut reader = match master.try_clone_reader() {
            Ok(reader) => reader,
            Err(err) => {
                tracing::warn!(error = %err, "pty reader clone failed");
                return;
            }
        };
        let mut buffer = [0_u8; 8192];
        loop {
            while let Ok(size) = resize_rx.try_recv() {
                if let Err(err) = master.resize(pty_size(size)) {
                    tracing::warn!(error = %err, "pty resize failed");
                }
            }
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if output_tx.blocking_send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
}

#[cfg(unix)]
fn run_unix_pty_master_pump(
    master: Box<dyn portable_pty::MasterPty + Send>,
    output_tx: mpsc::Sender<Vec<u8>>,
    resize_rx: &mut mpsc::Receiver<ContainerPtySize>,
) {
    use std::io;
    use std::os::fd::RawFd;

    const POLL_TIMEOUT_MS: libc::c_int = 100;

    let Some(fd) = master.as_raw_fd() else {
        tracing::warn!("pty master does not expose a unix fd");
        return;
    };
    let mut buffer = [0_u8; 8192];
    loop {
        if !drain_pty_resize_requests(&*master, resize_rx) {
            break;
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut poll_fd, 1, POLL_TIMEOUT_MS) };
        if poll_result < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(error = %err, "pty poll failed");
            break;
        }
        if poll_result == 0 {
            continue;
        }
        if poll_fd.revents & (libc::POLLERR | libc::POLLHUP) != 0
            && poll_fd.revents & libc::POLLIN == 0
        {
            break;
        }
        if poll_fd.revents & libc::POLLIN == 0 {
            continue;
        }
        let bytes_read = unsafe {
            libc::read(
                fd as RawFd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        if bytes_read == 0 {
            break;
        }
        if bytes_read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(error = %err, "pty read failed");
            break;
        }
        let bytes_read = bytes_read as usize;
        if output_tx
            .blocking_send(buffer[..bytes_read].to_vec())
            .is_err()
        {
            break;
        }
    }
}

fn drain_pty_resize_requests(
    master: &dyn portable_pty::MasterPty,
    resize_rx: &mut mpsc::Receiver<ContainerPtySize>,
) -> bool {
    loop {
        match resize_rx.try_recv() {
            Ok(size) => {
                if let Err(err) = master.resize(pty_size(size)) {
                    tracing::warn!(error = %err, "pty resize failed");
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => return true,
            Err(mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContainerInspect {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub config: ContainerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedContainer {
    pub name: String,
    pub image: String,
    pub running: bool,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerState {
    #[serde(rename = "Running")]
    pub running: bool,
    #[serde(rename = "Pid")]
    pub pid: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Labels", default)]
    pub labels: BTreeMap<String, String>,
}

impl ContainerRuntime {
    pub fn from_config(cfg: &RuntimeConfig, user: &str, home: &Path) -> anyhow::Result<Self> {
        let program = cfg
            .program
            .clone()
            .unwrap_or_else(|| default_program(cfg.runtime_type).to_string());
        let mut env = BTreeMap::new();
        match cfg.runtime_type {
            ContainerRuntimeType::Podman => {}
            ContainerRuntimeType::Docker => {
                if let Some(docker_host) = &cfg.docker_host {
                    env.insert(
                        "DOCKER_HOST".to_string(),
                        render_runtime_value(docker_host, user, home)?,
                    );
                }
            }
            ContainerRuntimeType::Colima => {
                let profile = cfg.profile.as_deref().unwrap_or("default");
                env.insert(
                    "DOCKER_HOST".to_string(),
                    format!("unix://{}/.colima/{profile}/docker.sock", home.display()),
                );
            }
        }
        Ok(Self {
            kind: cfg.runtime_type,
            program,
            env,
        })
    }

    pub fn kind(&self) -> ContainerRuntimeType {
        self.kind
    }

    pub fn is_podman(&self) -> bool {
        self.kind == ContainerRuntimeType::Podman
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub async fn inspect(&self, name: &str) -> anyhow::Result<Option<ContainerInspect>> {
        let output = self
            .command()
            .args(["inspect", name])
            .output()
            .await
            .with_context(|| format!("run {} inspect {name}", self.runtime_label()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if missing_container_error(&stderr) {
                return Ok(None);
            }
            anyhow::bail!(
                "{} inspect {name} failed: {}",
                self.runtime_label(),
                stderr.trim()
            );
        }
        parse::parse_container_inspect(&output.stdout, self.runtime_label())
    }

    pub async fn run_detached(&self, spec: &ContainerRunSpec) -> anyhow::Result<()> {
        self.run_status("run", self.run_args(spec)).await
    }

    pub async fn stop(&self, name: &str) -> anyhow::Result<()> {
        self.run_status("stop", [name]).await
    }

    pub async fn start(&self, name: &str) -> anyhow::Result<()> {
        self.run_status("start", [name]).await
    }

    pub async fn rm(&self, name: &str) -> anyhow::Result<()> {
        self.run_status("rm", [name]).await
    }

    pub async fn remove_host_dir_all(&self, path: &Path) -> anyhow::Result<()> {
        if self.is_podman() {
            let args = vec![
                OsString::from("rm"),
                OsString::from("-rf"),
                OsString::from("--"),
                path.as_os_str().to_os_string(),
            ];
            return self.run_status("unshare", args).await.with_context(|| {
                format!("remove workspace {} with podman unshare", path.display())
            });
        }

        tokio::fs::remove_dir_all(path)
            .await
            .with_context(|| format!("remove workspace {}", path.display()))
    }

    pub async fn exec(&self, spec: &ContainerExecSpec) -> anyhow::Result<i32> {
        self.exec_with_timeout(spec, None).await
    }

    pub async fn exec_with_timeout(
        &self,
        spec: &ContainerExecSpec,
        timeout_duration: Option<Duration>,
    ) -> anyhow::Result<i32> {
        let mut command = self.command();
        command
            .arg("exec")
            .args(self.exec_args(spec))
            .kill_on_drop(true);
        let status = match timeout_duration {
            Some(timeout_duration) => tokio::time::timeout(timeout_duration, command.status())
                .await
                .with_context(|| {
                    format!(
                        "{} exec {} timed out after {:?}",
                        self.runtime_label(),
                        spec.container_name,
                        timeout_duration
                    )
                })?,
            None => command.status().await,
        }
        .with_context(|| format!("run {} exec {}", self.runtime_label(), spec.container_name))?;
        Ok(exit_code(status))
    }

    pub async fn exec_capture(
        &self,
        spec: &ContainerExecSpec,
    ) -> anyhow::Result<ContainerExecOutput> {
        self.exec_capture_with_timeout(spec, None).await
    }

    pub async fn exec_capture_with_timeout(
        &self,
        spec: &ContainerExecSpec,
        timeout_duration: Option<Duration>,
    ) -> anyhow::Result<ContainerExecOutput> {
        let mut command = self.command();
        command
            .arg("exec")
            .args(self.exec_args(spec))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = match timeout_duration {
            Some(timeout_duration) => tokio::time::timeout(timeout_duration, command.output())
                .await
                .with_context(|| {
                    format!(
                        "{} exec {} timed out after {:?}",
                        self.runtime_label(),
                        spec.container_name,
                        timeout_duration
                    )
                })?,
            None => command.output().await,
        }
        .with_context(|| format!("run {} exec {}", self.runtime_label(), spec.container_name))?;
        Ok(ContainerExecOutput {
            exit_code: exit_code(output.status),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    pub async fn exec_capture_cancelable(
        &self,
        spec: &ContainerExecSpec,
        cancel: CancellationToken,
    ) -> anyhow::Result<ContainerExecCaptureResult> {
        let marker = next_cancel_marker("exec")?;
        let mut exec_spec = spec.clone();
        exec_spec.command = wrap_cancelable_command(&spec.command, &marker, "aw-gateway-exec");
        let cleanup_spec = cancel_cleanup_spec(spec, &marker, "aw-gateway-exec-cleanup");
        let remove_spec = cancel_marker_remove_spec(spec, &marker, "aw-gateway-exec-rm");

        let mut command = self.command();
        command
            .arg("exec")
            .args(self.exec_args(&exec_spec))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| {
            format!(
                "start {} exec {}",
                self.runtime_label(),
                spec.container_name
            )
        })?;
        let child_pid = child.id();
        let stdout = child
            .stdout
            .take()
            .context("container exec stdout pipe was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("container exec stderr pipe was not captured")?;
        let mut wait_task = tokio::spawn(async move { child.wait().await });
        let stdout_task = tokio::spawn(read_pipe_to_end(stdout));
        let stderr_task = tokio::spawn(read_pipe_to_end(stderr));

        tokio::select! {
            status = &mut wait_task => {
                let status = status.context("join container exec wait task")?
                    .with_context(|| format!("run {} exec {}", self.runtime_label(), spec.container_name))?;
                let stdout = stdout_task.await.context("join stdout drain task")??;
                let stderr = stderr_task.await.context("join stderr drain task")??;
                let output = ContainerExecOutput {
                    exit_code: exit_code(status),
                    stdout,
                    stderr,
                };
                run_container_marker_remove(
                    self.clone(),
                    remove_spec,
                    marker.path.clone(),
                    spec.container_name.clone(),
                    "normal-completion",
                )
                .await;
                Ok(ContainerExecCaptureResult::Completed(output))
            }
            _ = cancel.cancelled() => {
                cancel_container_exec_child(
                    self.clone(),
                    cleanup_spec,
                    marker.path,
                    spec.container_name.clone(),
                    child_pid,
                    wait_task,
                )
                .await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                Ok(ContainerExecCaptureResult::Canceled)
            }
        }
    }

    pub async fn exec_cancelable(
        &self,
        spec: &ContainerExecSpec,
        cancel: CancellationToken,
    ) -> anyhow::Result<ContainerExecStatusResult> {
        let marker = next_cancel_marker("exec")?;
        let mut exec_spec = spec.clone();
        exec_spec.command = wrap_cancelable_command(&spec.command, &marker, "aw-gateway-exec");
        let cleanup_spec = cancel_cleanup_spec(spec, &marker, "aw-gateway-exec-cleanup");
        let remove_spec = cancel_marker_remove_spec(spec, &marker, "aw-gateway-exec-rm");

        let mut command = self.command();
        command
            .arg("exec")
            .args(self.exec_args(&exec_spec))
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| {
            format!(
                "start {} exec {}",
                self.runtime_label(),
                spec.container_name
            )
        })?;
        let child_pid = child.id();
        let mut wait_task = tokio::spawn(async move { child.wait().await });

        tokio::select! {
            status = &mut wait_task => {
                let status = status.context("join container exec wait task")?
                    .with_context(|| format!("run {} exec {}", self.runtime_label(), spec.container_name))?;
                run_container_marker_remove(
                    self.clone(),
                    remove_spec,
                    marker.path.clone(),
                    spec.container_name.clone(),
                    "normal-completion",
                )
                .await;
                Ok(ContainerExecStatusResult::Completed(exit_code(status)))
            }
            _ = cancel.cancelled() => {
                cancel_container_exec_child(
                    self.clone(),
                    cleanup_spec,
                    marker.path,
                    spec.container_name.clone(),
                    child_pid,
                    wait_task,
                )
                .await;
                Ok(ContainerExecStatusResult::Canceled)
            }
        }
    }

    pub async fn exec_discard(&self, spec: &ContainerExecSpec) -> anyhow::Result<i32> {
        self.exec_discard_with_timeout(spec, None).await
    }

    pub async fn exec_discard_with_timeout(
        &self,
        spec: &ContainerExecSpec,
        timeout_duration: Option<Duration>,
    ) -> anyhow::Result<i32> {
        let mut command = self.command();
        command
            .arg("exec")
            .args(self.exec_args(spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let status = match timeout_duration {
            Some(timeout_duration) => tokio::time::timeout(timeout_duration, command.status())
                .await
                .with_context(|| {
                    format!(
                        "{} exec {} timed out after {:?}",
                        self.runtime_label(),
                        spec.container_name,
                        timeout_duration
                    )
                })?,
            None => command.status().await,
        }
        .with_context(|| format!("run {} exec {}", self.runtime_label(), spec.container_name))?;
        Ok(exit_code(status))
    }

    pub fn exec_pty(
        &self,
        spec: &ContainerExecSpec,
        size: ContainerPtySize,
    ) -> anyhow::Result<ContainerPtySession> {
        const PTY_OUTPUT_BUFFER: usize = 64;
        const PTY_INPUT_BUFFER: usize = 64;
        const PTY_RESIZE_BUFFER: usize = 16;

        let mut pty_spec = spec.clone();
        pty_spec.stdin_tty = true;
        pty_spec.stdout_tty = true;
        let marker = next_cancel_marker("pty")?;
        pty_spec.command = wrap_cancelable_command(&spec.command, &marker, "aw-gateway-pty");
        let cleanup_spec = cancel_cleanup_spec(spec, &marker, "aw-gateway-pty-cleanup");

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system.openpty(pty_size(size))?;
        let mut command = portable_pty::CommandBuilder::new(&self.program);
        command.arg("exec");
        command.args(self.exec_args(&pty_spec));
        for (key, value) in &self.env {
            command.env(key, value);
        }

        let mut writer = pair.master.take_writer()?;
        let mut child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let child_pid = child.process_id();
        let killer = Arc::new(Mutex::new(child.clone_killer()));

        let (output_tx, output) = mpsc::channel(PTY_OUTPUT_BUFFER);
        let (input, mut input_rx) = mpsc::channel::<Vec<u8>>(PTY_INPUT_BUFFER);
        let (resize, resize_rx) = mpsc::channel::<ContainerPtySize>(PTY_RESIZE_BUFFER);

        std::thread::spawn(move || run_pty_master_pump(pair.master, output_tx, resize_rx));

        std::thread::spawn(move || {
            while let Some(bytes) = input_rx.blocking_recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });

        let exit = tokio::task::spawn_blocking(move || {
            let status = child.wait()?;
            Ok(status.exit_code() as i32)
        });

        Ok(ContainerPtySession {
            output,
            input,
            resize,
            exit,
            killer,
            child_pid,
            cleanup_runtime: self.clone(),
            cleanup_spec,
            cleanup_marker_path: marker.path,
        })
    }

    pub async fn exec_quiet<I, S>(&self, name: &str, args: I) -> anyhow::Result<i32>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .command()
            .arg("exec")
            .arg(name)
            .args(args)
            .output()
            .await
            .with_context(|| format!("run {} exec {name}", self.runtime_label()))?;
        Ok(output.status.code().unwrap_or(1))
    }

    pub async fn published_port(
        &self,
        name: &str,
        container_port: u16,
    ) -> anyhow::Result<Option<u16>> {
        let output = self
            .command()
            .args(["port", name, &format!("{container_port}/tcp")])
            .output()
            .await
            .with_context(|| format!("run {} port {name}", self.runtime_label()))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(parse_published_port(&output.stdout))
    }

    pub async fn list_managed_containers(
        &self,
        user: &str,
        uid: u32,
    ) -> anyhow::Result<Vec<ManagedContainer>> {
        let output = self
            .command()
            .args(self.list_managed_args(user, uid))
            .output()
            .await
            .with_context(|| format!("run {} ps", self.runtime_label()))?;
        if !output.status.success() {
            anyhow::bail!(
                "{} ps failed: {}",
                self.runtime_label(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let containers = parse_managed_containers(&output.stdout).with_context(|| {
            format!("parse {} managed container list JSON", self.runtime_label())
        })?;
        Ok(containers
            .into_iter()
            .filter(|container| parse::is_aw_gateway_managed_container_for(container, user, uid))
            .collect())
    }

    pub fn run_args(&self, spec: &ContainerRunSpec) -> Vec<String> {
        match self.kind {
            ContainerRuntimeType::Podman => self.podman_run_args(spec),
            ContainerRuntimeType::Docker | ContainerRuntimeType::Colima => {
                self.docker_run_args(spec)
            }
        }
    }

    pub fn list_managed_args(&self, user: &str, uid: u32) -> Vec<String> {
        vec![
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            "label=io.aw-gateway.gateway=true".into(),
            "--filter".into(),
            format!("label=io.aw-gateway.user={user}"),
            "--filter".into(),
            format!("label=io.aw-gateway.uid={uid}"),
            "--format".into(),
            "json".into(),
        ]
    }

    pub fn exec_args(&self, spec: &ContainerExecSpec) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(stdio_arg) = exec_stdio_arg(spec.stdin_tty, spec.stdout_tty) {
            args.push(stdio_arg.to_string());
        }
        args.push("--user".to_string());
        args.push(spec.user.clone());
        if let Some(cwd) = &spec.cwd {
            args.push("--workdir".to_string());
            args.push(cwd.display().to_string());
        }
        for (key, value) in &spec.env {
            args.push("--env".to_string());
            args.push(format!("{key}={value}"));
        }
        args.push(spec.container_name.clone());
        args.extend(spec.command.clone());
        args
    }

    fn podman_run_args(&self, spec: &ContainerRunSpec) -> Vec<String> {
        let mut args = vec![
            "-d".to_string(),
            "--init".into(),
            "--userns=keep-id".into(),
            "--user".into(),
            "0:0".into(),
        ];
        if let Some(passwd_entry) = &spec.passwd_entry {
            args.push("--passwd-entry".into());
            args.push(passwd_entry.clone());
        }
        args.extend(spec.extra_run_args.clone());
        self.push_publish_ssh_args(&mut args, spec);
        self.push_common_run_args(&mut args, spec, true);
        args.push(podman_image_ref(&spec.image));
        args.extend(spec.command.clone());
        args
    }

    fn docker_run_args(&self, spec: &ContainerRunSpec) -> Vec<String> {
        let mut args = vec![
            "-d".to_string(),
            "--init".into(),
            "--user".into(),
            spec.container_user.clone(),
        ];
        args.extend(spec.extra_run_args.clone());
        self.push_publish_ssh_args(&mut args, spec);
        self.push_common_run_args(&mut args, spec, false);
        args.push(docker_image_ref(&spec.image));
        args.extend(spec.command.clone());
        args
    }

    fn push_publish_ssh_args(&self, args: &mut Vec<String>, spec: &ContainerRunSpec) {
        if spec.publish_ssh {
            args.push("-p".into());
            args.push("127.0.0.1::22".into());
        }
    }

    fn push_common_run_args(
        &self,
        args: &mut Vec<String>,
        spec: &ContainerRunSpec,
        podman_selinux_mount: bool,
    ) {
        args.push("--name".into());
        args.push(spec.name.clone());
        args.push("--hostname".into());
        args.push(spec.hostname.clone());
        args.push("-v".into());
        let selinux_suffix = if podman_selinux_mount { ":Z" } else { "" };
        args.push(format!(
            "{}:{}{}",
            spec.workspace.display(),
            spec.container_home.display(),
            selinux_suffix
        ));
        for mount in &spec.mounts {
            args.push("-v".into());
            let options = match (mount.readonly, podman_selinux_mount) {
                (true, true) => ":ro,Z",
                (true, false) => ":ro",
                (false, true) => ":Z",
                (false, false) => "",
            };
            args.push(format!(
                "{}:{}{}",
                mount.source.display(),
                mount.target.display(),
                options
            ));
        }
        args.push("-w".into());
        args.push(spec.container_home.display().to_string());
        args.push("-e".into());
        args.push(format!("HOME={}", spec.container_home.display()));
        args.push("-e".into());
        args.push(format!(
            "AW_CONTAINER_STATE_DIR={}",
            spec.state_dir_in_container.display()
        ));
        if Path::new("/etc/localtime").exists() {
            args.push("-v".into());
            args.push("/etc/localtime:/etc/localtime:ro".into());
        }
        if Path::new("/etc/bashrc").exists() {
            args.push("-v".into());
            args.push("/etc/bashrc:/etc/bashrc:ro".into());
        }
        for (key, value) in &spec.env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        for (key, value) in &spec.labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    async fn run_status<I, S>(&self, subcommand: &str, args: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .command()
            .arg(subcommand)
            .args(args)
            .output()
            .await
            .with_context(|| format!("run {} {subcommand}", self.runtime_label()))?;
        if !output.status.success() {
            anyhow::bail!(
                "{} {subcommand} failed: {}",
                self.runtime_label(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn runtime_label(&self) -> &'static str {
        match self.kind {
            ContainerRuntimeType::Podman => "podman",
            ContainerRuntimeType::Docker => "docker",
            ContainerRuntimeType::Colima => "colima/docker",
        }
    }
}

fn pty_size(size: ContainerPtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

#[derive(Debug, Clone)]
struct ContainerCancelMarker {
    path: String,
    token: String,
}

fn next_cancel_marker(kind: &str) -> anyhow::Result<ContainerCancelMarker> {
    let id = NEXT_CANCEL_MARKER_ID.fetch_add(1, Ordering::Relaxed);
    Ok(ContainerCancelMarker {
        path: format!("/tmp/aw-gateway-{kind}-{}-{id}.pid", std::process::id()),
        token: random_cancel_marker_token()?,
    })
}

fn random_cancel_marker_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn wrap_cancelable_command(
    command: &[String],
    marker: &ContainerCancelMarker,
    argv0: &'static str,
) -> Vec<String> {
    let mut wrapped = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"printf "%s %s\n" "$$" "$2" > "$1"; shift 2; exec "$@""#.to_string(),
        argv0.to_string(),
        marker.path.clone(),
        marker.token.clone(),
    ];
    wrapped.extend(command.iter().cloned());
    wrapped
}

fn cancel_cleanup_spec(
    spec: &ContainerExecSpec,
    marker: &ContainerCancelMarker,
    argv0: &'static str,
) -> ContainerExecSpec {
    let mut cleanup = spec.clone();
    cleanup.stdin_tty = false;
    cleanup.stdout_tty = false;
    cleanup.command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"read pid token < "$1" 2>/dev/null || exit 0
[ "$token" = "$2" ] || exit 0
case "$pid" in ""|*[!0-9]*|0|1) exit 0 ;; esac
descendants=$(ps -e -o pid= -o ppid= 2>/dev/null | awk -v root="$pid" '
  {
    children[$2] = children[$2] " " $1
  }
  END {
    frontier = root
    while (frontier != "") {
      next_frontier = ""
      count = split(frontier, parents, /[[:space:]]+/)
      for (i = 1; i <= count; i++) {
        if (parents[i] == "") {
          continue
        }
        count_children = split(children[parents[i]], ids, /[[:space:]]+/)
        for (j = 1; j <= count_children; j++) {
          if (ids[j] == "") {
            continue
          }
          print ids[j]
          next_frontier = next_frontier " " ids[j]
        }
      }
      frontier = next_frontier
    }
  }')
kill -HUP "-$pid" 2>/dev/null || kill -HUP "$pid" 2>/dev/null || true
kill -TERM "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
[ -z "$descendants" ] || kill -TERM $descendants 2>/dev/null || true
sleep 0.2
kill -KILL "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
[ -z "$descendants" ] || kill -KILL $descendants 2>/dev/null || true
rm -f "$1""#
            .to_string(),
        argv0.to_string(),
        marker.path.clone(),
        marker.token.clone(),
    ];
    cleanup
}

fn cancel_marker_remove_spec(
    spec: &ContainerExecSpec,
    marker: &ContainerCancelMarker,
    argv0: &'static str,
) -> ContainerExecSpec {
    let mut cleanup = spec.clone();
    cleanup.stdin_tty = false;
    cleanup.stdout_tty = false;
    cleanup.command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"read pid token < "$1" 2>/dev/null || exit 0
[ "$token" = "$2" ] || exit 0
rm -f "$1""#
            .to_string(),
        argv0.to_string(),
        marker.path.clone(),
        marker.token.clone(),
    ];
    cleanup
}

fn render_runtime_value(value: &str, user: &str, home: &Path) -> anyhow::Result<String> {
    let mut vars = Vars::new();
    vars.insert("user".into(), user.to_string());
    vars.insert("home".into(), home.display().to_string());
    template::render(value, &vars)
}

fn default_program(runtime_type: ContainerRuntimeType) -> &'static str {
    match runtime_type {
        ContainerRuntimeType::Podman => "podman",
        ContainerRuntimeType::Docker | ContainerRuntimeType::Colima => "docker",
    }
}

fn missing_container_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no such") || lower.contains("does not exist") || lower.contains("not found")
}

pub fn podman_image_ref(image: &str) -> String {
    let first_segment = image.split('/').next().unwrap_or(image);
    let last_segment = image.rsplit('/').next().unwrap_or(image);
    let has_registry =
        first_segment == "localhost" || first_segment.contains('.') || first_segment.contains(':');
    let has_tag = last_segment.contains(':');
    if has_registry || image.contains('@') {
        image.to_string()
    } else if has_tag {
        format!("localhost/{image}")
    } else {
        format!("localhost/{image}:latest")
    }
}

pub fn docker_image_ref(image: &str) -> String {
    let first_segment = image.split('/').next().unwrap_or(image);
    let last_segment = image.rsplit('/').next().unwrap_or(image);
    let has_registry =
        first_segment == "localhost" || first_segment.contains('.') || first_segment.contains(':');
    let has_tag = last_segment.contains(':');
    if has_registry || has_tag || image.contains('@') {
        image.to_string()
    } else {
        format!("{image}:latest")
    }
}

pub fn exec_stdio_arg(stdin_tty: bool, stdout_tty: bool) -> Option<&'static str> {
    if stdin_tty && stdout_tty {
        Some("-it")
    } else if stdin_tty {
        Some("-i")
    } else {
        None
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        status.signal().map(|signal| 128 + signal).unwrap_or(129)
    }
    #[cfg(not(unix))]
    {
        129
    }
}

pub fn parse_published_port(stdout: &[u8]) -> Option<u16> {
    let stdout = String::from_utf8_lossy(stdout);
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let (_, port) = line.rsplit_once(':')?;
    port.parse().ok()
}

#[cfg(test)]
fn is_aw_gateway_managed_container_for(container: &ManagedContainer, user: &str, uid: u32) -> bool {
    parse::is_aw_gateway_managed_container_for(container, user, uid)
}

pub fn validate_gateway_labels(
    inspect: &ContainerInspect,
    expected: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for (key, value) in expected {
        match inspect.config.labels.get(key) {
            Some(actual) if actual == value => {}
            Some(actual) => {
                return Err(GatewayLabelError::mismatch(key, value, actual).into());
            }
            None => return Err(GatewayLabelError::missing(key).into()),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayLabelError {
    Mismatch {
        key: String,
        expected: String,
        actual: String,
    },
    Missing {
        key: String,
    },
}

impl GatewayLabelError {
    fn mismatch(key: &str, expected: &str, actual: &str) -> Self {
        Self::Mismatch {
            key: key.into(),
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    fn missing(key: &str) -> Self {
        Self::Missing { key: key.into() }
    }
}

impl fmt::Display for GatewayLabelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch {
                key,
                expected,
                actual,
            } => write!(
                formatter,
                "container label mismatch for {key}: expected {expected:?}, got {actual:?}"
            ),
            Self::Missing { key } => {
                write!(formatter, "container missing required label {key:?}")
            }
        }
    }
}

impl std::error::Error for GatewayLabelError {}

pub fn socket_is_safe(path: &Path) -> anyhow::Result<()> {
    socket_is_safe_for(path, unsafe { libc::geteuid() }, unsafe { libc::getegid() })
}

pub fn socket_is_safe_for(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        if !meta.file_type().is_socket() {
            anyhow::bail!("{} is not a Unix socket", path.display());
        }
        if meta.uid() != uid {
            anyhow::bail!(
                "{} socket owner uid mismatch: expected {}, got {}",
                path.display(),
                uid,
                meta.uid()
            );
        }
        if meta.gid() != gid {
            anyhow::bail!(
                "{} socket group gid mismatch: expected {}, got {}",
                path.display(),
                gid,
                meta.gid()
            );
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} socket permissions too broad: expected no group/other bits, got {:o}",
                path.display(),
                mode
            );
        }
        if let Some(parent) = path.parent() {
            let parent_meta = std::fs::symlink_metadata(parent)
                .with_context(|| format!("stat socket parent {}", parent.display()))?;
            if parent_meta.uid() != uid {
                anyhow::bail!(
                    "{} parent owner uid mismatch: expected {}, got {}",
                    parent.display(),
                    uid,
                    parent_meta.uid()
                );
            }
            let parent_mode = parent_meta.permissions().mode() & 0o777;
            if parent_mode & 0o077 != 0 {
                anyhow::bail!(
                    "{} parent permissions too broad: expected no group/other bits, got {:o}",
                    parent.display(),
                    parent_mode
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    #[test]
    fn podman_image_ref_adds_localhost_latest() {
        assert_eq!(
            podman_image_ref("ubuntu/dev"),
            "localhost/ubuntu/dev:latest"
        );
        assert_eq!(
            podman_image_ref("localhost/ubuntu/dev:latest"),
            "localhost/ubuntu/dev:latest"
        );
        assert_eq!(
            podman_image_ref("ubuntu/dev:dev"),
            "localhost/ubuntu/dev:dev"
        );
        assert_eq!(
            podman_image_ref("registry.example.com/ubuntu/dev:dev"),
            "registry.example.com/ubuntu/dev:dev"
        );
    }

    #[test]
    fn docker_image_ref_does_not_add_localhost() {
        assert_eq!(docker_image_ref("ubuntu/dev"), "ubuntu/dev:latest");
        assert_eq!(docker_image_ref("ubuntu/dev:dev"), "ubuntu/dev:dev");
        assert_eq!(
            docker_image_ref("registry.example.com/ubuntu/dev:dev"),
            "registry.example.com/ubuntu/dev:dev"
        );
    }

    #[test]
    fn exec_stdio_matches_pty_presence() {
        assert_eq!(exec_stdio_arg(true, true), Some("-it"));
        assert_eq!(exec_stdio_arg(true, false), Some("-i"));
        assert_eq!(exec_stdio_arg(false, true), None);
        assert_eq!(exec_stdio_arg(false, false), None);
    }

    #[test]
    fn pty_wrapper_and_cleanup_commands_are_stable() {
        let marker = ContainerCancelMarker {
            path: "/tmp/aw-gateway-pty-test.pid".into(),
            token: "test-token".into(),
        };
        let command = vec!["/bin/bash".into(), "-lc".into(), "echo ok".into()];

        let wrapped = wrap_cancelable_command(&command, &marker, "aw-gateway-pty");
        assert_eq!(wrapped[0], "/bin/sh");
        assert_eq!(wrapped[1], "-c");
        assert_eq!(wrapped[3], "aw-gateway-pty");
        assert_eq!(wrapped[4], marker.path.as_str());
        assert_eq!(wrapped[5], marker.token.as_str());
        assert_eq!(&wrapped[6..], command.as_slice());

        let spec = ContainerExecSpec {
            stdin_tty: true,
            stdout_tty: true,
            user: "2450:100".into(),
            cwd: Some(PathBuf::from("/home/alice/project")),
            env: BTreeMap::new(),
            container_name: "ubuntu-dev".into(),
            command,
        };
        let cleanup = cancel_cleanup_spec(&spec, &marker, "aw-gateway-pty-cleanup");
        assert!(!cleanup.stdin_tty);
        assert!(!cleanup.stdout_tty);
        assert_eq!(cleanup.command[0], "/bin/sh");
        assert_eq!(cleanup.command[1], "-c");

        let script = &cleanup.command[2];
        let kill_pos = script.rfind("kill -KILL").expect("kill command");
        let rm_pos = script.rfind("rm -f \"$1\"").expect("marker removal");
        assert!(rm_pos > kill_pos);
        assert!(script.contains("ps -e -o pid= -o ppid="));

        let status = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", script])
            .status()
            .expect("run shell syntax check");
        assert!(status.success());
    }

    #[test]
    fn success_marker_remove_command_is_rm_only() {
        let marker = ContainerCancelMarker {
            path: "/tmp/aw-gateway-exec-test.pid".into(),
            token: "test-token".into(),
        };
        let spec = ContainerExecSpec {
            stdin_tty: false,
            stdout_tty: false,
            user: "2450:100".into(),
            cwd: None,
            env: BTreeMap::new(),
            container_name: "ubuntu-dev".into(),
            command: vec!["true".into()],
        };

        let cleanup = cancel_marker_remove_spec(&spec, &marker, "aw-gateway-exec-rm");
        let script = &cleanup.command[2];
        assert!(script.contains("rm -f \"$1\""));
        assert!(!script.contains("kill "));
        assert!(!script.contains("ps -e"));

        let status = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", script])
            .status()
            .expect("run shell syntax check");
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_uses_shell_signal_convention() {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(15)), 143);
    }

    #[test]
    fn colima_sets_docker_host_from_profile() {
        let cfg = RuntimeConfig {
            runtime_type: ContainerRuntimeType::Colima,
            program: None,
            docker_host: None,
            profile: Some("default".into()),
        };
        let runtime =
            ContainerRuntime::from_config(&cfg, "alice", Path::new("/Users/alice")).unwrap();
        assert_eq!(
            runtime.env().get("DOCKER_HOST").map(String::as_str),
            Some("unix:///Users/alice/.colima/default/docker.sock")
        );
    }

    #[test]
    fn list_managed_args_filter_by_gateway_user_and_uid() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::new(),
        };
        assert_eq!(
            runtime.list_managed_args("alice", 2450),
            vec![
                "ps",
                "-a",
                "--filter",
                "label=io.aw-gateway.gateway=true",
                "--filter",
                "label=io.aw-gateway.user=alice",
                "--filter",
                "label=io.aw-gateway.uid=2450",
                "--format",
                "json",
            ]
        );
    }

    #[test]
    fn docker_and_colima_use_same_managed_list_args() {
        let docker = ContainerRuntime {
            kind: ContainerRuntimeType::Docker,
            program: "docker".into(),
            env: BTreeMap::new(),
        };
        let colima = ContainerRuntime {
            kind: ContainerRuntimeType::Colima,
            program: "docker".into(),
            env: BTreeMap::from([(
                "DOCKER_HOST".into(),
                "unix:///Users/alice/.colima/default/docker.sock".into(),
            )]),
        };

        assert_eq!(
            docker.list_managed_args("alice", 2450),
            colima.list_managed_args("alice", 2450)
        );
        assert_eq!(
            colima.env().get("DOCKER_HOST").map(String::as_str),
            Some("unix:///Users/alice/.colima/default/docker.sock")
        );
    }

    #[test]
    fn parse_managed_containers_accepts_podman_array_json() {
        let raw = br#"
[
  {
    "Names": ["aw-ubuntu"],
    "Image": "ubuntu/dev",
    "State": "running",
    "Labels": {
      "io.aw-gateway.gateway": "true",
      "io.aw-gateway.user": "alice",
      "io.aw-gateway.uid": "2450",
      "io.aw-gateway.target": "ubuntu"
    }
  },
  {
    "Names": "unrelated",
    "Image": "busybox",
    "State": "running",
    "Labels": {"com.example": "true"}
  }
]
"#;

        let containers = parse_managed_containers(raw).unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].name, "aw-ubuntu");
        assert_eq!(containers[0].image, "ubuntu/dev");
        assert!(containers[0].running);
        assert_eq!(
            containers[0]
                .labels
                .get("io.aw-gateway.target")
                .map(String::as_str),
            Some("ubuntu")
        );
    }

    #[test]
    fn managed_container_for_requires_matching_user_and_uid() {
        let raw = br#"
[
  {
    "Names": ["aw-ubuntu"],
    "Image": "ubuntu/dev",
    "State": "running",
    "Labels": {
      "io.aw-gateway.gateway": "true",
      "io.aw-gateway.user": "alice",
      "io.aw-gateway.uid": "2450"
    }
  }
]
"#;
        let containers = parse_managed_containers(raw).unwrap();

        assert!(is_aw_gateway_managed_container_for(
            &containers[0],
            "alice",
            2450
        ));
        assert!(!is_aw_gateway_managed_container_for(
            &containers[0],
            "bob",
            2450
        ));
        assert!(!is_aw_gateway_managed_container_for(
            &containers[0],
            "alice",
            2451
        ));
    }

    #[test]
    fn validate_gateway_labels_returns_typed_label_errors() {
        let mut expected = BTreeMap::new();
        expected.insert("io.aw-gateway.target".into(), "default".into());
        let inspect = ContainerInspect {
            id: "abc".into(),
            name: "aw-default".into(),
            state: ContainerState {
                running: true,
                pid: 123,
            },
            config: ContainerConfig {
                labels: BTreeMap::new(),
            },
        };

        let err = validate_gateway_labels(&inspect, &expected).unwrap_err();
        assert!(err.is::<GatewayLabelError>());
        assert_eq!(
            err.to_string(),
            "container missing required label \"io.aw-gateway.target\""
        );

        let inspect = ContainerInspect {
            config: ContainerConfig {
                labels: BTreeMap::from([("io.aw-gateway.target".into(), "other".into())]),
            },
            ..inspect
        };
        let err = validate_gateway_labels(&inspect, &expected).unwrap_err();
        assert!(err.is::<GatewayLabelError>());
        assert_eq!(
            err.to_string(),
            "container label mismatch for io.aw-gateway.target: expected \"default\", got \"other\""
        );
    }

    #[test]
    fn parse_managed_containers_accepts_docker_ndjson_labels() {
        let raw = br#"
{"Names":"scratch-1a2b3c4d5e6f","Image":"scratch/dev","State":"exited","Status":"Exited (0) 2 minutes ago","Labels":"io.aw-gateway.gateway=true,io.aw-gateway.user=alice,io.aw-gateway.uid=2450,io.aw-gateway.target=scratch,io.aw-gateway.session_id=1a2b3c4d5e6f"}
{"Names":"other","Image":"busybox","State":"running","Status":"Up 2 minutes","Labels":"io.aw-gateway.gateway=false,io.aw-gateway.user=alice,io.aw-gateway.uid=2450"}
"#;

        let containers = parse_managed_containers(raw).unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].name, "scratch-1a2b3c4d5e6f");
        assert!(!containers[0].running);
        assert_eq!(
            containers[0]
                .labels
                .get("io.aw-gateway.session_id")
                .map(String::as_str),
            Some("1a2b3c4d5e6f")
        );
    }

    #[test]
    fn docker_run_args_do_not_use_podman_only_options() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Docker,
            program: "docker".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerRunSpec {
            name: "aw-local".into(),
            hostname: "aw-local".into(),
            image: "aw-gateway/ubuntu-dev-local".into(),
            workspace: PathBuf::from("/Users/alice/workspace"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            env: BTreeMap::from([("HOME".into(), "/root".into())]),
            labels: BTreeMap::from([("io.aw-gateway.gateway".into(), "true".into())]),
            publish_ssh: false,
            extra_run_args: Vec::new(),
            command: vec!["aw-container-agent".into(), "run".into()],
        };
        let args = runtime.run_args(&spec);
        assert!(!args.contains(&"--userns=keep-id".to_string()));
        assert!(!args.contains(&"--passwd-entry".to_string()));
        assert!(args.contains(&"--user".to_string()));
        assert!(args.contains(&"root".to_string()));
        assert!(args.contains(&"/Users/alice/workspace:/root".to_string()));
        assert!(args.contains(&"aw-gateway/ubuntu-dev-local:latest".to_string()));
    }

    #[test]
    fn exec_args_put_runtime_options_before_container_name() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::new(),
        };
        let args = runtime.exec_args(&ContainerExecSpec {
            stdin_tty: true,
            stdout_tty: true,
            user: "2450:100".into(),
            cwd: Some(PathBuf::from("/home/alice/project")),
            env: BTreeMap::from([("SHELL".into(), "/usr/bin/bash".into())]),
            container_name: "ubuntu-dev".into(),
            command: vec!["/usr/bin/bash".into(), "-lc".into(), "id -u".into()],
        });

        assert_eq!(
            args,
            vec![
                "-it",
                "--user",
                "2450:100",
                "--workdir",
                "/home/alice/project",
                "--env",
                "SHELL=/usr/bin/bash",
                "ubuntu-dev",
                "/usr/bin/bash",
                "-lc",
                "id -u",
            ]
        );
    }

    #[tokio::test]
    async fn exec_with_timeout_reports_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        std::fs::write(
            &runtime_program,
            "#!/bin/sh\nif [ \"$1\" = exec ]; then sleep 5; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&runtime_program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };
        let spec = ContainerExecSpec {
            stdin_tty: false,
            stdout_tty: false,
            user: "2450:100".into(),
            cwd: None,
            env: BTreeMap::new(),
            container_name: "ubuntu-dev".into(),
            command: vec!["sleep".into(), "5".into()],
        };

        let started = std::time::Instant::now();
        let err = runtime
            .exec_with_timeout(&spec, Some(Duration::from_millis(100)))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn exec_capture_closes_stdin_for_noninteractive_wait() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        std::fs::write(
            &runtime_program,
            "#!/bin/sh\nif [ \"$1\" = exec ]; then read line || true; echo done; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&runtime_program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };
        let spec = ContainerExecSpec {
            stdin_tty: false,
            stdout_tty: false,
            user: "2450:100".into(),
            cwd: None,
            env: BTreeMap::new(),
            container_name: "ubuntu-dev".into(),
            command: vec!["read".into()],
        };

        let output = runtime
            .exec_capture_with_timeout(&spec, Some(Duration::from_secs(1)))
            .await
            .unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"done\n");
    }

    #[tokio::test]
    async fn socket_safety_rejects_broad_permissions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("test.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();

        let err = socket_is_safe(&socket).unwrap_err().to_string();
        assert!(err.contains("permissions too broad"));
    }

    #[tokio::test]
    async fn socket_safety_accepts_private_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("test.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

        socket_is_safe(&socket).unwrap();
    }

    #[test]
    fn run_args_publish_ssh_binds_loopback_random_host_port() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Docker,
            program: "docker".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerRunSpec {
            name: "aw-local".into(),
            hostname: "aw-local".into(),
            image: "aw-gateway/ubuntu-dev-local".into(),
            workspace: PathBuf::from("/workspace"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: true,
            extra_run_args: Vec::new(),
            command: vec!["aw-container-agent".into(), "run".into()],
        };
        let args = runtime.run_args(&spec);
        assert!(args.windows(2).any(|pair| pair == ["-p", "127.0.0.1::22"]));
    }

    #[test]
    fn docker_run_args_include_configured_extra_args() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Docker,
            program: "docker".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerRunSpec {
            name: "aw-local".into(),
            hostname: "aw-local".into(),
            image: "aw-gateway/ubuntu-base".into(),
            workspace: PathBuf::from("/workspace"),
            container_home: PathBuf::from("/home/user"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/home/user/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: false,
            extra_run_args: vec![
                "--cap-add".into(),
                "SYS_ADMIN".into(),
                "--security-opt".into(),
                "seccomp=unconfined".into(),
            ],
            command: vec!["aw-container-agent".into(), "run".into()],
        };
        let args = runtime.run_args(&spec);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--cap-add", "SYS_ADMIN"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--security-opt", "seccomp=unconfined"])
        );
    }

    #[test]
    fn parses_docker_published_port_output() {
        assert_eq!(parse_published_port(b"127.0.0.1:49153\n"), Some(49153));
        assert_eq!(parse_published_port(b"0.0.0.0:2222\n"), Some(2222));
        assert_eq!(
            parse_published_port(b"127.0.0.1:49153\n[::1]:49154\n"),
            Some(49153)
        );
        assert_eq!(parse_published_port(b""), None);
        assert_eq!(parse_published_port(b"not-a-port\n"), None);
    }
}
