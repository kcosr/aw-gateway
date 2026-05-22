use super::Runtime;
use super::fileutil::{random_hex_token, write_private_file};
use super::model::{LocalListenerStatus, SessionMarker, SessionStatus};
use crate::config::LocalSshConfig;
use crate::paths;
use anyhow::Context;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

impl Runtime {
    pub(super) async fn acquire_lifecycle_lock(&self) -> anyhow::Result<LifecycleLock> {
        let lock_dir = self.user.state_dir().join("locks");
        let lock_path = lock_dir.join(format!("{}.lock", self.container_name));
        tokio::task::spawn_blocking(move || {
            paths::ensure_private_dir(&lock_dir)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)
                .with_context(|| format!("open lifecycle lock {}", lock_path.display()))?;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("lock {}", lock_path.display()));
            }
            Ok(LifecycleLock { file, lock_path })
        })
        .await
        .context("join lifecycle lock task")?
    }

    pub(super) fn create_session_marker(&self, kind: &str) -> anyhow::Result<SessionGuard> {
        self.create_session_marker_with_launch(kind, None)
    }

    pub(super) fn create_launch_session_marker(&self, kind: &str) -> anyhow::Result<SessionGuard> {
        self.create_session_marker_with_launch(kind, self.launch_name.as_deref())
    }

    fn create_session_marker_with_launch(
        &self,
        kind: &str,
        launch: Option<&str>,
    ) -> anyhow::Result<SessionGuard> {
        let dir = self.session_marker_dir();
        paths::ensure_private_dir(&dir)?;
        let id = generate_session_id_value()?;
        let path = dir.join(format!("{id}.json"));
        let marker = SessionMarker {
            id,
            kind: kind.to_string(),
            gateway_pid: std::process::id(),
            gateway_start_time: process_start_time(std::process::id()).unwrap_or_default(),
            container: self.container_name.clone(),
            target: self.target_name.clone(),
            launch: launch.map(str::to_string),
            created_at_ms: unix_time_ms()?,
        };
        write_private_file(&path, &serde_json::to_vec_pretty(&marker)?, 0o600)
            .with_context(|| format!("write session marker {}", path.display()))?;
        Ok(SessionGuard { path })
    }

    pub(super) fn active_session_markers(&self) -> anyhow::Result<Vec<SessionStatus>> {
        let dir = self.session_marker_dir();
        let mut sessions = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
            Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match read_session_marker(&path) {
                Ok(marker) if session_marker_is_active(&marker) => {
                    sessions.push(SessionStatus::from(marker));
                }
                Ok(_) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "failed to read session marker"
                    );
                }
            }
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    pub(super) fn session_marker_dir(&self) -> PathBuf {
        self.container_state_dir.join("sessions")
    }

    fn local_listener_status_path(&self) -> PathBuf {
        self.container_state_dir.join("local-ssh-listener.json")
    }

    pub(super) fn active_local_listener_status(
        &self,
    ) -> anyhow::Result<Option<LocalListenerStatus>> {
        let path = self.local_listener_status_path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        let status: LocalListenerStatus =
            serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
        if local_listener_is_active(&status) {
            Ok(Some(status))
        } else {
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }

    pub(super) fn write_local_listener_status(
        &self,
        host: &str,
        port: u16,
    ) -> anyhow::Result<LocalListenerGuard> {
        let path = self.local_listener_status_path();
        let status = LocalListenerStatus {
            gateway_pid: std::process::id(),
            gateway_start_time: process_start_time(std::process::id()).unwrap_or_default(),
            host: host.to_string(),
            port,
            created_at_ms: unix_time_ms()?,
        };
        write_private_file(&path, &serde_json::to_vec_pretty(&status)?, 0o600)
            .with_context(|| format!("write local listener status {}", path.display()))?;
        Ok(LocalListenerGuard { path })
    }

    pub(super) fn local_ssh_port(&self, configured: &LocalSshConfig) -> anyhow::Result<u16> {
        if let Some(port) = configured.port {
            return Ok(port);
        }
        let path = self.local_listener_status_path();
        let raw = std::fs::read(&path).with_context(|| {
            format!(
                "local_ssh.port is not configured and no active listener status exists at {}",
                path.display()
            )
        })?;
        let status: LocalListenerStatus =
            serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
        if status.host != configured.host || !local_listener_is_active(&status) {
            let _ = std::fs::remove_file(&path);
            anyhow::bail!(
                "local_ssh.port is not configured and listener status is stale; start the local listener first"
            );
        }
        Ok(status.port)
    }
}

#[derive(Debug)]
pub(super) struct LifecycleLock {
    file: std::fs::File,
    lock_path: PathBuf,
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        let rc = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        if rc != 0 {
            tracing::warn!(
                path = %self.lock_path.display(),
                error = %std::io::Error::last_os_error(),
                "failed to unlock lifecycle lock"
            );
        }
    }
}

#[derive(Debug)]
pub(super) struct SessionGuard {
    path: PathBuf,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "failed to remove session marker"
            );
        }
    }
}

#[derive(Debug)]
pub(super) struct LocalListenerGuard {
    path: PathBuf,
}

impl Drop for LocalListenerGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "failed to remove local SSH listener status"
            );
        }
    }
}

pub(super) fn validate_session_id(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        anyhow::bail!("invalid session id {value:?}");
    }
    Ok(())
}

fn unix_time_ms() -> anyhow::Result<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time is before UNIX_EPOCH")?
        .as_millis())
}

pub(super) fn generate_session_id_value() -> anyhow::Result<String> {
    let token = random_hex_token()?;
    Ok(token[..12].to_string())
}

#[cfg(target_os = "linux")]
pub(super) fn process_start_time(pid: u32) -> anyhow::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("read /proc/{pid}/stat"))?;
    parse_process_start_time(&stat)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn process_start_time(pid: u32) -> anyhow::Result<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .with_context(|| format!("query process start time for pid {pid}"))?;
    if !output.status.success() {
        anyhow::bail!("process {pid} is not active");
    }
    let start_time = String::from_utf8(output.stdout)
        .context("process start time output is not valid UTF-8")?
        .trim()
        .to_string();
    if start_time.is_empty() {
        anyhow::bail!("process {pid} has no start time");
    }
    Ok(start_time)
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_process_start_time(stat: &str) -> anyhow::Result<String> {
    let Some((_, rest)) = stat.rsplit_once(") ") else {
        anyhow::bail!("malformed proc stat");
    };
    let start_time = rest
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow::anyhow!("proc stat missing starttime"))?;
    Ok(start_time.to_string())
}

fn read_session_marker(path: &Path) -> anyhow::Result<SessionMarker> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub(super) fn session_marker_is_active(marker: &SessionMarker) -> bool {
    process_start_time(marker.gateway_pid).is_ok_and(|start_time| {
        !marker.gateway_start_time.is_empty() && start_time == marker.gateway_start_time
    })
}

pub(super) fn local_listener_is_active(status: &LocalListenerStatus) -> bool {
    if status.gateway_pid == std::process::id() {
        return true;
    }
    process_start_time(status.gateway_pid).is_ok_and(|start_time| {
        !status.gateway_start_time.is_empty() && start_time == status.gateway_start_time
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_ids_are_short_lower_hex() {
        let id = generate_session_id_value().unwrap();

        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(id.chars().all(|ch| !ch.is_ascii_uppercase()));
    }

    #[test]
    fn explicit_session_id_validation_keeps_existing_shape() {
        validate_session_id("x9k2p.custom_id").unwrap();
    }
}
