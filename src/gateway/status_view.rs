use super::model::{AllStatusEntry, SessionStatus};
use crate::config::{GatewayConfig, TargetMode};
use crate::runtime::ManagedContainer;

const UNKNOWN_STATUS_LABEL: &str = "unknown";

pub(super) fn status_launch(
    session_id: Option<&str>,
    sessions: &[SessionStatus],
) -> Option<String> {
    match session_id {
        Some(session_id) => sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.launch.clone()),
        None => sessions.iter().find_map(|session| session.launch.clone()),
    }
}

pub(super) fn status_all_entries(
    cfg: &GatewayConfig,
    containers: Vec<ManagedContainer>,
) -> Vec<AllStatusEntry> {
    containers
        .into_iter()
        .map(|container| status_all_entry(cfg, container))
        .collect()
}

fn status_all_entry(cfg: &GatewayConfig, container: ManagedContainer) -> AllStatusEntry {
    let target = container
        .labels
        .get("io.aw-gateway.target")
        .cloned()
        .unwrap_or_else(|| UNKNOWN_STATUS_LABEL.into());
    let session_id = container.labels.get("io.aw-gateway.session_id").cloned();
    let mode = container
        .labels
        .get("io.aw-gateway.mode")
        .cloned()
        .unwrap_or_else(|| {
            infer_status_all_mode(cfg, &target, &container.name, session_id.as_deref())
        });
    // Launch labels are only persisted on ephemeral session containers. Fixed
    // targets can be reused across launches, so their live provenance comes
    // from per-session markers in `status <target>`.
    let launch = (mode == "ephemeral")
        .then(|| container.labels.get("io.aw-gateway.launch").cloned())
        .flatten();
    let user = container
        .labels
        .get("io.aw-gateway.user")
        .cloned()
        .unwrap_or_default();
    let uid = container
        .labels
        .get("io.aw-gateway.uid")
        .cloned()
        .unwrap_or_default();
    let image = container
        .labels
        .get("io.aw-gateway.image")
        .cloned()
        .unwrap_or(container.image);
    let container_name = container
        .labels
        .get("io.aw-gateway.container_id")
        .cloned()
        .unwrap_or(container.name);
    let status = if container.running {
        "running"
    } else {
        "stopped"
    }
    .to_string();
    AllStatusEntry {
        target,
        session_id,
        launch,
        mode,
        user,
        uid,
        image,
        container: container_name,
        status,
    }
}

fn infer_status_all_mode(
    cfg: &GatewayConfig,
    target: &str,
    container_name: &str,
    session_id: Option<&str>,
) -> String {
    let Ok(target_cfg) = cfg.effective_target(target) else {
        return UNKNOWN_STATUS_LABEL.into();
    };
    match target_cfg.mode {
        TargetMode::Fixed => match target_cfg.container_name(None) {
            Ok(expected) if expected == container_name => "fixed".into(),
            _ => UNKNOWN_STATUS_LABEL.into(),
        },
        TargetMode::Ephemeral if session_id.is_some() => "ephemeral".into(),
        TargetMode::Ephemeral => UNKNOWN_STATUS_LABEL.into(),
    }
}
