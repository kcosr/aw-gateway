use crate::config::{ContainerRuntimeType, RuntimeConfig};
use crate::template::{self, Vars};
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;

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
}

impl ContainerPtySession {
    pub async fn terminate(&self) -> anyhow::Result<()> {
        let killer = self.killer.clone();
        tokio::task::spawn_blocking(move || {
            killer
                .lock()
                .map_err(|_| anyhow::anyhow!("pty child killer lock poisoned"))?
                .kill()
                .map_err(anyhow::Error::from)
        })
        .await
        .context("join pty child terminate task")?
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

    pub async fn exec_discard(&self, spec: &ContainerExecSpec) -> anyhow::Result<i32> {
        let mut command = self.command();
        command
            .arg("exec")
            .args(self.exec_args(spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let status = command.status().await.with_context(|| {
            format!("run {} exec {}", self.runtime_label(), spec.container_name)
        })?;
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

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system.openpty(pty_size(size))?;
        let mut command = portable_pty::CommandBuilder::new(&self.program);
        command.arg("exec");
        command.args(self.exec_args(&pty_spec));
        for (key, value) in &self.env {
            command.env(key, value);
        }

        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let mut child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let killer = Arc::new(Mutex::new(child.clone_killer()));

        let (output_tx, output) = mpsc::channel(PTY_OUTPUT_BUFFER);
        let (input, mut input_rx) = mpsc::channel::<Vec<u8>>(PTY_INPUT_BUFFER);
        let (resize, mut resize_rx) = mpsc::channel::<ContainerPtySize>(PTY_RESIZE_BUFFER);

        std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
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
        });

        std::thread::spawn(move || {
            while let Some(bytes) = input_rx.blocking_recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });

        std::thread::spawn(move || {
            while let Some(size) = resize_rx.blocking_recv() {
                if let Err(err) = pair.master.resize(pty_size(size)) {
                    tracing::warn!(error = %err, "pty resize failed");
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
