use super::Runtime;
use super::published_port::published_port_bind_conflict;
use crate::config::{
    ContainerMountConfig, ContainerMountMode, ContainerRuntimeType, LocalSshBackend, TargetMode,
};
use crate::context::{context_from_labels, context_label_key};
use crate::runtime::{self, ContainerInspect, ContainerMountSpec, ContainerRunSpec};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(super) const DEFAULT_SESSION_SHELL_ENV: &str = "/usr/bin/bash";
const PUBLISHED_SSH_RUN_ATTEMPTS: usize = 3;
const WORKSPACE_SOURCE_LABEL: &str = "io.aw-gateway.workspace.source";
const WORKSPACE_TARGET_LABEL: &str = "io.aw-gateway.workspace.target";
const WORKSPACE_STATE_HOST_LABEL: &str = "io.aw-gateway.workspace.state_host";
const WORKSPACE_STATE_CONTAINER_LABEL: &str = "io.aw-gateway.workspace.state_container";

use super::host_socket_exposures::{HOST_SOCKET_EXPOSURE_MANIFEST_LABEL, PreparedContainerInputs};

impl Runtime {
    #[cfg(test)]
    pub(super) fn labels(&self) -> BTreeMap<String, String> {
        self.labels_with_exposure_manifest(None)
    }

    fn labels_with_exposure_manifest(
        &self,
        exposure_manifest: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut labels = self.validation_labels();
        labels.extend([
            ("io.aw-gateway.image".into(), self.target.image.clone()),
            (
                "io.aw-gateway.access".into(),
                self.target.access.method.as_str().into(),
            ),
            (
                "io.aw-gateway.mode".into(),
                format!("{:?}", self.target.mode).to_lowercase(),
            ),
        ]);
        labels.extend(self.workspace_layout_labels());
        if let Some(session_id) = &self.identity.session_id {
            labels.insert("io.aw-gateway.session_id".into(), session_id.clone());
        }
        if self.target.mode == TargetMode::Ephemeral
            && let Some(launch_name) = &self.identity.launch_name
        {
            labels.insert("io.aw-gateway.launch".into(), launch_name.clone());
        }
        if let Some(exposure_manifest) = exposure_manifest {
            labels.insert(
                HOST_SOCKET_EXPOSURE_MANIFEST_LABEL.into(),
                exposure_manifest.into(),
            );
        }
        labels
    }

    pub(super) fn validation_labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from([
            ("io.aw-gateway.gateway".into(), "true".into()),
            ("io.aw-gateway.user".into(), self.identity.user.user.clone()),
            (
                "io.aw-gateway.uid".into(),
                self.identity.user.uid.to_string(),
            ),
            (
                "io.aw-gateway.target".into(),
                self.identity.target_name.clone(),
            ),
            (
                "io.aw-gateway.container_id".into(),
                self.identity.container_name.clone(),
            ),
        ]);
        for (key, value) in self.context.as_map() {
            labels.insert(context_label_key(key), value.clone());
        }
        labels
    }

    pub(super) fn validate_labels(&self, inspect: &ContainerInspect) -> anyhow::Result<()> {
        self.validate_stable_labels(inspect)?;
        self.validate_access_label(inspect)?;
        self.validate_workspace_layout_labels(inspect)
    }

    pub(super) fn validate_stable_labels(&self, inspect: &ContainerInspect) -> anyhow::Result<()> {
        runtime::validate_gateway_labels(inspect, &self.validation_labels())?;
        let stored = context_from_labels(&inspect.config.labels);
        if !self.context.matches_stored(&stored) {
            anyhow::bail!("container context does not match supplied runtime context");
        }
        Ok(())
    }

    fn validate_access_label(&self, inspect: &ContainerInspect) -> anyhow::Result<()> {
        let expected = self.target.access.method.as_str();
        match inspect
            .config
            .labels
            .get("io.aw-gateway.access")
            .map(String::as_str)
        {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => anyhow::bail!(
                "container {:?} access label {actual:?} does not match configured access.method {expected:?}; stop or remove the existing container before switching access methods",
                self.identity.container_name
            ),
            None if self.target.access.method == crate::config::TargetAccessMethod::Ssh => Ok(()),
            None => anyhow::bail!(
                "container {:?} is missing io.aw-gateway.access and cannot be reused for access.method = \"runtime_exec\"; remove the existing container before using runtime-exec access",
                self.identity.container_name
            ),
        }
    }

    fn workspace_layout_labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                WORKSPACE_SOURCE_LABEL.into(),
                self.paths.workspace.display().to_string(),
            ),
            (
                WORKSPACE_TARGET_LABEL.into(),
                self.paths.workspace_container_path.display().to_string(),
            ),
            (
                WORKSPACE_STATE_HOST_LABEL.into(),
                self.paths.workspace_state_dir.display().to_string(),
            ),
            (
                WORKSPACE_STATE_CONTAINER_LABEL.into(),
                self.paths
                    .workspace_state_dir_in_container
                    .display()
                    .to_string(),
            ),
        ])
    }

    fn validate_workspace_layout_labels(&self, inspect: &ContainerInspect) -> anyhow::Result<()> {
        let expected = self.workspace_layout_labels();
        let has_layout_labels = expected
            .keys()
            .any(|key| inspect.config.labels.contains_key(key));
        let legacy_default_layout = self.paths.workspace_container_path
            == self.identity.container_home
            && self.paths.workspace_state_dir_in_container
                == self.identity.container_home.join(".aw-gateway");
        if !has_layout_labels && legacy_default_layout {
            return Ok(());
        }

        runtime::validate_gateway_labels(inspect, &expected).with_context(|| {
            format!(
                "container {:?} workspace layout does not match the configured workspace mount; remove the existing container before using the new layout",
                self.identity.container_name
            )
        })
    }

    pub(super) async fn start_container(
        &self,
        prepared: &PreparedContainerInputs,
    ) -> anyhow::Result<()> {
        let identity_token = self
            .target
            .container_agent
            .needs_identity_token()
            .then(|| self.ensure_identity_token())
            .transpose()?;
        let control_token = self
            .agent_control_enabled()
            .then(|| self.ensure_control_token())
            .transpose()?;
        if self.needs_explicit_published_ssh_port() {
            return self
                .start_container_with_explicit_published_ssh_port(
                    identity_token.as_deref(),
                    control_token.as_deref(),
                    prepared,
                )
                .await;
        }
        let run_spec = self.container_run_spec_with_inputs(
            identity_token.as_deref(),
            control_token.as_deref(),
            prepared,
            None,
        )?;
        self.container_runtime.run_detached(&run_spec).await
    }

    #[cfg(test)]
    pub(super) fn container_run_spec(
        &self,
        identity_token: Option<&str>,
        control_token: Option<&str>,
    ) -> anyhow::Result<ContainerRunSpec> {
        let mounts = self.container_mounts()?;
        if !self.target.host_socket_exposures.is_empty() {
            anyhow::bail!("host socket exposure tests must use prepare_container_inputs");
        }
        let prepared = PreparedContainerInputs {
            mounts,
            host_socket_exposures: Vec::new(),
            exposure_manifest: None,
        };
        self.container_run_spec_with_inputs(identity_token, control_token, &prepared, None)
    }

    pub(super) fn container_run_spec_with_inputs(
        &self,
        identity_token: Option<&str>,
        control_token: Option<&str>,
        prepared: &PreparedContainerInputs,
        published_ssh_host_port: Option<u16>,
    ) -> anyhow::Result<ContainerRunSpec> {
        let mut env = BTreeMap::new();
        if let Some(identity_token) = identity_token {
            env.insert("AW_IDENTITY_TOKEN".into(), identity_token.to_string());
        }
        if let Some(control_token) = control_token {
            env.insert(
                "AW_CONTAINER_CONTROL_TOKEN".into(),
                control_token.to_string(),
            );
        }
        if self.agent_enabled() {
            env.insert(
                "AW_AUTHENTICATED_UID".into(),
                self.identity.user.uid.to_string(),
            );
            env.insert(
                "AW_AUTHENTICATED_GID".into(),
                self.identity.user.gid.to_string(),
            );
        }
        env.extend(self.render_env_map(&self.target.container_env)?);
        validate_bind_mount_path("workspace path", &self.paths.workspace)?;
        validate_bind_mount_path("container_home", &self.identity.container_home)?;
        let workspace_container_path = self.paths.workspace_container_path.clone();
        validate_bind_mount_path("workspace.container_path", &workspace_container_path)?;
        let command = if self.agent_enabled() {
            if self.target.container_bootstrap.enabled {
                vec![
                    self.render_value(&self.target.container_bootstrap.entrypoint)?,
                    "--config".into(),
                    self.container_agent_config_in_container()
                        .display()
                        .to_string(),
                    "--bootstrap-config".into(),
                    self.container_bootstrap_config_in_container()
                        .display()
                        .to_string(),
                ]
            } else {
                vec![
                    "aw-container-agent".into(),
                    "--config".into(),
                    self.container_agent_config_in_container()
                        .display()
                        .to_string(),
                    "run".into(),
                ]
            }
        } else {
            vec!["sleep".into(), "infinity".into()]
        };
        Ok(ContainerRunSpec {
            name: self.identity.container_name.clone(),
            hostname: self.identity.container_name.clone(),
            image: self.target.image.clone(),
            workspace: self.paths.workspace.clone(),
            workspace_container_path,
            container_home: self.identity.container_home.clone(),
            container_user: if self.target.container_bootstrap.enabled {
                self.bootstrap_identity()
            } else {
                self.identity.container_user.clone()
            },
            passwd_entry: self
                .container_runtime
                .is_podman()
                .then(|| self.passwd_entry()),
            state_dir_in_container: self.paths.container_state_dir_in_container.clone(),
            mounts: prepared.mounts.clone(),
            host_socket_exposures: prepared.host_socket_exposures.clone(),
            env,
            labels: self.labels_with_exposure_manifest(prepared.exposure_manifest.as_deref()),
            publish_ssh: self.ssh_endpoint_configured()
                && self.ssh_backend() == LocalSshBackend::PublishedPort,
            published_ssh_host_port,
            extra_run_args: self
                .target
                .runtime
                .extra_run_args
                .iter()
                .map(|arg| self.render_value(arg))
                .collect::<anyhow::Result<Vec<_>>>()?,
            command,
        })
    }

    async fn start_container_with_explicit_published_ssh_port(
        &self,
        identity_token: Option<&str>,
        control_token: Option<&str>,
        prepared: &PreparedContainerInputs,
    ) -> anyhow::Result<()> {
        for attempt in 1..=PUBLISHED_SSH_RUN_ATTEMPTS {
            let pending = self
                .prepare_published_ssh_host_port_for_create()?
                .expect("explicit published SSH port should allocate pending state");
            let run_spec = self.container_run_spec_with_inputs(
                identity_token,
                control_token,
                prepared,
                Some(pending.port()),
            )?;
            drop(pending);
            let result = self.container_runtime.run_detached(&run_spec).await;
            match result {
                Ok(()) => {
                    self.promote_pending_published_ssh_port()?;
                    return Ok(());
                }
                Err(err)
                    if attempt < PUBLISHED_SSH_RUN_ATTEMPTS
                        && published_port_bind_conflict(&err) =>
                {
                    self.remove_pending_published_ssh_port();
                    self.remove_label_validated_leftover_after_failed_published_port_run(prepared)
                        .await?;
                }
                Err(err) => {
                    self.remove_pending_published_ssh_port();
                    return Err(err);
                }
            }
        }
        unreachable!("published SSH run attempts loop must return");
    }

    async fn remove_label_validated_leftover_after_failed_published_port_run(
        &self,
        prepared: &PreparedContainerInputs,
    ) -> anyhow::Result<()> {
        let Some(inspect) = self
            .container_runtime
            .inspect(&self.identity.container_name)
            .await?
        else {
            return Ok(());
        };
        self.validate_labels(&inspect)
            .with_context(|| {
                format!(
                    "not deleting leftover container {:?} after failed published SSH port start because labels did not match",
                    self.identity.container_name
                )
            })?;
        self.validate_exposure_manifest(&inspect, prepared)?;
        self.container_runtime
            .rm(&self.identity.container_name)
            .await
    }

    pub(super) fn vars(&self, container_pid: Option<&str>) -> Vars {
        let mut vars = Vars::new();
        vars.insert("user".into(), self.identity.user.user.clone());
        vars.insert("uid".into(), self.identity.session_uid.to_string());
        vars.insert("gid".into(), self.identity.session_gid.to_string());
        vars.insert("home".into(), self.identity.user.home.display().to_string());
        vars.insert(
            "container_user".into(),
            self.identity.container_user.clone(),
        );
        vars.insert(
            "container_home".into(),
            self.identity.container_home.display().to_string(),
        );
        vars.insert(
            "workspace".into(),
            self.paths.workspace.display().to_string(),
        );
        vars.insert(
            "state".into(),
            self.paths.workspace_state_dir.display().to_string(),
        );
        vars.insert(
            "state_dir".into(),
            self.identity.user.state_dir().display().to_string(),
        );
        vars.insert("target".into(), self.identity.target_name.clone());
        if let Some(session_id) = &self.identity.session_id {
            vars.insert("session_id".into(), session_id.clone());
        }
        self.context.insert_template_vars(&mut vars);
        vars.insert("image".into(), self.target.image.clone());
        vars.insert(
            "image_slug".into(),
            template::image_slug(&self.target.image),
        );
        vars.insert(
            "container_name".into(),
            self.identity.container_name.clone(),
        );
        vars.insert(
            "container_state_dir".into(),
            self.paths.container_state_dir.display().to_string(),
        );
        vars.insert(
            "container_state_dir_in_container".into(),
            self.paths
                .container_state_dir_in_container
                .display()
                .to_string(),
        );
        if let Some(container_pid) = container_pid {
            vars.insert("container_pid".into(), container_pid.to_string());
        }
        vars
    }

    pub(super) fn render_runtime_value(
        &self,
        value: &str,
        vars: &Vars,
        container_pid: Option<&str>,
    ) -> anyhow::Result<String> {
        self.ensure_runtime_template_value_supported(value, container_pid)?;
        template::render(value, vars)
    }

    pub(super) fn render_runtime_argv(
        &self,
        command: &[String],
        vars: &Vars,
        container_pid: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        command
            .iter()
            .map(|arg| self.render_runtime_value(arg, vars, container_pid))
            .collect()
    }

    pub(super) fn ensure_runtime_template_values_supported<'a>(
        &self,
        values: impl IntoIterator<Item = &'a str>,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        for value in values {
            self.ensure_runtime_template_value_supported(value, container_pid)?;
        }
        Ok(())
    }

    fn ensure_runtime_template_value_supported(
        &self,
        value: &str,
        container_pid: Option<&str>,
    ) -> anyhow::Result<()> {
        if container_pid.is_none()
            && self.container_runtime.kind() == ContainerRuntimeType::AppleContainer
            && template::referenced_keys(value)?.contains(&"container_pid")
        {
            anyhow::bail!(
                "apple_container runtime did not report a container PID; {{container_pid}} is not available for target {:?}",
                self.identity.target_name
            );
        }
        Ok(())
    }

    fn passwd_entry(&self) -> String {
        format!(
            "{}:x:{}:{}:{}:{}:{}",
            self.identity.container_user,
            self.identity.session_uid,
            self.identity.session_gid,
            self.identity.container_user,
            self.identity.container_home.display(),
            self.identity.session_shell,
        )
    }

    #[cfg(test)]
    pub(super) fn container_mounts(&self) -> anyhow::Result<Vec<ContainerMountSpec>> {
        render_container_mounts(self.container_mount_inputs())
    }

    pub(super) async fn container_mounts_async(&self) -> anyhow::Result<Vec<ContainerMountSpec>> {
        let inputs = self.container_mount_inputs();
        tokio::task::spawn_blocking(move || render_container_mounts(inputs))
            .await
            .context("join container mount resolution task")?
    }

    fn container_mount_inputs(&self) -> ContainerMountInputs {
        ContainerMountInputs {
            configured: self.target.container_mounts.clone(),
            vars: self.vars(None),
            control_socket_host_dir: self.paths.control_sockets.host_dir.clone(),
            control_socket_container_dir: self.paths.control_sockets.container_dir.clone(),
            include_control_socket_mount: self.requires_control_socket_dir(),
        }
    }

    fn render_value(&self, value: &str) -> anyhow::Result<String> {
        template::render(value, &self.vars(None))
    }

    pub(super) fn render_env_map(
        &self,
        env: &BTreeMap<String, String>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        env.iter()
            .map(|(key, value)| Ok((key.clone(), self.render_value(value)?)))
            .collect()
    }

    pub(super) fn exec_identity(&self) -> String {
        if self.container_runtime.is_podman() {
            format!(
                "{}:{}",
                self.identity.session_uid, self.identity.session_gid
            )
        } else {
            self.identity.container_user.clone()
        }
    }

    fn bootstrap_identity(&self) -> String {
        if self.container_runtime.is_podman() {
            "0:0".into()
        } else {
            self.identity.bootstrap_user.clone()
        }
    }

    pub(super) fn session_env(&self) -> anyhow::Result<BTreeMap<String, String>> {
        self.session_env_with_lookup(|key| match std::env::var(key) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                tracing::warn!(
                    key,
                    "inherited session environment variable is not valid UTF-8"
                );
                None
            }
        })
    }

    pub(super) fn session_env_with_lookup(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::from([
            ("SHELL".into(), DEFAULT_SESSION_SHELL_ENV.to_string()),
            (
                "PATH".into(),
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
            ),
        ]);
        for key in &self.target.session_env_inherit {
            if let Some(value) = lookup(key) {
                env.insert(key.clone(), value);
            }
        }
        env.extend(self.render_env_map(&self.target.session_env)?);
        Ok(env)
    }
}

struct ContainerMountInputs {
    configured: Vec<ContainerMountConfig>,
    vars: Vars,
    control_socket_host_dir: PathBuf,
    control_socket_container_dir: PathBuf,
    include_control_socket_mount: bool,
}

fn render_container_mounts(
    inputs: ContainerMountInputs,
) -> anyhow::Result<Vec<ContainerMountSpec>> {
    let mut mounts = inputs
        .configured
        .into_iter()
        .enumerate()
        .map(|(index, mount)| {
            let source = PathBuf::from(template::render(&mount.source, &inputs.vars)?);
            let source = super::host_socket_exposures::canonicalize_generic_mount_source(
                &source,
                &format!("container mount source #{index}"),
            )?;
            validate_bind_mount_path(&format!("container mount source #{index}"), &source)?;
            let target = PathBuf::from(template::render(&mount.target, &inputs.vars)?);
            super::host_socket_exposures::validate_normalized_container_path(
                &format!("container mount target #{index}"),
                &target,
            )?;
            validate_bind_mount_path(&format!("container mount target #{index}"), &target)?;
            Ok(ContainerMountSpec {
                source,
                target,
                readonly: mount.mode == ContainerMountMode::Ro,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if inputs.include_control_socket_mount {
        validate_bind_mount_path(
            "control socket host directory",
            &inputs.control_socket_host_dir,
        )?;
        validate_bind_mount_path(
            "control socket container directory",
            &inputs.control_socket_container_dir,
        )?;
        mounts.push(ContainerMountSpec {
            source: inputs.control_socket_host_dir,
            target: inputs.control_socket_container_dir,
            readonly: false,
        });
    }
    Ok(mounts)
}

fn validate_bind_mount_path(label: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let value = path.as_os_str().to_string_lossy();
    if value.contains(':') || value.contains(',') {
        anyhow::bail!("{label} must not contain ':' or ','");
    }
    Ok(())
}

pub(super) async fn warn_about_unsafe_container_mounts_async(
    mounts: &[ContainerMountSpec],
) -> anyhow::Result<()> {
    let mounts = mounts.to_vec();
    tokio::task::spawn_blocking(move || warn_about_unsafe_container_mounts(&mounts))
        .await
        .context("join container mount safety task")?
}

fn warn_about_unsafe_container_mounts(mounts: &[ContainerMountSpec]) -> anyhow::Result<()> {
    for (index, mount) in mounts.iter().enumerate() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(&mount.source).with_context(|| {
                format!(
                    "stat container mount source #{} {}",
                    index,
                    mount.source.display()
                )
            })?;
            if metadata.permissions().mode() & 0o002 != 0 {
                if !mount.readonly {
                    anyhow::bail!(
                        "container mount source #{} {} is world-writable; refusing read-write mount",
                        index,
                        mount.source.display()
                    );
                }
                tracing::warn!(
                    mount = index,
                    source = %mount.source.display(),
                    "container mount source is world-writable"
                );
            }
        }
    }
    Ok(())
}
