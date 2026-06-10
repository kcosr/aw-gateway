use crate::config::{ContainerAgentFile, GatewayConfig, LoggingConfig, WorkspaceConfig};
use crate::context::{RuntimeContext, validate_supplied_context};
use crate::fileutil::{ensure_private_dir, set_mode};
use crate::paths::{self, UserContext};
use crate::rotating_log::{RotationState, RotationStep};
use crate::template::{self, Vars};
use anyhow::Context;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

pub struct LoggingGuard;

pub fn init_gateway(
    config_path: Option<&Path>,
    level: Option<&str>,
    protocol_mode: bool,
    context: &RuntimeContext,
) -> anyhow::Result<LoggingGuard> {
    let config = match config_path {
        Some(path) => {
            let cfg = GatewayConfig::load(path)?;
            validate_supplied_context(&cfg.context_vars, context)?;
            let mut logging = cfg.logging.clone();
            cfg.effective_workspace_defaults()
                .and_then(|workspace| {
                    render_gateway_logging_directory(&mut logging, &workspace, context)
                })
                .context("render gateway logging directory")?;
            logging
        }
        None => fallback_config(!protocol_mode),
    };
    init(
        config,
        level,
        "AW_GATEWAY_LOG_LEVEL",
        "aw-gateway",
        protocol_mode,
    )
}

pub fn init_agent(config_path: Option<&Path>, level: Option<&str>) -> anyhow::Result<LoggingGuard> {
    let config = match config_path {
        Some(path) => {
            let cfg = ContainerAgentFile::load(path)?;
            let mut logging = cfg.logging;
            render_agent_logging_directory(&mut logging)
                .context("render container-agent logging directory")?;
            logging
        }
        None => fallback_config(true),
    };
    init(
        config,
        level,
        "AW_CONTAINER_AGENT_LOG_LEVEL",
        "aw-container-agent",
        false,
    )
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
        let writer = SizeRotatingMakeWriter::new(
            PathBuf::from(directory),
            file_prefix,
            config.max_bytes.unwrap_or(100 * 1024 * 1024),
            config.max_files.unwrap_or(5),
        )
        .with_context(|| format!("initialize file logging for {file_prefix}"))?;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .json()
            .try_init();
        return Ok(LoggingGuard);
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
    context: &RuntimeContext,
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
    context.insert_template_vars(&mut vars);
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

impl SizeRotatingWriter {
    fn lock_inner(&self) -> MutexGuard<'_, SizeRotatingFile> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock_inner().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock_inner().flush()
    }
}

struct SizeRotatingFile {
    rotation: RotationState,
    file: File,
}

impl SizeRotatingFile {
    fn new(
        directory: PathBuf,
        file_prefix: &str,
        max_bytes: u64,
        max_files: usize,
    ) -> anyhow::Result<Self> {
        ensure_private_dir(&directory)
            .with_context(|| format!("create log directory {}", directory.display()))?;
        let file_name = format!("{file_prefix}.log");
        let path = directory.join(&file_name);
        let file = open_log_file(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            rotation: RotationState::new(path, max_bytes, max_files, bytes_written),
            file,
        })
    }

    fn rotate_if_needed(&mut self, incoming: usize) -> io::Result<()> {
        if !self.rotation.should_rotate(incoming) {
            return Ok(());
        }
        self.file.flush()?;
        let plan = self.rotation.rotation_plan();
        for step in plan.steps() {
            match step {
                RotationStep::Remove { path } => {
                    let _ = std::fs::remove_file(path);
                }
                RotationStep::Rename { from, to } => {
                    rename_if_exists(from, to)?;
                }
            }
        }
        // Reopen the active path in append mode (not truncate): after the rename
        // above the path is gone, so create+append yields a fresh file, and any
        // file that unexpectedly survived the rename is appended to rather than
        // destroyed. Reset the byte counter whether or not the reopen succeeds,
        // so a failed reopen cannot leave the writer attached to the
        // rotated-away inode while still counting toward the next rotation --
        // which would otherwise re-rotate on every subsequent write.
        match open_log_file(plan.active_path()) {
            Ok(file) => {
                self.file = file;
                self.rotation.reset_after_rotation();
                Ok(())
            }
            Err(err) => {
                self.rotation.reset_after_rotation();
                Err(err)
            }
        }
    }
}

// Rename `from` to `to`, treating a missing source as success. Avoids the
// exists()-then-rename TOCTOU and tolerates a generation file that another
// rotation already moved.
fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn open_log_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_mode(path, 0o600).map_err(io::Error::other)?;
    Ok(file)
}

impl Write for SizeRotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        let written = self.file.write(buf)?;
        self.rotation.record_write(written);
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

    #[test]
    fn rotation_preserves_previous_generation_and_continues_in_fresh_active() {
        let dir = tempdir().unwrap();
        let mut writer = SizeRotatingFile::new(dir.path().to_path_buf(), "test", 4, 3).unwrap();
        writer.write_all(b"AAAA").unwrap();
        writer.write_all(b"BBBB").unwrap();
        writer.flush().unwrap();
        // The previous generation must survive rotation (no truncation / loss),
        // and writes after rotation land in a fresh active file.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.log.1")).unwrap(),
            "AAAA"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.log")).unwrap(),
            "BBBB"
        );
    }

    #[test]
    fn rotation_tolerates_missing_generation_files() {
        let dir = tempdir().unwrap();
        // max_files = 3 so the plan renames generations 1 and 2 that do not yet
        // exist; rename_if_exists must treat the missing sources as success.
        let mut writer = SizeRotatingFile::new(dir.path().to_path_buf(), "test", 4, 3).unwrap();
        writer.write_all(b"AAAA").unwrap();
        writer.write_all(b"BBBB").unwrap();
        writer.flush().unwrap();
        assert!(dir.path().join("test.log").exists());
        assert!(dir.path().join("test.log.1").exists());
    }

    #[test]
    fn size_rotating_writer_uses_private_directory_and_file_modes() {
        let dir = tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        let _writer = SizeRotatingFile::new(log_dir.clone(), "test", 8, 2).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&log_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = std::fs::metadata(log_dir.join("test.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn size_rotating_writer_recovers_from_poisoned_mutex() {
        let dir = tempdir().unwrap();
        let make_writer =
            SizeRotatingMakeWriter::new(dir.path().to_path_buf(), "test", 1024, 2).unwrap();
        let inner = make_writer.inner.clone();
        let _ = std::panic::catch_unwind(|| {
            let _guard = inner.lock().unwrap();
            panic!("poison log writer");
        });
        assert!(make_writer.inner.lock().is_err());

        let mut writer = make_writer.make_writer();
        writer.write_all(b"after poison\n").unwrap();
        writer.flush().unwrap();

        let text = std::fs::read_to_string(dir.path().join("test.log")).unwrap();
        assert!(text.contains("after poison"));
    }

    #[test]
    fn configured_file_logging_errors_instead_of_falling_back() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "block").unwrap();
        let config = LoggingConfig {
            directory: Some(blocker.join("logs").display().to_string()),
            ..LoggingConfig::default()
        };

        let err = match init(config, None, "AW_TEST_LOG_LEVEL", "test", false) {
            Ok(_) => panic!("expected configured file logging to fail"),
            Err(err) => err,
        };
        let message = format!("{err:#}");
        assert!(
            message.contains("initialize file logging for test"),
            "{message}"
        );
    }

    #[test]
    fn gateway_logging_init_does_not_require_all_context_keys() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[logging]
console = true

[runtime]
type = "podman"
program = "/bin/true"

[context_vars.tenant]
required = true

[target_defaults.workspace]
path = "{workspace}"
state_dir = ".aw-gateway"
cleanup = "never"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev-{{context.tenant}}"
stop_when_idle = false
"#,
                workspace = dir.path().join("workspace").display()
            ),
        )
        .unwrap();

        init_gateway(Some(&config), None, false, &RuntimeContext::empty()).unwrap();
    }
}
