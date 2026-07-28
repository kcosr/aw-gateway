use crate::config::{ContainerRuntimeType, RuntimeConfig, SelinuxRelabel};
use crate::context::{RuntimeContext, context_from_labels, context_label_key};
use crate::template::{self, Vars};
use anyhow::Context;
use serde::Deserialize;
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
#[cfg(not(unix))]
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

#[derive(Clone)]
pub struct ContainerRunSpec {
    pub name: String,
    pub hostname: String,
    pub image: String,
    pub workspace: PathBuf,
    pub workspace_container_path: PathBuf,
    pub container_home: PathBuf,
    pub container_user: String,
    pub passwd_entry: Option<String>,
    pub state_dir_in_container: PathBuf,
    pub mounts: Vec<ContainerMountSpec>,
    pub host_socket_exposures: Vec<HostSocketExposureSpec>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub publish_ssh: bool,
    pub published_ssh_host_port: Option<u16>,
    pub extra_run_args: Vec<String>,
    pub command: Vec<String>,
}

impl fmt::Debug for ContainerRunSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_keys = self.env.keys().collect::<Vec<_>>();
        formatter
            .debug_struct("ContainerRunSpec")
            .field("name", &self.name)
            .field("hostname", &self.hostname)
            .field("image", &self.image)
            .field("workspace", &self.workspace)
            .field("workspace_container_path", &self.workspace_container_path)
            .field("container_home", &self.container_home)
            .field("container_user", &self.container_user)
            .field("passwd_entry", &self.passwd_entry)
            .field("state_dir_in_container", &self.state_dir_in_container)
            .field("mounts", &self.mounts)
            .field("host_socket_exposures", &self.host_socket_exposures)
            .field("env_keys", &env_keys)
            .field("labels", &self.labels)
            .field("publish_ssh", &self.publish_ssh)
            .field("published_ssh_host_port", &self.published_ssh_host_port)
            .field("extra_run_args", &self.extra_run_args)
            .field("command", &self.command)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPort {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct ContainerMountSpec {
    pub source: PathBuf,
    pub target: PathBuf,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSocketExposureSpec {
    pub name: String,
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub readiness_user: String,
    pub selinux_relabel: SelinuxRelabel,
    pub realization: HostSocketExposureRealization,
    pub source_identity: UnixSourceIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostSocketExposureRealization {
    PathReconnect,
    PinnedInode,
}

impl HostSocketExposureRealization {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathReconnect => "path_reconnect",
            Self::PinnedInode => "pinned_inode",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixSourceIdentity {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
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
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
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
const CANCEL_MARKER_LIST_ARGV0: &str = "aw-gateway-marker-list";
const CANCEL_MARKER_SWEEP_ARGV0: &str = "aw-gateway-marker-sweep";
const CANCEL_MARKER_PATH_PREFIX: &str = "/tmp/aw-gateway-";
const CANCEL_MARKER_PATH_SUFFIX: &str = ".pid";
pub const MAX_CAPTURED_STREAM_BYTES: usize = 4 * 1024 * 1024;
const EXEC_CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

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
            run_container_cancel_exec(
                initial_cleanup_runtime,
                initial_cleanup_spec,
                initial_cleanup_marker_path,
                initial_cleanup_container_name,
                "initial",
                "cleanup",
                ContainerCancelExecLog::Info,
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
            run_container_cancel_exec(
                cleanup_runtime,
                cleanup_spec,
                cleanup_marker_path,
                cleanup_container_name,
                "post-host-terminate",
                "cleanup",
                ContainerCancelExecLog::Info,
            )
            .await;
        }
        host_result
    }
}

#[derive(Debug, Clone, Copy)]
enum ContainerCancelExecLog {
    Info,
    Debug,
}

async fn run_container_cancel_exec(
    runtime: ContainerRuntime,
    spec: ContainerExecSpec,
    marker_path: String,
    container_name: String,
    phase: &'static str,
    action: &'static str,
    success_log: ContainerCancelExecLog,
) -> bool {
    match runtime
        .exec_discard_with_timeout(&spec, Some(CANCEL_CLEANUP_TIMEOUT))
        .await
    {
        Ok(exit_code) => {
            match success_log {
                ContainerCancelExecLog::Info => {
                    tracing::info!(
                        container = %container_name,
                        marker = %marker_path,
                        phase,
                        action,
                        exit_code,
                        "container cancel exec finished"
                    );
                }
                ContainerCancelExecLog::Debug => {
                    tracing::debug!(
                        container = %container_name,
                        marker = %marker_path,
                        phase,
                        action,
                        exit_code,
                        "container cancel exec finished"
                    );
                }
            }
            true
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                container = %container_name,
                marker = %marker_path,
                phase,
                action,
                "container cancel exec failed"
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

async fn read_pipe_to_end<R>(reader: R) -> std::io::Result<BoundedOutput>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut limited = reader.take((MAX_CAPTURED_STREAM_BYTES + 1) as u64);
    limited.read_to_end(&mut bytes).await?;
    let truncated = bytes.len() > MAX_CAPTURED_STREAM_BYTES;
    if truncated {
        bytes.truncate(MAX_CAPTURED_STREAM_BYTES);
    }
    Ok(BoundedOutput { bytes, truncated })
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
        run_container_cancel_exec(
            initial_cleanup_runtime,
            initial_cleanup_spec,
            initial_marker_path,
            initial_container_name,
            "initial",
            "cleanup",
            ContainerCancelExecLog::Info,
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
        run_container_cancel_exec(
            runtime,
            cleanup_spec,
            marker_path.clone(),
            container_name.clone(),
            "post-host-terminate",
            "cleanup",
            ContainerCancelExecLog::Info,
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

#[cfg(unix)]
async fn terminate_host_exec_child(child_pid: Option<u32>) {
    if let Some(child_pid) = child_pid
        && let Err(err) =
            tokio::task::spawn_blocking(move || terminate_process_group(child_pid)).await
    {
        tracing::warn!(
            error = %err,
            child_pid,
            "host runtime exec process-group terminate task failed"
        );
    }
}

#[cfg(not(unix))]
async fn terminate_host_exec_child(_child_pid: Option<u32>) {}

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
static CANCEL_MARKER_SWEEP_KEYS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
static APPLE_PREFLIGHT_CACHE: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
const APPLE_CONTAINER_MIN_MACOS_MAJOR: u64 = 26;
const APPLE_CONTAINER_MIN_VERSION: (u64, u64, u64) = (1, 0, 0);
const APPLE_CONTAINER_SOCKET_EXPOSURE_MIN_VERSION: (u64, u64, u64) = (1, 1, 0);

#[cfg(test)]
thread_local! {
    static APPLE_PREFLIGHT_TEST_BYPASS: Cell<bool> = const { Cell::new(false) };
    static APPLE_PREFLIGHT_TEST_HOST: RefCell<Option<AppleHostInfo>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct ApplePreflightBypassGuard {
    previous: bool,
}

#[cfg(test)]
impl Drop for ApplePreflightBypassGuard {
    fn drop(&mut self) {
        APPLE_PREFLIGHT_TEST_BYPASS.with(|bypass| bypass.set(self.previous));
    }
}

#[cfg(test)]
pub(crate) fn disable_apple_preflight_for_tests() -> ApplePreflightBypassGuard {
    let previous = APPLE_PREFLIGHT_TEST_BYPASS.with(|bypass| {
        let previous = bypass.get();
        bypass.set(true);
        previous
    });
    ApplePreflightBypassGuard { previous }
}

#[cfg(test)]
struct ApplePreflightHostGuard {
    previous: Option<AppleHostInfo>,
}

#[cfg(test)]
impl Drop for ApplePreflightHostGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        APPLE_PREFLIGHT_TEST_HOST.with(|host| {
            *host.borrow_mut() = previous;
        });
    }
}

#[cfg(test)]
fn override_apple_preflight_host_for_tests(host: AppleHostInfo) -> ApplePreflightHostGuard {
    let previous = APPLE_PREFLIGHT_TEST_HOST.with(|current| current.replace(Some(host)));
    ApplePreflightHostGuard { previous }
}

#[cfg(test)]
fn apple_preflight_test_bypassed() -> bool {
    APPLE_PREFLIGHT_TEST_BYPASS.with(Cell::get)
}

#[cfg(test)]
fn apple_preflight_test_host() -> Option<AppleHostInfo> {
    APPLE_PREFLIGHT_TEST_HOST.with(|host| host.borrow().clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppleHostInfo {
    os: String,
    arch: String,
    macos_version: Option<String>,
}

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
    if let Some(pid) = signalable_pid(child_pid) {
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

#[cfg(unix)]
fn signal_process(child_pid: u32, signal: libc::c_int) {
    if let Some(pid) = signalable_pid(child_pid) {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(unix)]
fn signalable_pid(child_pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(child_pid).ok().filter(|pid| *pid > 1)
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
            if !drain_pty_resize_requests(master.as_ref(), &mut resize_rx) {
                break;
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

#[derive(Debug, Clone, Default)]
pub struct AppleContainerNameCandidates {
    exact: BTreeSet<String>,
    patterns: Vec<AppleContainerNamePattern>,
}

#[derive(Debug, Clone)]
struct AppleContainerNamePattern {
    prefix: String,
    suffix: String,
}

impl AppleContainerNameCandidates {
    pub fn insert_exact(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !name.is_empty() {
            self.exact.insert(name);
        }
    }

    pub fn insert_pattern(&mut self, prefix: impl Into<String>, suffix: impl Into<String>) {
        self.patterns.push(AppleContainerNamePattern {
            prefix: prefix.into(),
            suffix: suffix.into(),
        });
    }

    pub fn matches(&self, name: &str) -> bool {
        self.exact.contains(name) || self.patterns.iter().any(|pattern| pattern.matches(name))
    }
}

impl AppleContainerNamePattern {
    fn matches(&self, name: &str) -> bool {
        name.starts_with(&self.prefix)
            && name.ends_with(&self.suffix)
            && name.len() > self.prefix.len() + self.suffix.len()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerState {
    #[serde(rename = "Running")]
    pub running: bool,
    #[serde(rename = "Pid", default)]
    pub pid: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Labels", default)]
    pub labels: BTreeMap<String, String>,
}

fn current_apple_host_info() -> anyhow::Result<AppleHostInfo> {
    let os = std::env::consts::OS.to_string();
    let macos_version = if os == "macos" {
        Some(read_macos_product_version()?)
    } else {
        None
    };
    Ok(AppleHostInfo {
        os,
        arch: std::env::consts::ARCH.to_string(),
        macos_version,
    })
}

fn read_macos_product_version() -> anyhow::Result<String> {
    let output = StdCommand::new("sw_vers")
        .arg("-productVersion")
        .output()
        .context("run sw_vers -productVersion to determine macOS version")?;
    if !output.status.success() {
        anyhow::bail!(
            "sw_vers -productVersion failed while checking Apple container prerequisites: {}",
            command_output_text(&output)
        );
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        anyhow::bail!(
            "sw_vers -productVersion returned an empty macOS version while checking Apple container prerequisites"
        );
    }
    Ok(version)
}

fn run_apple_preflight_checks(
    program: &str,
    env: &BTreeMap<String, String>,
    host: &AppleHostInfo,
) -> anyhow::Result<()> {
    validate_apple_host(host)?;

    let version_output = run_apple_system_json_command(program, env, "version")?;
    validate_apple_cli_version_json(&version_output.stdout)?;

    let status_output = run_apple_system_json_command(program, env, "status")?;
    serde_json::from_slice::<serde_json::Value>(&status_output.stdout)
        .context("parse `container system status --format json` output as JSON")?;

    Ok(())
}

fn validate_apple_host(host: &AppleHostInfo) -> anyhow::Result<()> {
    if host.os != "macos" {
        anyhow::bail!(
            "apple container runtime requires Apple silicon macOS {APPLE_CONTAINER_MIN_MACOS_MAJOR} or newer; current host OS is {}",
            host.os
        );
    }
    if !matches!(host.arch.as_str(), "aarch64" | "arm64") {
        anyhow::bail!(
            "apple container runtime requires Apple silicon (aarch64/arm64); current host architecture is {}",
            host.arch
        );
    }
    let version = host.macos_version.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "apple container runtime requires macOS {APPLE_CONTAINER_MIN_MACOS_MAJOR} or newer, but macOS product version was unavailable"
        )
    })?;
    let major = parse_major_version(version).ok_or_else(|| {
        anyhow::anyhow!(
            "apple container runtime requires macOS {APPLE_CONTAINER_MIN_MACOS_MAJOR} or newer, but could not parse macOS product version {version:?}"
        )
    })?;
    if major < APPLE_CONTAINER_MIN_MACOS_MAJOR {
        anyhow::bail!(
            "apple container runtime requires macOS {APPLE_CONTAINER_MIN_MACOS_MAJOR} or newer; current macOS version is {version}"
        );
    }
    Ok(())
}

fn run_apple_system_json_command(
    program: &str,
    env: &BTreeMap<String, String>,
    subcommand: &str,
) -> anyhow::Result<std::process::Output> {
    let mut command = StdCommand::new(program);
    for (key, value) in env {
        command.env(key, value);
    }
    let command_label = format!("{program} system {subcommand} --format json");
    let output = command
        .args(["system", subcommand, "--format", "json"])
        .output()
        .with_context(|| {
            format!(
                "run Apple container preflight command `{command_label}`; install the Apple container CLI or set [runtime].program to its path"
            )
        })?;
    if !output.status.success() {
        let output_text = command_output_text(&output);
        if subcommand == "status" {
            anyhow::bail!(
                "apple container preflight command `{command_label}` failed: {output_text}; run `{program} system start` and retry"
            );
        }
        anyhow::bail!(
            "apple container preflight command `{command_label}` failed: {output_text}; install or upgrade Apple container CLI 1.0.0 or newer"
        );
    }
    Ok(output)
}

fn validate_apple_cli_version_json(stdout: &[u8]) -> anyhow::Result<()> {
    let value = serde_json::from_slice::<serde_json::Value>(stdout)
        .context("parse `container system version --format json` output as JSON")?;
    let Some((raw, version)) = apple_cli_version_candidate(&value) else {
        return Ok(());
    };
    if version < APPLE_CONTAINER_MIN_VERSION {
        anyhow::bail!(
            "apple container runtime requires Apple container CLI 1.0.0 or newer; `container system version --format json` reported {raw}"
        );
    }
    Ok(())
}

fn validate_apple_cli_minimum_version_json(
    stdout: &[u8],
    minimum: (u64, u64, u64),
    requirement: &str,
) -> anyhow::Result<()> {
    let value = serde_json::from_slice::<serde_json::Value>(stdout)
        .context("parse `container system version --format json` output as JSON")?;
    let (raw, version) = apple_cli_version_candidate(&value).ok_or_else(|| {
        anyhow::anyhow!(
            "{requirement}; `container system version --format json` did not contain a parseable container CLI version"
        )
    })?;
    if version < minimum {
        anyhow::bail!("{requirement}; `container system version --format json` reported {raw}");
    }
    Ok(())
}

fn require_linux_host(runtime: &str) -> anyhow::Result<()> {
    if std::env::consts::OS != "linux" {
        anyhow::bail!(
            "host_socket_exposures require {runtime} to use a native local Linux host; current host OS is {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

fn require_unix_socket(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat {label} {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if metadata.file_type().is_socket() {
            return Ok(());
        }
    }
    anyhow::bail!("{label} {} is not a Unix socket", path.display())
}

fn apple_cli_version_candidate(value: &serde_json::Value) -> Option<(&str, (u64, u64, u64))> {
    if let Some(version) = documented_apple_cli_version(value) {
        return version.and_then(|raw| parse_semver_triplet(raw).map(|version| (raw, version)));
    }
    first_parseable_version_string(value)
}

fn documented_apple_cli_version(value: &serde_json::Value) -> Option<Option<&str>> {
    let rows = match value {
        serde_json::Value::Array(rows) => Some(rows.as_slice()),
        serde_json::Value::Object(map) => map
            .get("components")
            .or_else(|| map.get("Components"))
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice),
        _ => None,
    }?;

    let mut saw_component_row = false;
    for row in rows {
        let Some(map) = row.as_object() else {
            continue;
        };
        let Some(app_name) = map
            .get("appName")
            .or_else(|| map.get("AppName"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let Some(version) = map
            .get("version")
            .or_else(|| map.get("Version"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        saw_component_row = true;
        if app_name == "container" {
            return Some(Some(version));
        }
    }

    saw_component_row.then_some(None)
}

fn first_parseable_version_string(value: &serde_json::Value) -> Option<(&str, (u64, u64, u64))> {
    let mut candidates = Vec::new();
    collect_version_strings(value, None, &mut candidates);
    candidates.sort_by_key(|(rank, _)| *rank);
    candidates
        .into_iter()
        .find_map(|(_, raw)| parse_semver_triplet(raw).map(|version| (raw, version)))
}

fn collect_version_strings<'a>(
    value: &'a serde_json::Value,
    inherited_rank: Option<u8>,
    candidates: &mut Vec<(u8, &'a str)>,
) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_version_strings(item, inherited_rank, candidates);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let rank = version_key_rank(key).or(inherited_rank);
                match value {
                    serde_json::Value::String(raw) => {
                        if let Some(rank) = rank {
                            candidates.push((rank, raw));
                        }
                    }
                    _ => collect_version_strings(value, rank, candidates),
                }
            }
        }
        serde_json::Value::String(raw) => {
            if let Some(rank) = inherited_rank {
                candidates.push((rank, raw));
            }
        }
        _ => {}
    }
}

fn version_key_rank(key: &str) -> Option<u8> {
    let lower = key.to_ascii_lowercase();
    if lower == "version" {
        Some(0)
    } else if (lower.contains("cli") || lower.contains("client") || lower.contains("container"))
        && lower.contains("version")
    {
        Some(1)
    } else {
        None
    }
}

fn parse_major_version(raw: &str) -> Option<u64> {
    raw.trim().split('.').next()?.parse().ok()
}

fn parse_semver_triplet(raw: &str) -> Option<(u64, u64, u64)> {
    let core = raw
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn command_output_text(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        format!("exited with status {}", output.status)
    } else {
        stdout
    }
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
            ContainerRuntimeType::AppleContainer => {}
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

    pub async fn validate_host_socket_exposure_support(
        &self,
        configured: bool,
    ) -> anyhow::Result<()> {
        if !configured {
            return Ok(());
        }
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            runtime.validate_host_socket_exposure_support_blocking()
        })
        .await
        .context("join host socket exposure runtime preflight")?
    }

    fn validate_host_socket_exposure_support_blocking(&self) -> anyhow::Result<()> {
        match self.kind {
            ContainerRuntimeType::AppleContainer => {
                self.preflight()?;
                #[cfg(test)]
                if apple_preflight_test_bypassed() {
                    return Ok(());
                }
                let output = run_apple_system_json_command(&self.program, &self.env, "version")?;
                validate_apple_cli_minimum_version_json(
                    &output.stdout,
                    APPLE_CONTAINER_SOCKET_EXPOSURE_MIN_VERSION,
                    "host_socket_exposures require Apple container CLI 1.1.0 or newer",
                )
            }
            ContainerRuntimeType::Colima => {
                anyhow::bail!("host_socket_exposures are not supported by the colima runtime")
            }
            ContainerRuntimeType::Podman => {
                require_linux_host("podman")?;
                for key in ["CONTAINER_HOST", "CONTAINER_CONNECTION"] {
                    if std::env::var_os(key).is_some() || self.env.contains_key(key) {
                        anyhow::bail!(
                            "host_socket_exposures require a native local Podman runtime; {key} selects a remote or machine connection"
                        );
                    }
                }
                let mut command = StdCommand::new(&self.program);
                command.envs(&self.env);
                let output = command
                    .args(["system", "connection", "list", "--format", "json"])
                    .output()
                    .context("run podman system connection list for host_socket_exposures")?;
                if !output.status.success() {
                    anyhow::bail!(
                        "podman connection preflight failed while validating host_socket_exposures: {}",
                        command_output_text(&output)
                    );
                }
                if podman_connection_list_has_default(&output.stdout)? {
                    anyhow::bail!(
                        "host_socket_exposures require native local Podman without a configured default remote or machine connection"
                    );
                }
                Ok(())
            }
            ContainerRuntimeType::Docker => {
                require_linux_host("docker")?;
                if std::env::var_os("DOCKER_CONTEXT").is_some()
                    || self.env.contains_key("DOCKER_CONTEXT")
                {
                    anyhow::bail!(
                        "host_socket_exposures require Docker's native local default context; DOCKER_CONTEXT is set"
                    );
                }
                let docker_host = self
                    .env
                    .get("DOCKER_HOST")
                    .cloned()
                    .or_else(|| std::env::var("DOCKER_HOST").ok());
                let socket_path = match docker_host {
                    Some(value) => value.strip_prefix("unix://").map(PathBuf::from).ok_or_else(|| {
                        anyhow::anyhow!(
                            "host_socket_exposures require a local Unix-socket Docker Engine; effective DOCKER_HOST is {value:?}"
                        )
                    })?,
                    None => {
                        let mut command = StdCommand::new(&self.program);
                        command.envs(&self.env);
                        let output = command
                            .args(["context", "show"])
                            .output()
                            .context("run docker context show for host_socket_exposures")?;
                        if !output.status.success() {
                            anyhow::bail!(
                                "docker context show failed while validating host_socket_exposures: {}",
                                command_output_text(&output)
                            );
                        }
                        let context = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if context != "default" {
                            anyhow::bail!(
                                "host_socket_exposures require Docker's native local default context; current context is {context:?}"
                            );
                        }
                        PathBuf::from("/var/run/docker.sock")
                    }
                };
                require_unix_socket(&socket_path, "Docker Engine endpoint")
            }
        }
    }

    pub async fn sweep_stale_cancel_markers(
        &self,
        container_name: &str,
        user: &str,
    ) -> anyhow::Result<usize> {
        let spec = cancel_marker_list_spec(container_name, user);
        let output = self
            .exec_capture_with_timeout(&spec, Some(CANCEL_CLEANUP_TIMEOUT))
            .await?;
        if output.exit_code != 0 {
            anyhow::bail!(
                "{} exec {} stale cancel marker list exited with status {}",
                self.runtime_label(),
                container_name,
                output.exit_code
            );
        }

        let listed = String::from_utf8_lossy(&output.stdout);
        let stale = stale_cancel_marker_paths(listed.lines(), host_process_is_active);
        if stale.is_empty() {
            return Ok(0);
        }

        for chunk in stale.chunks(100) {
            let spec = cancel_marker_sweep_spec(container_name, user, chunk);
            let exit_code = self
                .exec_discard_with_timeout(&spec, Some(CANCEL_CLEANUP_TIMEOUT))
                .await?;
            if exit_code != 0 {
                anyhow::bail!(
                    "{} exec {} stale cancel marker sweep exited with status {}",
                    self.runtime_label(),
                    container_name,
                    exit_code
                );
            }
        }
        Ok(stale.len())
    }

    pub async fn sweep_stale_cancel_markers_once(
        &self,
        container_name: &str,
        user: &str,
    ) -> anyhow::Result<Option<usize>> {
        let key = self.cancel_marker_sweep_key(container_name, user);
        let sweep_keys = CANCEL_MARKER_SWEEP_KEYS.get_or_init(|| Mutex::new(BTreeSet::new()));
        let should_sweep = sweep_keys
            .lock()
            .map_err(|_| anyhow::anyhow!("cancel marker sweep key lock poisoned"))?
            .insert(key);
        if !should_sweep {
            return Ok(None);
        }
        self.sweep_stale_cancel_markers(container_name, user)
            .await
            .map(Some)
    }

    fn cancel_marker_sweep_key(&self, container_name: &str, user: &str) -> String {
        format!(
            "{:?}\0{}\0{:?}\0{}\0{}",
            self.kind, self.program, self.env, container_name, user
        )
    }

    pub async fn inspect(&self, name: &str) -> anyhow::Result<Option<ContainerInspect>> {
        let mut command = self.async_command().await?;
        let output = command
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
        if self.kind == ContainerRuntimeType::AppleContainer {
            parse::parse_apple_container_inspect(&output.stdout, self.runtime_label())
        } else {
            parse::parse_container_inspect(&output.stdout, self.runtime_label())
        }
    }

    pub async fn run_detached(&self, spec: &ContainerRunSpec) -> anyhow::Result<()> {
        self.run_status_with_env("run", self.run_args(spec), self.run_env(spec))
            .await
    }

    pub async fn stop(&self, name: &str) -> anyhow::Result<()> {
        self.run_status("stop", [name]).await
    }

    pub async fn start(&self, name: &str) -> anyhow::Result<()> {
        self.run_status("start", [name]).await
    }

    pub async fn rm(&self, name: &str) -> anyhow::Result<()> {
        if self.kind == ContainerRuntimeType::AppleContainer {
            return self.run_status("delete", ["--force", name]).await;
        }
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
        let mut command = self.async_command().await?;
        apply_command_env(&mut command, self.exec_env(spec));
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
        let mut command = self.async_command().await?;
        apply_command_env(&mut command, self.exec_env(spec));
        command
            .arg("exec")
            .args(self.exec_args(spec))
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
        let stdout = child
            .stdout
            .take()
            .context("container exec stdout pipe was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("container exec stderr pipe was not captured")?;
        let stdout_task = tokio::spawn(read_pipe_to_end(stdout));
        let stderr_task = tokio::spawn(read_pipe_to_end(stderr));
        let status = match timeout_duration {
            Some(timeout_duration) => {
                match tokio::time::timeout(timeout_duration, child.wait()).await {
                    Ok(result) => result,
                    Err(_) => {
                        let _ = child.start_kill();
                        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                        stdout_task.abort();
                        stderr_task.abort();
                        anyhow::bail!(
                            "{} exec {} timed out after {:?}",
                            self.runtime_label(),
                            spec.container_name,
                            timeout_duration
                        );
                    }
                }
            }
            None => child.wait().await,
        }
        .with_context(|| format!("run {} exec {}", self.runtime_label(), spec.container_name))?;
        let (stdout, stderr) = drain_exec_capture_pipes(
            stdout_task,
            stderr_task,
            timeout_duration.map(|_| EXEC_CAPTURE_DRAIN_TIMEOUT),
            self.runtime_label(),
            &spec.container_name,
        )
        .await?;
        Ok(ContainerExecOutput {
            exit_code: exit_code(status),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
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

        let mut command = self.async_command().await?;
        apply_command_env(&mut command, self.exec_env(&exec_spec));
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
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                    stdout_truncated: stdout.truncated,
                    stderr_truncated: stderr.truncated,
                };
                run_container_cancel_exec(
                    self.clone(),
                    remove_spec,
                    marker.path.clone(),
                    spec.container_name.clone(),
                    "normal-completion",
                    "marker-remove",
                    ContainerCancelExecLog::Debug,
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

        let mut command = self.async_command().await?;
        apply_command_env(&mut command, self.exec_env(&exec_spec));
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
                run_container_cancel_exec(
                    self.clone(),
                    remove_spec,
                    marker.path.clone(),
                    spec.container_name.clone(),
                    "normal-completion",
                    "marker-remove",
                    ContainerCancelExecLog::Debug,
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
        let mut command = self.async_command().await?;
        apply_command_env(&mut command, self.exec_env(spec));
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
        self.preflight()?;
        let mut command = portable_pty::CommandBuilder::new(&self.program);
        command.arg("exec");
        command.args(self.exec_args(&pty_spec));
        for (key, value) in &self.env {
            command.env(key, value);
        }
        apply_pty_command_env(&mut command, self.exec_env(&pty_spec));

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

    pub async fn exec_quiet<I, S>(&self, name: &str, user: &str, args: I) -> anyhow::Result<i32>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if self.kind == ContainerRuntimeType::AppleContainer {
            let spec = ContainerExecSpec {
                stdin_tty: false,
                stdout_tty: false,
                user: user.to_string(),
                cwd: None,
                env: BTreeMap::new(),
                container_name: name.to_string(),
                command: args
                    .into_iter()
                    .map(|arg| arg.as_ref().to_string_lossy().into_owned())
                    .collect(),
            };
            let mut command = self.async_command().await?;
            let output = command
                .arg("exec")
                .args(self.exec_args(&spec))
                .output()
                .await
                .with_context(|| format!("run {} exec {name}", self.runtime_label()))?;
            return Ok(output.status.code().unwrap_or(1));
        }
        let mut command = self.async_command().await?;
        let output = command
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
    ) -> anyhow::Result<Option<PublishedPort>> {
        if self.kind == ContainerRuntimeType::AppleContainer {
            return Ok(None);
        }
        let mut command = self.async_command().await?;
        let output = command
            .args(["port", name, &format!("{container_port}/tcp")])
            .output()
            .await
            .with_context(|| format!("run {} port {name}", self.runtime_label()))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(parse_published_port_endpoint(&output.stdout))
    }

    pub async fn list_managed_containers(
        &self,
        user: &str,
        uid: u32,
        context: &RuntimeContext,
        apple_name_candidates: Option<&AppleContainerNameCandidates>,
    ) -> anyhow::Result<Vec<ManagedContainer>> {
        let command_label = self.list_command_label();
        let mut command = self.async_command().await?;
        let output = command
            .args(self.list_managed_args(user, uid, context))
            .output()
            .await
            .with_context(|| format!("run {} {command_label}", self.runtime_label()))?;
        if !output.status.success() {
            anyhow::bail!(
                "{} {command_label} failed: {}",
                self.runtime_label(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let containers = if self.kind == ContainerRuntimeType::AppleContainer {
            parse::parse_apple_container_list(&output.stdout)
        } else {
            parse_managed_containers(&output.stdout)
        }
        .with_context(|| format!("parse {} managed container list JSON", self.runtime_label()))?;
        if self.kind == ContainerRuntimeType::AppleContainer {
            return self
                .filter_apple_managed_containers(
                    containers,
                    user,
                    uid,
                    context,
                    apple_name_candidates,
                )
                .await;
        }
        Ok(containers
            .into_iter()
            .filter(|container| {
                parse::is_aw_gateway_managed_container_for(container, user, uid)
                    && context.matches_stored(&context_from_labels(&container.labels))
            })
            .collect())
    }

    async fn filter_apple_managed_containers(
        &self,
        containers: Vec<ManagedContainer>,
        user: &str,
        uid: u32,
        context: &RuntimeContext,
        apple_name_candidates: Option<&AppleContainerNameCandidates>,
    ) -> anyhow::Result<Vec<ManagedContainer>> {
        let mut managed = Vec::new();
        for container in containers {
            let container = if container.labels.is_empty() {
                let Some(candidates) = apple_name_candidates else {
                    continue;
                };
                if !candidates.matches(&container.name) {
                    continue;
                }
                let Some(inspect) = self.inspect(&container.name).await? else {
                    anyhow::bail!(
                        "apple container list row {:?} matched an aw-gateway candidate name but inspect returned no container; managed container listing cannot safely identify ownership",
                        container.name
                    );
                };
                if inspect.config.labels.is_empty() {
                    anyhow::bail!(
                        "apple container inspect output for candidate {:?} omitted labels; managed container listing cannot safely identify aw-gateway ownership",
                        container.name
                    );
                }
                let image = if container.image.is_empty() {
                    inspect
                        .config
                        .labels
                        .get("io.aw-gateway.image")
                        .cloned()
                        .unwrap_or_default()
                } else {
                    container.image
                };
                ManagedContainer {
                    name: container.name,
                    image,
                    running: inspect.state.running,
                    labels: inspect.config.labels,
                }
            } else {
                container
            };
            if parse::is_aw_gateway_managed_container_for(&container, user, uid)
                && context.matches_stored(&context_from_labels(&container.labels))
            {
                managed.push(container);
            }
        }
        Ok(managed)
    }

    pub fn run_args(&self, spec: &ContainerRunSpec) -> Vec<String> {
        match self.kind {
            ContainerRuntimeType::Podman => self.podman_run_args(spec),
            ContainerRuntimeType::Docker | ContainerRuntimeType::Colima => {
                self.docker_run_args(spec)
            }
            ContainerRuntimeType::AppleContainer => self.apple_run_args(spec),
        }
    }

    pub fn run_env<'a>(&self, spec: &'a ContainerRunSpec) -> &'a BTreeMap<String, String> {
        &spec.env
    }

    pub fn list_managed_args(&self, user: &str, uid: u32, context: &RuntimeContext) -> Vec<String> {
        if self.kind == ContainerRuntimeType::AppleContainer {
            return vec![
                "list".into(),
                "--all".into(),
                "--format".into(),
                "json".into(),
            ];
        }
        let mut args = vec![
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
        ];
        for (key, value) in context.as_map() {
            args.push("--filter".into());
            args.push(format!("label={}={value}", context_label_key(key)));
        }
        args
    }

    pub fn exec_args(&self, spec: &ContainerExecSpec) -> Vec<String> {
        if self.kind == ContainerRuntimeType::AppleContainer {
            return self.apple_exec_args(spec);
        }
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
            args.push(runtime_env_arg(key, value));
        }
        args.push(spec.container_name.clone());
        args.extend(spec.command.clone());
        args
    }

    pub fn exec_env<'a>(&self, spec: &'a ContainerExecSpec) -> &'a BTreeMap<String, String> {
        &spec.env
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

    fn apple_run_args(&self, spec: &ContainerRunSpec) -> Vec<String> {
        let mut args = vec![
            "--detach".into(),
            "--init".into(),
            "--name".into(),
            spec.name.clone(),
        ];
        args.push("--user".into());
        args.push(spec.container_user.clone());
        args.extend(spec.extra_run_args.clone());
        if let Some(port) = spec.published_ssh_host_port {
            args.push("--publish".into());
            args.push(format!("127.0.0.1:{port}:22/tcp"));
        }
        args.push("--volume".into());
        args.push(format!(
            "{}:{}",
            spec.workspace.display(),
            spec.workspace_container_path.display()
        ));
        for mount in &spec.mounts {
            args.push("--volume".into());
            let options = if mount.readonly { ":ro" } else { "" };
            args.push(format!(
                "{}:{}{}",
                mount.source.display(),
                mount.target.display(),
                options
            ));
        }
        for exposure in &spec.host_socket_exposures {
            args.push("--volume".into());
            args.push(format!(
                "{}:{}",
                exposure.host_path.display(),
                exposure.container_path.display()
            ));
        }
        args.push("--workdir".into());
        args.push(spec.container_home.display().to_string());
        args.push("--env".into());
        args.push(format!("HOME={}", spec.container_home.display()));
        args.push("--env".into());
        args.push(format!(
            "AW_CONTAINER_STATE_DIR={}",
            spec.state_dir_in_container.display()
        ));
        for (key, value) in &spec.env {
            args.push("--env".into());
            args.push(runtime_env_arg(key, value));
        }
        for (key, value) in &spec.labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        args.push(docker_image_ref(&spec.image));
        args.extend(spec.command.clone());
        args
    }

    fn apple_exec_args(&self, spec: &ContainerExecSpec) -> Vec<String> {
        let mut args = Vec::new();
        if spec.stdin_tty {
            args.push("--interactive".into());
        }
        if spec.stdout_tty {
            args.push("--tty".into());
        }
        args.push("--user".into());
        args.push(spec.user.clone());
        if let Some(cwd) = &spec.cwd {
            args.push("--workdir".into());
            args.push(cwd.display().to_string());
        }
        for (key, value) in &spec.env {
            args.push("--env".into());
            args.push(runtime_env_arg(key, value));
        }
        args.push(spec.container_name.clone());
        args.extend(spec.command.clone());
        args
    }

    fn push_publish_ssh_args(&self, args: &mut Vec<String>, spec: &ContainerRunSpec) {
        if spec.publish_ssh {
            args.push("-p".into());
            let publish = match spec.published_ssh_host_port {
                Some(port) => format!("127.0.0.1:{port}:22"),
                None => "127.0.0.1::22".into(),
            };
            args.push(publish);
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
            spec.workspace_container_path.display(),
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
        for exposure in &spec.host_socket_exposures {
            match (self.kind, exposure.selinux_relabel) {
                (ContainerRuntimeType::Podman, relabel) => {
                    args.push("--mount".into());
                    let relabel = match relabel {
                        SelinuxRelabel::None => String::new(),
                        SelinuxRelabel::Shared => ",relabel=shared".into(),
                        SelinuxRelabel::Private => ",relabel=private".into(),
                    };
                    args.push(format!(
                        "type=bind,src={},dst={}{}",
                        exposure.host_path.display(),
                        exposure.container_path.display(),
                        relabel
                    ));
                }
                (ContainerRuntimeType::Docker, SelinuxRelabel::None) => {
                    args.push("--mount".into());
                    args.push(format!(
                        "type=bind,src={},dst={}",
                        exposure.host_path.display(),
                        exposure.container_path.display()
                    ));
                }
                (ContainerRuntimeType::Docker, relabel) => {
                    args.push("--volume".into());
                    let suffix = match relabel {
                        SelinuxRelabel::Shared => "z",
                        SelinuxRelabel::Private => "Z",
                        SelinuxRelabel::None => unreachable!(),
                    };
                    args.push(format!(
                        "{}:{}:{suffix}",
                        exposure.host_path.display(),
                        exposure.container_path.display()
                    ));
                }
                _ => unreachable!("common run args are only used by Podman and Docker"),
            }
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
            args.push(runtime_env_arg(key, value));
        }
        for (key, value) in &spec.labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
    }

    async fn async_command(&self) -> anyhow::Result<Command> {
        if self.kind != ContainerRuntimeType::AppleContainer {
            self.preflight()?;
            return Ok(self.command_without_preflight());
        }
        #[cfg(test)]
        if apple_preflight_test_bypassed() || apple_preflight_test_host().is_some() {
            self.preflight()?;
            return Ok(self.command_without_preflight());
        }
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || runtime.preflight())
            .await
            .context("join Apple container preflight task")??;
        Ok(self.command_without_preflight())
    }

    fn command_without_preflight(&self) -> Command {
        let mut command = Command::new(&self.program);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    fn preflight(&self) -> anyhow::Result<()> {
        if self.kind != ContainerRuntimeType::AppleContainer {
            return Ok(());
        }
        #[cfg(test)]
        if apple_preflight_test_bypassed() {
            return Ok(());
        }

        let key = self.apple_preflight_cache_key();
        let cache = APPLE_PREFLIGHT_CACHE.get_or_init(|| Mutex::new(BTreeSet::new()));
        if cache
            .lock()
            .map_err(|_| anyhow::anyhow!("apple container preflight cache lock poisoned"))?
            .contains(&key)
        {
            return Ok(());
        }

        let host = Self::apple_preflight_host_info()?;
        run_apple_preflight_checks(&self.program, &self.env, &host)?;

        cache
            .lock()
            .map_err(|_| anyhow::anyhow!("apple container preflight cache lock poisoned"))?
            .insert(key);
        Ok(())
    }

    fn apple_preflight_cache_key(&self) -> String {
        self.program.clone()
    }

    fn apple_preflight_host_info() -> anyhow::Result<AppleHostInfo> {
        #[cfg(test)]
        if let Some(host) = apple_preflight_test_host() {
            return Ok(host);
        }
        current_apple_host_info()
    }

    async fn run_status<I, S>(&self, subcommand: &str, args: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let env = BTreeMap::new();
        self.run_status_with_env(subcommand, args, &env).await
    }

    async fn run_status_with_env<I, S>(
        &self,
        subcommand: &str,
        args: I,
        env: &BTreeMap<String, String>,
    ) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.async_command().await?;
        apply_command_env(&mut command, env);
        let output = command
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
            ContainerRuntimeType::AppleContainer => "apple container",
        }
    }

    fn list_command_label(&self) -> &'static str {
        match self.kind {
            ContainerRuntimeType::AppleContainer => "list",
            ContainerRuntimeType::Podman
            | ContainerRuntimeType::Docker
            | ContainerRuntimeType::Colima => "ps",
        }
    }
}

fn podman_connection_list_has_default(stdout: &[u8]) -> anyhow::Result<bool> {
    let entries = serde_json::from_slice::<Vec<serde_json::Value>>(stdout)
        .context("parse podman system connection list output as JSON")?;
    Ok(entries.iter().any(|entry| {
        entry
            .get("Default")
            .or_else(|| entry.get("default"))
            .and_then(|value| {
                value
                    .as_bool()
                    .or_else(|| value.as_str().map(|value| value == "true"))
            })
            .unwrap_or(false)
    }))
}

fn apply_command_env(command: &mut Command, env: &BTreeMap<String, String>) {
    for (key, value) in env {
        if runtime_env_can_passthrough_client_process(key) {
            command.env(key, value);
        }
    }
}

fn apply_pty_command_env(
    command: &mut portable_pty::CommandBuilder,
    env: &BTreeMap<String, String>,
) {
    for (key, value) in env {
        if runtime_env_can_passthrough_client_process(key) {
            command.env(key, value);
        }
    }
}

async fn drain_exec_capture_pipes(
    stdout_task: JoinHandle<std::io::Result<BoundedOutput>>,
    stderr_task: JoinHandle<std::io::Result<BoundedOutput>>,
    timeout_duration: Option<Duration>,
    runtime_label: &'static str,
    container_name: &str,
) -> anyhow::Result<(BoundedOutput, BoundedOutput)> {
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();
    let drain = async move {
        let stdout = stdout_task.await.context("join stdout drain task")??;
        let stderr = stderr_task.await.context("join stderr drain task")??;
        anyhow::Ok((stdout, stderr))
    };
    match timeout_duration {
        Some(timeout_duration) => match tokio::time::timeout(timeout_duration, drain).await {
            Ok(result) => result,
            Err(_) => {
                stdout_abort.abort();
                stderr_abort.abort();
                anyhow::bail!(
                    "{runtime_label} exec {container_name} timed out draining captured output after {timeout_duration:?}"
                );
            }
        },
        None => drain.await,
    }
}

fn runtime_env_arg(key: &str, value: &str) -> String {
    if runtime_env_can_passthrough_client_process(key) {
        key.to_string()
    } else {
        explicit_env_arg(key, value)
    }
}

fn explicit_env_arg(key: &str, value: &str) -> String {
    format!("{key}={value}")
}

fn runtime_env_can_passthrough_client_process(key: &str) -> bool {
    !runtime_client_sensitive_env_key(key)
}

fn runtime_client_sensitive_env_key(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "LD_DEBUG"
            | "DYLD_INSERT_LIBRARIES"
            | "DYLD_LIBRARY_PATH"
            | "HOME"
            | "BASH_ENV"
            | "ENV"
            | "IFS"
            | "TMPDIR"
            | "TMP"
            | "TEMP"
            | "XDG_RUNTIME_DIR"
            | "XDG_CONFIG_HOME"
            | "XDG_DATA_HOME"
            | "DOCKER_HOST"
            | "DOCKER_CONTEXT"
            | "DOCKER_CONFIG"
            | "DOCKER_TLS_VERIFY"
            | "DOCKER_CERT_PATH"
            | "CONTAINER_HOST"
            | "CONTAINER_CONNECTION"
            | "CONTAINER_SSHKEY"
            | "CONTAINER_DEBUG"
            | "CONTAINER_DEFAULT_PLATFORM"
            | "CONTAINERS_CONF"
            | "CONTAINERS_REGISTRIES_CONF"
            | "CONTAINERS_STORAGE_CONF"
            | "REGISTRY_AUTH_FILE"
            | "SSL_CERT_FILE"
            | "SSL_CERT_DIR"
    )
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
    crate::random::random_hex(32)
}

fn wrap_cancelable_command(
    command: &[String],
    marker: &ContainerCancelMarker,
    argv0: &'static str,
) -> Vec<String> {
    let mut wrapped = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"( umask 077 && printf "%s %s\n" "$$" "$2" > "$1" ) || exit $?; shift 2; exec "$@""#
            .to_string(),
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

fn cancel_marker_list_spec(container_name: &str, user: &str) -> ContainerExecSpec {
    ContainerExecSpec {
        stdin_tty: false,
        stdout_tty: false,
        user: user.to_string(),
        cwd: None,
        env: BTreeMap::new(),
        container_name: container_name.to_string(),
        command: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"for path in /tmp/aw-gateway-*-*.pid; do
  [ -e "$path" ] || continue
  printf '%s\n' "$path"
done"#
                .to_string(),
            CANCEL_MARKER_LIST_ARGV0.to_string(),
        ],
    }
}

fn cancel_marker_sweep_spec(
    container_name: &str,
    user: &str,
    marker_paths: &[String],
) -> ContainerExecSpec {
    let mut command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"for path do
  case "$path" in
    /tmp/aw-gateway-*-*.pid) rm -f -- "$path" 2>/dev/null || true ;;
  esac
done"#
            .to_string(),
        CANCEL_MARKER_SWEEP_ARGV0.to_string(),
    ];
    command.extend(marker_paths.iter().cloned());
    ContainerExecSpec {
        stdin_tty: false,
        stdout_tty: false,
        user: user.to_string(),
        cwd: None,
        env: BTreeMap::new(),
        container_name: container_name.to_string(),
        command,
    }
}

fn stale_cancel_marker_paths<'a, I, F>(paths: I, host_process_is_active: F) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
    F: Fn(u32) -> bool,
{
    paths
        .into_iter()
        .filter_map(|path| {
            let pid = parse_cancel_marker_host_pid(path)?;
            (!host_process_is_active(pid)).then(|| path.to_string())
        })
        .collect()
}

fn parse_cancel_marker_host_pid(path: &str) -> Option<u32> {
    let rest = path.strip_prefix(CANCEL_MARKER_PATH_PREFIX)?;
    let rest = rest.strip_suffix(CANCEL_MARKER_PATH_SUFFIX)?;
    let (kind_and_pid, id) = rest.rsplit_once('-')?;
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let (kind, pid) = kind_and_pid.rsplit_once('-')?;
    if kind.is_empty() {
        return None;
    }
    pid.parse().ok()
}

#[cfg(unix)]
fn host_process_is_active(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    (unsafe { libc::kill(pid, 0) }) == 0
}

#[cfg(not(unix))]
fn host_process_is_active(pid: u32) -> bool {
    pid == std::process::id()
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
        ContainerRuntimeType::AppleContainer => "container",
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
    parse_published_port_endpoint(stdout).map(|endpoint| endpoint.port)
}

pub fn parse_published_port_endpoint(stdout: &[u8]) -> Option<PublishedPort> {
    let stdout = String::from_utf8_lossy(stdout);
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if let Some(rest) = line.strip_prefix('[') {
        let (host, port) = rest.rsplit_once("]:")?;
        return Some(PublishedPort {
            host: host.into(),
            port: port.parse().ok()?,
        });
    }
    let (_, port) = line.rsplit_once(':')?;
    let (host, _) = line.rsplit_once(':')?;
    Some(PublishedPort {
        host: host.into(),
        port: port.parse().ok()?,
    })
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

    fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
        crate::test_support::write_executable_fixture(path, contents);
    }

    fn test_exposure(relabel: SelinuxRelabel) -> HostSocketExposureSpec {
        HostSocketExposureSpec {
            name: "traffic".into(),
            host_path: PathBuf::from("/tmp/traffic.sock"),
            container_path: PathBuf::from("/run/traffic.sock"),
            readiness_user: "root".into(),
            selinux_relabel: relabel,
            realization: HostSocketExposureRealization::PinnedInode,
            source_identity: UnixSourceIdentity {
                device: 1,
                inode: 2,
                uid: 1000,
                gid: 1000,
                mode: 0o600,
            },
        }
    }

    fn test_run_spec(exposures: Vec<HostSocketExposureSpec>) -> ContainerRunSpec {
        ContainerRunSpec {
            name: "aw-local".into(),
            hostname: "aw-local".into(),
            image: "ubuntu/dev".into(),
            workspace: PathBuf::from("/workspace"),
            workspace_container_path: PathBuf::from("/root"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway"),
            mounts: Vec::new(),
            host_socket_exposures: exposures,
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: false,
            published_ssh_host_port: None,
            extra_run_args: Vec::new(),
            command: vec!["sleep".into(), "infinity".into()],
        }
    }

    #[test]
    fn container_run_spec_debug_redacts_environment_values() {
        let mut spec = test_run_spec(Vec::new());
        spec.env.insert(
            "AW_IDENTITY_TOKEN".into(),
            "abcdefghijklmnopqrstuvwxyzABCDEF".into(),
        );
        spec.env
            .insert("AW_SAFE".into(), "visible-but-redacted".into());

        let debug = format!("{spec:?}");
        assert!(debug.contains("AW_IDENTITY_TOKEN"));
        assert!(debug.contains("AW_SAFE"));
        assert!(!debug.contains("abcdefghijklmnopqrstuvwxyzABCDEF"));
        assert!(!debug.contains("visible-but-redacted"));
    }

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
    fn apple_socket_exposure_uses_volume_without_reverse_publish_or_mount() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: "container".into(),
            env: BTreeMap::new(),
        };
        let mut exposure = test_exposure(SelinuxRelabel::None);
        exposure.realization = HostSocketExposureRealization::PathReconnect;
        let args = runtime.run_args(&test_run_spec(vec![exposure]));

        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--volume", "/tmp/traffic.sock:/run/traffic.sock"] })
        );
        assert!(!args.iter().any(|arg| arg == "--publish-socket"));
        assert!(!args.iter().any(|arg| arg == "--mount"));
    }

    #[test]
    fn native_linux_socket_exposure_renders_explicit_relabel_choice() {
        let podman = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::new(),
        };
        let docker = ContainerRuntime {
            kind: ContainerRuntimeType::Docker,
            program: "docker".into(),
            env: BTreeMap::new(),
        };

        let podman_args =
            podman.run_args(&test_run_spec(vec![test_exposure(SelinuxRelabel::Shared)]));
        assert!(podman_args.windows(2).any(|pair| {
            pair == [
                "--mount",
                "type=bind,src=/tmp/traffic.sock,dst=/run/traffic.sock,relabel=shared",
            ]
        }));

        let docker_none =
            docker.run_args(&test_run_spec(vec![test_exposure(SelinuxRelabel::None)]));
        assert!(docker_none.windows(2).any(|pair| {
            pair == [
                "--mount",
                "type=bind,src=/tmp/traffic.sock,dst=/run/traffic.sock",
            ]
        }));
        let docker_private =
            docker.run_args(&test_run_spec(vec![test_exposure(SelinuxRelabel::Private)]));
        assert!(
            docker_private
                .windows(2)
                .any(|pair| { pair == ["--volume", "/tmp/traffic.sock:/run/traffic.sock:Z"] })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_exposure_runtime_preflight_rejects_remote_or_vm_backends() {
        let colima = ContainerRuntime {
            kind: ContainerRuntimeType::Colima,
            program: "docker".into(),
            env: BTreeMap::new(),
        };
        assert!(
            colima
                .validate_host_socket_exposure_support_blocking()
                .unwrap_err()
                .to_string()
                .contains("colima")
        );

        let docker = ContainerRuntime {
            kind: ContainerRuntimeType::Docker,
            program: "docker".into(),
            env: BTreeMap::from([("DOCKER_HOST".into(), "ssh://remote/run/docker.sock".into())]),
        };
        assert!(
            docker
                .validate_host_socket_exposure_support_blocking()
                .unwrap_err()
                .to_string()
                .contains("local Unix-socket Docker Engine")
        );

        let podman = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::from([(
                "CONTAINER_HOST".into(),
                "ssh://remote/run/podman.sock".into(),
            )]),
        };
        assert!(
            podman
                .validate_host_socket_exposure_support_blocking()
                .unwrap_err()
                .to_string()
                .contains("native local Podman")
        );
    }

    #[test]
    fn podman_connection_preflight_detects_default_remote() {
        assert!(!podman_connection_list_has_default(br#"[]"#).unwrap());
        assert!(
            podman_connection_list_has_default(
                br#"[{"Name":"machine","URI":"ssh://host/run/podman.sock","Default":true}]"#
            )
            .unwrap()
        );
    }

    #[test]
    fn apple_socket_exposure_enforces_feature_version_minimum() {
        let err = validate_apple_cli_minimum_version_json(
            br#"{"version":"1.0.9"}"#,
            APPLE_CONTAINER_SOCKET_EXPOSURE_MIN_VERSION,
            "host_socket_exposures require Apple container CLI 1.1.0 or newer",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("1.1.0 or newer"), "{err}");
        validate_apple_cli_minimum_version_json(
            br#"{"version":"1.1.0"}"#,
            APPLE_CONTAINER_SOCKET_EXPOSURE_MIN_VERSION,
            "host_socket_exposures require Apple container CLI 1.1.0 or newer",
        )
        .unwrap();
        let err = validate_apple_cli_minimum_version_json(
            br#"{"build":"2026A42"}"#,
            APPLE_CONTAINER_SOCKET_EXPOSURE_MIN_VERSION,
            "host_socket_exposures require Apple container CLI 1.1.0 or newer",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("did not contain a parseable"), "{err}");
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
        assert!(wrapped[2].contains("umask 077"));

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

    #[test]
    fn stale_cancel_marker_paths_keep_active_and_malformed_markers() {
        let paths = [
            "/tmp/aw-gateway-exec-111-1.pid",
            "/tmp/aw-gateway-pty-222-2.pid",
            "/tmp/aw-gateway-exec-333.pid",
            "/tmp/aw-gateway-exec-444-nope.pid",
            "/tmp/not-aw-gateway-exec-555-5.pid",
        ];

        let stale = stale_cancel_marker_paths(paths, |pid| pid == 222);

        assert_eq!(stale, vec!["/tmp/aw-gateway-exec-111-1.pid"]);
    }

    #[cfg(unix)]
    #[test]
    fn host_process_is_active_rejects_out_of_range_pid() {
        assert!(!host_process_is_active(u32::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn signalable_pid_rejects_group_zero_one_and_out_of_range_pid() {
        assert_eq!(signalable_pid(0), None);
        assert_eq!(signalable_pid(1), None);
        assert_eq!(signalable_pid(u32::MAX), None);
        assert_eq!(signalable_pid(2), Some(2));
    }

    #[test]
    fn stale_cancel_marker_sweep_command_is_rm_only() {
        let test_pid = std::process::id();
        let paths = vec![
            format!("/tmp/aw-gateway-exec-{test_pid}-1001.pid"),
            format!("/tmp/aw-gateway-pty-{test_pid}-1002.pid"),
        ];
        let spec = cancel_marker_sweep_spec("ubuntu-dev", "2450:100", &paths);

        assert_eq!(spec.command[0], "/bin/sh");
        assert_eq!(spec.command[3], CANCEL_MARKER_SWEEP_ARGV0);
        assert_eq!(&spec.command[4..], paths.as_slice());
        let script = &spec.command[2];
        assert!(script.contains("rm -f -- \"$path\""));
        assert!(script.contains("|| true"));
        assert!(!script.contains("kill "));
        assert!(!script.contains("ps -e"));

        let status = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", script])
            .status()
            .expect("run shell syntax check");
        assert!(status.success());

        for path in &paths {
            std::fs::write(path, "").unwrap();
        }
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .arg(CANCEL_MARKER_SWEEP_ARGV0)
            .args(&paths)
            .status()
            .expect("run marker sweep script");
        assert!(status.success());
        for path in &paths {
            assert!(!Path::new(path).exists(), "{path} was not removed");
        }
    }

    #[tokio::test]
    async fn stale_cancel_marker_sweep_once_skips_repeated_runtime_container_user() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        let log = dir.path().join("runtime.log");
        write_executable(
            &runtime_program,
            format!(
                r#"#!/bin/sh
if [ "$1" = exec ]; then
  echo "$*" >> "{log}"
fi
exit 0
"#,
                log = log.display()
            ),
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };
        let container_name = format!("ubuntu-dev-{}", std::process::id());

        assert_eq!(
            runtime
                .sweep_stale_cancel_markers_once(&container_name, "2450:100")
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            runtime
                .sweep_stale_cancel_markers_once(&container_name, "2450:100")
                .await
                .unwrap(),
            None
        );

        let log = std::fs::read_to_string(log).unwrap();
        assert_eq!(log.matches(CANCEL_MARKER_LIST_ARGV0).count(), 1, "{log}");
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
    fn apple_container_defaults_to_container_program_without_docker_env() {
        let cfg = RuntimeConfig {
            runtime_type: ContainerRuntimeType::AppleContainer,
            program: None,
            docker_host: None,
            profile: None,
        };
        let runtime =
            ContainerRuntime::from_config(&cfg, "alice", Path::new("/Users/alice")).unwrap();

        assert_eq!(runtime.program, "container");
        assert!(runtime.env().is_empty());
    }

    fn valid_apple_preflight_host() -> AppleHostInfo {
        AppleHostInfo {
            os: "macos".into(),
            arch: "aarch64".into(),
            macos_version: Some("26.0".into()),
        }
    }

    #[test]
    fn apple_preflight_host_validation_rejects_non_macos() {
        let host = AppleHostInfo {
            os: "linux".into(),
            arch: "aarch64".into(),
            macos_version: None,
        };

        let err = validate_apple_host(&host).unwrap_err();

        assert!(err.to_string().contains("current host OS is linux"));
    }

    #[test]
    fn apple_preflight_host_validation_rejects_intel_macos() {
        let host = AppleHostInfo {
            os: "macos".into(),
            arch: "x86_64".into(),
            macos_version: Some("26.0".into()),
        };

        let err = validate_apple_host(&host).unwrap_err();

        assert!(err.to_string().contains("requires Apple silicon"));
    }

    #[test]
    fn apple_preflight_host_validation_rejects_old_macos() {
        let host = AppleHostInfo {
            os: "macos".into(),
            arch: "aarch64".into(),
            macos_version: Some("25.6".into()),
        };

        let err = validate_apple_host(&host).unwrap_err();

        assert!(err.to_string().contains("current macOS version is 25.6"));
    }

    #[test]
    fn apple_preflight_host_validation_accepts_apple_silicon_macos_26() {
        validate_apple_host(&valid_apple_preflight_host()).unwrap();
    }

    #[test]
    fn apple_preflight_rejects_missing_runtime_program() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("missing-container");

        let err = run_apple_preflight_checks(
            &program.display().to_string(),
            &BTreeMap::new(),
            &valid_apple_preflight_host(),
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("install the Apple container CLI"),
            "{err:#}"
        );
    }

    #[test]
    fn apple_preflight_accepts_successful_system_version_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-container");
        write_executable(
            &runtime_program,
            r#"#!/bin/sh
case "$*" in
  "system version --format json")
    echo '{"version":"1.0.0"}'
    ;;
  "system status --format json")
    echo '{"status":"running"}'
    ;;
  *)
    echo "unexpected args: $*" >&2
    exit 64
    ;;
esac
"#,
        );

        run_apple_preflight_checks(
            &runtime_program.display().to_string(),
            &BTreeMap::new(),
            &valid_apple_preflight_host(),
        )
        .unwrap();
    }

    #[test]
    fn apple_preflight_rejects_old_cli_semver() {
        let err = validate_apple_cli_version_json(br#"{"version":"0.9.0"}"#).unwrap_err();

        assert!(
            err.to_string()
                .contains("requires Apple container CLI 1.0.0")
        );
    }

    #[test]
    fn apple_base_preflight_accepts_unknown_cli_version_shape() {
        validate_apple_cli_version_json(br#"{"build":"2026A42"}"#).unwrap();
    }

    #[test]
    fn apple_preflight_prefers_client_version_over_api_version() {
        validate_apple_cli_version_json(br#"{"apiVersion":"0.1.0","clientVersion":"1.0.0"}"#)
            .unwrap();
    }

    #[test]
    fn apple_preflight_uses_documented_container_component_version() {
        validate_apple_cli_version_json(
            br#"[
  {"appName":"container-apiserver","version":"0.1.0"},
  {"appName":"container","version":"1.0.0"}
]"#,
        )
        .unwrap();

        let err = validate_apple_cli_version_json(
            br#"[
  {"appName":"container-apiserver","version":"1.0.0"},
  {"appName":"container","version":"0.9.0"}
]"#,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("requires Apple container CLI 1.0.0"),
            "{err:#}"
        );
    }

    #[test]
    fn apple_preflight_rejects_non_json_version_output() {
        let err = validate_apple_cli_version_json(b"not json").unwrap_err();

        assert!(
            format!("{err:#}").contains("system version --format json"),
            "{err:#}"
        );
    }

    #[test]
    fn apple_preflight_rejects_system_status_failure_with_start_hint() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-container");
        write_executable(
            &runtime_program,
            r#"#!/bin/sh
case "$*" in
  "system version --format json")
    echo '{"version":"1.0.0"}'
    ;;
  "system status --format json")
    echo "system is stopped" >&2
    exit 42
    ;;
esac
"#,
        );

        let err = run_apple_preflight_checks(
            &runtime_program.display().to_string(),
            &BTreeMap::new(),
            &valid_apple_preflight_host(),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("system start"), "{err:#}");
    }

    #[tokio::test]
    async fn apple_preflight_caches_successful_command_path_by_program() {
        let _host = override_apple_preflight_host_for_tests(valid_apple_preflight_host());
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-container");
        let preflight_log = dir.path().join("preflight.log");
        let list_log = dir.path().join("list.log");
        write_executable(
            &runtime_program,
            format!(
                r#"#!/bin/sh
case "$*" in
  "system version --format json")
    echo "$*" >> "{preflight_log}"
    echo '[{{"appName":"container-apiserver","version":"0.1.0"}},{{"appName":"container","version":"1.0.0"}}]'
    ;;
  "system status --format json")
    echo "$*" >> "{preflight_log}"
    echo '{{"status":"running"}}'
    ;;
  "list --all --format json")
    echo "$*" >> "{list_log}"
    echo '[]'
    ;;
  *)
    echo "unexpected args: $*" >&2
    exit 64
    ;;
esac
"#,
                preflight_log = preflight_log.display(),
                list_log = list_log.display()
            ),
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };

        runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), None)
            .await
            .unwrap();
        runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), None)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(preflight_log)
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(list_log).unwrap().lines().count(),
            2
        );
    }

    #[tokio::test]
    async fn apple_preflight_retries_after_failed_command_path_preflight() {
        let _host = override_apple_preflight_host_for_tests(valid_apple_preflight_host());
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-container");
        let preflight_log = dir.path().join("preflight.log");
        let list_log = dir.path().join("list.log");
        let fail_status = dir.path().join("fail-status");
        std::fs::write(&fail_status, "").unwrap();
        write_executable(
            &runtime_program,
            format!(
                r#"#!/bin/sh
case "$*" in
  "system version --format json")
    echo "$*" >> "{preflight_log}"
    echo '[{{"appName":"container","version":"1.0.0"}}]'
    ;;
  "system status --format json")
    echo "$*" >> "{preflight_log}"
    if [ -f "{fail_status}" ]; then
      rm -f "{fail_status}"
      echo "system is stopped" >&2
      exit 42
    fi
    echo '{{"status":"running"}}'
    ;;
  "list --all --format json")
    echo "$*" >> "{list_log}"
    echo '[]'
    ;;
  *)
    echo "unexpected args: $*" >&2
    exit 64
    ;;
esac
"#,
                preflight_log = preflight_log.display(),
                list_log = list_log.display(),
                fail_status = fail_status.display()
            ),
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };

        let err = runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("system start"), "{err:#}");

        runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), None)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(preflight_log)
                .unwrap()
                .lines()
                .count(),
            4
        );
        assert_eq!(
            std::fs::read_to_string(list_log).unwrap().lines().count(),
            1
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
            runtime.list_managed_args("alice", 2450, &RuntimeContext::empty()),
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
    fn list_managed_args_include_supplied_context_filters() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::new(),
        };
        let context = RuntimeContext::from_map(BTreeMap::from([
            ("tenant".into(), "acme".into()),
            ("workspace".into(), "web".into()),
        ]));

        let args = runtime.list_managed_args("alice", 2450, &context);

        assert!(args.contains(&"label=io.aw-gateway.context.tenant=acme".to_string()));
        assert!(args.contains(&"label=io.aw-gateway.context.workspace=web".to_string()));
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
            docker.list_managed_args("alice", 2450, &RuntimeContext::empty()),
            colima.list_managed_args("alice", 2450, &RuntimeContext::empty())
        );
        assert_eq!(
            colima.env().get("DOCKER_HOST").map(String::as_str),
            Some("unix:///Users/alice/.colima/default/docker.sock")
        );
    }

    #[test]
    fn apple_list_managed_args_use_unfiltered_json_list() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: "container".into(),
            env: BTreeMap::new(),
        };

        assert_eq!(
            runtime.list_managed_args("alice", 2450, &RuntimeContext::empty()),
            vec!["list", "--all", "--format", "json"]
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
    fn parse_container_inspect_requires_zero_or_one_result() {
        let empty = parse::parse_container_inspect(b"[]", "podman").unwrap();
        assert!(empty.is_none());

        let one = parse::parse_container_inspect(
            br#"[{
  "Id": "abc",
  "Name": "/aw-default",
  "State": {"Running": true, "Pid": 42},
  "Config": {"Labels": {"io.aw-gateway.target": "default"}}
}]"#,
            "podman",
        )
        .unwrap()
        .unwrap();
        assert_eq!(one.id, "abc");
        assert_eq!(one.name, "aw-default");
        assert!(one.state.running);

        let err = parse::parse_container_inspect(
            br#"[
  {
    "Id": "abc",
    "Name": "/aw-default",
    "State": {"Running": true, "Pid": 42},
    "Config": {"Labels": {}}
  },
  {
    "Id": "def",
    "Name": "/aw-other",
    "State": {"Running": true, "Pid": 43},
    "Config": {"Labels": {}}
  }
]"#,
            "podman",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "podman inspect returned 2 containers; expected one"
        );
    }

    #[test]
    fn apple_parse_container_inspect_accepts_lowercase_shape() {
        let inspect = parse::parse_apple_container_inspect(
            br#"[{
  "status": "running",
  "pid": 42,
  "configuration": {
    "id": "aw-default",
    "hostname": "aw-default",
    "labels": {
      "io.aw-gateway.gateway": "true",
      "io.aw-gateway.user": "alice",
      "io.aw-gateway.uid": "2450"
    }
  }
}]"#,
            "apple container",
        )
        .unwrap()
        .unwrap();

        assert_eq!(inspect.id, "aw-default");
        assert_eq!(inspect.name, "aw-default");
        assert!(inspect.state.running);
        assert_eq!(inspect.state.pid, Some(42));
        assert_eq!(
            inspect
                .config
                .labels
                .get("io.aw-gateway.user")
                .map(String::as_str),
            Some("alice")
        );
    }

    #[test]
    fn apple_parse_container_inspect_accepts_missing_pid() {
        let inspect = parse::parse_apple_container_inspect(
            br#"[{
  "status": "running",
  "configuration": {
    "id": "aw-default",
    "hostname": "aw-default",
    "labels": {"io.aw-gateway.gateway": "true"}
  }
}]"#,
            "apple container",
        )
        .unwrap()
        .unwrap();

        assert_eq!(inspect.id, "aw-default");
        assert_eq!(inspect.state.pid, None);
    }

    #[test]
    fn apple_parse_container_inspect_accepts_fixture_object_shape() {
        let inspect = parse::parse_apple_container_inspect(
            br#"[{
  "configuration": {
    "id": "aw-default",
    "image": {
      "reference": "docker.io/library/alpine:latest"
    },
    "labels": {
      "io.aw-gateway.gateway": "true",
      "io.aw-gateway.user": "alice",
      "io.aw-gateway.uid": "2450"
    }
  },
  "id": "aw-default",
  "status": {
    "networks": [],
    "startedDate": "2026-07-02T12:00:25Z",
    "state": "running"
  }
}]"#,
            "apple container",
        )
        .unwrap()
        .unwrap();

        assert_eq!(inspect.id, "aw-default");
        assert_eq!(inspect.name, "aw-default");
        assert!(inspect.state.running);
        assert_eq!(inspect.state.pid, None);
        assert_eq!(
            inspect
                .config
                .labels
                .get("io.aw-gateway.uid")
                .map(String::as_str),
            Some("2450")
        );
    }

    #[test]
    fn apple_parse_container_inspect_requires_zero_or_one_result() {
        let empty = parse::parse_apple_container_inspect(b"[]", "apple container").unwrap();
        assert!(empty.is_none());

        let err = parse::parse_apple_container_inspect(
            br#"[
  {"status": "running", "pid": 42, "configuration": {"id": "aw-default"}},
  {"status": "stopped", "pid": 43, "configuration": {"id": "aw-other"}}
]"#,
            "apple container",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "apple container inspect returned 2 containers; expected one"
        );
    }

    #[test]
    fn apple_parse_container_list_normalizes_rows_without_pre_filtering() {
        let raw = br#"
[
  {
    "status": "running",
    "image": "ubuntu/dev:latest",
    "configuration": {
      "id": "aw-ubuntu",
      "labels": {
        "io.aw-gateway.gateway": "true",
        "io.aw-gateway.user": "alice",
        "io.aw-gateway.uid": "2450"
      }
    }
  },
  {
    "status": "stopped",
    "configuration": {
      "id": "unrelated",
      "image": "busybox:latest",
      "labels": {"com.example": "true"}
    }
  }
]
"#;

        let containers = parse::parse_apple_container_list(raw).unwrap();

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].name, "aw-ubuntu");
        assert_eq!(containers[0].image, "ubuntu/dev:latest");
        assert!(containers[0].running);
        assert_eq!(containers[1].name, "unrelated");
        assert_eq!(containers[1].image, "busybox:latest");
        assert!(!containers[1].running);
        assert!(is_aw_gateway_managed_container_for(
            &containers[0],
            "alice",
            2450
        ));
        assert!(!is_aw_gateway_managed_container_for(
            &containers[1],
            "alice",
            2450
        ));
    }

    #[test]
    fn apple_parse_container_list_accepts_fixture_object_shape() {
        let raw = br#"
[
  {
    "configuration": {
      "id": "aw-default",
      "image": {
        "reference": "docker.io/library/alpine:latest"
      },
      "labels": {
        "io.aw-gateway.gateway": "true",
        "io.aw-gateway.user": "alice",
        "io.aw-gateway.uid": "2450"
      }
    },
    "id": "aw-default",
    "status": {
      "networks": [],
      "state": "running"
    }
  },
  {
    "configuration": {
      "id": "aw-stopped",
      "image": {
        "reference": "docker.io/library/busybox:latest"
      },
      "labels": {
        "io.aw-gateway.gateway": "true",
        "io.aw-gateway.user": "alice",
        "io.aw-gateway.uid": "2450"
      }
    },
    "id": "aw-stopped",
    "status": {
      "networks": [],
      "state": "stopped"
    }
  }
]
"#;

        let containers = parse::parse_apple_container_list(raw).unwrap();

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].name, "aw-default");
        assert_eq!(containers[0].image, "docker.io/library/alpine:latest");
        assert!(containers[0].running);
        assert_eq!(containers[1].name, "aw-stopped");
        assert_eq!(containers[1].image, "docker.io/library/busybox:latest");
        assert!(!containers[1].running);
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
                pid: Some(123),
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
            workspace_container_path: PathBuf::from("/root"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            host_socket_exposures: Vec::new(),
            env: BTreeMap::from([("HOME".into(), "/root".into())]),
            labels: BTreeMap::from([("io.aw-gateway.gateway".into(), "true".into())]),
            publish_ssh: false,
            published_ssh_host_port: None,
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
    fn apple_run_args_use_apple_container_shape() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: "container".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerRunSpec {
            name: "aw-local".into(),
            hostname: "aw-local".into(),
            image: "aw-gateway/ubuntu-dev-local".into(),
            workspace: PathBuf::from("/Users/alice/workspace"),
            workspace_container_path: PathBuf::from("/home/alice/aw-shared"),
            container_home: PathBuf::from("/home/alice"),
            container_user: "alice".into(),
            passwd_entry: Some("ignored:x:2450:2450::/home/alice:/bin/bash".into()),
            state_dir_in_container: PathBuf::from("/home/alice/.aw-gateway/containers/aw-local"),
            mounts: vec![
                ContainerMountSpec {
                    source: PathBuf::from("/Users/alice/.aw-gateway/agent"),
                    target: PathBuf::from("/usr/local/bin/aw-container-agent"),
                    readonly: true,
                },
                ContainerMountSpec {
                    source: PathBuf::from("/Users/alice/cache"),
                    target: PathBuf::from("/cache"),
                    readonly: false,
                },
            ],
            host_socket_exposures: Vec::new(),
            env: BTreeMap::from([
                ("AW_IDENTITY_TOKEN".into(), "token-secret".into()),
                ("AW_SAFE".into(), "1".into()),
                ("PATH".into(), "/tmp/bad".into()),
            ]),
            labels: BTreeMap::from([
                ("io.aw-gateway.gateway".into(), "true".into()),
                ("io.aw-gateway.user".into(), "alice".into()),
            ]),
            publish_ssh: true,
            published_ssh_host_port: Some(40222),
            extra_run_args: vec!["--cpus".into(), "2".into()],
            command: vec!["aw-container-agent".into(), "run".into()],
        };

        let args = runtime.run_args(&spec);

        assert_eq!(&args[..4], ["--detach", "--init", "--name", "aw-local"]);
        assert!(args.windows(2).any(|pair| pair == ["--user", "alice"]));
        assert!(args.windows(2).any(|pair| pair == ["--cpus", "2"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--publish", "127.0.0.1:40222:22/tcp"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--volume", "/Users/alice/workspace:/home/alice/aw-shared"])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                "--volume",
                "/Users/alice/.aw-gateway/agent:/usr/local/bin/aw-container-agent:ro"
            ]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--volume", "/Users/alice/cache:/cache"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--workdir", "/home/alice"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env", "HOME=/home/alice"])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                "--env",
                "AW_CONTAINER_STATE_DIR=/home/alice/.aw-gateway/containers/aw-local"
            ]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env", "AW_IDENTITY_TOKEN"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--env", "AW_SAFE"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env", "PATH=/tmp/bad"])
        );
        assert!(!args.iter().any(|arg| arg.contains("token-secret")));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--label", "io.aw-gateway.gateway=true"])
        );
        assert!(args.contains(&"aw-gateway/ubuntu-dev-local:latest".to_string()));
        assert!(args.ends_with(&["aw-container-agent".to_string(), "run".to_string()]));
        assert!(!args.contains(&"--hostname".to_string()));
        assert!(!args.contains(&"--userns=keep-id".to_string()));
        assert!(!args.contains(&"--passwd-entry".to_string()));
        assert!(!args.iter().any(|arg| arg.contains(":Z")
            || arg.contains("/etc/localtime")
            || arg.contains("/etc/bashrc")));
    }

    #[test]
    fn apple_run_args_do_not_publish_without_explicit_host_port() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: "container".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerRunSpec {
            name: "aw-local".into(),
            hostname: "aw-local".into(),
            image: "ubuntu/dev".into(),
            workspace: PathBuf::from("/workspace"),
            workspace_container_path: PathBuf::from("/root"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            host_socket_exposures: Vec::new(),
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: true,
            published_ssh_host_port: None,
            extra_run_args: Vec::new(),
            command: vec!["sleep".into(), "infinity".into()],
        };

        let args = runtime.run_args(&spec);

        assert!(!args.contains(&"--publish".to_string()));
        assert!(args.contains(&"ubuntu/dev:latest".to_string()));
    }

    #[test]
    fn exec_args_put_runtime_options_before_container_name() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerExecSpec {
            stdin_tty: true,
            stdout_tty: true,
            user: "2450:100".into(),
            cwd: Some(PathBuf::from("/home/alice/project")),
            env: BTreeMap::from([("SHELL".into(), "/usr/bin/bash".into())]),
            container_name: "ubuntu-dev".into(),
            command: vec!["/usr/bin/bash".into(), "-lc".into(), "id -u".into()],
        };
        let args = runtime.exec_args(&spec);

        assert_eq!(
            args,
            vec![
                "-it",
                "--user",
                "2450:100",
                "--workdir",
                "/home/alice/project",
                "--env",
                "SHELL",
                "ubuntu-dev",
                "/usr/bin/bash",
                "-lc",
                "id -u",
            ]
        );
        assert_eq!(
            runtime.exec_env(&spec).get("SHELL").map(String::as_str),
            Some("/usr/bin/bash")
        );
    }

    #[test]
    fn apple_exec_args_use_separate_stdio_flags() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: "container".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerExecSpec {
            stdin_tty: true,
            stdout_tty: true,
            user: "2450:100".into(),
            cwd: Some(PathBuf::from("/home/alice/project")),
            env: BTreeMap::from([
                ("AW_IDENTITY_TOKEN".into(), "token-secret".into()),
                ("CODEX_HOME".into(), "/var/lib/codex".into()),
                ("PATH".into(), "/tmp/bad".into()),
            ]),
            container_name: "ubuntu-dev".into(),
            command: vec!["/usr/bin/bash".into(), "-lc".into(), "id -u".into()],
        };

        assert_eq!(
            runtime.exec_args(&spec),
            vec![
                "--interactive",
                "--tty",
                "--user",
                "2450:100",
                "--workdir",
                "/home/alice/project",
                "--env",
                "AW_IDENTITY_TOKEN",
                "--env",
                "CODEX_HOME",
                "--env",
                "PATH=/tmp/bad",
                "ubuntu-dev",
                "/usr/bin/bash",
                "-lc",
                "id -u",
            ]
        );
        assert!(
            !runtime
                .exec_args(&spec)
                .iter()
                .any(|arg| arg.contains("token-secret"))
        );
    }

    #[tokio::test]
    async fn apple_exec_quiet_uses_apple_exec_renderer() {
        let _apple_preflight_bypass = disable_apple_preflight_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        let log = dir.path().join("runtime.log");
        write_executable(
            &runtime_program,
            format!("#!/bin/sh\necho \"$@\" > \"{}\"\nexit 7\n", log.display()),
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };

        let code = runtime
            .exec_quiet("aw-local", "alice", ["pgrep", "-x", "sshd"])
            .await
            .unwrap();

        assert_eq!(code, 7);
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "exec --user alice aw-local pgrep -x sshd\n"
        );
    }

    #[tokio::test]
    async fn apple_list_managed_containers_ignores_unlabeled_non_candidates() {
        let _apple_preflight_bypass = disable_apple_preflight_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        write_executable(
            &runtime_program,
            r#"#!/bin/sh
case "$1" in
  list)
    cat <<'JSON'
[{"status":"running","configuration":{"id":"unrelated"}}]
JSON
    ;;
esac
exit 0
"#,
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };
        let mut candidates = AppleContainerNameCandidates::default();
        candidates.insert_exact("aw-ubuntu");

        let containers = runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), Some(&candidates))
            .await
            .unwrap();

        assert!(containers.is_empty());
    }

    #[tokio::test]
    async fn apple_list_managed_containers_inspects_unlabeled_candidates() {
        let _apple_preflight_bypass = disable_apple_preflight_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        let user = "alice";
        write_executable(
            &runtime_program,
            format!(
                r#"#!/bin/sh
case "$1" in
  list)
    cat <<'JSON'
[{{"status":"running","configuration":{{"id":"aw-ubuntu"}}}}]
JSON
    ;;
  inspect)
    cat <<'JSON'
[{{"status":"running","configuration":{{"id":"aw-ubuntu","hostname":"aw-ubuntu","labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"2450","io.aw-gateway.target":"default","io.aw-gateway.container_id":"aw-ubuntu","io.aw-gateway.image":"ubuntu/dev"}}}}}}]
JSON
    ;;
esac
exit 0
"#,
                user = user,
            ),
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };
        let mut candidates = AppleContainerNameCandidates::default();
        candidates.insert_exact("aw-ubuntu");

        let containers = runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), Some(&candidates))
            .await
            .unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].name, "aw-ubuntu");
        assert_eq!(containers[0].image, "ubuntu/dev");
        assert!(containers[0].running);
    }

    #[tokio::test]
    async fn apple_list_managed_containers_fails_closed_when_candidate_inspect_lacks_labels() {
        let _apple_preflight_bypass = disable_apple_preflight_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        write_executable(
            &runtime_program,
            r#"#!/bin/sh
case "$1" in
  list)
    cat <<'JSON'
[{"status":"running","configuration":{"id":"aw-ubuntu"}}]
JSON
    ;;
  inspect)
    cat <<'JSON'
[{"status":"running","configuration":{"id":"aw-ubuntu","hostname":"aw-ubuntu"}}]
JSON
    ;;
esac
exit 0
"#,
        );
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::AppleContainer,
            program: runtime_program.display().to_string(),
            env: BTreeMap::new(),
        };
        let mut candidates = AppleContainerNameCandidates::default();
        candidates.insert_exact("aw-ubuntu");

        let err = runtime
            .list_managed_containers("alice", 2450, &RuntimeContext::empty(), Some(&candidates))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("omitted labels"), "{err:#}");
    }

    #[test]
    fn runtime_sensitive_env_values_do_not_passthrough_client_process_env() {
        let runtime = ContainerRuntime {
            kind: ContainerRuntimeType::Podman,
            program: "podman".into(),
            env: BTreeMap::new(),
        };
        let spec = ContainerExecSpec {
            stdin_tty: false,
            stdout_tty: false,
            user: "2450:100".into(),
            cwd: None,
            env: BTreeMap::from([
                ("AW_SAFE".into(), "secret".into()),
                ("PATH".into(), "/tmp/attacker".into()),
                ("HOME".into(), "/tmp/home".into()),
                ("XDG_CONFIG_HOME".into(), "/tmp/config".into()),
                ("XDG_DATA_HOME".into(), "/tmp/data".into()),
                ("LD_PRELOAD".into(), "/tmp/libhook.so".into()),
                ("DOCKER_HOST".into(), "tcp://example.test:2375".into()),
                ("DOCKER_TLS_VERIFY".into(), "0".into()),
                ("DOCKER_CERT_PATH".into(), "/tmp/certs".into()),
                ("CONTAINER_CONNECTION".into(), "attacker".into()),
                ("CONTAINER_SSHKEY".into(), "/tmp/key".into()),
                ("CONTAINER_DEBUG".into(), "1".into()),
                ("CONTAINER_DEFAULT_PLATFORM".into(), "linux/amd64".into()),
            ]),
            container_name: "ubuntu-dev".into(),
            command: vec!["true".into()],
        };

        let args = runtime.exec_args(&spec);
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "AW_SAFE")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "PATH=/tmp/attacker")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "HOME=/tmp/home")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "XDG_CONFIG_HOME=/tmp/config")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "XDG_DATA_HOME=/tmp/data")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "LD_PRELOAD=/tmp/libhook.so")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "DOCKER_HOST=tcp://example.test:2375")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "DOCKER_TLS_VERIFY=0")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "DOCKER_CERT_PATH=/tmp/certs")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "CONTAINER_CONNECTION=attacker")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "CONTAINER_SSHKEY=/tmp/key")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == "CONTAINER_DEBUG=1")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env"
                    && pair[1] == "CONTAINER_DEFAULT_PLATFORM=linux/amd64")
        );

        assert!(runtime_env_can_passthrough_client_process("AW_SAFE"));
        assert!(!runtime_env_can_passthrough_client_process("PATH"));
        assert!(!runtime_env_can_passthrough_client_process("HOME"));
        assert!(!runtime_env_can_passthrough_client_process(
            "XDG_CONFIG_HOME"
        ));
        assert!(!runtime_env_can_passthrough_client_process("XDG_DATA_HOME"));
        assert!(!runtime_env_can_passthrough_client_process("LD_PRELOAD"));
        assert!(!runtime_env_can_passthrough_client_process("DOCKER_HOST"));
        assert!(!runtime_env_can_passthrough_client_process(
            "DOCKER_TLS_VERIFY"
        ));
        assert!(!runtime_env_can_passthrough_client_process(
            "DOCKER_CERT_PATH"
        ));
        assert!(!runtime_env_can_passthrough_client_process(
            "CONTAINER_CONNECTION"
        ));
        assert!(!runtime_env_can_passthrough_client_process(
            "CONTAINER_SSHKEY"
        ));
        assert!(!runtime_env_can_passthrough_client_process(
            "CONTAINER_DEBUG"
        ));
        assert!(!runtime_env_can_passthrough_client_process(
            "CONTAINER_DEFAULT_PLATFORM"
        ));
    }

    #[tokio::test]
    async fn exec_with_timeout_reports_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        write_executable(
            &runtime_program,
            "#!/bin/sh\nif [ \"$1\" = exec ]; then sleep 5; fi\n",
        );
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
        write_executable(
            &runtime_program,
            "#!/bin/sh\nif [ \"$1\" = exec ]; then read line || true; echo done; fi\n",
        );
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
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[tokio::test]
    async fn exec_capture_with_timeout_bounds_pipe_drain_after_child_exit() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        write_executable(
            &runtime_program,
            "#!/bin/sh\nif [ \"$1\" = exec ]; then (sleep 5) & echo done; exit 0; fi\n",
        );
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
            command: vec!["pipe-holder".into()],
        };

        let started = std::time::Instant::now();
        let err = runtime
            .exec_capture_with_timeout(&spec, Some(Duration::from_secs(5)))
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("timed out draining captured output"),
            "{err:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn exec_capture_truncates_oversized_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_program = dir.path().join("fake-runtime");
        write_executable(
            &runtime_program,
            format!(
                "#!/bin/sh\nif [ \"$1\" = exec ]; then dd if=/dev/zero bs={} count=1 2>/dev/null; fi\n",
                MAX_CAPTURED_STREAM_BYTES + 1
            ),
        );
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
            command: vec!["large-output".into()],
        };

        let output = runtime.exec_capture(&spec).await.unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.len(), MAX_CAPTURED_STREAM_BYTES);
        assert!(output.stdout_truncated);
        assert!(output.stderr.is_empty());
        assert!(!output.stderr_truncated);
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
            workspace_container_path: PathBuf::from("/root"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            host_socket_exposures: Vec::new(),
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: true,
            published_ssh_host_port: None,
            extra_run_args: Vec::new(),
            command: vec!["aw-container-agent".into(), "run".into()],
        };
        let args = runtime.run_args(&spec);
        assert!(args.windows(2).any(|pair| pair == ["-p", "127.0.0.1::22"]));
    }

    #[test]
    fn run_args_publish_ssh_honors_explicit_loopback_host_port() {
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
            workspace_container_path: PathBuf::from("/root"),
            container_home: PathBuf::from("/root"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/root/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            host_socket_exposures: Vec::new(),
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: true,
            published_ssh_host_port: Some(40222),
            extra_run_args: Vec::new(),
            command: vec!["aw-container-agent".into(), "run".into()],
        };
        let args = runtime.run_args(&spec);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-p", "127.0.0.1:40222:22"])
        );
        assert!(!args.windows(2).any(|pair| pair == ["-p", "127.0.0.1::22"]));
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
            workspace_container_path: PathBuf::from("/home/user"),
            container_home: PathBuf::from("/home/user"),
            container_user: "root".into(),
            passwd_entry: None,
            state_dir_in_container: PathBuf::from("/home/user/.aw-gateway/containers/aw-local"),
            mounts: Vec::new(),
            host_socket_exposures: Vec::new(),
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            publish_ssh: false,
            published_ssh_host_port: None,
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
            parse_published_port_endpoint(b"127.0.0.1:49153\n"),
            Some(PublishedPort {
                host: "127.0.0.1".into(),
                port: 49153,
            })
        );
        assert_eq!(
            parse_published_port_endpoint(b"0.0.0.0:2222\n"),
            Some(PublishedPort {
                host: "0.0.0.0".into(),
                port: 2222,
            })
        );
        assert_eq!(
            parse_published_port_endpoint(b"[::1]:49154\n"),
            Some(PublishedPort {
                host: "::1".into(),
                port: 49154,
            })
        );
        assert_eq!(
            parse_published_port(b"127.0.0.1:49153\n[::1]:49154\n"),
            Some(49153)
        );
        assert_eq!(parse_published_port(b""), None);
        assert_eq!(parse_published_port(b"not-a-port\n"), None);
    }
}
