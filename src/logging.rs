use crate::config::{ContainerAgentFile, GatewayConfig, LoggingConfig, WorkspaceConfig};
use crate::paths::{self, UserContext};
use crate::template::{self, Vars};
use anyhow::Context;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

pub struct LoggingGuard;

pub fn init_gateway(
    config_path: Option<&Path>,
    level: Option<&str>,
    protocol_mode: bool,
) -> anyhow::Result<LoggingGuard> {
    let (config, render_error) = config_path
        .and_then(|path| GatewayConfig::load(path).ok())
        .map(|cfg| {
            let mut logging = cfg.logging;
            match render_gateway_logging_directory(&mut logging, &cfg.workspace) {
                Ok(()) => (logging, None),
                Err(err) => (fallback_config(!protocol_mode), Some(err)),
            }
        })
        .unwrap_or_else(|| (fallback_config(!protocol_mode), None));
    let guard = init(
        config,
        level,
        "AW_GATEWAY_LOG_LEVEL",
        "aw-gateway",
        protocol_mode,
    )?;
    if let Some(err) = render_error
        && !protocol_mode
    {
        tracing::error!(error = %err, "failed to render gateway logging directory; file logging disabled");
    }
    Ok(guard)
}

pub fn init_agent(config_path: Option<&Path>, level: Option<&str>) -> anyhow::Result<LoggingGuard> {
    let (config, render_error) = config_path
        .and_then(|path| ContainerAgentFile::load(path).ok())
        .map(|cfg| {
            let mut logging = cfg.logging;
            match render_agent_logging_directory(&mut logging) {
                Ok(()) => (logging, None),
                Err(err) => (fallback_config(true), Some(err)),
            }
        })
        .unwrap_or_else(|| (fallback_config(true), None));
    let guard = init(
        config,
        level,
        "AW_CONTAINER_AGENT_LOG_LEVEL",
        "aw-container-agent",
        false,
    )?;
    if let Some(err) = render_error {
        tracing::error!(error = %err, "failed to render container-agent logging directory; file logging disabled");
    }
    Ok(guard)
}

fn init(
    mut config: LoggingConfig,
    level: Option<&str>,
    env_name: &str,
    file_prefix: &str,
    protocol_mode: bool,
) -> anyhow::Result<LoggingGuard> {
    let directive = level
        .map(str::to_owned)
        .or_else(|| std::env::var(env_name).ok())
        .unwrap_or_else(|| config.level.clone());
    let filter = EnvFilter::try_new(directive).context("invalid log filter")?;
    if protocol_mode {
        config.console = false;
    }

    if let Some(directory) = config.directory {
        match SizeRotatingMakeWriter::new(
            PathBuf::from(directory),
            file_prefix,
            config.max_bytes.unwrap_or(100 * 1024 * 1024),
            config.max_files.unwrap_or(5),
        ) {
            Ok(writer) => {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(writer)
                    .json()
                    .try_init();
                return Ok(LoggingGuard);
            }
            Err(err) => {
                eprintln!("failed to initialize file logging for {file_prefix}: {err:#}");
            }
        }
    }

    if config.console || protocol_mode {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .try_init();
    }
    Ok(LoggingGuard)
}

fn fallback_config(console: bool) -> LoggingConfig {
    LoggingConfig {
        console,
        ..LoggingConfig::default()
    }
}

fn render_gateway_logging_directory(
    config: &mut LoggingConfig,
    workspace_cfg: &WorkspaceConfig,
) -> anyhow::Result<()> {
    let Some(directory) = config.directory.as_deref() else {
        return Ok(());
    };
    let user = UserContext::current()?;
    let workspace = paths::resolve_workspace(&user.home, &workspace_cfg.path);
    let state = workspace.join(&workspace_cfg.state_dir);
    let mut vars = Vars::new();
    vars.insert("user".into(), user.user);
    vars.insert("uid".into(), user.uid.to_string());
    vars.insert("gid".into(), user.gid.to_string());
    vars.insert("home".into(), user.home.display().to_string());
    vars.insert("workspace".into(), workspace.display().to_string());
    vars.insert("state".into(), state.display().to_string());
    vars.insert("state_dir".into(), state.display().to_string());
    config.directory = Some(template::render(directory, &vars)?);
    Ok(())
}

fn render_agent_logging_directory(config: &mut LoggingConfig) -> anyhow::Result<()> {
    let Some(directory) = config.directory.as_deref() else {
        return Ok(());
    };
    let state_dir = std::env::var("AW_CONTAINER_STATE_DIR")
        .unwrap_or_else(|_| paths::DEFAULT_AGENT_STATE_DIR.into());
    let mut vars = Vars::new();
    vars.insert("container_state_dir".into(), state_dir);
    config.directory = Some(template::render(directory, &vars)?);
    Ok(())
}

#[derive(Clone)]
struct SizeRotatingMakeWriter {
    inner: Arc<Mutex<SizeRotatingFile>>,
}

impl SizeRotatingMakeWriter {
    fn new(
        directory: PathBuf,
        file_prefix: &str,
        max_bytes: u64,
        max_files: usize,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(SizeRotatingFile::new(
                directory,
                file_prefix,
                max_bytes,
                max_files,
            )?)),
        })
    }
}

impl<'a> MakeWriter<'a> for SizeRotatingMakeWriter {
    type Writer = SizeRotatingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SizeRotatingWriter {
            inner: self.inner.clone(),
        }
    }
}

struct SizeRotatingWriter {
    inner: Arc<Mutex<SizeRotatingFile>>,
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .expect("log writer mutex poisoned")
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .expect("log writer mutex poisoned")
            .flush()
    }
}

struct SizeRotatingFile {
    directory: PathBuf,
    file_name: String,
    max_bytes: u64,
    max_files: usize,
    file: File,
    bytes_written: u64,
}

impl SizeRotatingFile {
    fn new(
        directory: PathBuf,
        file_prefix: &str,
        max_bytes: u64,
        max_files: usize,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create log directory {}", directory.display()))?;
        let file_name = format!("{file_prefix}.log");
        let path = directory.join(&file_name);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            directory,
            file_name,
            max_bytes: max_bytes.max(1),
            max_files,
            file,
            bytes_written,
        })
    }

    fn path_for_generation(&self, generation: usize) -> PathBuf {
        if generation == 0 {
            self.directory.join(&self.file_name)
        } else {
            self.directory
                .join(format!("{}.{}", self.file_name, generation))
        }
    }

    fn rotate_if_needed(&mut self, incoming: usize) -> io::Result<()> {
        if self.max_files == 0 || self.bytes_written + incoming as u64 <= self.max_bytes {
            return Ok(());
        }
        self.file.flush()?;
        for generation in (1..=self.max_files).rev() {
            let path = self.path_for_generation(generation);
            if generation == self.max_files {
                let _ = std::fs::remove_file(path);
            } else {
                let next = self.path_for_generation(generation + 1);
                if path.exists() {
                    std::fs::rename(path, next)?;
                }
            }
        }
        let current = self.path_for_generation(0);
        let first = self.path_for_generation(1);
        if current.exists() {
            std::fs::rename(&current, first)?;
        }
        self.file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(current)?;
        self.bytes_written = 0;
        Ok(())
    }
}

impl Write for SizeRotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        let written = self.file.write(buf)?;
        self.bytes_written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn size_rotating_writer_rotates_by_bytes() {
        let dir = tempdir().unwrap();
        let mut writer = SizeRotatingFile::new(dir.path().to_path_buf(), "test", 8, 2).unwrap();
        writer.write_all(b"12345678").unwrap();
        writer.write_all(b"abcdef").unwrap();
        writer.flush().unwrap();
        assert!(dir.path().join("test.log").exists());
        assert!(dir.path().join("test.log.1").exists());
    }
}
