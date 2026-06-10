mod bridge;
mod control;
mod idle;
mod lifecycle;
mod process;
mod service;
mod socket;
mod state;
mod status;

use crate::cli::{AgentArgs, AgentCommand, AgentConfigCommand};
use crate::config::{ContainerAgentFile, ControlSocketConfig};
use crate::fileutil;
use crate::paths;
use crate::template::{self, Vars};
use std::path::PathBuf;
use std::sync::Arc;

use bridge::run_bridge;
use control::{run_control_socket, wait_for_shutdown_signal};
use idle::run_idle_cleanup;
use service::{ManagedService, service_supervisor};
use state::{AgentState, SocketOwner};

pub const DEFAULT_AGENT_CONFIG: &str = include_str!("../container-agent.sample.toml");

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        Some(AgentCommand::Config(AgentConfigCommand::Validate)) => {
            let path = paths::agent_config_path(args.config);
            ContainerAgentFile::load(&path)?;
            println!("ok");
            Ok(())
        }
        Some(AgentCommand::Config(AgentConfigCommand::Init(init))) => {
            let path = init
                .path
                .unwrap_or_else(|| paths::agent_config_path(args.config));
            if path.exists() && !init.force {
                anyhow::bail!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                );
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_AGENT_CONFIG)?;
            println!("{}", path.display());
            Ok(())
        }
        Some(AgentCommand::Run) | None => run_agent(args.config).await,
    }
}

async fn run_agent(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = ContainerAgentFile::load(&paths::agent_config_path(config_path))?;
    let state_dir = std::env::var_os("AW_CONTAINER_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(paths::DEFAULT_AGENT_STATE_DIR));
    fileutil::ensure_private_dir(&state_dir)?;
    let bridge_enabled = cfg
        .container_agent
        .ssh_bridge
        .as_ref()
        .is_some_and(|bridge| bridge.enabled);
    let socket_owner = SocketOwner::from_env()?;
    let state = Arc::new(AgentState::new(
        state_dir.clone(),
        cfg.container_agent.idle_cleanup.clone(),
        bridge_enabled,
        std::env::var("AW_CONTAINER_CONTROL_TOKEN").ok(),
        socket_owner,
    ));

    let services: Vec<_> = cfg
        .container_agent
        .services
        .clone()
        .into_iter()
        .map(|service| {
            Arc::new(ManagedService::new(
                service,
                state_dir.clone(),
                cfg.logging.clone(),
            ))
        })
        .collect();
    *state.services.lock().await = services.clone();
    for service in services.clone() {
        tokio::spawn(service_supervisor(service, services.clone()));
    }

    if let Some(bridge) = cfg
        .container_agent
        .ssh_bridge
        .clone()
        .filter(|bridge| bridge.enabled)
    {
        let socket = bridge
            .socket
            .expect("validated enabled ssh_bridge must include socket");
        let bridge_state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = run_bridge(bridge_state, socket, bridge.target).await {
                tracing::error!(error = %err, "ssh bridge exited");
            }
        });
    }

    if state.idle_cleanup.is_some() {
        let cleanup_state = state.clone();
        tokio::spawn(async move {
            run_idle_cleanup(cleanup_state).await;
        });
    }

    if let Some(control_socket) = configured_control_socket(&cfg.container_agent.control_socket) {
        let mut vars = Vars::new();
        vars.insert(
            "container_state_dir".into(),
            state_dir.display().to_string(),
        );
        let control_socket = PathBuf::from(template::render(&control_socket, &vars)?);
        run_control_socket(state, &control_socket).await
    } else {
        wait_for_shutdown_signal(state).await
    }
}

fn configured_control_socket(config: &Option<ControlSocketConfig>) -> Option<String> {
    match config {
        Some(ControlSocketConfig::Path(path)) => Some(path.clone()),
        Some(ControlSocketConfig::Enabled(false)) => None,
        Some(ControlSocketConfig::Enabled(true)) | None => {
            Some("{container_state_dir}/agent.sock".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::control::unauthorized_if_needed;
    use crate::agent::idle::build_reap_plan;
    use crate::agent::lifecycle::{shutdown_agent, shutdown_watchdog_delay};
    use crate::agent::process::{ProcInfo, current_uid, process_exists};
    use crate::agent::service::{
        RotatingServiceLog, health_check_interval, health_check_timeout, resolve_service_user,
        service_stop_order, should_restart, wait_for_dependencies,
    };
    use crate::agent::socket::validate_control_peer;
    use crate::agent::status::status_payload;
    use crate::agent_control::ControlRequest;
    use crate::config::{
        HealthCheck, IdleCleanupAction, IdleCleanupConfig, LoggingConfig, RestartPolicy,
        ServiceConfig,
    };
    use crate::health_probe::{JsonFieldCheck, check_json_fields};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::{Duration, Instant, sleep};

    #[test]
    fn sample_agent_config_validates() {
        let cfg: ContainerAgentFile = toml::from_str(DEFAULT_AGENT_CONFIG).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn reap_plan_preserves_named_process_tree_and_managed_services() {
        let config = IdleCleanupConfig {
            action: IdleCleanupAction::ReapProcesses,
            preserve_processes: vec!["tmux".to_string()],
            ..IdleCleanupConfig::default()
        };
        let processes = vec![
            proc_info(1, 0, 0, "init"),
            proc_info(10, 1, 0, "aw-container-agent"),
            proc_info(20, 1, 0, "sshd"),
            proc_info(30, 1, 1000, "bash"),
            proc_info(40, 1, 1000, "tmux"),
            proc_info(41, 40, 1000, "codex"),
            proc_info(50, 30, 1000, "node"),
        ];
        let managed = BTreeSet::from([20]);
        let plan = build_reap_plan(&processes, &config, &managed, 0, 10);
        let terminate: Vec<_> = plan
            .would_terminate
            .iter()
            .map(|process| process.pid)
            .collect();
        let preserved: Vec<_> = plan.preserved.iter().map(|process| process.pid).collect();
        assert_eq!(terminate, vec![30, 50]);
        assert_eq!(preserved, vec![40, 41]);
    }

    #[test]
    fn reap_plan_for_non_root_agent_only_targets_same_uid() {
        let config = IdleCleanupConfig {
            action: IdleCleanupAction::ReapProcesses,
            preserve_processes: Vec::new(),
            ..IdleCleanupConfig::default()
        };
        let processes = vec![
            proc_info(1, 0, 0, "init"),
            proc_info(10, 1, 1000, "aw-container-agent"),
            proc_info(20, 1, 0, "root-service"),
            proc_info(30, 1, 1000, "bash"),
            proc_info(40, 1, 1001, "other-user"),
        ];
        let plan = build_reap_plan(&processes, &config, &BTreeSet::new(), 1000, 10);
        let terminate: Vec<_> = plan
            .would_terminate
            .iter()
            .map(|process| process.pid)
            .collect();
        assert_eq!(terminate, vec![30]);
    }

    #[test]
    fn resolves_root_service_user() {
        let root = resolve_service_user("root").unwrap();
        assert_eq!(root.uid, 0);
        assert_eq!(root.gid, 0);
    }

    #[test]
    fn restart_policy_only_restarts_on_failure_when_configured() {
        assert!(!should_restart(RestartPolicy::Never, false));
        assert!(!should_restart(RestartPolicy::Never, true));
        assert!(should_restart(RestartPolicy::Always, false));
        assert!(should_restart(RestartPolicy::Always, true));
        assert!(should_restart(RestartPolicy::OnFailure, false));
        assert!(!should_restart(RestartPolicy::OnFailure, true));
    }

    #[test]
    fn control_auth_helper_requires_token_for_mutating_methods() {
        let no_token_state = AgentState::new(PathBuf::from("/tmp"), None, false, None, None);
        let token_state = AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            Some("secret".into()),
            None,
        );
        let id = serde_json::Value::String("request".into());
        let status = ControlRequest::Status;
        let wrong_hold = ControlRequest::SessionHold(crate::agent_control::SessionHoldParams {
            token: Some("wrong".into()),
            kind: Some("run".into()),
        });
        let correct_hold = ControlRequest::SessionHold(crate::agent_control::SessionHoldParams {
            token: Some("secret".into()),
            kind: Some("run".into()),
        });

        assert!(unauthorized_if_needed(&token_state, &status, &id).is_none());
        assert!(unauthorized_if_needed(&token_state, &correct_hold, &id).is_none());

        let failure = unauthorized_if_needed(&no_token_state, &wrong_hold, &id).unwrap();
        assert_eq!(failure.id, id);
        assert!(!failure.ok);
        assert_eq!(failure.error.code, "unauthorized");
        assert_eq!(failure.error.message, "control token is required");

        let failure = unauthorized_if_needed(&token_state, &wrong_hold, &id).unwrap();
        assert_eq!(failure.id, id);
        assert!(!failure.ok);
        assert_eq!(failure.error.code, "unauthorized");
        assert_eq!(failure.error.message, "control token is required");
    }

    #[test]
    fn required_health_restart_only_applies_to_required_non_process_checks() {
        let service = ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                    interval: None,
                    timeout: None,
                }),
                ..test_service("proxy", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        assert!(service.required_health_restart());

        let optional = ManagedService::new(
            ServiceConfig {
                required: false,
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                    interval: None,
                    timeout: None,
                }),
                ..test_service("metrics", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        assert!(!optional.required_health_restart());

        let process = ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Process),
                ..test_service("worker", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        assert!(!process.required_health_restart());
    }

    #[test]
    fn health_check_timing_uses_configured_interval_and_timeout() {
        let check = HealthCheck::Tcp {
            host: "127.0.0.1".into(),
            port: 1,
            interval: Some("3s".into()),
            timeout: Some("75ms".into()),
        };
        assert_eq!(health_check_interval(Some(&check)), Duration::from_secs(3));
        assert_eq!(
            health_check_timeout(Some(&check)),
            Duration::from_millis(75)
        );
        assert_eq!(health_check_interval(None), Duration::from_millis(250));
        assert_eq!(health_check_timeout(None), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn dependency_wait_can_exceed_startup_timeout_until_dependency_is_healthy() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);

        let proxy = Arc::new(ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port,
                    interval: Some("10ms".into()),
                    timeout: Some("10ms".into()),
                }),
                ..test_service("proxy", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let sshd = ManagedService::new(
            ServiceConfig {
                startup_timeout: Some("25ms".into()),
                ..test_service("container-sshd", vec!["proxy"])
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        let listener = tokio::spawn(async move {
            sleep(Duration::from_millis(150)).await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            let _ = listener.accept().await;
        });

        let started_at = Instant::now();
        let ready = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_dependencies(&sshd, &[proxy]),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(ready);
        assert!(started_at.elapsed() >= Duration::from_millis(100));
        assert!(sshd.last_error.lock().await.is_none());
        listener.abort();
    }

    #[tokio::test]
    async fn dependency_wait_exits_when_service_is_stopping() {
        let proxy = Arc::new(ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                    interval: Some("1s".into()),
                    timeout: Some("10ms".into()),
                }),
                ..test_service("proxy", Vec::new())
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let sshd = ManagedService::new(
            test_service("container-sshd", vec!["proxy"]),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        );
        sshd.stopping.store(true, Ordering::SeqCst);

        let ready = wait_for_dependencies(&sshd, &[proxy]).await.unwrap();

        assert!(!ready);
    }

    #[test]
    fn json_health_expectation_matches_top_level_fields() {
        let expected = BTreeMap::from([("status".to_string(), "ready".to_string())]);
        assert!(matches!(
            check_json_fields(r#"{"status":"ready"}"#, &expected).unwrap(),
            JsonFieldCheck::Match
        ));
        assert!(matches!(
            check_json_fields(r#"{"status":"starting"}"#, &expected).unwrap(),
            JsonFieldCheck::Mismatch { .. }
        ));
        assert!(matches!(
            check_json_fields(r#"{"state":"ready"}"#, &expected).unwrap(),
            JsonFieldCheck::Missing { .. }
        ));
    }

    #[test]
    fn service_command_templates_render_container_state_dir() {
        let vars = BTreeMap::from([(
            "container_state_dir".to_string(),
            "/tmp/agent-state".to_string(),
        )]);
        let command = vec![
            "/bin/echo".to_string(),
            "{container_state_dir}/ready".to_string(),
        ];

        assert_eq!(
            template::render_argv(&command, &vars).unwrap(),
            vec![
                "/bin/echo".to_string(),
                "/tmp/agent-state/ready".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn status_is_not_ready_when_agent_is_shutting_down() {
        let state = AgentState::new(PathBuf::from("/tmp"), None, true, None, None);
        state.bridge_ready.store(true, Ordering::SeqCst);
        state.shutting_down.store(true, Ordering::SeqCst);

        let status = status_payload(&state).await;

        assert!(!status.ready);
        assert!(!status.ssh_bridge.ready);
    }

    #[tokio::test]
    async fn shutdown_agent_disables_bridge_accepts() {
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            true,
            None,
            None,
        ));
        state.accepting_bridge.store(true, Ordering::SeqCst);

        assert!(shutdown_agent(state.clone()).await);

        assert!(state.shutting_down.load(Ordering::SeqCst));
        assert!(!state.accepting_bridge.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_agent_is_idempotent() {
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            true,
            None,
            None,
        ));

        assert!(shutdown_agent(state.clone()).await);
        assert!(!shutdown_agent(state.clone()).await);
    }

    #[tokio::test]
    async fn repeated_shutdown_waits_for_in_flight_service_stop() {
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            true,
            None,
            None,
        ));
        state.shutting_down.store(true, Ordering::SeqCst);
        let completing = state.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            completing.shutdown_complete.store(true, Ordering::SeqCst);
            completing.shutdown_complete_notify.notify_waiters();
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(50), shutdown_agent(state.clone()))
                .await
                .is_err()
        );

        assert!(
            !tokio::time::timeout(Duration::from_millis(200), shutdown_agent(state.clone()))
                .await
                .unwrap()
        );
        assert!(state.shutdown_complete.load(Ordering::SeqCst));
        assert!(!shutdown_agent(state.clone()).await);
    }

    #[tokio::test]
    async fn shutdown_watchdog_delay_covers_sequential_service_timeouts() {
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            true,
            None,
            None,
        ));
        *state.services.lock().await = vec![
            Arc::new(ManagedService::new(
                ServiceConfig {
                    shutdown_timeout: Some("40ms".into()),
                    ..test_service("one", Vec::new())
                },
                PathBuf::from("/tmp"),
                LoggingConfig::default(),
            )),
            Arc::new(ManagedService::new(
                ServiceConfig {
                    shutdown_timeout: Some("20ms".into()),
                    ..test_service("two", Vec::new())
                },
                PathBuf::from("/tmp"),
                LoggingConfig::default(),
            )),
        ];

        assert_eq!(
            shutdown_watchdog_delay(&state, Duration::from_millis(30)).await,
            Duration::from_millis(5060)
        );
        assert_eq!(
            shutdown_watchdog_delay(&state, Duration::from_secs(6)).await,
            Duration::from_secs(6)
        );
    }

    #[tokio::test]
    async fn ensure_private_dir_sets_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fileutil::ensure_private_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[tokio::test]
    async fn rotating_service_log_rotates_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.log");
        let mut log = RotatingServiceLog::new(path.clone(), 8, 2).await.unwrap();
        log.write_all(b"12345678").await.unwrap();
        log.write_all(b"abcdef").await.unwrap();
        log.file.flush().await.unwrap();

        assert!(path.exists());
        assert!(dir.path().join("service.log.1").exists());
        assert!(!dir.path().join("service.log.3").exists());
    }

    #[tokio::test]
    async fn rotating_service_log_can_disable_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.log");
        let mut log = RotatingServiceLog::new(path.clone(), 4, 0).await.unwrap();
        log.write_all(b"1234").await.unwrap();
        log.write_all(b"5678").await.unwrap();
        log.file.flush().await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"12345678");
        assert!(!dir.path().join("service.log.1").exists());
    }

    #[tokio::test]
    async fn control_peer_validation_checks_uid() {
        let (client, _server) = UnixStream::pair().unwrap();
        validate_control_peer(&client, Some(current_uid())).unwrap();
        assert!(validate_control_peer(&client, Some(current_uid().wrapping_add(1))).is_err());
    }

    #[test]
    fn process_exists_detects_current_process() {
        assert!(process_exists(std::process::id()));
    }

    #[test]
    fn service_stop_order_stops_dependents_before_dependencies() {
        let sshd = Arc::new(ManagedService::new(
            test_service("sshd", Vec::new()),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let proxy = Arc::new(ManagedService::new(
            test_service("proxy", vec!["sshd"]),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let metrics = Arc::new(ManagedService::new(
            test_service("metrics", Vec::new()),
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ));
        let ordered_services = service_stop_order(&[sshd, proxy, metrics]);
        let ordered: Vec<_> = ordered_services
            .iter()
            .map(|service| service.config.name.clone())
            .collect();
        assert_eq!(ordered, vec!["proxy", "sshd", "metrics"]);
    }

    fn test_service(name: &str, depends_on: Vec<&str>) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            required: true,
            user: "root".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            cwd: None,
            restart: RestartPolicy::Always,
            restart_backoff: None,
            restart_backoff_max: None,
            startup_timeout: None,
            shutdown_timeout: None,
            depends_on: depends_on.into_iter().map(str::to_string).collect(),
            env: BTreeMap::new(),
            health_check: None,
        }
    }

    fn proc_info(pid: u32, ppid: u32, uid: u32, comm: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            uid,
            comm: comm.to_string(),
            start_time: Some(pid as u64 * 10),
        }
    }
}
