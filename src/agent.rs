mod bridge;
mod control;
mod idle;
mod lifecycle;
mod process;
mod relay;
mod relay_transport;
mod service;
mod socket;
mod state;
mod status;

use crate::cli::{AgentArgs, AgentCommand, AgentConfigCommand};
use crate::config::{ContainerAgentFile, ControlSocketConfig};
use crate::fileutil;
use crate::paths;
use crate::template::{self, Vars};
use access_identity::{IdentityPresentation, SensitiveBearer};
use anyhow::Context as _;
#[cfg(test)]
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use zeroize::Zeroize;
use zeroize::Zeroizing;

use bridge::run_bridge;
use control::{run_control_socket, run_signal_broker};
use idle::run_idle_cleanup;
use relay::{RelayControl, run_relay_supervisor};
use service::{ManagedService, service_supervisor};
use state::{AgentState, SocketOwner};

pub const DEFAULT_AGENT_CONFIG: &str = include_str!("../container-agent.sample.toml");

pub fn run(args: AgentArgs) -> anyhow::Result<()> {
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
        Some(AgentCommand::Run) | None => {
            let prepared = prepare_agent(args.config)?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(run_agent(prepared))
        }
    }
}

struct PreparedAgent {
    cfg: ContainerAgentFile,
    relay_presentation: Option<IdentityPresentation>,
    service_identity_token: Option<Arc<SensitiveBearer>>,
}

fn prepare_agent(config_path: Option<PathBuf>) -> anyhow::Result<PreparedAgent> {
    let cfg = ContainerAgentFile::load(&paths::agent_config_path(config_path))?;
    let (relay_presentation, service_identity_token) =
        activate_identity_environment(&cfg.container_agent)?;
    Ok(PreparedAgent {
        cfg,
        relay_presentation,
        service_identity_token,
    })
}

fn activate_identity_environment(
    config: &crate::config::ContainerAgentConfig,
) -> anyhow::Result<(Option<IdentityPresentation>, Option<Arc<SensitiveBearer>>)> {
    activate_identity_environment_with(config, take_identity_environment)
}

fn activate_identity_environment_with(
    config: &crate::config::ContainerAgentConfig,
    mut take_environment: impl FnMut(&str) -> Option<Zeroizing<Vec<u8>>>,
) -> anyhow::Result<(Option<IdentityPresentation>, Option<Arc<SensitiveBearer>>)> {
    const CANONICAL_SOURCE: &str = "AW_IDENTITY_TOKEN";

    let relay_source = config
        .access_flow_relay
        .as_ref()
        .and_then(|relay| relay.presentation.environment_variable());
    let custom_relay_source = relay_source.filter(|source| *source != CANONICAL_SOURCE);
    let canonical_required =
        config.services_need_identity_token() || relay_source == Some(CANONICAL_SOURCE);

    // Consume every required locator before validating any value so an error
    // cannot leave another required bearer in the process environment.
    let canonical_value = if canonical_required {
        take_environment(CANONICAL_SOURCE)
    } else {
        None
    };
    let custom_relay_value = custom_relay_source.and_then(&mut take_environment);

    let relay_presentation = config
        .access_flow_relay
        .as_ref()
        .map(|relay| match &relay.presentation {
            crate::config::AccessFlowRelayPresentation::Disabled {} => {
                Ok(IdentityPresentation::Disabled)
            }
            crate::config::AccessFlowRelayPresentation::Anonymous {} => {
                Ok(IdentityPresentation::Anonymous)
            }
            crate::config::AccessFlowRelayPresentation::BearerEnvironment { variable } => {
                let value = if variable == CANONICAL_SOURCE {
                    canonical_value.as_deref()
                } else {
                    custom_relay_value.as_deref()
                }
                .ok_or_else(|| anyhow::anyhow!("required agent identity environment is missing"))?;
                validated_sensitive_bearer(value).map(IdentityPresentation::Bearer)
            }
        })
        .transpose()?;

    let service_identity_token = config
        .services_need_identity_token()
        .then(|| {
            let value = canonical_value
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("required agent identity environment is missing"))?;
            validated_sensitive_bearer(value).map(Arc::new)
        })
        .transpose()?;

    Ok((relay_presentation, service_identity_token))
}

fn take_identity_environment(variable: &str) -> Option<Zeroizing<Vec<u8>>> {
    let value = std::env::var_os(variable);
    // SAFETY: agent preparation runs before the Tokio runtime or any worker
    // thread is constructed.
    unsafe {
        std::env::remove_var(variable);
    }
    value.map(|value| Zeroizing::new(value.into_vec()))
}

fn validated_sensitive_bearer(value: &[u8]) -> anyhow::Result<SensitiveBearer> {
    SensitiveBearer::new(value)
        .map_err(|_| anyhow::anyhow!("required agent identity environment is invalid"))
}

#[cfg(test)]
fn sensitive_bearer_from_environment(value: OsString) -> anyhow::Result<IdentityPresentation> {
    let mut bytes = value.into_vec();
    let bearer = validated_sensitive_bearer(&bytes);
    bytes.zeroize();
    bearer.map(IdentityPresentation::Bearer)
}

async fn run_agent(prepared: PreparedAgent) -> anyhow::Result<()> {
    let PreparedAgent {
        cfg,
        relay_presentation,
        service_identity_token,
    } = prepared;
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
    let (access_flow_relay, relay_commands) = cfg
        .container_agent
        .access_flow_relay
        .as_ref()
        .map(RelayControl::configured)
        .map_or((None, None), |(control, commands)| {
            (Some(control), Some(commands))
        });
    let state = Arc::new(AgentState::new(
        state_dir.clone(),
        cfg.container_agent.idle_cleanup.clone(),
        bridge_enabled,
        std::env::var("AW_CONTAINER_CONTROL_TOKEN").ok(),
        socket_owner,
        access_flow_relay.clone(),
    ));

    let services: Vec<_> = cfg
        .container_agent
        .services
        .clone()
        .into_iter()
        .map(|service| {
            let identity_token = service
                .needs_identity_token()
                .then(|| service_identity_token.clone())
                .flatten();
            Arc::new(ManagedService::new_with_identity_token(
                service,
                state_dir.clone(),
                cfg.logging.clone(),
                identity_token,
            ))
        })
        .collect();
    *state.services.lock().await = services.clone();
    for service in services.clone() {
        tokio::spawn(service_supervisor(service, services.clone(), state.clone()));
    }
    let relay_task = if let (Some(config), Some(presentation), Some(control), Some(commands)) = (
        cfg.container_agent.access_flow_relay.clone(),
        relay_presentation,
        access_flow_relay,
        relay_commands,
    ) {
        Some(tokio::spawn(run_relay_supervisor(
            config,
            presentation,
            cfg.container_agent.access_flow_execution_context.clone(),
            services.clone(),
            state.clone(),
            control,
            commands,
        )))
    } else {
        None
    };

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

    let control: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> =
        if let Some(control_socket) = configured_control_socket(&cfg.container_agent.control_socket)
        {
            let mut vars = Vars::new();
            vars.insert(
                "container_state_dir".into(),
                state_dir.display().to_string(),
            );
            let control_socket = PathBuf::from(template::render(&control_socket, &vars)?);
            let state = state.clone();
            Box::pin(async move { run_control_socket(state, &control_socket).await })
        } else {
            Box::pin(std::future::pending())
        };
    tokio::pin!(control);
    let signals = run_signal_broker(state.clone());
    tokio::pin!(signals);
    if let Some(mut relay_task) = relay_task {
        enum AgentEvent {
            Control(anyhow::Result<()>),
            Signal(anyhow::Result<()>),
            RelayFatal(relay::RelayFatalKind),
            RelayTerminated(Result<anyhow::Result<()>, tokio::task::JoinError>),
        }
        let event = tokio::select! {
            result = &mut control => AgentEvent::Control(result),
            result = &mut signals => AgentEvent::Signal(result),
            fatal = wait_for_relay_fatal(&state) => AgentEvent::RelayFatal(fatal),
            result = &mut relay_task => AgentEvent::RelayTerminated(result),
        };
        match event {
            AgentEvent::Control(result) | AgentEvent::Signal(result) => {
                if !state
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    lifecycle::shutdown_agent(state.clone()).await;
                }
                let relay_result = relay_task.await;
                if let Some(fatal) = state.relay_fatal() {
                    return finish_relay_fatal_shutdown(state, fatal).await;
                }
                if let Some(fatal) = relay_termination_fatal_kind(
                    &relay_result,
                    state
                        .shutting_down
                        .load(std::sync::atomic::Ordering::Acquire),
                ) {
                    return finish_relay_fatal_shutdown(state, fatal).await;
                }
                result
            }
            AgentEvent::RelayFatal(fatal) => {
                let result = finish_relay_fatal_shutdown(state, fatal).await;
                let _ = relay_task.await;
                result
            }
            AgentEvent::RelayTerminated(result) => {
                let shutting_down = state
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Acquire);
                if let Some(fatal) = state
                    .relay_fatal()
                    .or_else(|| relay_termination_fatal_kind(&result, shutting_down))
                {
                    finish_relay_fatal_shutdown(state, fatal).await
                } else if shutting_down {
                    lifecycle::shutdown_agent(state.clone()).await;
                    result.context("join access flow relay supervisor")?
                } else {
                    control.await
                }
            }
        }
    } else {
        tokio::select! {
            result = &mut control => result,
            result = &mut signals => result,
        }
    }
}

fn relay_termination_fatal_kind(
    result: &Result<anyhow::Result<()>, tokio::task::JoinError>,
    shutting_down: bool,
) -> Option<relay::RelayFatalKind> {
    match result {
        Err(_) => Some(relay::RelayFatalKind::ManagerPanic),
        Ok(result) => relay_manager_completion_fatal_kind(result, shutting_down),
    }
}

fn relay_manager_completion_fatal_kind(
    result: &anyhow::Result<()>,
    shutting_down: bool,
) -> Option<relay::RelayFatalKind> {
    match result {
        Err(_) => Some(relay::RelayFatalKind::ManagerFailure),
        Ok(()) if !shutting_down => Some(relay::RelayFatalKind::UnexpectedExit),
        Ok(()) => None,
    }
}

async fn finish_relay_fatal_shutdown(
    state: Arc<AgentState>,
    fatal: relay::RelayFatalKind,
) -> anyhow::Result<()> {
    state.publish_relay_fatal(fatal);
    let delay =
        lifecycle::shutdown_watchdog_delay(&state, std::time::Duration::from_secs(30)).await;
    lifecycle::schedule_forced_exit_after(
        state.clone(),
        delay,
        "access-flow-relay-failure",
        lifecycle::ForcedExitStatus::Fatal,
    );
    lifecycle::shutdown_agent(state).await;
    anyhow::bail!("access flow relay terminated unexpectedly ({fatal:?})")
}

async fn wait_for_relay_fatal(state: &AgentState) -> relay::RelayFatalKind {
    loop {
        let notified = state.relay_fatal_notify.notified();
        if let Some(fatal) = state.relay_fatal() {
            return fatal;
        }
        notified.await;
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
    use crate::agent::lifecycle::{ForcedExitStatus, shutdown_agent, shutdown_watchdog_delay};
    use crate::agent::process::{ProcInfo, current_uid, process_exists};
    use crate::agent::service::{
        RotatingServiceLog, health_check_interval, health_check_timeout,
        relay_dependent_service_stop_order, resolve_service_user, service_stop_order,
        should_restart, wait_for_dependencies,
    };
    use crate::agent::socket::validate_control_peer;
    use crate::agent::status::status_payload;
    use crate::agent_control::ControlRequest;
    #[cfg(target_os = "linux")]
    use crate::config::{
        AccessFlowRelayPresentation, AccessFlowRelayValidationMode,
        CompiledAccessFlowRelayEndpoint, GatewayConfig,
    };
    use crate::config::{
        ContainerAgentConfig, ContainerAgentFile, HealthCheck, IdleCleanupAction,
        IdleCleanupConfig, LoggingConfig, RestartPolicy, ServiceConfig,
    };
    use crate::health_probe::{JsonFieldCheck, check_json_fields};
    #[cfg(target_os = "linux")]
    use access_flow::{
        AccessFlowDestination, AccessFlowPreface, AccessFlowWireVersion, write_access_flow_preface,
    };
    #[cfg(target_os = "linux")]
    use access_flow_conformance::{
        NormalizedAccessFlowTransport, load_adapter_parity_fixture, project_relay_plan,
    };
    #[cfg(target_os = "linux")]
    use access_flow_unix::{NormalizedUnixSocketPath, UnixAccessFlowEndpoint};
    use std::collections::{BTreeMap, BTreeSet};
    #[cfg(target_os = "linux")]
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    #[cfg(target_os = "linux")]
    const PRODUCT_HTTP_ENDPOINT: &str = "/run/acl-proxy/transparent-http.sock";
    #[cfg(target_os = "linux")]
    const PRODUCT_HTTPS_ENDPOINT: &str = "/run/acl-proxy/transparent-https.sock";
    #[cfg(target_os = "linux")]
    const CONFORMANCE_HTTP_ENDPOINT: &str = "/run/access-flow/conformance-http.sock";
    #[cfg(target_os = "linux")]
    const CONFORMANCE_HTTPS_ENDPOINT: &str = "/run/access-flow/conformance-https.sock";

    #[cfg(target_os = "linux")]
    fn product_gateway_config() -> GatewayConfig {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/apple-container/gateway-host-proxy.toml");
        let config = GatewayConfig::load(&path).unwrap();
        config.validate().unwrap();
        config
    }

    #[cfg(target_os = "linux")]
    fn normalize_product_endpoint(
        endpoint: &CompiledAccessFlowRelayEndpoint,
    ) -> anyhow::Result<NormalizedAccessFlowTransport> {
        let CompiledAccessFlowRelayEndpoint::Unix(endpoint) = endpoint else {
            anyhow::bail!("shipped host-proxy route unexpectedly uses TLS/TCP");
        };
        let normalized = match endpoint.path().as_str() {
            PRODUCT_HTTP_ENDPOINT => CONFORMANCE_HTTP_ENDPOINT,
            PRODUCT_HTTPS_ENDPOINT => CONFORMANCE_HTTPS_ENDPOINT,
            _ => anyhow::bail!("unrecognized AW Gateway Access Flow endpoint"),
        };
        Ok(NormalizedAccessFlowTransport::unix(normalized)?)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn product_access_flow_relay_matches_shared_adapter_fixture() {
        let config = product_gateway_config();
        let target = config.effective_target("ubuntu-host-proxy").unwrap();
        let execution_context = target.container_agent.access_flow_execution_context.clone();
        let relay = target.container_agent.access_flow_relay.unwrap();
        let AccessFlowRelayPresentation::BearerEnvironment { variable } = &relay.presentation
        else {
            panic!("shipped host-proxy relay must use bearer_environment");
        };
        assert_eq!(variable, "AW_IDENTITY_TOKEN");
        let compiled = relay
            .compile_with_presentation(
                AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Disabled,
                execution_context.as_deref(),
            )
            .unwrap();

        let product_paths = compiled
            .plan
            .routes()
            .iter()
            .map(|route| match route.endpoint() {
                CompiledAccessFlowRelayEndpoint::Unix(endpoint) => endpoint.path().as_str(),
                CompiledAccessFlowRelayEndpoint::TlsTcp { .. } => {
                    panic!("shipped host-proxy route unexpectedly uses TLS/TCP")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            product_paths,
            [PRODUCT_HTTP_ENDPOINT, PRODUCT_HTTPS_ENDPOINT]
        );

        let projected = project_relay_plan(&compiled.plan, compiled.drain_timeout, |endpoint| {
            normalize_product_endpoint(endpoint).unwrap()
        });
        let fixture = load_adapter_parity_fixture().unwrap();
        let expected = fixture.relay_plan();
        assert_eq!(
            projected
                .execution_context()
                .map(access_execution_context::ExecutionContext::as_str),
            Some("external")
        );
        assert_eq!(
            projected.copy_buffer_bytes_per_direction(),
            expected.copy_buffer_bytes_per_direction()
        );
        assert_eq!(projected.drain_timeout(), expected.drain_timeout());
        assert_eq!(projected.max_connections(), expected.max_connections());
        assert_eq!(projected.presentation(), expected.presentation());
        assert_eq!(projected.routes(), expected.routes());
        assert_eq!(projected.setup_timeout(), expected.setup_timeout());

        let unknown_path = "/run/acl-proxy/unrecognized-sensitive-name.sock";
        let unknown =
            UnixAccessFlowEndpoint::new(NormalizedUnixSocketPath::new(unknown_path).unwrap());
        let error = normalize_product_endpoint(&CompiledAccessFlowRelayEndpoint::Unix(unknown))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "unrecognized AW Gateway Access Flow endpoint");
        assert!(!error.contains(unknown_path), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn product_access_flow_writer_emits_v2_with_optional_plan_context() {
        let config = product_gateway_config();
        let target = config.effective_target("ubuntu-host-proxy").unwrap();
        let relay = target.container_agent.access_flow_relay.unwrap();
        let with_context = relay
            .compile_with_presentation(
                AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Disabled,
                target
                    .container_agent
                    .access_flow_execution_context
                    .as_deref(),
            )
            .unwrap();
        assert_eq!(with_context.plan.routes().len(), 2);
        assert_eq!(
            with_context
                .plan
                .execution_context()
                .map(access_execution_context::ExecutionContext::as_str),
            Some("external")
        );

        for route in with_context.plan.routes() {
            let destination = AccessFlowDestination::new(
                "192.0.2.1".parse().unwrap(),
                route.allowed_destination_ports()[0].get(),
            )
            .unwrap();
            let mut wire = Zeroizing::new(Vec::<u8>::new());
            write_access_flow_preface(
                &mut *wire,
                destination,
                with_context.plan.presentation(),
                with_context.plan.execution_context(),
            )
            .await
            .unwrap();
            let decoded =
                AccessFlowPreface::decode_exact(wire.as_slice(), route.allowed_destination_ports())
                    .unwrap();
            assert_eq!(decoded.wire_version(), AccessFlowWireVersion::Two);
            assert_eq!(
                decoded
                    .execution_context()
                    .map(access_execution_context::ExecutionContext::as_str),
                Some("external")
            );
        }

        let without_context = relay
            .compile_with_presentation(
                AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Disabled,
                None,
            )
            .unwrap();
        let route = &without_context.plan.routes()[0];
        let destination = AccessFlowDestination::new(
            "192.0.2.1".parse().unwrap(),
            route.allowed_destination_ports()[0].get(),
        )
        .unwrap();
        let mut wire = Zeroizing::new(Vec::<u8>::new());
        write_access_flow_preface(
            &mut *wire,
            destination,
            without_context.plan.presentation(),
            without_context.plan.execution_context(),
        )
        .await
        .unwrap();
        let decoded =
            AccessFlowPreface::decode_exact(wire.as_slice(), route.allowed_destination_ports())
                .unwrap();
        assert_eq!(decoded.wire_version(), AccessFlowWireVersion::Two);
        assert!(decoded.execution_context().is_none());
    }

    fn identity_activation_config(
        relay_source: Option<&str>,
        service_identity: bool,
    ) -> ContainerAgentConfig {
        let mut raw =
            "schema_version = \"1\"\n[container_agent]\ncontrol_socket = false\n".to_string();
        if let Some(source) = relay_source {
            raw.push_str(&format!(
                r#"
[container_agent.access_flow_relay]
setup_timeout = "2s"
drain_timeout = "10s"
max_connections = 4
copy_buffer_bytes_per_direction = 4096

[container_agent.access_flow_relay.presentation]
kind = "bearer_environment"
variable = "{source}"

[[container_agent.access_flow_relay.routes]]
name = "http"
listen = "127.0.0.1:3128"
allowed_destination_ports = [80]

[container_agent.access_flow_relay.routes.transport]
kind = "unix"
path = "/tmp/access-flow.sock"
"#
            ));
        }
        if service_identity {
            raw.push_str(
                r#"
[[container_agent.services]]
name = "approved"
command = ["/bin/true"]
restart = "never"

[container_agent.services.env]
AW_IDENTITY_TOKEN = { inherit = "AW_IDENTITY_TOKEN" }
"#,
            );
        }
        toml::from_str::<ContainerAgentFile>(&raw)
            .unwrap()
            .container_agent
    }

    #[test]
    fn identity_activation_matrix_reads_each_locator_once_into_independent_sensitive_state() {
        const CANONICAL: &str = "AW_IDENTITY_TOKEN";
        const CUSTOM: &str = "AW_ACCESS_FLOW_TEST_TOKEN";
        let canonical_bearer = b"canonicalABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let custom_bearer = b"custom-bearer-ABCDEFGHIJKLMNOPQRSTUVWXYZ";

        for (relay_source, service_identity, expected_locators) in [
            (Some(CANONICAL), false, vec![CANONICAL]),
            (Some(CUSTOM), false, vec![CUSTOM]),
            (None, true, vec![CANONICAL]),
            (Some(CANONICAL), true, vec![CANONICAL]),
            (Some(CUSTOM), true, vec![CANONICAL, CUSTOM]),
        ] {
            let config = identity_activation_config(relay_source, service_identity);
            let mut values = BTreeMap::from([
                (CANONICAL.to_string(), canonical_bearer.to_vec()),
                (CUSTOM.to_string(), custom_bearer.to_vec()),
            ]);
            let mut locators = Vec::new();
            let (relay, service) = activate_identity_environment_with(&config, |locator| {
                locators.push(locator.to_string());
                values.remove(locator).map(Zeroizing::new)
            })
            .unwrap();
            locators.sort();
            let mut expected: Vec<_> = expected_locators.into_iter().map(str::to_string).collect();
            expected.sort();
            assert_eq!(locators, expected);

            match (relay_source, relay.as_ref()) {
                (Some(CANONICAL), Some(IdentityPresentation::Bearer(bearer))) => {
                    bearer.expose(|value| assert_eq!(value, canonical_bearer));
                }
                (Some(CUSTOM), Some(IdentityPresentation::Bearer(bearer))) => {
                    bearer.expose(|value| assert_eq!(value, custom_bearer));
                }
                (None, None) => {}
                _ => panic!("unexpected relay presentation"),
            }
            match (service_identity, service.as_ref()) {
                (true, Some(bearer)) => {
                    bearer.expose(|value| assert_eq!(value, canonical_bearer));
                }
                (false, None) => {}
                _ => panic!("unexpected service identity state"),
            }

            if relay_source == Some(CANONICAL) && service_identity {
                let relay_pointer = match relay.as_ref().unwrap() {
                    IdentityPresentation::Bearer(bearer) => bearer.expose(|value| value.as_ptr()),
                    _ => unreachable!(),
                };
                let service_pointer = service.as_ref().unwrap().expose(|value| value.as_ptr());
                assert_ne!(relay_pointer, service_pointer);
            }
        }
    }

    #[test]
    fn identity_activation_consumes_all_required_locators_before_sanitized_failure() {
        const CANONICAL: &str = "AW_IDENTITY_TOKEN";
        const CUSTOM: &str = "AW_ACCESS_FLOW_TEST_TOKEN";
        let config = identity_activation_config(Some(CUSTOM), true);

        for values in [
            BTreeMap::new(),
            BTreeMap::from([(CUSTOM.to_string(), b"invalid bearer".to_vec())]),
            BTreeMap::from([(
                CANONICAL.to_string(),
                b"canonicalABCDEFGHIJKLMNOPQRSTUVWXYZ".to_vec(),
            )]),
        ] {
            let mut values = values;
            let mut locators = Vec::new();
            let error = match activate_identity_environment_with(&config, |locator| {
                locators.push(locator.to_string());
                values.remove(locator).map(Zeroizing::new)
            }) {
                Ok(_) => panic!("identity activation unexpectedly succeeded"),
                Err(error) => error.to_string(),
            };
            locators.sort();
            let mut expected = vec![CANONICAL.to_string(), CUSTOM.to_string()];
            expected.sort();
            assert_eq!(locators, expected);
            assert!(!error.contains(CANONICAL), "{error}");
            assert!(!error.contains(CUSTOM), "{error}");
            assert!(!error.contains("invalid bearer"), "{error}");
        }
    }
    use tokio::time::{Duration, sleep};

    #[test]
    fn sample_agent_config_validates() {
        let cfg: ContainerAgentFile = toml::from_str(DEFAULT_AGENT_CONFIG).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn bearer_environment_value_is_validated_into_sensitive_storage() {
        let presentation =
            sensitive_bearer_from_environment(OsString::from("abcdefghijklmnopqrstuvwxyzABCDEF"))
                .unwrap();
        let IdentityPresentation::Bearer(bearer) = presentation else {
            panic!("expected bearer presentation");
        };
        assert_eq!(bearer.len(), 32);
        bearer.expose(|value| assert_eq!(value, b"abcdefghijklmnopqrstuvwxyzABCDEF"));
    }

    #[test]
    fn invalid_bearer_environment_error_is_secret_free() {
        let error = sensitive_bearer_from_environment(OsString::from("short-secret"))
            .err()
            .expect("short bearer must fail")
            .to_string();
        assert_eq!(error, "required agent identity environment is invalid");
        assert!(!error.contains("short-secret"));
    }

    #[test]
    fn production_health_probe_uses_control_listener() {
        let cfg: ContainerAgentFile = toml::from_str(DEFAULT_AGENT_CONFIG).unwrap();
        let acl_proxy = cfg
            .container_agent
            .services
            .iter()
            .find(|service| service.name == "acl-proxy")
            .expect("sample config must define the acl-proxy service");
        let HealthCheck::Http { url, .. } = acl_proxy
            .health_check
            .as_ref()
            .expect("acl-proxy service must define an HTTP health check")
        else {
            panic!("acl-proxy service health check must use HTTP");
        };

        assert_eq!(url, "http://127.0.0.1:8898/_acl-proxy/ready");
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
        let no_token_state = AgentState::new(PathBuf::from("/tmp"), None, false, None, None, None);
        let token_state = AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            Some("secret".into()),
            None,
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
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let healthy = Arc::new(AtomicBool::new(false));
        let server_healthy = Arc::clone(&healthy);
        let (first_unhealthy_tx, first_unhealthy_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut first_unhealthy_tx = Some(first_unhealthy_tx);
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 256];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }

                let is_healthy = server_healthy.load(Ordering::Acquire);
                let status = if is_healthy {
                    "200 OK"
                } else {
                    "503 Service Unavailable"
                };
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                if stream.write_all(response.as_bytes()).await.is_ok()
                    && !is_healthy
                    && let Some(sender) = first_unhealthy_tx.take()
                {
                    let _ = sender.send(());
                }
            }
        });
        let controller_healthy = Arc::clone(&healthy);
        let controller = tokio::spawn(async move {
            first_unhealthy_rx.await.unwrap();
            sleep(Duration::from_millis(150)).await;
            controller_healthy.store(true, Ordering::Release);
        });

        let proxy = Arc::new(ManagedService::new(
            ServiceConfig {
                health_check: Some(HealthCheck::Http {
                    url: format!("http://127.0.0.1:{port}/ready"),
                    expect_status: Some(200),
                    expect_json: BTreeMap::new(),
                    interval: Some("10ms".into()),
                    timeout: Some("250ms".into()),
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

        let ready = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_dependencies(&sshd, &[proxy], None),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(ready);
        controller.await.unwrap();
        assert!(healthy.load(Ordering::Acquire));
        assert!(sshd.last_error.lock().await.is_none());
        server.abort();
        let _ = server.await;
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

        let ready = wait_for_dependencies(&sshd, &[proxy], None).await.unwrap();

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
        let state = AgentState::new(PathBuf::from("/tmp"), None, true, None, None, None);
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

    #[test]
    fn relay_dependent_stop_order_excludes_prerequisites_and_unrelated_services() {
        let services = vec![
            Arc::new(ManagedService::new(
                test_service("transparent-firewall", Vec::new()),
                PathBuf::from("/tmp"),
                LoggingConfig::default(),
            )),
            Arc::new(ManagedService::new(
                test_service("workload", vec!["@access-flow-relay"]),
                PathBuf::from("/tmp"),
                LoggingConfig::default(),
            )),
            Arc::new(ManagedService::new(
                test_service("dependent", vec!["workload"]),
                PathBuf::from("/tmp"),
                LoggingConfig::default(),
            )),
            Arc::new(ManagedService::new(
                test_service("unrelated", Vec::new()),
                PathBuf::from("/tmp"),
                LoggingConfig::default(),
            )),
        ];
        let names: Vec<_> = relay_dependent_service_stop_order(&services)
            .into_iter()
            .map(|service| service.config.name.clone())
            .collect();
        assert_eq!(names, ["dependent", "workload"]);
    }

    #[test]
    fn relay_manager_termination_and_watchdog_status_are_fail_closed() {
        assert_eq!(
            relay_manager_completion_fatal_kind(&Ok(()), false),
            Some(relay::RelayFatalKind::UnexpectedExit)
        );
        assert_eq!(relay_manager_completion_fatal_kind(&Ok(()), true), None);
        assert_eq!(
            relay_manager_completion_fatal_kind(&Err(anyhow::anyhow!("failed")), true),
            Some(relay::RelayFatalKind::ManagerFailure)
        );
        assert_eq!(ForcedExitStatus::Success.code(), 0);
        assert_ne!(ForcedExitStatus::Fatal.code(), 0);
    }

    #[tokio::test]
    async fn relay_manager_join_panic_is_classified_as_fatal() {
        let task = tokio::spawn(async {
            panic!("injected relay manager panic");
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });
        let result = task.await;
        assert_eq!(
            relay_termination_fatal_kind(&result, false),
            Some(relay::RelayFatalKind::ManagerPanic)
        );
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
