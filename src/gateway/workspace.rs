use super::{Runtime, SessionOutcome, session};
use crate::config::{TargetConfig, WorkspaceCleanup};
use crate::paths::{self, UserContext};
use crate::template::{self, Vars};
use anyhow::Context;
use std::path::{Component, Path, PathBuf};

impl Runtime {
    pub(super) async fn finish_post_session<T>(
        &self,
        session: session::SessionGuard,
        result: anyhow::Result<T>,
        outcome: SessionOutcome,
    ) -> anyhow::Result<T> {
        drop(session);
        self.apply_post_session_cleanup(outcome).await;
        result
    }

    async fn apply_post_session_cleanup(&self, outcome: SessionOutcome) {
        if let Err(err) = self.apply_gateway_idle_cleanup().await {
            tracing::warn!(error = %err, "gateway-owned idle cleanup failed");
        }
        if !self.should_cleanup_workspace(outcome) {
            return;
        }
        let _lock = match self.acquire_lifecycle_lock().await {
            Ok(lock) => lock,
            Err(err) => {
                tracing::warn!(error = %err, "workspace cleanup skipped because lifecycle lock failed");
                return;
            }
        };
        match self.active_session_markers() {
            Ok(sessions) if sessions.is_empty() => {}
            Ok(_) => {
                tracing::warn!(
                    target = %self.identity.target_name,
                    workspace = %self.paths.workspace.display(),
                    "workspace cleanup skipped because active sessions remain"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    target = %self.identity.target_name,
                    workspace = %self.paths.workspace.display(),
                    error = %err,
                    "workspace cleanup skipped because active sessions could not be checked"
                );
                return;
            }
        }
        if let Err(err) = self.remove_session_workspace().await {
            tracing::warn!(
                target = %self.identity.target_name,
                workspace = %self.paths.workspace.display(),
                error = %err,
                "workspace cleanup failed"
            );
        }
    }

    pub(super) fn should_cleanup_workspace(&self, outcome: SessionOutcome) -> bool {
        match self.target.workspace.cleanup {
            WorkspaceCleanup::Never => false,
            WorkspaceCleanup::Success => outcome == SessionOutcome::Success,
            WorkspaceCleanup::Always => true,
        }
    }

    // Caller must already hold this runtime's lifecycle lock. Explicit remove
    // intentionally bypasses the active-session marker check used by
    // post-session cleanup because the operator asked to remove this concrete
    // session footprint.
    pub(super) async fn apply_explicit_remove_workspace_cleanup(&self) {
        if self.target.workspace.cleanup == WorkspaceCleanup::Never {
            return;
        }
        if self.identity.session_id.is_none() {
            return;
        }
        if let Err(err) = self.remove_session_workspace().await {
            tracing::warn!(
                target = %self.identity.target_name,
                workspace = %self.paths.workspace.display(),
                error = %err,
                "explicit remove workspace cleanup failed"
            );
        }
    }

    pub(super) async fn validate_workspace_cleanup_path(&self) -> anyhow::Result<()> {
        if self.target.workspace.cleanup == WorkspaceCleanup::Never {
            return Ok(());
        }
        let session_id =
            self.identity.session_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!("workspace_cleanup requires an ephemeral session id")
            })?;
        validate_workspace_cleanup_path(
            &self.paths.workspace,
            &self.identity.user.home,
            session_id,
            Some(self.target.workspace.path.as_str()),
        )?;
        match tokio::fs::symlink_metadata(&self.paths.workspace).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "workspace_cleanup path {} must not be a symlink",
                    self.paths.workspace.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect workspace cleanup path {}",
                        self.paths.workspace.display()
                    )
                });
            }
        }
        Ok(())
    }

    pub(super) async fn remove_session_workspace(&self) -> anyhow::Result<()> {
        self.validate_workspace_cleanup_path().await?;
        let metadata = match tokio::fs::symlink_metadata(&self.paths.workspace).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect workspace cleanup path {}",
                        self.paths.workspace.display()
                    )
                });
            }
        };
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "workspace cleanup path {} must not be a symlink",
                self.paths.workspace.display()
            );
        }
        if !metadata.is_dir() {
            anyhow::bail!(
                "workspace cleanup path {} exists but is not a directory",
                self.paths.workspace.display()
            );
        }
        self.container_runtime
            .remove_host_dir_all(&self.paths.workspace)
            .await
    }
}

pub(super) fn validate_workspace_cleanup_path(
    workspace: &Path,
    home: &Path,
    session_id: &str,
    configured_workspace: Option<&str>,
) -> anyhow::Result<()> {
    if workspace.as_os_str().is_empty() {
        anyhow::bail!("workspace_cleanup resolved workspace must not be empty");
    }
    if workspace
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        anyhow::bail!(
            "workspace_cleanup path must not contain '.' or '..' components: {}",
            workspace.display()
        );
    }
    if workspace == Path::new("/") {
        anyhow::bail!("workspace_cleanup refuses to delete /");
    }
    if workspace == home {
        anyhow::bail!(
            "workspace_cleanup refuses to delete user home directory {}",
            workspace.display()
        );
    }
    if session_id.len() < 3 {
        anyhow::bail!("workspace_cleanup session_id {session_id:?} must be at least 3 characters");
    }
    let leaf = workspace
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !leaf.contains(session_id) {
        anyhow::bail!(
            "workspace_cleanup path {} leaf must contain session_id {session_id:?}",
            workspace.display()
        );
    }
    if let Some(configured_workspace) = configured_workspace
        && Path::new(configured_workspace)
            .components()
            .any(|component| component.as_os_str() == "aw-gateway")
        && !workspace
            .components()
            .any(|component| component.as_os_str() == "aw-gateway")
    {
        anyhow::bail!(
            "workspace_cleanup path {} is outside the configured aw-gateway workspace root",
            workspace.display()
        );
    }
    Ok(())
}

pub(super) fn resolve_target_workspace(
    target: &TargetConfig,
    target_name: &str,
    user: &UserContext,
    session_id: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let mut vars = Vars::new();
    vars.insert("user".into(), user.user.clone());
    vars.insert("uid".into(), user.uid.to_string());
    vars.insert("gid".into(), user.gid.to_string());
    vars.insert("home".into(), user.home.display().to_string());
    vars.insert("target".into(), target_name.to_string());
    vars.insert("image".into(), target.image.clone());
    vars.insert("image_slug".into(), template::image_slug(&target.image));
    if let Some(session_id) = session_id {
        vars.insert("session_id".into(), session_id.to_string());
    }
    let rendered = template::render(&target.workspace.path, &vars)?;
    Ok(paths::resolve_workspace(&user.home, &rendered))
}
