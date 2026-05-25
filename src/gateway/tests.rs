use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn write_fake_runtime(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

#[cfg(unix)]
fn assert_file_mode(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, expected, "unexpected mode for {}", path.display());
}

#[cfg(not(unix))]
fn assert_file_mode(_path: &Path, _expected: u32) {}

fn fake_running_runtime_script(exit_code: i32) -> String {
    let user = UserContext::current().unwrap();
    format!(
        r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    exit {exit_code}
    ;;
esac
exit 0
"#,
        user = user.user,
        uid = user.uid,
    )
}

fn agent_status_response(ready: bool) -> String {
    format!(
        r#"{{"id":"status","ok":true,"result":{{"ready":{ready},"version":"0.2.0","services":[],"ssh_bridge":{{"enabled":true,"ready":{ready},"active_streams":0,"active_sessions":0}},"idle_cleanup":null,"shutting_down":false}}}}
"#
    )
}

fn bind_fake_agent_control(socket: &Path, response: String) -> tokio::task::JoinHandle<()> {
    if let Some(parent) = socket.parent() {
        paths::ensure_private_dir(parent).unwrap();
    }
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let response = response.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_ok() {
                    let mut stream = reader.into_inner();
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
        }
    })
}

fn fake_background_runtime_script(log: &Path) -> String {
    let user = UserContext::current().unwrap();
    format!(
        r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    echo started > "{log}"
    sleep 0.2
    echo done >> "{log}"
    ;;
esac
exit 0
"#,
        user = user.user,
        uid = user.uid,
        log = log.display()
    )
}

fn session_marker_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("json")
                })
                .count()
        })
        .unwrap_or(0)
}

async fn wait_for_background_marker_clear(log: &Path, marker_dir: &Path, panic_message: &str) {
    for _ in 0..20 {
        let done = std::fs::read_to_string(log)
            .map(|value| value.contains("done"))
            .unwrap_or(false);
        if done && session_marker_count(marker_dir) == 0 {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("{panic_message}");
}

fn test_control_socket_paths(base: &Path) -> ControlSocketPaths {
    let host_dir = base.join("runtime-sockets");
    let container_dir = PathBuf::from("/run/aw-gateway");
    ControlSocketPaths {
        host_agent_socket: host_dir.join("agent.sock"),
        host_ssh_socket: host_dir.join("ssh.sock"),
        container_agent_socket: container_dir.join("agent.sock"),
        container_ssh_socket: container_dir.join("ssh.sock"),
        host_dir,
        container_dir,
        default_host_dir: false,
    }
}

fn test_runtime_from_parts(
    cfg: GatewayConfig,
    target: TargetConfig,
    container_runtime: ContainerRuntime,
    user: UserContext,
    container_state_dir: PathBuf,
    dir: &tempfile::TempDir,
    launch_name: Option<String>,
) -> Runtime {
    let container_state_dir_in_container = user.home.join(".aw-gateway/containers/ubuntu-dev");
    Runtime {
        cfg,
        target,
        identity: RuntimeIdentity {
            target_name: "default".into(),
            launch_name,
            session_id: None,
            bootstrap_user: "root".into(),
            session_uid: user.uid,
            session_gid: user.gid,
            session_shell: "/bin/bash".into(),
            container_user: user.user.clone(),
            container_home: user.home.clone(),
            container_name: "ubuntu-dev".into(),
            user,
        },
        paths: RuntimePaths {
            workspace: dir.path().join("workspace"),
            container_state_dir,
            container_state_dir_in_container,
            control_sockets: test_control_socket_paths(dir.path()),
        },
        container_runtime,
    }
}

fn test_alice_runtime(
    cfg: GatewayConfig,
    target: TargetConfig,
    container_runtime: ContainerRuntime,
    dir: &tempfile::TempDir,
    container_state_dir: PathBuf,
) -> Runtime {
    test_runtime_from_parts(
        cfg,
        target,
        container_runtime,
        UserContext {
            uid: 2450,
            gid: 2450,
            user: "alice".into(),
            home: PathBuf::from("/home/alice"),
        },
        container_state_dir,
        dir,
        None,
    )
}

fn disable_default_container_agent(cfg: &mut GatewayConfig) {
    cfg.target_defaults.container_agent = Some(crate::config::ContainerAgentConfigInput {
        enabled: Some(false),
        services: Vec::new(),
        ssh_bridge: None,
        control_socket: None,
        idle_cleanup: None,
    });
}

fn enable_default_ssh_bridge(cfg: &mut GatewayConfig) {
    cfg.target_defaults.container_agent = Some(crate::config::ContainerAgentConfigInput {
        enabled: Some(true),
        services: Vec::new(),
        ssh_bridge: Some(crate::config::SshBridgeConfigInput {
            enabled: Some(true),
            socket: None,
            target: Some("127.0.0.1:22".into()),
            mode: Some("0600".into()),
        }),
        control_socket: None,
        idle_cleanup: None,
    });
}

fn test_runtime(
    dir: &tempfile::TempDir,
    program: PathBuf,
    configure: impl FnOnce(&mut GatewayConfig),
) -> Runtime {
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.runtime.program = Some(program.display().to_string());
    disable_default_container_agent(&mut cfg);
    configure(&mut cfg);
    cfg.validate().unwrap();

    let target = cfg.effective_target("default").unwrap();
    let user = UserContext::current().unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, &user.user, &user.home).unwrap();
    test_runtime_from_parts(
        cfg,
        target,
        container_runtime,
        user.clone(),
        dir.path()
            .join("workspace/.aw-gateway/containers/ubuntu-dev"),
        dir,
        Some("agent-pack-codex".into()),
    )
}

fn launch_test_config(
    dir: &tempfile::TempDir,
    fake_runtime: &Path,
    launch_name: &str,
    command: Vec<String>,
) -> GatewayConfig {
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.runtime.program = Some(fake_runtime.display().to_string());
    disable_default_container_agent(&mut cfg);
    cfg.target_defaults.host_steps.clear();
    cfg.target_defaults.workspace = Some(crate::config::WorkspaceConfigInput {
        path: Some(dir.path().join("workspace").display().to_string()),
        state_dir: Some(".aw-gateway".into()),
        cleanup: None,
    });
    cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
    cfg.launches.insert(
        launch_name.into(),
        crate::config::LaunchConfigInput {
            target: Some("default".into()),
            command: Some(command),
            ..Default::default()
        },
    );
    cfg.validate().unwrap();
    cfg
}

fn launch_var_boundary_config() -> GatewayConfig {
    toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
target = "default"
command = ["true", "{var.repo}", "{var.count}", "{var.debug}", "{var.mode}"]

[launches.agent.vars]
repo = { type = "string", required = true }
count = { type = "number", default = 1 }
debug = { type = "boolean", default = false }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }
"#,
    )
    .unwrap()
}

fn write_gateway_config(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, body).unwrap();
    config
}

fn assert_invalid_launch_variable(err: OperationError, expected: &str) {
    let OperationError::InvalidLaunchVariable { message } = err else {
        panic!("expected invalid launch variable error, got {err:?}");
    };
    assert!(
        message.contains(expected),
        "expected {expected:?} in {message:?}"
    );
}

fn assert_invalid_session(err: OperationError, expected: &str) {
    let OperationError::InvalidSession { message } = err else {
        panic!("expected invalid session error, got {err:?}");
    };
    assert!(
        message.contains(expected),
        "expected {expected:?} in {message:?}"
    );
}

fn configure_workspace_cleanup_runtime(
    runtime: &mut Runtime,
    cleanup: WorkspaceCleanup,
    workspace: PathBuf,
    home: PathBuf,
    session_id: &str,
) {
    runtime.target.mode = TargetMode::Ephemeral;
    runtime.target.ephemeral_name = Some("ubuntu-dev-{session_id}".into());
    runtime.target.stop_when_idle = true;
    runtime.target.workspace.path =
        "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}".into();
    runtime.target.workspace.cleanup = cleanup;
    runtime.identity.session_id = Some(session_id.into());
    runtime.paths.workspace = workspace;
    runtime.paths.container_state_dir = runtime
        .paths
        .workspace
        .join(&runtime.target.workspace.state_dir)
        .join("sessions")
        .join(session_id);
    runtime.identity.user.home = home;
}

#[test]
fn workspace_cleanup_policy_matches_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});

    runtime.target.workspace.cleanup = WorkspaceCleanup::Never;
    assert!(!runtime.should_cleanup_workspace(SessionOutcome::Success));
    assert!(!runtime.should_cleanup_workspace(SessionOutcome::Failure));

    runtime.target.workspace.cleanup = WorkspaceCleanup::Success;
    assert!(runtime.should_cleanup_workspace(SessionOutcome::Success));
    assert!(!runtime.should_cleanup_workspace(SessionOutcome::Failure));

    runtime.target.workspace.cleanup = WorkspaceCleanup::Always;
    assert!(runtime.should_cleanup_workspace(SessionOutcome::Success));
    assert!(runtime.should_cleanup_workspace(SessionOutcome::Failure));
}

#[test]
fn session_outcome_maps_exit_code_results() {
    assert_eq!(
        SessionOutcome::from_exit_code_result(&Ok(0)),
        SessionOutcome::Success
    );
    assert_eq!(
        SessionOutcome::from_exit_code_result(&Ok(7)),
        SessionOutcome::Failure
    );
    assert_eq!(
        SessionOutcome::from_exit_code_result(&Err(anyhow::anyhow!("setup failed"))),
        SessionOutcome::Failure
    );
}

#[test]
fn workspace_cleanup_path_allows_missing_session_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456");

    validate_workspace_cleanup_path(
        &workspace,
        dir.path(),
        session_id,
        Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
    )
    .unwrap();
}

#[test]
fn workspace_cleanup_path_allows_three_character_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join(".cache/aw-gateway/workspaces/default-abc");

    validate_workspace_cleanup_path(
        &workspace,
        dir.path(),
        "abc",
        Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
    )
    .unwrap();

    let err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            &workspace,
            dir.path(),
            "ab",
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );
    assert!(err.contains("must be at least 3 characters"), "{err}");
}

#[test]
fn workspace_cleanup_path_rejects_unsafe_roots() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";

    let root_err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            Path::new("/"),
            dir.path(),
            session_id,
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );
    assert!(root_err.contains("refuses to delete /"), "{root_err}");

    let home_err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            dir.path(),
            dir.path(),
            session_id,
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );
    assert!(
        home_err.contains("refuses to delete user home directory"),
        "{home_err}"
    );
}

#[test]
fn workspace_cleanup_path_rejects_paths_without_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join(".cache/aw-gateway/workspaces/default");

    let err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            &workspace,
            dir.path(),
            "abc123def456",
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );

    assert!(err.contains("must contain session_id"), "{err}");
}

#[test]
fn workspace_cleanup_path_rejects_empty_and_dot_components() {
    let dir = tempfile::tempdir().unwrap();

    let empty_err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            Path::new(""),
            dir.path(),
            "abc123def456",
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );
    assert!(
        empty_err.contains("resolved workspace must not be empty"),
        "{empty_err}"
    );

    let dot_err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            Path::new("/tmp/aw-gateway/../default-abc123def456"),
            dir.path(),
            "abc123def456",
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );
    assert!(
        dot_err.contains("must not contain '.' or '..' components"),
        "{dot_err}"
    );
}

#[test]
fn workspace_cleanup_path_rejects_aw_gateway_template_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspaces/default-abc123def456");

    let err = format!(
        "{:#}",
        validate_workspace_cleanup_path(
            &workspace,
            dir.path(),
            "abc123def456",
            Some("{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"),
        )
        .unwrap_err()
    );

    assert!(
        err.contains("outside the configured aw-gateway workspace root"),
        "{err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_cleanup_path_rejects_symlink_root() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let real_workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456-real");
    let symlink_workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456");
    std::fs::create_dir_all(&real_workspace).unwrap();
    symlink(&real_workspace, &symlink_workspace).unwrap();
    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        symlink_workspace,
        dir.path().into(),
        session_id,
    );

    let err = format!(
        "{:#}",
        runtime.validate_workspace_cleanup_path().await.unwrap_err()
    );

    assert!(err.contains("must not be a symlink"), "{err}");
    assert!(real_workspace.exists());
}

#[tokio::test]
async fn remove_session_workspace_treats_missing_workspace_as_success() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456");
    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        workspace,
        dir.path().into(),
        session_id,
    );

    runtime.remove_session_workspace().await.unwrap();
}

#[tokio::test]
async fn remove_session_workspace_removes_only_resolved_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace_root = dir.path().join(".cache/aw-gateway/workspaces");
    let workspace = workspace_root.join("default-abc123def456");
    let sibling = workspace_root.join("sibling-abc123def456");
    std::fs::create_dir_all(workspace.join("nested")).unwrap();
    std::fs::write(workspace.join("nested/file.txt"), "data").unwrap();
    std::fs::create_dir_all(&sibling).unwrap();

    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
        cfg.runtime.runtime_type = crate::config::ContainerRuntimeType::Docker;
    });
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        workspace.clone(),
        dir.path().into(),
        session_id,
    );

    runtime.remove_session_workspace().await.unwrap();

    assert!(!workspace.exists());
    assert!(workspace_root.exists());
    assert!(sibling.exists());
}

#[tokio::test]
async fn remove_session_workspace_uses_podman_unshare_for_podman() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace_root = dir.path().join(".cache/aw-gateway/workspaces");
    let workspace = workspace_root.join("default-abc123def456");
    std::fs::create_dir_all(workspace.join("nested")).unwrap();
    std::fs::write(workspace.join("nested/file.txt"), "data").unwrap();
    let args_log = dir.path().join("podman-args.txt");
    let fake_podman = dir.path().join("podman");
    write_fake_runtime(
        &fake_podman,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > "{}"
if [ "$1" = "unshare" ]; then
  shift
  exec "$@"
fi
exit 1
"#,
            args_log.display()
        ),
    );

    let mut runtime = test_runtime(&dir, fake_podman, |cfg| {
        cfg.runtime.runtime_type = crate::config::ContainerRuntimeType::Podman;
    });
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        workspace.clone(),
        dir.path().into(),
        session_id,
    );

    runtime.remove_session_workspace().await.unwrap();

    assert!(!workspace.exists());
    let args = std::fs::read_to_string(args_log).unwrap();
    assert!(args.contains("unshare\nrm\n-rf\n--\n"), "{args}");
    assert!(args.contains(&workspace.display().to_string()), "{args}");
}

#[tokio::test]
async fn remove_session_workspace_rejects_non_directory_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456");
    std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
    std::fs::write(&workspace, "not a directory").unwrap();

    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        workspace,
        dir.path().into(),
        session_id,
    );

    let err = format!(
        "{:#}",
        runtime.remove_session_workspace().await.unwrap_err()
    );
    assert!(err.contains("exists but is not a directory"), "{err}");
}

#[tokio::test]
async fn finish_post_session_removes_workspace_for_failure_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
        cfg.runtime.runtime_type = crate::config::ContainerRuntimeType::Docker;
    });
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        workspace.clone(),
        dir.path().into(),
        session_id,
    );
    runtime.target.idle_cleanup = None;
    let session = runtime.create_session_marker("test").unwrap();

    let result = runtime
        .finish_post_session::<()>(
            session,
            Err(anyhow::anyhow!("simulated readiness failure")),
            SessionOutcome::Failure,
        )
        .await;

    assert!(result.is_err());
    assert!(!workspace.exists());
}

#[tokio::test]
async fn finish_post_session_preserves_success_when_workspace_cleanup_fails() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "abc123def456";
    let workspace = dir
        .path()
        .join(".cache/aw-gateway/workspaces/default-abc123def456");
    std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
    std::fs::write(&workspace, "not a directory").unwrap();

    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    configure_workspace_cleanup_runtime(
        &mut runtime,
        WorkspaceCleanup::Always,
        workspace.clone(),
        dir.path().into(),
        session_id,
    );
    runtime.target.idle_cleanup = None;
    runtime.paths.container_state_dir = dir.path().join("state");
    let session = runtime.create_session_marker("test").unwrap();

    let code = runtime
        .finish_post_session(session, Ok(0), SessionOutcome::Success)
        .await
        .unwrap();

    assert_eq!(code, 0);
    assert!(workspace.is_file());
}

fn inspect_with_running(running: bool) -> ContainerInspect {
    ContainerInspect {
        id: "id".into(),
        name: "container".into(),
        state: runtime::ContainerState { running, pid: 123 },
        config: runtime::ContainerConfig {
            labels: BTreeMap::new(),
        },
    }
}

fn managed_container(
    name: &str,
    image: &str,
    running: bool,
    labels: BTreeMap<String, String>,
) -> ManagedContainer {
    ManagedContainer {
        name: name.into(),
        image: image.into(),
        running,
        labels,
    }
}

fn managed_labels(target: &str, container: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("io.aw-gateway.gateway".into(), "true".into()),
        ("io.aw-gateway.user".into(), "alice".into()),
        ("io.aw-gateway.uid".into(), "2450".into()),
        ("io.aw-gateway.target".into(), target.into()),
        ("io.aw-gateway.image".into(), "ubuntu/dev".into()),
        ("io.aw-gateway.container_id".into(), container.into()),
    ])
}

#[test]
fn status_all_entries_empty_when_runtime_has_no_matches() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();

    let entries = status_all_entries(&cfg, Vec::new());

    assert!(entries.is_empty());
}

#[test]
fn status_all_entry_projects_fixed_container_from_labels() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let mut labels = managed_labels("default", "ubuntu-dev");
    labels.insert("io.aw-gateway.mode".into(), "fixed".into());

    let entries = status_all_entries(
        &cfg,
        vec![managed_container(
            "ubuntu-dev",
            "runtime-image",
            true,
            labels,
        )],
    );

    assert_eq!(
        entries,
        vec![AllStatusEntry {
            target: "default".into(),
            session_id: None,
            launch: None,
            mode: "fixed".into(),
            user: "alice".into(),
            uid: "2450".into(),
            image: "ubuntu/dev".into(),
            container: "ubuntu-dev".into(),
            status: "running".into(),
        }]
    );
}

#[test]
fn status_all_entries_project_multiple_ephemeral_containers() {
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.targets.get_mut("default").unwrap().mode = Some(TargetMode::Ephemeral);
    let mut first = managed_labels("default", "ubuntu-dev-1a2b3c4d5e6f");
    first.insert("io.aw-gateway.image".into(), "scratch/dev".into());
    first.insert("io.aw-gateway.mode".into(), "ephemeral".into());
    first.insert("io.aw-gateway.session_id".into(), "1a2b3c4d5e6f".into());
    let mut second = managed_labels("default", "ubuntu-dev-0f1e2d3c4b5a");
    second.insert("io.aw-gateway.image".into(), "scratch/dev".into());
    second.insert("io.aw-gateway.mode".into(), "ephemeral".into());
    second.insert("io.aw-gateway.session_id".into(), "0f1e2d3c4b5a".into());

    let entries = status_all_entries(
        &cfg,
        vec![
            managed_container("ubuntu-dev-1a2b3c4d5e6f", "scratch/dev", false, first),
            managed_container("ubuntu-dev-0f1e2d3c4b5a", "scratch/dev", true, second),
        ],
    );

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].session_id.as_deref(), Some("1a2b3c4d5e6f"));
    assert_eq!(entries[0].launch, None);
    assert_eq!(entries[0].status, "stopped");
    assert_eq!(entries[1].session_id.as_deref(), Some("0f1e2d3c4b5a"));
    assert_eq!(entries[1].status, "running");
}

#[test]
fn status_all_entry_keeps_stale_labeled_container_without_config_match() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let labels = managed_labels("old-target", "aw-old");

    let entries = status_all_entries(
        &cfg,
        vec![managed_container("aw-old", "runtime/old", true, labels)],
    );

    assert_eq!(entries[0].target, "old-target");
    assert_eq!(entries[0].mode, "unknown");
    assert_eq!(entries[0].session_id, None);
    assert_eq!(entries[0].launch, None);
    assert_eq!(entries[0].status, "running");
}

#[test]
fn status_all_entry_uses_unknown_policy_for_missing_labels() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();

    let entries = status_all_entries(
        &cfg,
        vec![managed_container(
            "aw-unlabeled",
            "runtime/old",
            false,
            BTreeMap::new(),
        )],
    );

    assert_eq!(entries[0].target, "unknown");
    assert_eq!(entries[0].mode, "unknown");
    assert_eq!(entries[0].status, "stopped");
}

#[test]
fn status_all_entry_projects_ephemeral_launch_label() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let mut labels = managed_labels("default", "ubuntu-dev-1a2b3c4d5e6f");
    labels.insert("io.aw-gateway.mode".into(), "ephemeral".into());
    labels.insert("io.aw-gateway.session_id".into(), "1a2b3c4d5e6f".into());
    labels.insert("io.aw-gateway.launch".into(), "agent-pack-codex".into());

    let entries = status_all_entries(
        &cfg,
        vec![managed_container(
            "ubuntu-dev-1a2b3c4d5e6f",
            "runtime-image",
            true,
            labels,
        )],
    );

    assert_eq!(entries[0].launch.as_deref(), Some("agent-pack-codex"));
    let serialized = serde_json::to_string(&entries).unwrap();
    assert!(serialized.contains("agent-pack-codex"));
    assert!(!serialized.contains("repo"));
    assert!(!serialized.contains("pack_id"));
    assert!(!serialized.contains("AGENT_PACK_ID"));
}

#[test]
fn status_all_entry_ignores_stale_fixed_launch_label() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let mut labels = managed_labels("default", "ubuntu-dev");
    labels.insert("io.aw-gateway.mode".into(), "fixed".into());
    labels.insert("io.aw-gateway.launch".into(), "stale-launch".into());

    let entries = status_all_entries(
        &cfg,
        vec![managed_container(
            "ubuntu-dev",
            "runtime-image",
            true,
            labels,
        )],
    );

    assert_eq!(entries[0].launch, None);
    assert!(
        !serde_json::to_string(&entries)
            .unwrap()
            .contains("stale-launch")
    );
}

#[test]
fn runtime_labels_only_persist_launch_for_ephemeral_targets() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});

    assert_eq!(runtime.target.mode, TargetMode::Fixed);
    assert!(!runtime.labels().contains_key("io.aw-gateway.launch"));

    runtime.target.mode = TargetMode::Ephemeral;
    runtime.identity.session_id = Some("1a2b3c4d5e6f".into());

    assert_eq!(
        runtime
            .labels()
            .get("io.aw-gateway.launch")
            .map(String::as_str),
        Some("agent-pack-codex")
    );
}

#[test]
fn status_launch_prefers_selected_session() {
    let sessions = vec![
        model::SessionStatus {
            id: "s1".into(),
            kind: "launch".into(),
            gateway_pid: 1,
            container: "ubuntu-dev".into(),
            target: "default".into(),
            launch: Some("first".into()),
            created_at_ms: 1,
        },
        model::SessionStatus {
            id: "s2".into(),
            kind: "launch".into(),
            gateway_pid: 1,
            container: "ubuntu-dev".into(),
            target: "default".into(),
            launch: Some("second".into()),
            created_at_ms: 2,
        },
    ];

    assert_eq!(
        status_launch(Some("s2"), &sessions).as_deref(),
        Some("second")
    );
    assert_eq!(status_launch(Some("missing"), &sessions), None);
    assert_eq!(status_launch(None, &sessions).as_deref(), Some("first"));
}

#[test]
fn launch_env_precedence_is_session_then_launch_then_step() {
    let mut vars = Vars::new();
    vars.insert("var.step".into(), "step-rendered".into());
    let session_env = BTreeMap::from([
        ("KEEP".into(), "session".into()),
        ("OVERRIDE".into(), "session".into()),
        ("STEP".into(), "session".into()),
    ]);
    let launch_env = BTreeMap::from([("OVERRIDE".into(), "launch".into())]);
    let step_env = BTreeMap::from([("STEP".into(), "{var.step}".into())]);

    let container_env =
        launch_container_step_env(&session_env, &launch_env, &step_env, &vars).unwrap();
    assert_eq!(container_env["KEEP"], "session");
    assert_eq!(container_env["OVERRIDE"], "launch");
    assert_eq!(container_env["STEP"], "step-rendered");

    let final_env = launch_final_env(&session_env, &launch_env);
    assert_eq!(final_env["KEEP"], "session");
    assert_eq!(final_env["OVERRIDE"], "launch");
    assert_eq!(final_env["STEP"], "session");
}

#[tokio::test]
async fn final_container_command_returns_runtime_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(
        &fake_runtime,
        r#"#!/bin/sh
if [ "$1" = "exec" ]; then
  exit 37
fi
exit 0
"#,
    );
    let runtime = test_runtime(&dir, fake_runtime, |_| {});

    let outcome = exec_final_container_command(
        &runtime,
        vec!["/bin/launch-final".into()],
        None,
        BTreeMap::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, ExecutionOutcome::new(37));
}

#[tokio::test]
async fn wait_capture_final_container_command_returns_selected_output() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(
        &fake_runtime,
        r#"#!/bin/sh
if [ "$1" = "exec" ]; then
  echo "captured stdout"
  echo "captured stderr" >&2
  exit 23
fi
exit 0
"#,
    );
    let runtime = test_runtime(&dir, fake_runtime, |_| {});

    let both = exec_final_container_command_with_options(
        &runtime,
        vec!["/bin/capture".into()],
        None,
        BTreeMap::new(),
        OperationExecutionOptions::WAIT,
    )
    .await
    .unwrap();

    assert_eq!(
        both,
        ExecutionOutcome::captured(
            23,
            Some(b"captured stdout\n".to_vec()),
            Some(b"captured stderr\n".to_vec()),
        )
    );

    let stdout_only = exec_final_container_command_with_options(
        &runtime,
        vec!["/bin/capture".into()],
        None,
        BTreeMap::new(),
        OperationExecutionOptions {
            mode: OperationMode::Wait,
            output: OutputSelection {
                stdout: true,
                stderr: false,
            },
        },
    )
    .await
    .unwrap();

    assert_eq!(
        stdout_only,
        ExecutionOutcome::captured(23, Some(b"captured stdout\n".to_vec()), None)
    );
}

#[test]
fn detached_runner_uses_detach_mode_without_selected_output() {
    assert_eq!(
        detach_discard_options(),
        OperationExecutionOptions {
            mode: OperationMode::Detach,
            output: OutputSelection {
                stdout: false,
                stderr: false,
            },
        }
    );
}

#[tokio::test]
async fn wait_run_operation_core_returns_selected_output() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let user = UserContext::current().unwrap();
    write_fake_runtime(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    echo "captured stdout"
    echo "captured stderr" >&2
    exit 23
    ;;
esac
exit 0
"#,
            user = user.user,
            uid = user.uid,
        ),
    );
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.host_steps.clear();
        cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
    });

    let outcome = run_container_command_with_runtime(
        runtime,
        None,
        vec!["/bin/capture".into()],
        OperationExecutionOptions {
            mode: OperationMode::Wait,
            output: OutputSelection {
                stdout: true,
                stderr: false,
            },
        },
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        ExecutionOutcome::captured(23, Some(b"captured stdout\n".to_vec()), None)
    );
}

#[tokio::test]
async fn run_operation_core_returns_nonzero_exit_without_exiting_process() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(37));
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.host_steps.clear();
        cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
    });

    let outcome = run_container_command_with_runtime(
        runtime,
        None,
        vec!["/bin/command-that-returns-37".into()],
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap();

    assert_eq!(outcome, ExecutionOutcome::new(37));
}

#[tokio::test]
async fn detached_run_keeps_session_marker_until_background_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_background_runtime_script(&log));
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.host_steps.clear();
        cfg.targets.get_mut("default").unwrap().stop_when_idle = Some(false);
    });
    let marker_dir = runtime.session_marker_dir();

    let outcome = run_container_command_with_runtime(
        runtime,
        None,
        vec!["/bin/background".into()],
        OperationExecutionOptions::DETACH,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ExecutionOutcome::Detached { .. }));
    assert_eq!(session_marker_count(&marker_dir), 1);

    wait_for_background_marker_clear(
        &log,
        &marker_dir,
        "detached background operation did not finish and clear marker",
    )
    .await;
}

#[tokio::test]
async fn launch_operation_core_returns_nonzero_exit_without_exiting_process() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(42));
    let cfg = launch_test_config(
        &dir,
        &fake_runtime,
        "returns-nonzero",
        vec!["/bin/command-that-returns-42".into()],
    );

    let outcome = launch_execute_with_config(
        cfg,
        "returns-nonzero",
        None,
        SuppliedLaunchVars::default(),
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap();

    assert_eq!(outcome, ExecutionOutcome::new(42));
}

#[tokio::test]
async fn detached_launch_keeps_launch_marker_until_background_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_background_runtime_script(&log));
    let cfg = launch_test_config(
        &dir,
        &fake_runtime,
        "detached-launch",
        vec!["/bin/background-launch".into()],
    );
    let marker_runtime = Runtime::from_config(
        cfg.clone(),
        Some("default"),
        None,
        true,
        Some("detached-launch".into()),
    )
    .await
    .unwrap();
    let marker_dir = marker_runtime.session_marker_dir();

    let outcome = launch_execute_with_config(
        cfg,
        "detached-launch",
        None,
        SuppliedLaunchVars::default(),
        OperationExecutionOptions::DETACH,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ExecutionOutcome::Detached { .. }));
    assert_eq!(session_marker_count(&marker_dir), 1);
    let sessions = marker_runtime.active_session_markers().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].kind, "launch");
    assert_eq!(sessions[0].launch.as_deref(), Some("detached-launch"));

    wait_for_background_marker_clear(
        &log,
        &marker_dir,
        "detached launch background operation did not finish and clear marker",
    )
    .await;
}

#[tokio::test]
async fn gateway_idle_cleanup_runs_after_launch_session_marker_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    let user = UserContext::current().unwrap();
    write_fake_runtime(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  stop)
    echo "stop $2" >> "{log}"
    ;;
esac
exit 0
"#,
            user = user.user,
            uid = user.uid,
            log = log.display()
        ),
    );
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        let target = cfg.targets.get_mut("default").unwrap();
        target.stop_when_idle = Some(true);
        target.remove_on_stop = Some(false);
        target.idle_cleanup = Some(crate::config::IdleCleanupConfigInput {
            owner: Some(IdleCleanupOwner::Gateway),
            action: Some(IdleCleanupAction::ExitContainer),
            ..Default::default()
        });
    });
    std::fs::create_dir_all(&runtime.paths.container_state_dir).unwrap();
    let session = runtime.create_launch_session_marker("launch").unwrap();

    runtime.apply_gateway_idle_cleanup().await.unwrap();
    assert!(!log.exists());

    drop(session);
    runtime.apply_gateway_idle_cleanup().await.unwrap();

    assert_eq!(std::fs::read_to_string(log).unwrap(), "stop ubuntu-dev\n");
}

#[test]
fn launch_var_resolution_rejects_duplicates_and_normalizes_values() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
target = "default"
command = ["true", "{var.count}", "{var.debug}", "{var.mode}"]

[launches.agent.vars]
repo = { type = "string", required = true }
count = { type = "number", default = 1 }
debug = { type = "boolean", default = false }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }
"#,
    )
    .unwrap();
    let launch = cfg.effective_launch("agent").unwrap();
    let supplied = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=https://example.test/repo.git".into(),
        "count=2.0".into(),
        "debug=true".into(),
        "mode=safe".into(),
    ])
    .unwrap();
    let vars = resolve_launch_vars("agent", &launch, &supplied).unwrap();
    assert_eq!(vars["count"], "2");
    assert_eq!(vars["debug"], "true");
    assert_eq!(vars["mode"], "safe");

    let err = SuppliedLaunchVars::from_cli_pairs(vec!["repo=a".into(), "repo=b".into()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("duplicate launch variable"), "{err}");

    let unknown = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=https://example.test/repo.git".into(),
        "extra=value".into(),
    ])
    .unwrap();
    let err = resolve_launch_vars("agent", &launch, &unknown)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown launch variable"), "{err}");

    let missing = SuppliedLaunchVars::default();
    let err = resolve_launch_vars("agent", &launch, &missing)
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing required launch variable"), "{err}");

    let invalid_bool = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=https://example.test/repo.git".into(),
        "debug=yes".into(),
    ])
    .unwrap();
    let err = resolve_launch_vars("agent", &launch, &invalid_bool)
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid boolean launch variable"), "{err}");

    let mut typed = SuppliedLaunchVars::default();
    typed
        .insert(
            "repo".into(),
            CanonicalLaunchVarValue::String("https://example.test/repo.git".into()),
        )
        .unwrap();
    typed
        .insert("count".into(), CanonicalLaunchVarValue::Number("3".into()))
        .unwrap();
    typed
        .insert("debug".into(), CanonicalLaunchVarValue::Boolean(true))
        .unwrap();
    typed
        .insert(
            "mode".into(),
            CanonicalLaunchVarValue::String("safe".into()),
        )
        .unwrap();
    let vars = resolve_launch_vars("agent", &launch, &typed).unwrap();
    assert_eq!(vars["count"], "3");
    assert_eq!(vars["debug"], "true");
}

#[test]
fn launch_var_resolution_renders_command_env_cwd_and_steps_from_canonical_values() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
target = "default"
cwd = "/repo-{var.repo}-{var.count}"
env = { REPO = "{var.repo}", DEBUG = "{var.debug}", COUNT = "{var.count}", MODE = "{var.mode}" }
command = ["launch-command", "{var.repo}", "{var.mode}", "{var.debug}", "{var.count}"]

[launches.agent.vars]
repo = { type = "string", required = true }
count = { type = "number", default = 1 }
debug = { type = "boolean", default = false }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }

[[launches.agent.steps]]
phase = "post_ready"
location = "container"
name = "prepare"
cwd = "/step-{var.mode}-{var.count}"
env = { STEP_REPO = "{var.repo}", STEP_DEBUG = "{var.debug}" }
command = ["step-command", "{var.repo}", "{var.count}"]
"#,
    )
    .unwrap();
    let launch = cfg.effective_launch("agent").unwrap();
    let supplied = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=alpha".into(),
        "count=2.0".into(),
        "debug=true".into(),
        "mode=safe".into(),
    ])
    .unwrap();
    let resolved = resolve_launch_vars("agent", &launch, &supplied).unwrap();
    let vars = launch_template_vars(&runtime, &resolved, Some("123"));

    let launch_env = render_template_map(&launch.env, &vars).unwrap();
    assert_eq!(launch_env["REPO"], "alpha");
    assert_eq!(launch_env["DEBUG"], "true");
    assert_eq!(launch_env["COUNT"], "2");
    assert_eq!(launch_env["MODE"], "safe");

    let step = &launch.steps[0];
    let step_cwd = render_launch_cwd(
        step.cwd.as_deref(),
        &vars,
        runtime.identity.container_home.as_path(),
    )
    .unwrap();
    assert_eq!(step_cwd, Some(PathBuf::from("/step-safe-2")));
    let step_env =
        launch_container_step_env(&BTreeMap::new(), &launch_env, &step.env, &vars).unwrap();
    assert_eq!(step_env["STEP_REPO"], "alpha");
    assert_eq!(step_env["STEP_DEBUG"], "true");
    assert_eq!(
        template::render_argv(&step.command, &vars).unwrap(),
        ["step-command", "alpha", "2"]
    );

    let final_cwd = render_launch_cwd(
        launch.cwd.as_deref(),
        &vars,
        runtime.identity.container_home.as_path(),
    )
    .unwrap();
    assert_eq!(final_cwd, Some(PathBuf::from("/repo-alpha-2")));
    let final_env = launch_final_env(&BTreeMap::new(), &launch_env);
    assert_eq!(final_env["COUNT"], "2");
    assert_eq!(
        template::render_argv(&launch.command, &vars).unwrap(),
        ["launch-command", "alpha", "safe", "true", "2"]
    );
}

#[tokio::test]
async fn operation_boundary_classifies_launch_variable_errors() {
    let duplicate = SshGatewayOperation::from_action(&GatewayAction::LaunchRun {
        name: "agent".into(),
        session_id: None,
        vars: vec!["repo=a".into(), "repo=b".into()],
    })
    .unwrap_err();
    assert_invalid_launch_variable(duplicate, "duplicate launch variable");

    let cfg = launch_var_boundary_config();

    let unknown = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=https://example.test/repo.git".into(),
        "extra=value".into(),
    ])
    .unwrap();
    let err = launch_execute_with_config(
        cfg.clone(),
        "agent",
        None,
        unknown,
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap_err();
    assert_invalid_launch_variable(err, "unknown launch variable");

    let err = launch_execute_with_config(
        cfg.clone(),
        "agent",
        None,
        SuppliedLaunchVars::default(),
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap_err();
    assert_invalid_launch_variable(err, "missing required launch variable");

    let invalid_enum = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=https://example.test/repo.git".into(),
        "mode=slow".into(),
    ])
    .unwrap();
    let err = launch_execute_with_config(
        cfg.clone(),
        "agent",
        None,
        invalid_enum,
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap_err();
    assert_invalid_launch_variable(err, "invalid enum launch variable");

    let invalid_number = SuppliedLaunchVars::from_cli_pairs(vec![
        "repo=https://example.test/repo.git".into(),
        "count=abc".into(),
    ])
    .unwrap();
    let err = launch_execute_with_config(
        cfg.clone(),
        "agent",
        None,
        invalid_number,
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap_err();
    assert_invalid_launch_variable(err, "invalid number launch variable");

    let mut invalid_type = SuppliedLaunchVars::default();
    invalid_type
        .insert("repo".into(), CanonicalLaunchVarValue::Boolean(true))
        .unwrap();
    let err = launch_execute_with_config(
        cfg,
        "agent",
        None,
        invalid_type,
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap_err();
    assert_invalid_launch_variable(err, "invalid string launch variable");
}

#[tokio::test]
async fn operation_boundary_classifies_unknown_launch_and_session_errors() {
    let cfg = launch_var_boundary_config();
    let err = launch_execute_with_config(
        cfg,
        "missing",
        None,
        SuppliedLaunchVars::default(),
        OperationExecutionOptions::STREAM,
    )
    .await
    .unwrap_err();
    let OperationError::UnknownLaunch { message } = err else {
        panic!("expected unknown launch error, got {err:?}");
    };
    assert_eq!(message, "unknown launch \"missing\"");

    let dir = tempfile::tempdir().unwrap();
    let fixed_config = write_gateway_config(
        &dir,
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    );
    let err = execute_gateway_operation(
        Some(fixed_config),
        GatewayOperation::Status {
            target: Some("default".into()),
            session_id: Some("abc123def456".into()),
        },
    )
    .await
    .unwrap_err();
    assert_invalid_session(err, "--session-id is only valid for ephemeral targets");

    let dir = tempfile::tempdir().unwrap();
    let ephemeral_config = write_gateway_config(
        &dir,
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{image_slug}-{session_id}"
stop_when_idle = true
"#,
    );
    let err = execute_gateway_operation(
        Some(ephemeral_config.clone()),
        GatewayOperation::Status {
            target: Some("default".into()),
            session_id: Some("..".into()),
        },
    )
    .await
    .unwrap_err();
    assert_invalid_session(err, "invalid session id");

    let err = execute_gateway_operation(
        Some(ephemeral_config),
        GatewayOperation::Status {
            target: Some("default".into()),
            session_id: None,
        },
    )
    .await
    .unwrap_err();
    assert_invalid_session(err, "requires --session-id");
}

#[test]
fn status_json_serializes_nullable_launch_fields() {
    let status = GatewayStatus {
        target: "default".into(),
        session_id: None,
        launch: None,
        mode: "fixed".into(),
        user: "alice".into(),
        image: "ubuntu/dev".into(),
        container: Some("ubuntu-dev".into()),
        container_pid: Some(123),
        active_sessions: 1,
        sessions: vec![model::SessionStatus {
            id: "s1".into(),
            kind: "run-command".into(),
            gateway_pid: 1234,
            container: "ubuntu-dev".into(),
            target: "default".into(),
            launch: None,
            created_at_ms: 10,
        }],
        agent_ready: false,
        ssh_socket: PathBuf::from("/tmp/ssh.sock"),
        status: "container-running".into(),
        agent: None,
    };
    let value = serde_json::to_value(&status).unwrap();
    assert!(value.get("launch").unwrap().is_null());
    assert!(value["sessions"][0].get("launch").unwrap().is_null());

    let all = AllStatusEntry {
        target: "default".into(),
        session_id: None,
        launch: None,
        mode: "fixed".into(),
        user: "alice".into(),
        uid: "2450".into(),
        image: "ubuntu/dev".into(),
        container: "ubuntu-dev".into(),
        status: "running".into(),
    };
    let value = serde_json::to_value(&all).unwrap();
    assert!(value.get("launch").unwrap().is_null());
}

#[test]
fn lifecycle_result_text_preserves_stop_and_remove_messages() {
    assert_eq!(
        stop_result_text(&StopResult {
            container: "ubuntu-dev".into(),
            stopped: true,
        }),
        "stopped ubuntu-dev"
    );
    assert_eq!(
        remove_result_text(&RemoveResult {
            container: "ubuntu-dev".into(),
            removed: true,
        }),
        "removed ubuntu-dev"
    );
}

#[test]
fn readiness_plan_skips_pre_start_for_running_container() {
    let running = inspect_with_running(true);
    let stopped = inspect_with_running(false);

    match readiness_plan(Some(running.clone())) {
        ContainerReadinessPlan::ReuseRunning(inspect) => assert_eq!(inspect.name, running.name),
        other => panic!("expected running container reuse, got {other:?}"),
    }
    match readiness_plan(Some(stopped.clone())) {
        ContainerReadinessPlan::StartStopped(inspect) => assert_eq!(inspect.name, stopped.name),
        other => panic!("expected stopped container start, got {other:?}"),
    }
    assert!(matches!(
        readiness_plan(None),
        ContainerReadinessPlan::CreateMissing
    ));
}

#[tokio::test]
async fn ensure_ready_reuse_running_preserves_control_socket_files() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(0));
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.host_steps.clear();
    });
    runtime.prepare_control_socket_dir().unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_agent_socket, "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_ssh_socket, "").unwrap();

    runtime.ensure_ready().await.unwrap();

    assert!(runtime.paths.control_sockets.host_agent_socket.exists());
    assert!(runtime.paths.control_sockets.host_ssh_socket.exists());
}

#[tokio::test]
async fn ensure_ready_start_stopped_removes_stale_control_socket_files_before_start() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let state = dir.path().join("running");
    let log = dir.path().join("start.log");
    let user = UserContext::current().unwrap();
    write_fake_runtime(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    if [ -f "{state}" ]; then
      running=true
    else
      running=false
    fi
    cat <<JSON
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":$running,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  start)
    if [ -e "{agent_socket}" ] || [ -e "{ssh_socket}" ]; then
      echo stale-present > "{log}"
      exit 9
    fi
    echo clean > "{log}"
    touch "{state}"
    ;;
esac
exit 0
"#,
            state = state.display(),
            log = log.display(),
            agent_socket = dir.path().join("runtime-sockets/agent.sock").display(),
            ssh_socket = dir.path().join("runtime-sockets/ssh.sock").display(),
            user = user.user,
            uid = user.uid,
        ),
    );
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.lifecycle_steps.clear();
        cfg.target_defaults.host_steps.clear();
    });
    runtime.prepare_control_socket_dir().unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_agent_socket, "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_ssh_socket, "").unwrap();

    runtime.ensure_ready().await.unwrap();

    assert_eq!(std::fs::read_to_string(log).unwrap(), "clean\n");
    assert!(!runtime.paths.control_sockets.host_agent_socket.exists());
    assert!(!runtime.paths.control_sockets.host_ssh_socket.exists());
}

#[tokio::test]
async fn ensure_ready_cleans_failed_runtime_start_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("cleanup.log");
    let user = UserContext::current().unwrap();
    write_fake_runtime(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<JSON
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":false,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  start)
    echo start >> "{log}"
    exit 7
    ;;
  stop)
    echo stop >> "{log}"
    ;;
esac
exit 0
"#,
            log = log.display(),
            user = user.user,
            uid = user.uid,
        ),
    );
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.lifecycle_steps.clear();
        cfg.target_defaults.host_steps.clear();
    });

    let err = runtime.ensure_ready().await.unwrap_err();

    assert!(err.to_string().contains("start"));
    assert_eq!(std::fs::read_to_string(log).unwrap(), "start\nstop\n");
    assert!(!runtime.paths.control_sockets.host_dir.exists());
}

#[tokio::test]
async fn ensure_ready_create_missing_removes_stale_control_socket_files_before_run() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let state = dir.path().join("running");
    let log = dir.path().join("run.log");
    let user = UserContext::current().unwrap();
    write_fake_runtime(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    if [ ! -f "{state}" ]; then
      echo "container not found" >&2
      exit 1
    fi
    cat <<JSON
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  run)
    if [ -e "{agent_socket}" ] || [ -e "{ssh_socket}" ]; then
      echo stale-present > "{log}"
      exit 9
    fi
    echo clean > "{log}"
    touch "{state}"
    ;;
esac
exit 0
"#,
            state = state.display(),
            log = log.display(),
            agent_socket = dir.path().join("runtime-sockets/agent.sock").display(),
            ssh_socket = dir.path().join("runtime-sockets/ssh.sock").display(),
            user = user.user,
            uid = user.uid,
        ),
    );
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.lifecycle_steps.clear();
        cfg.target_defaults.host_steps.clear();
    });
    runtime.prepare_control_socket_dir().unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_agent_socket, "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_ssh_socket, "").unwrap();

    runtime.ensure_ready().await.unwrap();

    assert_eq!(std::fs::read_to_string(log).unwrap(), "clean\n");
    assert!(!runtime.paths.control_sockets.host_agent_socket.exists());
    assert!(!runtime.paths.control_sockets.host_ssh_socket.exists());
}

#[tokio::test]
async fn runtime_load_rejects_rendered_passwd_delimiters() {
    for (field, identity_line) in [
        ("session_user", r#"session_user = "bad:user""#),
        ("session_home", r#"session_home = "/home/bad:user""#),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"

[targets.default.identity]
{identity_line}
"#,
                dir.path().join("workspace").display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(Some(config), Some("default"), None, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(field),
            "expected {field} in error, got {err}"
        );
    }
}

#[test]
fn unix_socket_path_inventory_includes_host_and_container_paths() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
        enable_default_ssh_bridge(cfg);
    });

    let labels = runtime
        .effective_unix_socket_paths()
        .unwrap()
        .into_iter()
        .map(|(label, _)| label)
        .collect::<Vec<_>>();

    assert!(labels.contains(&"host agent socket path"));
    assert!(labels.contains(&"host ssh socket path"));
    assert!(labels.contains(&"container agent socket path"));
    assert!(labels.contains(&"container ssh socket path"));
}

#[tokio::test]
async fn runtime_load_rejects_overlong_generated_host_socket_path() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let host_dir = dir.path().join("h".repeat(120)).join("{runtime_id}");
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
host_dir = "{}"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{{image_slug}}-{{session_id}}"
stop_when_idle = true
"#,
            workspace.display(),
            host_dir.display(),
        ),
    )
    .unwrap();

    let err = Runtime::load(Some(config), Some("default"), None, true)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("host agent socket path"), "{err}");
    assert!(err.contains("too long for a Unix domain socket"), "{err}");
    assert!(err.contains("control_sockets.host_dir"), "{err}");
}

#[tokio::test]
async fn runtime_load_uses_explicit_ephemeral_session_id_for_names() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{{image_slug}}-{{session_id}}"
stop_when_idle = true
"#,
            workspace.display(),
        ),
    )
    .unwrap();

    let runtime = Runtime::load(
        Some(config),
        Some("default"),
        Some("abc123def456".into()),
        true,
    )
    .await
    .unwrap();

    assert_eq!(runtime.identity.session_id.as_deref(), Some("abc123def456"));
    assert_eq!(runtime.identity.container_name, "ubuntu-dev-abc123def456");
    assert!(
        runtime
            .paths
            .container_state_dir
            .ends_with(".aw-gateway/sessions/abc123def456")
    );
    assert_eq!(
        runtime.paths.control_sockets.host_agent_socket,
        PathBuf::from(format!(
            "/run/user/{}/aw-gateway/abc123def456/agent.sock",
            runtime.identity.user.uid
        ))
    );
    assert_eq!(
        runtime.paths.control_sockets.host_ssh_socket,
        PathBuf::from(format!(
            "/run/user/{}/aw-gateway/abc123def456/ssh.sock",
            runtime.identity.user.uid
        ))
    );
    assert_eq!(
        runtime.paths.control_sockets.container_agent_socket,
        PathBuf::from("/run/aw-gateway/agent.sock")
    );
    assert_eq!(
        runtime.paths.control_sockets.container_ssh_socket,
        PathBuf::from("/run/aw-gateway/ssh.sock")
    );
}

#[tokio::test]
async fn runtime_load_uses_fixed_target_id_for_default_control_socket_runtime_id() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"
default_target = "dev-shell"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.dev-shell]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
            workspace.display(),
        ),
    )
    .unwrap();

    let runtime = Runtime::load(Some(config), Some("dev-shell"), None, true)
        .await
        .unwrap();

    assert_eq!(runtime.identity.session_id, None);
    assert_eq!(
        runtime.paths.control_sockets.host_agent_socket,
        PathBuf::from(format!(
            "/run/user/{}/aw-gateway/dev-shell/agent.sock",
            runtime.identity.user.uid
        ))
    );
    assert_eq!(
        runtime.paths.control_sockets.host_ssh_socket,
        PathBuf::from(format!(
            "/run/user/{}/aw-gateway/dev-shell/ssh.sock",
            runtime.identity.user.uid
        ))
    );
    assert_eq!(
        runtime.paths.control_sockets.container_agent_socket,
        PathBuf::from("/run/aw-gateway/agent.sock")
    );
    assert_eq!(
        runtime.paths.control_sockets.container_ssh_socket,
        PathBuf::from("/run/aw-gateway/ssh.sock")
    );
}

#[tokio::test]
async fn runtime_load_applies_global_and_target_control_socket_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let global_host = dir.path().join("global/{runtime_id}");
    let target_host = dir.path().join("target/{runtime_id}");
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"
default_target = "global"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
host_dir = "{}"
container_dir = "/run/global-aw"

[targets.global]
image = "ubuntu/global"
mode = "fixed"
name = "{{image_slug}}"

[targets.targeted]
image = "ubuntu/targeted"
mode = "fixed"
name = "{{image_slug}}"

[targets.targeted.control_sockets]
host_dir = "{}"
container_dir = "/tmp/aw-gateway"
"#,
            workspace.display(),
            global_host.display(),
            target_host.display(),
        ),
    )
    .unwrap();

    let global = Runtime::load(Some(config.clone()), Some("global"), None, true)
        .await
        .unwrap();
    assert_eq!(
        global.paths.control_sockets.host_agent_socket,
        dir.path().join("global/global/agent.sock")
    );
    assert_eq!(
        global.paths.control_sockets.container_ssh_socket,
        PathBuf::from("/run/global-aw/ssh.sock")
    );

    let targeted = Runtime::load(Some(config), Some("targeted"), None, true)
        .await
        .unwrap();
    assert_eq!(
        targeted.paths.control_sockets.host_agent_socket,
        dir.path().join("target/targeted/agent.sock")
    );
    assert_eq!(
        targeted.paths.control_sockets.container_ssh_socket,
        PathBuf::from("/tmp/aw-gateway/ssh.sock")
    );
}

#[tokio::test]
async fn runtime_load_rejects_relative_control_socket_dirs() {
    for (field, config_fragment) in [
        (
            "control_sockets.host_dir",
            r#"[target_defaults.control_sockets]
host_dir = "relative/{runtime_id}"
"#,
        ),
        (
            "control_sockets.container_dir",
            r#"[target_defaults.control_sockets]
container_dir = "relative-container"
"#,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

{config_fragment}
[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                dir.path().join("workspace").display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(Some(config), Some("default"), None, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(field), "{err}");
        assert!(err.contains("absolute path"), "{err}");
    }
}

#[tokio::test]
async fn runtime_load_rejects_unsafe_control_socket_runtime_ids_and_host_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "{{image_slug}}-{{session_id}}"
stop_when_idle = true
"#,
            dir.path().join("workspace").display(),
        ),
    )
    .unwrap();

    let err = Runtime::load(Some(config), Some("default"), Some("..".into()), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid session id"), "{err}");

    for (host_dir, expected) in [
        ("/tmp", "dangerous shared path"),
        ("/run/user/{uid}", "dangerous shared path"),
        ("/run/user/{uid}/aw-gateway", "must end with runtime_id"),
        (
            "/run/user/{uid}/aw-gateway/../{runtime_id}",
            "must not contain '.' or '..'",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
host_dir = "{host_dir}"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
                dir.path().join("workspace").display(),
            ),
        )
        .unwrap();

        let err = Runtime::load(Some(config), Some("default"), None, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{err}");
    }
}

#[tokio::test]
async fn runtime_load_rejects_explicit_session_id_for_fixed_target() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
            workspace.display(),
        ),
    )
    .unwrap();

    let err = Runtime::load(
        Some(config),
        Some("default"),
        Some("abc123def456".into()),
        true,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(
        err.contains("--session-id is only valid for ephemeral targets"),
        "{err}"
    );
}

#[tokio::test]
async fn runtime_load_rejects_overlong_explicit_container_socket_path() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let container_dir = format!("/home/{}/aw-gateway", "b".repeat(100));
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"

[target_defaults.workspace]
path = "{}"
state_dir = ".aw-gateway"

[target_defaults.control_sockets]
container_dir = "{container_dir}"

[target_defaults.container_agent]
control_socket = false

[target_defaults.container_agent.ssh_bridge]
enabled = true
target = "127.0.0.1:22"
mode = "0600"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
            workspace.display(),
        ),
    )
    .unwrap();

    let err = Runtime::load(Some(config), Some("default"), None, false)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("container ssh socket path"), "{err}");
    assert!(err.contains("too long for a Unix domain socket"), "{err}");
    assert!(err.contains(&container_dir), "{err}");
    assert!(err.contains("control_sockets.container_dir"), "{err}");
}

#[test]
fn podman_run_args_start_agent_as_root_with_workspace_and_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);

    let old_labels = runtime.validation_labels();
    assert!(!old_labels.contains_key("io.aw-gateway.mode"));
    assert!(!old_labels.contains_key("io.aw-gateway.session_id"));
    runtime
        .validate_labels(&ContainerInspect {
            id: "old-id".into(),
            name: "ubuntu-dev".into(),
            state: runtime::ContainerState {
                running: true,
                pid: 123,
            },
            config: runtime::ContainerConfig { labels: old_labels },
        })
        .unwrap();

    let args = runtime.container_runtime.run_args(
        &runtime
            .container_run_spec(Some("identity-token"), Some("control-token"))
            .unwrap(),
    );
    let arg = |value: &str| args.iter().position(|item| item == value);

    assert!(args.contains(&"--userns=keep-id".to_string()));
    assert_eq!(arg("--user").map(|idx| args[idx + 1].as_str()), Some("0:0"));
    assert!(args.contains(&"--init".to_string()));
    assert!(args.contains(&"--passwd-entry".to_string()));
    assert!(args.contains(&"alice:x:2450:2450:alice:/home/alice:/bin/bash".to_string()));
    assert!(args.contains(&format!(
        "{}:/home/alice:Z",
        runtime.paths.workspace.display()
    )));
    assert!(args.contains(&"AW_IDENTITY_TOKEN=identity-token".to_string()));
    assert!(args.contains(&"AW_CONTAINER_CONTROL_TOKEN=control-token".to_string()));
    assert!(args.contains(&"AW_AUTHENTICATED_UID=2450".to_string()));
    assert!(args.contains(&"AW_AUTHENTICATED_GID=2450".to_string()));
    assert!(args.contains(&"io.aw-gateway.gateway=true".to_string()));
    assert!(args.contains(&"io.aw-gateway.target=default".to_string()));
    assert!(args.contains(&"io.aw-gateway.mode=fixed".to_string()));
    assert!(args.contains(&format!(
        "{}:/run/aw-gateway:Z",
        dir.path().join("runtime-sockets").display()
    )));
    assert!(args.contains(&"localhost/ubuntu/dev:latest".to_string()));
    assert_eq!(
        &args[args.len() - 3..],
        [
            "--config",
            "/home/alice/.aw-gateway/containers/ubuntu-dev/container-agent.toml",
            "run"
        ]
    );
    assert!(args.iter().any(|arg| arg == "aw-container-agent"));
}

#[test]
fn prepare_control_socket_dir_is_private_and_preserves_socket_files() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
        enable_default_ssh_bridge(cfg);
    });

    std::fs::create_dir_all(&runtime.paths.control_sockets.host_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &runtime.paths.control_sockets.host_dir,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    std::fs::write(&runtime.paths.control_sockets.host_agent_socket, "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_ssh_socket, "").unwrap();

    runtime.prepare_control_socket_dir().unwrap();

    assert!(runtime.paths.control_sockets.host_dir.is_dir());
    assert!(runtime.paths.control_sockets.host_agent_socket.exists());
    assert!(runtime.paths.control_sockets.host_ssh_socket.exists());
}

#[test]
fn remove_stale_control_socket_files_removes_socket_paths() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
        enable_default_ssh_bridge(cfg);
    });

    std::fs::create_dir_all(&runtime.paths.control_sockets.host_dir).unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_agent_socket, "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_ssh_socket, "").unwrap();

    runtime.remove_stale_control_socket_files().unwrap();

    assert!(!runtime.paths.control_sockets.host_agent_socket.exists());
    assert!(!runtime.paths.control_sockets.host_ssh_socket.exists());
}

#[cfg(unix)]
#[test]
fn prepare_control_socket_dir_rejects_symlink_and_non_private_existing_dir() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    let target = dir.path().join("real-runtime-dir");
    std::fs::create_dir_all(&target).unwrap();
    symlink(&target, &runtime.paths.control_sockets.host_dir).unwrap();
    let err = runtime
        .prepare_control_socket_dir()
        .unwrap_err()
        .to_string();
    assert!(err.contains("must not be a symlink"), "{err}");
    assert!(target.is_dir());

    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    std::fs::create_dir_all(&runtime.paths.control_sockets.host_dir).unwrap();
    std::fs::set_permissions(
        &runtime.paths.control_sockets.host_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let err = runtime
        .prepare_control_socket_dir()
        .unwrap_err()
        .to_string();
    assert!(err.contains("exists with permissions 755"), "{err}");
    let mode = std::fs::symlink_metadata(&runtime.paths.control_sockets.host_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755);
}

#[test]
fn cleanup_control_socket_dir_removes_only_runtime_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |cfg| {
        enable_default_ssh_bridge(cfg);
    });
    let parent = runtime
        .paths
        .control_sockets
        .host_dir
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(&runtime.paths.control_sockets.host_dir).unwrap();
    std::fs::write(parent.join("parent-marker"), "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_agent_socket, "").unwrap();
    std::fs::write(&runtime.paths.control_sockets.host_ssh_socket, "").unwrap();

    runtime.cleanup_control_socket_dir();

    assert!(!runtime.paths.control_sockets.host_dir.exists());
    assert!(parent.join("parent-marker").exists());
    assert!(parent.is_dir());
}

#[cfg(unix)]
#[test]
fn cleanup_control_socket_dir_refuses_symlink_deletion_root() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime(&dir, dir.path().join("runtime"), |_| {});
    let target = dir.path().join("real-runtime-dir");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("marker"), "").unwrap();
    symlink(&target, &runtime.paths.control_sockets.host_dir).unwrap();

    runtime.cleanup_control_socket_dir();

    assert!(target.join("marker").exists());
    assert!(runtime.paths.control_sockets.host_dir.exists());
}

#[test]
fn target_workspace_override_resolves_relative_to_user_home() {
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let target = cfg.targets.get_mut("default").unwrap();
    target.workspace = Some(crate::config::WorkspaceConfigInput {
        path: Some("{home}/workspace-internal".into()),
        ..Default::default()
    });
    let user = UserContext {
        uid: 2450,
        gid: 2450,
        user: "alice".into(),
        home: PathBuf::from("/home/alice"),
    };

    let workspace = resolve_target_workspace(
        &cfg.effective_target("default").unwrap(),
        "default",
        &user,
        None,
    )
    .unwrap();

    assert_eq!(workspace, PathBuf::from("/home/alice/workspace-internal"));
}

#[test]
fn target_service_override_is_written_to_container_agent_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    let mut override_service = cfg
        .effective_target("default")
        .unwrap()
        .container_agent
        .services
        .iter()
        .find(|service| service.name == "acl-proxy")
        .unwrap()
        .clone();
    override_service.command = vec![
        "acl-proxy".into(),
        "--config".into(),
        "/etc/acl-proxy/internal-acl-proxy.toml".into(),
    ];
    cfg.targets.get_mut("default").unwrap().container_agent =
        Some(crate::config::ContainerAgentConfigInput {
            services: vec![override_service],
            ..Default::default()
        });
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    std::fs::create_dir_all(&container_state_dir).unwrap();
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);

    let agent_path = runtime.write_container_agent_config().unwrap();
    assert_file_mode(&agent_path, 0o600);
    let agent_config = std::fs::read_to_string(agent_path).unwrap();
    assert!(agent_config.contains("/etc/acl-proxy/internal-acl-proxy.toml"));
    assert!(!agent_config.contains("/etc/acl-proxy/acl-proxy.toml"));
    assert_eq!(agent_config.matches("name = \"acl-proxy\"").count(), 1);
}

#[test]
fn gateway_status_distinguishes_agent_unready_from_ready() {
    assert_eq!(
        gateway_status_name(false, true, false, false),
        "not-running"
    );
    assert_eq!(
        gateway_status_name(true, false, false, false),
        "container-running"
    );
    assert_eq!(
        gateway_status_name(true, true, false, false),
        "container-running-agent-unavailable"
    );
    assert_eq!(
        gateway_status_name(true, true, true, false),
        "container-running-agent-not-ready"
    );
    assert_eq!(gateway_status_name(true, true, true, true), "ready");
}

#[test]
fn typed_agent_control_failure_response_is_reported() {
    let err = Runtime::parse_agent_control_success::<SessionHoldResult>(
            r#"{"id":"hold","ok":false,"error":{"code":"unauthorized","message":"control token is required"}}"#,
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("agent control request failed: unauthorized"));
    assert!(err.contains("control token is required"));
}

#[test]
fn typed_agent_control_ok_false_without_error_is_reported() {
    let err = Runtime::parse_agent_control_success::<SessionHoldResult>(
        r#"{"id":"hold","ok":false,"result":{"held":true}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("agent control request returned ok=false without error"));
    assert!(err.contains("hold"));
}

#[test]
fn typed_agent_status_parses_remote_smoke_response_shape() {
    let response = Runtime::parse_agent_control_success::<AgentStatus>(
            r#"{"id":"status","ok":true,"result":{"ready":true,"version":"0.2.0","services":[{"name":"container-sshd","required":true,"state":"running","pid":13,"healthy":true,"restart_count":0,"last_error":null}],"ssh_bridge":{"enabled":true,"ready":true,"active_streams":0,"active_sessions":0},"idle_cleanup":{"owner":"agent","action":"exit_container","state":"idle_pending","idle_for_ms":11947,"preserve":false,"preserve_reason":null,"matched_processes":[],"last_reap_result":null},"shutting_down":false}}"#,
        )
        .unwrap();
    assert!(response.result.ready);
}

#[tokio::test]
async fn typed_agent_status_ready_drives_wait_and_runtime_status() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(0));
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.container_agent = Some(crate::config::ContainerAgentConfigInput {
            enabled: Some(true),
            services: Vec::new(),
            ssh_bridge: None,
            control_socket: None,
            idle_cleanup: None,
        });
    });
    let agent_server = bind_fake_agent_control(
        &runtime.paths.control_sockets.host_agent_socket,
        agent_status_response(true),
    );

    runtime.wait_agent_ready().await.unwrap();
    let status = runtime.status().await.unwrap();

    assert!(status.agent_ready);
    assert_eq!(status.status, "ready");
    assert!(status.agent.as_ref().unwrap().ready);
    assert!(status.agent.as_ref().unwrap().ssh_bridge.ready);

    agent_server.abort();
}

#[tokio::test]
async fn malformed_typed_agent_status_is_agent_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(0));
    let runtime = test_runtime(&dir, fake_runtime, |cfg| {
        cfg.target_defaults.container_agent = Some(crate::config::ContainerAgentConfigInput {
            enabled: Some(true),
            services: Vec::new(),
            ssh_bridge: None,
            control_socket: None,
            idle_cleanup: None,
        });
    });
    let agent_server = bind_fake_agent_control(
        &runtime.paths.control_sockets.host_agent_socket,
        r#"{"id":"status","ok":true,"result":{"ready":"yes"}}
"#
        .to_string(),
    );

    let status = runtime.status().await.unwrap();

    assert!(!status.agent_ready);
    assert!(status.agent.is_none());
    assert_eq!(status.status, "container-running-agent-unavailable");

    agent_server.abort();
}

#[test]
fn container_run_env_does_not_include_target_session_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.targets
        .get_mut("default")
        .unwrap()
        .container_env
        .insert("START_ONLY".into(), "start".into());
    cfg.targets
        .get_mut("default")
        .unwrap()
        .session_env
        .insert("SESSION_ONLY".into(), "session".into());
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);
    let spec = runtime.container_run_spec(None, None).unwrap();
    assert_eq!(spec.env.get("START_ONLY"), Some(&"start".to_string()));
    assert!(!spec.env.contains_key("SESSION_ONLY"));

    let exec_env = runtime.session_env().unwrap();
    assert_eq!(
        exec_env.get("SHELL").map(String::as_str),
        Some(DEFAULT_SESSION_SHELL_ENV)
    );
    assert_eq!(exec_env.get("SESSION_ONLY"), Some(&"session".to_string()));

    std::fs::create_dir_all(&runtime.paths.container_state_dir).unwrap();
    let env_path = runtime.write_sshd_session_env_config().unwrap();
    assert_file_mode(&env_path, 0o600);
    let env_config = std::fs::read_to_string(env_path).unwrap();
    assert!(env_config.contains("SESSION_ONLY=session"));
}

#[test]
fn disabled_agent_run_spec_uses_plain_sleep_without_agent_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    disable_default_container_agent(&mut cfg);
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);

    let args = runtime
        .container_runtime
        .run_args(&runtime.container_run_spec(None, None).unwrap());

    assert_eq!(&args[args.len() - 2..], ["sleep", "infinity"]);
    assert!(!args.iter().any(|arg| arg == "aw-container-agent"));
    assert!(!args.iter().any(|arg| arg.starts_with("AW_IDENTITY_TOKEN=")));
    assert!(
        !args
            .iter()
            .any(|arg| arg.starts_with("AW_CONTAINER_CONTROL_TOKEN="))
    );
}

#[test]
fn writes_container_ssh_policy_and_injects_sshd_policy_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.targets.get_mut("default").unwrap().container_ssh =
        Some(crate::config::TargetContainerSshConfig {
            transfer: Some(crate::config::TargetContainerSshTransferConfig {
                sftp: Some(crate::config::SftpTransferMode::Deny),
                legacy_scp: Some(crate::config::LegacyScpTransferMode::Deny),
            }),
        });
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    std::fs::create_dir_all(&container_state_dir).unwrap();
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);

    let policy_path = runtime.write_ssh_command_filter_policy().unwrap();
    assert_file_mode(&policy_path, 0o600);
    let policy = std::fs::read_to_string(policy_path).unwrap();
    assert!(policy.contains("sftp = \"deny\""));
    assert!(policy.contains("legacy_scp = \"deny\""));

    let agent_path = runtime.write_container_agent_config().unwrap();
    assert_file_mode(&agent_path, 0o600);
    let agent_config = std::fs::read_to_string(agent_path).unwrap();
    assert!(agent_config.contains("AW_SSHD_POLICY_CONFIG"));
    assert!(
        agent_config
            .contains("/home/alice/.aw-gateway/containers/ubuntu-dev/ssh-command-filter.toml")
    );
    assert!(agent_config.contains("AW_SSHD_SETENV_CONFIG"));
    assert!(
        agent_config
            .contains("/home/alice/.aw-gateway/containers/ubuntu-dev/sshd-session-env.conf")
    );
    assert!(agent_config.contains("control_socket = \"/run/aw-gateway/agent.sock\""));
    assert!(agent_config.contains("socket = \"/run/aw-gateway/ssh.sock\""));
    assert!(!agent_config.contains("/home/alice/.aw-gateway/containers/ubuntu-dev/agent.sock"));
    assert!(!agent_config.contains("/home/alice/.aw-gateway/containers/ubuntu-dev/ssh.sock"));
}

#[test]
fn bootstrap_enabled_run_spec_uses_bootstrap_entrypoint_and_mounts() {
    let dir = tempfile::tempdir().unwrap();
    let bootstrap_agent = dir.path().join("bootstrap/aw-container-agent");
    std::fs::create_dir_all(bootstrap_agent.parent().unwrap()).unwrap();
    std::fs::write(&bootstrap_agent, "").unwrap();
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.target_defaults
        .container_bootstrap_steps
        .push(crate::config::RawContainerBootstrapStep {
            name: "global-bootstrap".into(),
            enabled: true,
            before: None,
            after: None,
            required: Some(true),
            user: Some("root".into()),
            command: Some(vec!["/bin/global".into()]),
            timeout: None,
        });
    let target_cfg = cfg.targets.get_mut("default").unwrap();
    target_cfg.container_bootstrap = Some(crate::config::TargetContainerBootstrapConfig {
        enabled: Some(true),
        entrypoint: Some("/opt/aw-gateway/bin/target-bootstrap".into()),
        agent_program: Some("/opt/aw-gateway/bin/target-agent".into()),
    });
    target_cfg.container_bootstrap_steps = vec![
        crate::config::RawContainerBootstrapStep {
            name: "global-bootstrap".into(),
            enabled: false,
            before: None,
            after: None,
            required: None,
            user: None,
            command: None,
            timeout: None,
        },
        crate::config::RawContainerBootstrapStep {
            name: "target-bootstrap".into(),
            enabled: true,
            before: None,
            after: None,
            required: Some(false),
            user: Some("root".into()),
            command: Some(vec!["/bin/target".into()]),
            timeout: Some("5s".into()),
        },
    ];
    cfg.target_defaults
        .container_mounts
        .push(crate::config::ContainerMountConfig {
            source: bootstrap_agent.display().to_string(),
            target: "/opt/aw-gateway/bin/aw-container-agent".into(),
            mode: ContainerMountMode::Ro,
        });
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);

    let args = runtime.container_runtime.run_args(
        &runtime
            .container_run_spec(Some("identity-token"), Some("control-token"))
            .unwrap(),
    );

    let expected_mount = format!(
        "{}:/opt/aw-gateway/bin/aw-container-agent:ro,Z",
        bootstrap_agent.canonicalize().unwrap().display()
    );
    assert!(args.iter().any(|arg| arg == &expected_mount));
    assert_eq!(
        &args[args.len() - 5..],
        [
            "/opt/aw-gateway/bin/target-bootstrap",
            "--config",
            "/home/alice/.aw-gateway/containers/ubuntu-dev/container-agent.toml",
            "--bootstrap-config",
            "/home/alice/.aw-gateway/containers/ubuntu-dev/container-bootstrap.toml",
        ]
    );

    std::fs::create_dir_all(&runtime.paths.container_state_dir).unwrap();
    let bootstrap_path = runtime.write_container_bootstrap_config().unwrap();
    assert_file_mode(&bootstrap_path, 0o600);
    let bootstrap_config = std::fs::read_to_string(bootstrap_path).unwrap();
    assert!(bootstrap_config.contains("agent_program = \"/opt/aw-gateway/bin/target-agent\""));
    assert!(bootstrap_config.contains("name = \"target-bootstrap\""));
    assert!(bootstrap_config.contains("command = [\"/bin/target\"]"));
    assert!(!bootstrap_config.contains("global-bootstrap"));
    assert!(!bootstrap_config.contains("enabled"));
    assert!(!bootstrap_config.contains("before"));
    assert!(!bootstrap_config.contains("after"));
}

#[test]
fn container_mounts_error_when_source_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-bootstrap-file");
    let mut cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.target_defaults
        .container_mounts
        .push(crate::config::ContainerMountConfig {
            source: missing.display().to_string(),
            target: "/opt/aw-gateway/bin/missing".into(),
            mode: ContainerMountMode::Ro,
        });
    let target = cfg.effective_target("default").unwrap();
    let container_runtime =
        ContainerRuntime::from_config(&cfg.runtime, "alice", Path::new("/home/alice")).unwrap();
    let container_state_dir = dir
        .path()
        .join("workspace/.aw-gateway/containers/ubuntu-dev");
    let runtime = test_alice_runtime(cfg, target, container_runtime, &dir, container_state_dir);

    let err = runtime.container_mounts().unwrap_err();
    assert!(
        err.to_string().contains("container mount source #0"),
        "{err:#}"
    );
}

#[test]
fn target_selection_accepts_configured_target_or_image() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    assert_eq!(
        resolve_target_selection(&cfg, Some("default")).unwrap(),
        "default"
    );
    assert_eq!(
        resolve_target_selection(&cfg, Some("ubuntu/dev")).unwrap(),
        "default"
    );
    assert_eq!(
        resolve_target_selection(&cfg, Some("localhost/ubuntu/dev:latest")).unwrap(),
        "default"
    );
    assert!(resolve_target_selection(&cfg, Some("fedora/dev")).is_err());
}

#[test]
fn configured_default_display_uses_configured_default_target() {
    let cfg: GatewayConfig = toml::from_str(DEFAULT_GATEWAY_CONFIG).unwrap();
    assert_eq!(configured_default_display(&cfg), "default");
}

#[test]
fn image_selection_normalizes_localhost_latest() {
    assert_eq!(normalize_image_selection("ubuntu/dev"), "ubuntu/dev");
    assert_eq!(
        normalize_image_selection("localhost/ubuntu/dev:latest"),
        "ubuntu/dev"
    );
}

#[test]
fn host_hook_timeout_defaults_to_sixty_seconds() {
    assert_eq!(host_hook_timeout(None).unwrap(), Duration::from_secs(60));
    assert_eq!(
        host_hook_timeout(Some("250ms")).unwrap(),
        Duration::from_millis(250)
    );
    assert!(host_hook_timeout(Some("5")).is_err());
}

#[test]
fn parses_proc_stat_start_time() {
    let stat = "123 (bash) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20";
    assert_eq!(parse_process_start_time(stat).unwrap(), "987654");
}

#[test]
fn detects_current_process_session_marker_as_active() {
    let marker = SessionMarker {
        id: "test".into(),
        kind: "connect".into(),
        gateway_pid: std::process::id(),
        gateway_start_time: process_start_time(std::process::id()).unwrap(),
        container: "ubuntu-dev".into(),
        target: "default".into(),
        launch: None,
        created_at_ms: 0,
    };
    assert!(session_marker_is_active(&marker));
}

#[test]
fn old_shape_session_marker_deserializes_without_launch() {
    let raw = r#"
{
  "id": "test",
  "kind": "run-command",
  "gateway_pid": 123,
  "gateway_start_time": "456",
  "container": "ubuntu-dev",
  "target": "default",
  "created_at_ms": 789
}
"#;
    let marker: SessionMarker = serde_json::from_str(raw).unwrap();
    assert_eq!(marker.launch, None);
}

#[test]
fn session_marker_launch_round_trips_and_none_is_omitted() {
    let marker = SessionMarker {
        id: "test".into(),
        kind: "launch".into(),
        gateway_pid: 123,
        gateway_start_time: "456".into(),
        container: "ubuntu-dev".into(),
        target: "default".into(),
        launch: Some("agent-pack-codex".into()),
        created_at_ms: 789,
    };
    let raw = serde_json::to_string(&marker).unwrap();
    let parsed: SessionMarker = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.launch.as_deref(), Some("agent-pack-codex"));

    let without_launch = SessionMarker {
        launch: None,
        ..marker
    };
    let raw = serde_json::to_string(&without_launch).unwrap();
    assert!(!raw.contains("\"launch\":"));
}

#[test]
fn detects_current_process_local_listener_status_as_active() {
    let status = LocalListenerStatus {
        gateway_pid: std::process::id(),
        gateway_start_time: process_start_time(std::process::id()).unwrap(),
        host: "127.0.0.1".into(),
        port: 40222,
        created_at_ms: 0,
    };
    assert!(local_listener_is_active(&status));
}

#[test]
fn public_key_validation_accepts_known_types() {
    assert!(is_plausible_public_key("ssh-ed25519 AAAAC3Nza comment"));
    assert!(!is_plausible_public_key("not-a-key"));
    assert!(validate_public_key_content("ssh-ed25519 AAAAC3Nza comment\n").is_ok());
    assert!(
        validate_public_key_content("ssh-ed25519 AAAAC3Nza one\nssh-ed25519 AAAAC3Nza two")
            .is_err()
    );
    assert!(validate_public_key_content(" ssh-ed25519 AAAAC3Nza").is_err());
}

#[test]
fn identity_token_validation_requires_single_non_empty_line() {
    let path = PathBuf::from("/tmp/token");
    assert_eq!(
        validate_identity_token_content("abc\n", &path).unwrap(),
        "abc"
    );
    assert!(validate_identity_token_content("", &path).is_err());
    assert!(validate_identity_token_content("a\nb", &path).is_err());
    assert!(validate_identity_token_content(&"x".repeat(4097), &path).is_err());
}

#[test]
fn identity_token_is_generated_once_with_private_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/identity-token");

    let first = ensure_identity_token_file(&path).unwrap();
    let second = ensure_identity_token_file(&path).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.len(), 36);
    assert_eq!(&first[14..15], "4");
    assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn command_health_check_uses_exit_status() {
    let vars = Vars::new();
    let ok = HealthCheck::Command {
        command: vec!["/usr/bin/true".into()],
    };
    let fail = HealthCheck::Command {
        command: vec!["/usr/bin/false".into()],
    };
    assert!(run_health_check(&ok, &vars).await.is_ok());
    assert!(run_health_check(&fail, &vars).await.is_err());
}

#[tokio::test]
async fn command_health_check_renders_variables() {
    let mut vars = Vars::new();
    vars.insert("value".into(), "expected".into());
    let check = HealthCheck::Command {
        command: vec![
            "/bin/test".into(),
            "{value}".into(),
            "=".into(),
            "expected".into(),
        ],
    };
    assert!(run_health_check(&check, &vars).await.is_ok());
}
