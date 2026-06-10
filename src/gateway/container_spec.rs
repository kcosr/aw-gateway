use super::Runtime;
use crate::config::{ContainerMountConfig, ContainerMountMode, LocalSshBackend, TargetMode};
use crate::context::{context_from_labels, context_label_key};
use crate::runtime::{self, ContainerInspect, ContainerMountSpec, ContainerRunSpec};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(super) const DEFAULT_SESSION_SHELL_ENV: &str = "/usr/bin/bash";

impl Runtime {
    pub(super) fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.validation_labels();
        labels.extend([
            ("io.aw-gateway.image".into(), self.target.image.clone()),
            (
                "io.aw-gateway.mode".into(),
                format!("{:?}", self.target.mode).to_lowercase(),
            ),
        ]);
        if let Some(session_id) = &self.identity.session_id {
            labels.insert("io.aw-gateway.session_id".into(), session_id.clone());
        }
        if self.target.mode == TargetMode::Ephemeral
            && let Some(launch_name) = &self.identity.launch_name
        {
            labels.insert("io.aw-gateway.launch".into(), launch_name.clone());
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
        runtime::validate_gateway_labels(inspect, &self.validation_labels())?;
        let stored = context_from_labels(&inspect.config.labels);
        if !self.context.matches_stored(&stored) {
            anyhow::bail!("container context does not match supplied runtime context");
        }
        Ok(())
    }

    pub(super) async fn start_container(&self) -> anyhow::Result<()> {
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
        let mounts = self.container_mounts_async().await?;
        warn_about_unsafe_container_mounts_async(&mounts).await?;
        let run_spec = self.container_run_spec_with_mounts(
            identity_token.as_deref(),
            control_token.as_deref(),
            mounts,
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
        self.container_run_spec_with_mounts(identity_token, control_token, mounts)
    }

    fn container_run_spec_with_mounts(
        &self,
        identity_token: Option<&str>,
        control_token: Option<&str>,
        mounts: Vec<ContainerMountSpec>,
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
            mounts,
            env,
            labels: self.labels(),
            publish_ssh: self.ssh_endpoint_configured()
                && self.ssh_backend() == LocalSshBackend::PublishedPort,
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
            self.paths
                .workspace
                .join(&self.target.workspace.state_dir)
                .display()
                .to_string(),
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

    async fn container_mounts_async(&self) -> anyhow::Result<Vec<ContainerMountSpec>> {
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
            let source = source
                .canonicalize()
                .with_context(|| format!("container mount source #{index} {}", source.display()))?;
            validate_bind_mount_path(&format!("container mount source #{index}"), &source)?;
            let target = PathBuf::from(template::render(&mount.target, &inputs.vars)?);
            if !target.is_absolute() {
                anyhow::bail!(
                    "container mount target #{} must render to an absolute path",
                    index
                );
            }
            validate_bind_mount_path(&format!("container mount target #{index}"), &target)?;
            Ok(ContainerMountSpec {
                source,
                target,
                readonly: mount.mode == ContainerMountMode::Ro,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    mounts.push(ContainerMountSpec {
        source: inputs.control_socket_host_dir,
        target: inputs.control_socket_container_dir,
        readonly: false,
    });
    Ok(mounts)
}

fn validate_bind_mount_path(label: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let value = path.as_os_str().to_string_lossy();
    if value.contains(':') || value.contains(',') {
        anyhow::bail!("{label} must not contain ':' or ','");
    }
    Ok(())
}

async fn warn_about_unsafe_container_mounts_async(
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
