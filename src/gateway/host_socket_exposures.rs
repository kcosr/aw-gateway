use super::{Runtime, UNIX_SOCKET_PATH_MAX_BYTES};
use crate::config::{ContainerRuntimeType, HostSocketExposureConfig};
use crate::runtime::{
    ContainerMountSpec, HostSocketExposureRealization, HostSocketExposureSpec, UnixSourceIdentity,
};
use crate::template::Vars;
use anyhow::Context;
use futures_util::future::join_all;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tokio::time::Duration;

pub(super) const HOST_SOCKET_EXPOSURE_MANIFEST_LABEL: &str =
    "io.aw-gateway.host-socket-exposures.v1";
const LINUX_UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;

#[derive(Debug, Clone)]
pub(super) struct PreparedContainerInputs {
    pub(super) mounts: Vec<ContainerMountSpec>,
    pub(super) host_socket_exposures: Vec<HostSocketExposureSpec>,
    pub(super) exposure_manifest: Option<String>,
}

impl Runtime {
    pub(super) async fn prepare_container_inputs(&self) -> anyhow::Result<PreparedContainerInputs> {
        self.container_runtime
            .validate_host_socket_exposure_support(!self.target.host_socket_exposures.is_empty())
            .await?;
        let mounts = self.container_mounts_async().await?;
        super::container_spec::warn_about_unsafe_container_mounts_async(&mounts).await?;
        let configured = self.target.host_socket_exposures.clone();
        let vars = self.vars(None);
        let runtime_kind = self.container_runtime.kind();
        let workspace_host = self.paths.workspace.clone();
        let workspace_container = self.paths.workspace_container_path.clone();
        let container_home = self.identity.container_home.clone();
        let (control_host, control_container) = self
            .requires_control_socket_dir()
            .then(|| {
                (
                    self.paths.control_sockets.host_dir.clone(),
                    self.paths.control_sockets.container_dir.clone(),
                )
            })
            .unzip();
        tokio::task::spawn_blocking(move || {
            prepare_host_socket_exposures(
                configured,
                &vars,
                runtime_kind,
                mounts,
                &workspace_host,
                &workspace_container,
                &container_home,
                control_host.as_deref(),
                control_container.as_deref(),
            )
        })
        .await
        .context("join host socket exposure preparation task")?
    }

    pub(super) fn validate_exposure_manifest(
        &self,
        inspect: &crate::runtime::ContainerInspect,
        prepared: &PreparedContainerInputs,
    ) -> anyhow::Result<()> {
        validate_exposure_manifest_values(
            &self.identity.container_name,
            prepared.exposure_manifest.as_deref(),
            inspect
                .config
                .labels
                .get(HOST_SOCKET_EXPOSURE_MANIFEST_LABEL)
                .map(String::as_str),
        )
    }

    pub(super) async fn validate_container_exposure_endpoints(
        &self,
        prepared: &PreparedContainerInputs,
    ) -> anyhow::Result<()> {
        const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
        const READY_RETRY_INTERVAL: Duration = Duration::from_millis(100);
        self.validate_container_exposure_endpoints_with_timing(
            prepared,
            CHECK_TIMEOUT,
            READY_RETRY_INTERVAL,
        )
        .await
    }

    pub(super) async fn validate_container_exposure_endpoints_with_timing(
        &self,
        prepared: &PreparedContainerInputs,
        check_timeout: Duration,
        retry_interval: Duration,
    ) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + check_timeout;
        let results = join_all(prepared.host_socket_exposures.iter().map(|exposure| {
            self.validate_container_exposure_endpoint_until(exposure, deadline, retry_interval)
        }))
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    async fn validate_container_exposure_endpoint_until(
        &self,
        exposure: &HostSocketExposureSpec,
        deadline: tokio::time::Instant,
        retry_interval: Duration,
    ) -> anyhow::Result<()> {
        loop {
            let mut failed = None;
            for (flag, description) in [("-S", "a Unix socket"), ("-w", "accessible")] {
                let spec = crate::runtime::ContainerExecSpec {
                    stdin_tty: false,
                    stdout_tty: false,
                    user: exposure.readiness_user.clone(),
                    cwd: None,
                    env: BTreeMap::new(),
                    container_name: self.identity.container_name.clone(),
                    command: vec![
                        "/bin/test".into(),
                        flag.into(),
                        exposure.container_path.display().to_string(),
                    ],
                };
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let exit = self
                    .container_runtime
                    .exec_discard_with_timeout(&spec, Some(remaining))
                    .await
                    .with_context(|| {
                        format!(
                            "check host socket exposure {:?} container endpoint as {:?}",
                            exposure.name, exposure.readiness_user
                        )
                    })?;
                if exit != 0 {
                    failed = Some(description);
                    break;
                }
            }
            let Some(description) = failed else {
                return Ok(());
            };
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(retry_interval.min(remaining)).await;
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "host socket exposure {:?} container endpoint failed the {description} check as configured readiness user {:?}",
                    exposure.name,
                    exposure.readiness_user
                );
            }
        }
    }

    pub(super) async fn current_host_socket_exposure_statuses(
        &self,
        inspect: Option<&crate::runtime::ContainerInspect>,
        prepared: Option<&PreparedContainerInputs>,
    ) -> Vec<super::HostSocketExposureStatus> {
        if self.target.host_socket_exposures.is_empty() {
            return removed_config_manifest_status(inspect, self.container_runtime.kind());
        }
        if let Some(prepared) = prepared {
            return self
                .current_prepared_host_socket_exposure_statuses(inspect, prepared)
                .await;
        }
        match self.prepare_container_inputs().await {
            Ok(prepared) => {
                self.current_prepared_host_socket_exposure_statuses(inspect, &prepared)
                    .await
            }
            Err(_) => {
                let realization = match self.container_runtime.kind() {
                    ContainerRuntimeType::AppleContainer => "path_reconnect",
                    _ => "pinned_inode",
                };
                self.target
                    .host_socket_exposures
                    .keys()
                    .map(|name| super::HostSocketExposureStatus {
                        name: name.clone(),
                        realization: realization.into(),
                        ready: false,
                        failure_category: Some("source_unavailable".into()),
                    })
                    .collect()
            }
        }
    }

    async fn current_prepared_host_socket_exposure_statuses(
        &self,
        inspect: Option<&crate::runtime::ContainerInspect>,
        prepared: &PreparedContainerInputs,
    ) -> Vec<super::HostSocketExposureStatus> {
        self.current_prepared_host_socket_exposure_statuses_impl(
            inspect,
            prepared,
            Duration::from_secs(5),
            Duration::from_millis(100),
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn current_prepared_host_socket_exposure_statuses_with_timing(
        &self,
        inspect: Option<&crate::runtime::ContainerInspect>,
        prepared: &PreparedContainerInputs,
        check_timeout: Duration,
        retry_interval: Duration,
    ) -> Vec<super::HostSocketExposureStatus> {
        self.current_prepared_host_socket_exposure_statuses_impl(
            inspect,
            prepared,
            check_timeout,
            retry_interval,
        )
        .await
    }

    async fn current_prepared_host_socket_exposure_statuses_impl(
        &self,
        inspect: Option<&crate::runtime::ContainerInspect>,
        prepared: &PreparedContainerInputs,
        check_timeout: Duration,
        retry_interval: Duration,
    ) -> Vec<super::HostSocketExposureStatus> {
        let manifest_valid = inspect
            .map(|inspect| self.validate_exposure_manifest(inspect, prepared).is_ok())
            .unwrap_or(false);
        let running = inspect.is_some_and(|inspect| inspect.state.running);
        let common_failure = if inspect.is_none() {
            Some("container_absent")
        } else if !manifest_valid {
            Some("recreate_required")
        } else if !running {
            Some("container_stopped")
        } else {
            None
        };
        let mut statuses = Vec::with_capacity(prepared.host_socket_exposures.len());
        if let Some(category) = common_failure {
            return prepared
                .host_socket_exposures
                .iter()
                .map(|exposure| super::HostSocketExposureStatus {
                    name: exposure.name.clone(),
                    realization: exposure.realization.as_str().into(),
                    ready: false,
                    failure_category: Some(category.into()),
                })
                .collect();
        }
        let deadline = tokio::time::Instant::now() + check_timeout;
        let results = join_all(prepared.host_socket_exposures.iter().map(|exposure| {
            self.validate_container_exposure_endpoint_until(exposure, deadline, retry_interval)
        }))
        .await;
        for (exposure, result) in prepared.host_socket_exposures.iter().zip(results) {
            let failure_category = result.is_err().then(|| "guest_endpoint_unavailable".into());
            statuses.push(super::HostSocketExposureStatus {
                name: exposure.name.clone(),
                realization: exposure.realization.as_str().into(),
                ready: failure_category.is_none(),
                failure_category,
            });
        }
        statuses
    }
}

fn removed_config_manifest_status(
    inspect: Option<&crate::runtime::ContainerInspect>,
    runtime_kind: ContainerRuntimeType,
) -> Vec<super::HostSocketExposureStatus> {
    if inspect.is_some_and(|inspect| {
        inspect
            .config
            .labels
            .contains_key(HOST_SOCKET_EXPOSURE_MANIFEST_LABEL)
    }) {
        vec![super::HostSocketExposureStatus {
            name: "container_manifest".into(),
            realization: match runtime_kind {
                ContainerRuntimeType::AppleContainer => "path_reconnect",
                ContainerRuntimeType::Docker
                | ContainerRuntimeType::Podman
                | ContainerRuntimeType::Colima => "pinned_inode",
            }
            .into(),
            ready: false,
            failure_category: Some("recreate_required".into()),
        }]
    } else {
        Vec::new()
    }
}

fn validate_exposure_manifest_values(
    container_name: &str,
    expected: Option<&str>,
    actual: Option<&str>,
) -> anyhow::Result<()> {
    match (expected, actual) {
        (None, None) => Ok(()),
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (Some(_), None) => anyhow::bail!(
            "container {container_name:?} is missing the host socket exposure manifest; remove and recreate the container"
        ),
        (None, Some(_)) => anyhow::bail!(
            "container {container_name:?} has host socket exposures but the target does not; remove and recreate the container"
        ),
        (Some(_), Some(_)) => anyhow::bail!(
            "container {container_name:?} host socket exposure manifest does not match the effective target; remove and recreate the container"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_host_socket_exposures(
    configured: BTreeMap<String, HostSocketExposureConfig>,
    vars: &Vars,
    runtime_kind: ContainerRuntimeType,
    mut mounts: Vec<ContainerMountSpec>,
    workspace_host: &Path,
    workspace_container: &Path,
    container_home: &Path,
    control_host: Option<&Path>,
    control_container: Option<&Path>,
) -> anyhow::Result<PreparedContainerInputs> {
    if configured.is_empty() {
        return Ok(PreparedContainerInputs {
            mounts,
            host_socket_exposures: Vec::new(),
            exposure_manifest: None,
        });
    }
    let workspace_host = workspace_host.canonicalize().with_context(|| {
        format!(
            "canonicalize workspace host path {}",
            workspace_host.display()
        )
    })?;
    let control_host = control_host
        .map(|path| {
            path.canonicalize().with_context(|| {
                format!(
                    "canonicalize control socket host directory {}",
                    path.display()
                )
            })
        })
        .transpose()?;
    for (index, mount) in mounts.iter_mut().enumerate() {
        mount.source = mount.source.canonicalize().with_context(|| {
            format!(
                "canonicalize container mount source #{index} {}",
                mount.source.display()
            )
        })?;
        validate_normalized_container_path(
            &format!("container mount target #{index}"),
            &mount.target,
        )?;
    }
    for (label, path) in [
        ("workspace container path", workspace_container),
        ("container home", container_home),
    ] {
        validate_normalized_container_path(label, path)?;
    }
    if let Some(control_container) = control_container {
        validate_normalized_container_path(
            "control socket container directory",
            control_container,
        )?;
    }
    let realization = match runtime_kind {
        ContainerRuntimeType::AppleContainer => HostSocketExposureRealization::PathReconnect,
        ContainerRuntimeType::Podman | ContainerRuntimeType::Docker => {
            HostSocketExposureRealization::PinnedInode
        }
        ContainerRuntimeType::Colima => {
            anyhow::bail!("host_socket_exposures are not supported with the colima runtime")
        }
    };

    let mut host_paths = BTreeSet::<PathBuf>::new();
    let mut container_paths = BTreeSet::<PathBuf>::new();
    let mut exposures = Vec::with_capacity(configured.len());
    for (name, config) in configured {
        let rendered_host = PathBuf::from(crate::template::render(&config.host_path, vars)?);
        if !rendered_host.is_absolute() {
            anyhow::bail!("host_socket_exposures.{name}.host_path must render to an absolute path");
        }
        let host_path = canonicalize_final_without_following(
            &rendered_host,
            &format!("host_socket_exposures.{name}.host_path"),
        )?;
        let container_path = PathBuf::from(crate::template::render(&config.container_path, vars)?);
        validate_normalized_container_path(
            &format!("host_socket_exposures.{name}.container_path"),
            &container_path,
        )?;
        validate_runtime_path_separators(
            &format!("host_socket_exposures.{name}.host_path"),
            &host_path,
        )?;
        validate_runtime_path_separators(
            &format!("host_socket_exposures.{name}.container_path"),
            &container_path,
        )?;
        validate_socket_path_length(
            "host socket exposure host path",
            &host_path,
            UNIX_SOCKET_PATH_MAX_BYTES,
        )?;
        let readiness_user = crate::template::render(&config.user, vars)?;
        crate::config::validate_name(
            &format!("host_socket_exposures.{name}.user"),
            &readiness_user,
        )?;
        validate_socket_path_length(
            "host socket exposure container path",
            &container_path,
            LINUX_UNIX_SOCKET_PATH_MAX_BYTES,
        )?;
        let identity = unix_socket_identity(&host_path, &name)?;
        for existing in &host_paths {
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.host_path"),
                &host_path,
                "another host socket exposure path",
                existing,
            )?;
        }
        for existing in &container_paths {
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.container_path"),
                &container_path,
                "another host socket exposure path",
                existing,
            )?;
        }
        host_paths.insert(host_path.clone());
        container_paths.insert(container_path.clone());

        reject_path_overlap(
            &format!("host_socket_exposures.{name}.host_path"),
            &host_path,
            "workspace host path",
            &workspace_host,
        )?;
        if let Some(control_host) = control_host.as_deref() {
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.host_path"),
                &host_path,
                "control socket host directory",
                control_host,
            )?;
        }
        for (label, other) in [
            ("workspace container path", workspace_container),
            ("container home", container_home),
        ] {
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.container_path"),
                &container_path,
                label,
                other,
            )?;
        }
        if let Some(control_container) = control_container {
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.container_path"),
                &container_path,
                "control socket container directory",
                control_container,
            )?;
        }
        for mount in &mounts {
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.host_path"),
                &host_path,
                "container mount source",
                &mount.source,
            )?;
            reject_path_overlap(
                &format!("host_socket_exposures.{name}.container_path"),
                &container_path,
                "container mount target",
                &mount.target,
            )?;
        }
        exposures.push(HostSocketExposureSpec {
            name,
            host_path,
            container_path,
            readiness_user,
            selinux_relabel: config.selinux_relabel,
            realization,
            source_identity: identity,
        });
    }

    let exposure_manifest = Some(exposure_manifest(&exposures));
    Ok(PreparedContainerInputs {
        mounts,
        host_socket_exposures: exposures,
        exposure_manifest,
    })
}

pub(super) fn canonicalize_generic_mount_source(
    path: &Path,
    label: &str,
) -> anyhow::Result<PathBuf> {
    let path = canonicalize_final_without_following(path, label)?;
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("stat {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{label} {} must not be a symlink", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if metadata.file_type().is_socket() {
            anyhow::bail!(
                "{label} {} is a Unix socket; configure it through host_socket_exposures",
                path.display()
            );
        }
    }
    if !metadata.is_file() && !metadata.is_dir() {
        anyhow::bail!(
            "{label} {} must be a regular file or directory",
            path.display()
        );
    }
    Ok(path)
}

pub(super) fn validate_normalized_container_path(label: &str, path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{label} must render to an absolute path");
    }
    let value = path.as_os_str().to_string_lossy();
    if value == "/"
        || value.strip_prefix('/').is_none_or(|rest| {
            rest.split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        })
    {
        anyhow::bail!("{label} must be a normalized absolute non-root path");
    }
    Ok(())
}

fn canonicalize_final_without_following(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        anyhow::anyhow!("{label} {} must name a filesystem entry", path.display())
    })?;
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("{label} {} must have a parent directory", path.display())
    })?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalize parent for {label} {}", path.display()))?;
    Ok(parent.join(file_name))
}

#[cfg(unix)]
fn unix_socket_identity(path: &Path, name: &str) -> anyhow::Result<UnixSourceIdentity> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "stat host_socket_exposures.{name}.host_path {}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_socket() {
        anyhow::bail!(
            "host_socket_exposures.{name}.host_path {} must be a Unix socket",
            path.display()
        );
    }
    Ok(UnixSourceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode() & 0o777,
    })
}

#[cfg(not(unix))]
fn unix_socket_identity(_path: &Path, _name: &str) -> anyhow::Result<UnixSourceIdentity> {
    anyhow::bail!("host_socket_exposures require a Unix host")
}

fn reject_path_overlap(a_label: &str, a: &Path, b_label: &str, b: &Path) -> anyhow::Result<()> {
    if a == b || a.starts_with(b) || b.starts_with(a) {
        anyhow::bail!(
            "{a_label} {} overlaps {b_label} {}; exact socket exposure paths must not be contained by or contain other managed paths",
            a.display(),
            b.display()
        );
    }
    Ok(())
}

fn validate_socket_path_length(label: &str, path: &Path, maximum: usize) -> anyhow::Result<()> {
    #[cfg(unix)]
    let length = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    };
    #[cfg(not(unix))]
    let length = path.as_os_str().to_string_lossy().len();
    if length > maximum {
        anyhow::bail!(
            "{label} {} is too long for a Unix domain socket: {length} bytes exceeds {maximum}",
            path.display()
        );
    }
    Ok(())
}

fn validate_runtime_path_separators(label: &str, path: &Path) -> anyhow::Result<()> {
    let value = path.as_os_str().to_string_lossy();
    if value.contains(':') || value.contains(',') {
        anyhow::bail!("{label} must not render with ':' or ','");
    }
    Ok(())
}

fn exposure_manifest(exposures: &[HostSocketExposureSpec]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"aw-gateway-host-socket-exposures\0v1\0");
    for exposure in exposures {
        hash_field(&mut hasher, exposure.name.as_bytes());
        hash_field(&mut hasher, exposure.host_path.to_string_lossy().as_bytes());
        hash_field(
            &mut hasher,
            exposure.container_path.to_string_lossy().as_bytes(),
        );
        hash_field(
            &mut hasher,
            format!("{:?}", exposure.selinux_relabel).as_bytes(),
        );
        hash_field(&mut hasher, exposure.realization.as_str().as_bytes());
        if exposure.realization == HostSocketExposureRealization::PinnedInode {
            hasher.update(exposure.source_identity.device.to_be_bytes());
            hasher.update(exposure.source_identity.inode.to_be_bytes());
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::SelinuxRelabel;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;

    fn exposure(host_path: &Path, container_path: &str) -> HostSocketExposureConfig {
        HostSocketExposureConfig {
            host_path: host_path.display().to_string(),
            container_path: container_path.into(),
            user: "{container_user}".into(),
            selinux_relabel: SelinuxRelabel::None,
        }
    }

    fn prepare(
        entries: impl IntoIterator<Item = (&'static str, HostSocketExposureConfig)>,
        runtime: ContainerRuntimeType,
        _session_uid: u32,
        _session_gid: u32,
    ) -> anyhow::Result<PreparedContainerInputs> {
        let vars = Vars::from([("container_user".into(), "test".into())]);
        prepare_host_socket_exposures(
            entries
                .into_iter()
                .map(|(name, config)| (name.to_string(), config))
                .collect(),
            &vars,
            runtime,
            Vec::new(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
            Path::new("/workspace"),
            Path::new("/home/test"),
            None,
            None,
        )
    }

    #[test]
    fn real_socket_prepares_without_mutating_source_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("traffic.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o606)).unwrap();
        let before = std::fs::symlink_metadata(&socket).unwrap();

        let prepared = prepare(
            [("traffic", exposure(&socket, "/run/aw-gateway/traffic.sock"))],
            ContainerRuntimeType::AppleContainer,
            1000,
            1000,
        )
        .unwrap();

        let after = std::fs::symlink_metadata(&socket).unwrap();
        assert_eq!(prepared.host_socket_exposures.len(), 1);
        assert_eq!(
            prepared.host_socket_exposures[0].realization,
            HostSocketExposureRealization::PathReconnect
        );
        assert_eq!(before.permissions().mode(), after.permissions().mode());
    }

    #[test]
    fn source_validation_rejects_missing_regular_directory_fifo_and_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        let regular = dir.path().join("regular");
        let directory = dir.path().join("directory");
        let fifo = dir.path().join("fifo");
        let listener_path = dir.path().join("listener.sock");
        let link = dir.path().join("link.sock");
        std::fs::write(&regular, b"not a socket").unwrap();
        std::fs::create_dir(&directory).unwrap();
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let _listener = UnixListener::bind(&listener_path).unwrap();
        symlink(&listener_path, &link).unwrap();

        for path in [&missing, &regular, &directory, &fifo, &link] {
            let err = prepare(
                [("traffic", exposure(path, "/run/traffic.sock"))],
                ContainerRuntimeType::Docker,
                1000,
                1000,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("stat host_socket_exposures") || err.contains("must be a Unix socket"),
                "{path:?}: {err}"
            );
        }
    }

    #[test]
    fn generic_mounts_reject_socket_and_final_symlink_sources() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("traffic.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let regular = dir.path().join("regular");
        let link = dir.path().join("link");
        std::fs::write(&regular, b"data").unwrap();
        symlink(&regular, &link).unwrap();

        let socket_err = canonicalize_generic_mount_source(&socket, "mount")
            .unwrap_err()
            .to_string();
        assert!(socket_err.contains("host_socket_exposures"), "{socket_err}");
        let link_err = canonicalize_generic_mount_source(&link, "mount")
            .unwrap_err()
            .to_string();
        assert!(link_err.contains("must not be a symlink"), "{link_err}");
    }

    #[test]
    fn exposure_paths_reject_ancestor_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.sock");
        let second = dir.path().join("second.sock");
        let _first_listener = UnixListener::bind(&first).unwrap();
        let _second_listener = UnixListener::bind(&second).unwrap();

        let err = prepare(
            [
                ("first", exposure(&first, "/run/traffic")),
                ("second", exposure(&second, "/run/traffic/nested.sock")),
            ],
            ContainerRuntimeType::Docker,
            1000,
            1000,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("another host socket exposure path"), "{err}");
    }

    #[test]
    fn rendered_exposure_paths_and_readiness_users_are_validated_after_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.sock");
        let second = dir.path().join("second.sock");
        let _first_listener = UnixListener::bind(&first).unwrap();
        let _second_listener = UnixListener::bind(&second).unwrap();
        let mut first_config = exposure(&first, "{container_home}/traffic.sock");
        first_config.user = "{container_user}".into();
        let mut second_config = exposure(&second, "/opt/socket-base/traffic.sock");
        second_config.user = "{container_user}".into();
        let vars = Vars::from([
            ("container_home".into(), "/opt/socket-base".into()),
            ("container_user".into(), "acl-relay".into()),
        ]);

        let err = prepare_host_socket_exposures(
            BTreeMap::from([
                ("first".into(), first_config),
                ("second".into(), second_config),
            ]),
            &vars,
            ContainerRuntimeType::Docker,
            Vec::new(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
            Path::new("/workspace"),
            Path::new("/home/test"),
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("another host socket exposure path"), "{err}");

        let mut single = exposure(&first, "/run/traffic.sock");
        single.user = "{container_user}".into();
        let prepared = prepare_host_socket_exposures(
            BTreeMap::from([("traffic".into(), single)]),
            &vars,
            ContainerRuntimeType::Docker,
            Vec::new(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
            Path::new("/workspace"),
            Path::new("/home/test"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            prepared.host_socket_exposures[0].readiness_user,
            "acl-relay"
        );
    }

    #[test]
    fn exposure_container_path_rejects_lexical_normalization_bypasses() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("traffic.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        for container_path in ["/run/../workspace/traffic.sock", "/run//traffic.sock", "/"] {
            let err = prepare(
                [("traffic", exposure(&socket, container_path))],
                ContainerRuntimeType::Docker,
                1000,
                1000,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("normalized absolute non-root"), "{err}");
        }
    }

    #[test]
    fn canonicalized_workspace_paths_cannot_implicitly_expose_socket() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("real-workspace");
        std::fs::create_dir(&workspace).unwrap();
        let socket = workspace.join("traffic.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let workspace_link = dir.path().join("workspace-link");
        symlink(&workspace, &workspace_link).unwrap();
        std::fs::create_dir(workspace.join("nested")).unwrap();

        for workspace_path in [workspace_link, workspace.join("nested/..")] {
            let vars = Vars::from([("container_user".into(), "test".into())]);
            let err = prepare_host_socket_exposures(
                BTreeMap::from([("traffic".into(), exposure(&socket, "/run/traffic.sock"))]),
                &vars,
                ContainerRuntimeType::Docker,
                Vec::new(),
                &workspace_path,
                Path::new("/workspace"),
                Path::new("/home/test"),
                None,
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("workspace host path"),
                "{workspace_path:?}: {err}"
            );
        }
    }

    #[test]
    fn removed_config_reports_stale_container_manifest() {
        let inspect = crate::runtime::ContainerInspect {
            id: "id".into(),
            name: "target".into(),
            state: crate::runtime::ContainerState {
                running: true,
                pid: Some(123),
            },
            config: crate::runtime::ContainerConfig {
                labels: BTreeMap::from([(
                    HOST_SOCKET_EXPOSURE_MANIFEST_LABEL.into(),
                    "sha256:stale".into(),
                )]),
            },
        };
        let statuses = removed_config_manifest_status(Some(&inspect), ContainerRuntimeType::Docker);
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].ready);
        assert_eq!(
            statuses[0].failure_category.as_deref(),
            Some("recreate_required")
        );
        assert_eq!(statuses[0].realization, "pinned_inode");
        let apple =
            removed_config_manifest_status(Some(&inspect), ContainerRuntimeType::AppleContainer);
        assert_eq!(apple[0].realization, "path_reconnect");
        assert!(removed_config_manifest_status(None, ContainerRuntimeType::Docker).is_empty());
    }

    #[test]
    fn apple_readiness_user_does_not_participate_in_host_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("traffic.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut config = exposure(&socket, "/run/relay.sock");
        config.user = "acl-relay".into();
        let prepared = prepare(
            [("relay", config)],
            ContainerRuntimeType::AppleContainer,
            0,
            0,
        )
        .unwrap();
        assert_eq!(
            prepared.host_socket_exposures[0].readiness_user,
            "acl-relay"
        );
    }

    #[test]
    fn manifest_tracks_linux_inode_but_not_apple_inode() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("traffic.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let linux_before = prepare(
            [("traffic", exposure(&socket, "/run/traffic.sock"))],
            ContainerRuntimeType::Docker,
            0,
            0,
        )
        .unwrap()
        .exposure_manifest;
        let apple_before = prepare(
            [("traffic", exposure(&socket, "/run/traffic.sock"))],
            ContainerRuntimeType::AppleContainer,
            0,
            0,
        )
        .unwrap()
        .exposure_manifest;
        std::fs::remove_file(&socket).unwrap();
        let _replacement = UnixListener::bind(&socket).unwrap();
        let linux_after = prepare(
            [("traffic", exposure(&socket, "/run/traffic.sock"))],
            ContainerRuntimeType::Docker,
            0,
            0,
        )
        .unwrap()
        .exposure_manifest;
        let apple_after = prepare(
            [("traffic", exposure(&socket, "/run/traffic.sock"))],
            ContainerRuntimeType::AppleContainer,
            0,
            0,
        )
        .unwrap()
        .exposure_manifest;

        assert_ne!(linux_before, linux_after);
        assert_eq!(apple_before, apple_after);
        drop(listener);
    }

    #[test]
    fn manifest_ignores_readiness_user() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("traffic.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o602)).unwrap();
        let mut root = exposure(&socket, "/run/traffic.sock");
        root.user = "root".into();
        let mut relay = root.clone();
        relay.user = "acl-relay".into();

        let root_manifest = prepare(
            [("traffic", root)],
            ContainerRuntimeType::AppleContainer,
            1000,
            1000,
        )
        .unwrap()
        .exposure_manifest;
        let relay_manifest = prepare(
            [("traffic", relay)],
            ContainerRuntimeType::AppleContainer,
            1000,
            1000,
        )
        .unwrap()
        .exposure_manifest;
        assert_eq!(root_manifest, relay_manifest);
    }

    #[test]
    fn existing_container_manifest_has_no_compatibility_fallback() {
        validate_exposure_manifest_values("target", Some("sha256:a"), Some("sha256:a")).unwrap();
        let missing = validate_exposure_manifest_values("target", Some("sha256:a"), None)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("missing"), "{missing}");
        let changed =
            validate_exposure_manifest_values("target", Some("sha256:a"), Some("sha256:b"))
                .unwrap_err()
                .to_string();
        assert!(changed.contains("does not match"), "{changed}");
        let removed = validate_exposure_manifest_values("target", None, Some("sha256:a"))
            .unwrap_err()
            .to_string();
        assert!(removed.contains("target does not"), "{removed}");
    }
}
