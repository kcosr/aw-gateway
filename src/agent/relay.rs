use access_async_contracts::{AccessCancellation, BoxAccessFuture};
use access_flow_relay::{
    AccessFlowRelay, AccessFlowRelayEvent, AccessFlowRelayEventKind, AccessFlowRelayFailure,
    AccessFlowRelayObserver, AccessFlowRelayResourceBudget, ConnectionCloseCategory,
    RunningAccessFlowRelay,
};
use access_flow_unix::UnixAccessFlowConnector;
use access_identity::IdentityPresentation;
use anyhow::Context;
use std::fs::FileType;
use std::os::unix::fs::FileTypeExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use crate::agent_control::{
    AccessFlowRelayRouteStatus, AccessFlowRelayStateName, AccessFlowRelayStatus,
};
use crate::config::{AccessFlowRelayConfig, CompiledAccessFlowRelayConfig};

use super::service::ManagedService;
use super::state::AgentState;

const TOTAL_AGENT_MEMORY_PREFLIGHT_BYTES: u64 = 256 * 1024 * 1024;
const BRIDGE_NON_RELAY_MEMORY_BYTES: u64 = 56 * 1024 * 1024;
const NO_BRIDGE_NON_RELAY_MEMORY_BYTES: u64 = 24 * 1024 * 1024;
const PER_SERVICE_NON_RELAY_MEMORY_BYTES: u64 = 256 * 1024;
const BRIDGE_BASE_NON_RELAY_DESCRIPTORS: u64 = 2322;
const NO_BRIDGE_BASE_NON_RELAY_DESCRIPTORS: u64 = 273;
const PER_SERVICE_NON_RELAY_DESCRIPTORS: u64 = 3;
const RELAY_PHASE_PREPARING: u8 = 0;
const RELAY_PHASE_ACTIVE: u8 = 1;
const RELAY_PHASE_CLOSING: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RelayFatalKind {
    RuntimeFailure,
    ManagerFailure,
    UnexpectedExit,
    ManagerPanic,
}

enum RelayCommand {
    CloseAdmission(oneshot::Sender<()>),
    Shutdown {
        deadline: Instant,
        completed: oneshot::Sender<()>,
    },
    #[cfg(test)]
    InjectFailure {
        failure: AccessFlowRelayFailure,
        observed: oneshot::Sender<()>,
    },
}

#[derive(Debug)]
pub(super) struct RelayControl {
    state: Mutex<AccessFlowRelayStateName>,
    route_names: Box<[String]>,
    active_flows: Arc<AtomicUsize>,
    accepting: AtomicBool,
    phase: AtomicU8,
    command_tx: mpsc::Sender<RelayCommand>,
    startup_cancellation: RelayCancellation,
    drain_timeout: Duration,
}

impl RelayControl {
    pub(super) fn configured(config: &AccessFlowRelayConfig) -> (Arc<Self>, RelayCommandReceiver) {
        let (command_tx, command_rx) = mpsc::channel(2);
        let startup_cancellation = RelayCancellation::default();
        let drain_timeout = crate::config::parse_duration(&config.drain_timeout)
            .expect("validated relay drain timeout");
        (
            Arc::new(Self {
                state: Mutex::new(AccessFlowRelayStateName::Preparing),
                route_names: config
                    .routes
                    .iter()
                    .map(|route| route.name.clone())
                    .collect(),
                active_flows: Arc::new(AtomicUsize::new(0)),
                accepting: AtomicBool::new(false),
                phase: AtomicU8::new(RELAY_PHASE_PREPARING),
                command_tx,
                startup_cancellation,
                drain_timeout,
            }),
            RelayCommandReceiver(command_rx),
        )
    }

    pub(super) async fn status(&self) -> AccessFlowRelayStatus {
        let state = *self.state.lock().await;
        let accepting = self.accepting.load(Ordering::Acquire);
        AccessFlowRelayStatus {
            state,
            ready: state == AccessFlowRelayStateName::Accepting && accepting,
            active_flows: self.active_flows.load(Ordering::Acquire),
            routes: self
                .route_names
                .iter()
                .map(|name| AccessFlowRelayRouteStatus {
                    name: name.clone(),
                    accepting,
                })
                .collect(),
        }
    }

    pub(super) fn active_flows(&self) -> usize {
        self.active_flows.load(Ordering::Acquire)
    }

    pub(super) fn is_ready(&self) -> bool {
        self.phase.load(Ordering::Acquire) == RELAY_PHASE_ACTIVE
            && self.accepting.load(Ordering::Acquire)
    }

    pub(super) fn drain_timeout(&self) -> Duration {
        self.drain_timeout
    }

    pub(super) async fn close_admission(&self) {
        let phase = self.phase.swap(RELAY_PHASE_CLOSING, Ordering::AcqRel);
        self.accepting.store(false, Ordering::Release);
        let mut state = self.state.lock().await;
        if *state != AccessFlowRelayStateName::Failed {
            *state = AccessFlowRelayStateName::Draining;
        }
        drop(state);
        if phase == RELAY_PHASE_PREPARING {
            self.startup_cancellation.cancel();
        }
        let (completed, wait) = oneshot::channel();
        if self
            .command_tx
            .send(RelayCommand::CloseAdmission(completed))
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
    }

    pub(super) async fn shutdown(&self, deadline: Instant) {
        let (completed, wait) = oneshot::channel();
        if self
            .command_tx
            .send(RelayCommand::Shutdown {
                deadline,
                completed,
            })
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
    }

    #[cfg(test)]
    async fn inject_failure(&self, failure: AccessFlowRelayFailure) {
        let (observed, wait) = oneshot::channel();
        if self
            .command_tx
            .send(RelayCommand::InjectFailure { failure, observed })
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
    }

    async fn set_state(&self, state: AccessFlowRelayStateName) {
        *self.state.lock().await = state;
    }
}

pub(super) struct RelayCommandReceiver(mpsc::Receiver<RelayCommand>);

#[derive(Clone, Debug, Default)]
struct RelayCancellation(CancellationToken);

impl RelayCancellation {
    fn cancel(&self) {
        self.0.cancel();
    }
}

impl AccessCancellation for RelayCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn cancelled(&self) -> BoxAccessFuture<'_, ()> {
        Box::pin(self.0.cancelled())
    }
}

struct RelayObserver {
    active_flows: Arc<AtomicUsize>,
}

impl AccessFlowRelayObserver for RelayObserver {
    fn observe(&self, event: AccessFlowRelayEvent) {
        match event.kind {
            AccessFlowRelayEventKind::ConnectionOpened => {
                self.active_flows.fetch_add(1, Ordering::AcqRel);
            }
            AccessFlowRelayEventKind::ConnectionClosed
                if event.close_category != Some(ConnectionCloseCategory::Saturated) =>
            {
                if self
                    .active_flows
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                        active.checked_sub(1)
                    })
                    .is_err()
                {
                    tracing::error!("access flow relay active-flow observation underflow");
                }
            }
            AccessFlowRelayEventKind::Drained => {
                self.active_flows.store(0, Ordering::Release);
            }
            _ => {}
        }
        tracing::debug!(
            event = ?event.kind,
            route = event.route.as_ref().map(|route| route.as_str()),
            active_flows = event.active_flows,
            "access flow relay event"
        );
    }
}

pub(super) async fn run_relay_supervisor(
    config: AccessFlowRelayConfig,
    presentation: IdentityPresentation,
    services: Vec<Arc<ManagedService>>,
    state: Arc<AgentState>,
    control: Arc<RelayControl>,
    mut commands: RelayCommandReceiver,
) -> anyhow::Result<()> {
    let result = run_relay(
        &config,
        presentation,
        &services,
        &state,
        &control,
        &mut commands.0,
    )
    .await;
    if let Err(err) = result {
        control.accepting.store(false, Ordering::Release);
        control.set_state(AccessFlowRelayStateName::Failed).await;
        tracing::error!(error = %err, "access flow relay failed");
        state.publish_relay_fatal(RelayFatalKind::ManagerFailure);
        return Err(err);
    }
    Ok(())
}

async fn run_relay(
    config: &AccessFlowRelayConfig,
    presentation: IdentityPresentation,
    services: &[Arc<ManagedService>],
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<()> {
    if !wait_for_start_dependencies(config, services, state, control, commands).await? {
        return finish_cancelled_start(control, commands).await;
    }
    let compiled = config
        .compile_with_presentation(
            crate::config::AccessFlowRelayValidationMode::Agent,
            presentation,
        )
        .context("compile access flow relay configuration")?;
    probe_unix_endpoints(&compiled)?;
    let budget = relay_resource_budget(state.bridge_enabled, services.len())?;
    let relay = AccessFlowRelay::new(
        compiled.plan,
        UnixAccessFlowConnector::new(),
        Arc::new(RelayObserver {
            active_flows: Arc::clone(&control.active_flows),
        }),
        budget,
    )
    .context("access flow relay resource preflight")?;
    tracing::info!(
        descriptors = relay.resource_projection().total_descriptors,
        memory_bytes = relay.resource_projection().total_memory_bytes,
        "access flow relay resource preflight passed"
    );
    let prepared = relay
        .prepare(Arc::new(control.startup_cancellation.clone()))
        .await
        .context("prepare access flow relay")?;
    let running = prepared
        .activate()
        .await
        .context("activate access flow relay")?;
    control.accepting.store(true, Ordering::Release);
    if control
        .phase
        .compare_exchange(
            RELAY_PHASE_PREPARING,
            RELAY_PHASE_ACTIVE,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        control.set_state(AccessFlowRelayStateName::Accepting).await;
    } else {
        control.accepting.store(false, Ordering::Release);
    }
    run_active_relay(running, compiled.drain_timeout, state, control, commands).await
}

async fn wait_for_start_dependencies(
    config: &AccessFlowRelayConfig,
    services: &[Arc<ManagedService>],
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<bool> {
    loop {
        if state.shutting_down.load(Ordering::Acquire)
            || control.startup_cancellation.is_cancelled()
        {
            return Ok(false);
        }
        let mut missing = Vec::new();
        for name in &config.start_after_services {
            let service = services
                .iter()
                .find(|service| service.config.name == *name)
                .ok_or_else(|| anyhow::anyhow!("relay dependency {name:?} is not configured"))?;
            if !service.health_check().await.unwrap_or(false) {
                missing.push(name.as_str());
            }
        }
        if missing.is_empty() {
            return Ok(true);
        }
        tokio::select! {
            command = commands.recv() => {
                if handle_start_command(command, control).await {
                    return Ok(false);
                }
            }
            () = sleep(Duration::from_millis(250)) => {}
        }
    }
}

async fn handle_start_command(command: Option<RelayCommand>, control: &RelayControl) -> bool {
    match command {
        Some(RelayCommand::CloseAdmission(completed)) => {
            control.startup_cancellation.cancel();
            control.accepting.store(false, Ordering::Release);
            control.set_state(AccessFlowRelayStateName::Draining).await;
            let _ = completed.send(());
            true
        }
        Some(RelayCommand::Shutdown { completed, .. }) => {
            control.startup_cancellation.cancel();
            control.accepting.store(false, Ordering::Release);
            control.set_state(AccessFlowRelayStateName::Stopped).await;
            let _ = completed.send(());
            true
        }
        #[cfg(test)]
        Some(RelayCommand::InjectFailure { observed, .. }) => {
            let _ = observed.send(());
            false
        }
        None => true,
    }
}

async fn finish_cancelled_start(
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<()> {
    control.accepting.store(false, Ordering::Release);
    while let Some(command) = commands.recv().await {
        match command {
            RelayCommand::CloseAdmission(completed) => {
                let _ = completed.send(());
            }
            RelayCommand::Shutdown { completed, .. } => {
                control.set_state(AccessFlowRelayStateName::Stopped).await;
                let _ = completed.send(());
                return Ok(());
            }
            #[cfg(test)]
            RelayCommand::InjectFailure { observed, .. } => {
                let _ = observed.send(());
            }
        }
    }
    control.set_state(AccessFlowRelayStateName::Stopped).await;
    Ok(())
}

async fn run_active_relay(
    mut running: RunningAccessFlowRelay,
    configured_drain_timeout: Duration,
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            failure = running.wait_for_failure() => {
                return begin_failed_relay_shutdown(
                    running,
                    configured_drain_timeout,
                    state,
                    control,
                    commands,
                    failure,
                    None,
                ).await;
            }
            command = commands.recv() => {
                match command {
                    Some(RelayCommand::CloseAdmission(completed)) => {
                        control.accepting.store(false, Ordering::Release);
                        control.set_state(AccessFlowRelayStateName::Draining).await;
                        running.close_admission().await;
                        let _ = completed.send(());
                    }
                    Some(RelayCommand::Shutdown { deadline, completed }) => {
                        let configured_deadline = Instant::now()
                            .checked_add(configured_drain_timeout)
                            .unwrap_or(deadline);
                        let deadline = std::cmp::min(deadline, configured_deadline);
                        let result = running.shutdown(deadline).await.map(|_| ());
                        return finish_shutdown_command(
                            result,
                            state,
                            control,
                            completed,
                        ).await;
                    }
                    None => {
                        control.accepting.store(false, Ordering::Release);
                        let _ = running.shutdown(Instant::now()).await;
                        return Err(anyhow::anyhow!("access flow relay control channel closed"));
                    }
                    #[cfg(test)]
                    Some(RelayCommand::InjectFailure { failure, observed }) => {
                        return begin_failed_relay_shutdown(
                            running,
                            configured_drain_timeout,
                            state,
                            control,
                            commands,
                            failure,
                            Some(observed),
                        )
                        .await;
                    }
                }
            }
        }
    }
}

async fn finish_shutdown_command(
    result: Result<(), AccessFlowRelayFailure>,
    state: &AgentState,
    control: &RelayControl,
    completed: oneshot::Sender<()>,
) -> anyhow::Result<()> {
    control.accepting.store(false, Ordering::Release);
    if result.is_err() {
        control.set_state(AccessFlowRelayStateName::Failed).await;
        state.publish_relay_fatal(RelayFatalKind::RuntimeFailure);
    } else {
        control.set_state(AccessFlowRelayStateName::Stopped).await;
    }
    let _ = completed.send(());
    result.map_err(anyhow::Error::new)
}

async fn begin_failed_relay_shutdown(
    running: RunningAccessFlowRelay,
    configured_drain_timeout: Duration,
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
    failure: AccessFlowRelayFailure,
    observed: Option<oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    control.accepting.store(false, Ordering::Release);
    control.phase.store(RELAY_PHASE_CLOSING, Ordering::Release);
    control.set_state(AccessFlowRelayStateName::Failed).await;
    running.close_admission().await;
    state.publish_relay_fatal(RelayFatalKind::RuntimeFailure);
    if let Some(observed) = observed {
        let _ = observed.send(());
    }
    await_failed_relay_shutdown(running, configured_drain_timeout, commands, failure).await
}

async fn await_failed_relay_shutdown(
    running: RunningAccessFlowRelay,
    configured_drain_timeout: Duration,
    commands: &mut mpsc::Receiver<RelayCommand>,
    failure: access_flow_relay::AccessFlowRelayFailure,
) -> anyhow::Result<()> {
    while let Some(command) = commands.recv().await {
        match command {
            RelayCommand::CloseAdmission(completed) => {
                running.close_admission().await;
                let _ = completed.send(());
            }
            RelayCommand::Shutdown {
                deadline,
                completed,
            } => {
                let configured_deadline = Instant::now()
                    .checked_add(configured_drain_timeout)
                    .unwrap_or(deadline);
                let deadline = std::cmp::min(deadline, configured_deadline);
                let _ = running.shutdown(deadline).await;
                let _ = completed.send(());
                return Err(anyhow::Error::new(failure));
            }
            #[cfg(test)]
            RelayCommand::InjectFailure { observed, .. } => {
                let _ = observed.send(());
            }
        }
    }
    let _ = running.shutdown(Instant::now()).await;
    Err(anyhow::anyhow!(
        "access flow relay control channel closed after runtime failure"
    ))
}

fn probe_unix_endpoints(compiled: &CompiledAccessFlowRelayConfig) -> anyhow::Result<()> {
    for route in compiled.plan.routes() {
        let path = route.endpoint().path().as_str();
        let metadata = std::fs::symlink_metadata(path).with_context(|| {
            format!(
                "access flow route {:?} endpoint is unavailable",
                route.name().as_str()
            )
        })?;
        let file_type: FileType = metadata.file_type();
        if file_type.is_symlink() || !file_type.is_socket() {
            anyhow::bail!(
                "access flow route {:?} endpoint is not a Unix socket",
                route.name().as_str()
            );
        }
    }
    Ok(())
}

fn relay_resource_budget(
    bridge_enabled: bool,
    service_count: usize,
) -> anyhow::Result<AccessFlowRelayResourceBudget> {
    let services = u64::try_from(service_count).context("service count exceeds u64")?;
    let descriptor_reserve = if bridge_enabled {
        BRIDGE_BASE_NON_RELAY_DESCRIPTORS
    } else {
        NO_BRIDGE_BASE_NON_RELAY_DESCRIPTORS
    }
    .checked_add(
        PER_SERVICE_NON_RELAY_DESCRIPTORS
            .checked_mul(services)
            .ok_or_else(|| anyhow::anyhow!("non-relay descriptor reserve overflow"))?,
    )
    .ok_or_else(|| anyhow::anyhow!("non-relay descriptor reserve overflow"))?;
    let descriptors = soft_nofile_limit()?
        .checked_sub(descriptor_reserve)
        .ok_or_else(|| anyhow::anyhow!("RLIMIT_NOFILE is below the agent non-relay reserve"))?;

    let memory_base = if bridge_enabled {
        BRIDGE_NON_RELAY_MEMORY_BYTES
    } else {
        NO_BRIDGE_NON_RELAY_MEMORY_BYTES
    };
    let memory_reserve = memory_base
        .checked_add(
            PER_SERVICE_NON_RELAY_MEMORY_BYTES
                .checked_mul(services)
                .ok_or_else(|| anyhow::anyhow!("non-relay memory reserve overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("non-relay memory reserve overflow"))?;
    let memory_bytes = TOTAL_AGENT_MEMORY_PREFLIGHT_BYTES
        .checked_sub(memory_reserve)
        .ok_or_else(|| {
            anyhow::anyhow!("agent non-relay reserve exceeds 256 MiB preflight ceiling")
        })?;
    Ok(AccessFlowRelayResourceBudget {
        descriptors,
        memory_bytes,
    })
}

fn soft_nofile_limit() -> anyhow::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("read RLIMIT_NOFILE");
    }
    Ok(if limit.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        limit.rlim_cur
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AccessFlowRelayPresentation, AccessFlowRelayRoute, AccessFlowRelayTransport,
    };
    use access_flow_relay::AccessFlowRelayFailureKind;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_config(listen: String, path: String) -> AccessFlowRelayConfig {
        let allowed_port = listen
            .parse::<std::net::SocketAddrV4>()
            .map(|address| address.port())
            .unwrap_or(80);
        AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::Disabled {},
            routes: vec![AccessFlowRelayRoute {
                name: "http".into(),
                listen,
                allowed_destination_ports: vec![allowed_port],
                transport: AccessFlowRelayTransport::Unix { path },
            }],
        }
    }

    fn observed_event(
        kind: AccessFlowRelayEventKind,
        close_category: Option<ConnectionCloseCategory>,
        active_flows: usize,
    ) -> AccessFlowRelayEvent {
        AccessFlowRelayEvent {
            kind,
            route: None,
            destination: None,
            close_category,
            duration: None,
            bytes_to_channel: 0,
            bytes_to_source: 0,
            active_flows,
        }
    }

    #[test]
    fn resource_budget_subtracts_frozen_non_relay_memory() {
        let no_bridge = relay_resource_budget(false, 4).unwrap();
        let bridge = relay_resource_budget(true, 4).unwrap();
        assert_eq!(
            no_bridge.memory_bytes,
            TOTAL_AGENT_MEMORY_PREFLIGHT_BYTES
                - NO_BRIDGE_NON_RELAY_MEMORY_BYTES
                - 4 * PER_SERVICE_NON_RELAY_MEMORY_BYTES
        );
        assert_eq!(
            bridge.memory_bytes,
            TOTAL_AGENT_MEMORY_PREFLIGHT_BYTES
                - BRIDGE_NON_RELAY_MEMORY_BYTES
                - 4 * PER_SERVICE_NON_RELAY_MEMORY_BYTES
        );
    }

    #[test]
    fn relay_resource_projection_accounts_for_retained_bearer() {
        let config = test_config("127.0.0.1:3128".into(), "/tmp/access-flow.sock".into());
        let compiled = config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(b"abcdefghijklmnopqrstuvwxyzABCDEF")
                        .unwrap(),
                ),
            )
            .unwrap();
        let relay = AccessFlowRelay::new(
            compiled.plan,
            UnixAccessFlowConnector::new(),
            Arc::new(RelayObserver {
                active_flows: Arc::new(AtomicUsize::new(0)),
            }),
            relay_resource_budget(false, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(relay.resource_projection().presentation_bytes, 32);
        assert!(
            relay.resource_projection().total_memory_bytes
                >= relay.resource_projection().presentation_bytes
        );
    }

    #[test]
    fn observer_delta_accounting_is_stable_under_snapshot_interleaving() {
        let active_flows = Arc::new(AtomicUsize::new(0));
        let observer = RelayObserver {
            active_flows: Arc::clone(&active_flows),
        };

        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionOpened,
            None,
            2,
        ));
        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionOpened,
            None,
            1,
        ));
        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionClosed,
            Some(ConnectionCloseCategory::Saturated),
            2,
        ));
        assert_eq!(active_flows.load(Ordering::Acquire), 2);

        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionClosed,
            Some(ConnectionCloseCategory::Complete),
            0,
        ));
        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionClosed,
            Some(ConnectionCloseCategory::Complete),
            1,
        ));
        assert_eq!(active_flows.load(Ordering::Acquire), 0);

        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionOpened,
            None,
            1,
        ));
        observer.observe(observed_event(AccessFlowRelayEventKind::Drained, None, 0));
        assert_eq!(active_flows.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn supervisor_probes_prepares_reports_ready_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("access-flow.sock");
        let endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        endpoint_listener.set_nonblocking(true).unwrap();
        let endpoint_listener = tokio::net::UnixListener::from_std(endpoint_listener).unwrap();
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let config = test_config(listen.to_string(), endpoint.display().to_string());
        let (control, commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Disabled,
            Vec::new(),
            state,
            control.clone(),
            commands,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while !control.is_ready() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status = control.status().await;
        assert_eq!(status.state, AccessFlowRelayStateName::Accepting);
        assert!(status.ready);
        assert_eq!(status.routes.len(), 1);
        assert_eq!(status.routes[0].name, "http");

        let mut client = tokio::net::TcpStream::connect(listen).await.unwrap();
        let (mut channel, _) = endpoint_listener.accept().await.unwrap();
        let mut preface = [0_u8; 16];
        channel.read_exact(&mut preface).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while control.active_flows() != 1 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        control.close_admission().await;
        assert!(!control.status().await.ready);
        assert!(!control.startup_cancellation.is_cancelled());
        client.write_all(b"still-open").await.unwrap();
        let mut received = [0_u8; 10];
        channel.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"still-open");
        assert_eq!(control.active_flows(), 1);

        drop(client);
        drop(channel);
        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        supervisor.await.unwrap().unwrap();
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Stopped
        );
    }

    #[tokio::test]
    async fn fatal_listener_path_retains_live_flows_until_root_ordered_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("access-flow.sock");
        let endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        endpoint_listener.set_nonblocking(true).unwrap();
        let endpoint_listener = tokio::net::UnixListener::from_std(endpoint_listener).unwrap();
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let config = test_config(listen.to_string(), endpoint.display().to_string());
        let (control, commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Disabled,
            Vec::new(),
            state.clone(),
            control.clone(),
            commands,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while !control.is_ready() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut client = tokio::net::TcpStream::connect(listen).await.unwrap();
        let (mut channel, _) = endpoint_listener.accept().await.unwrap();
        let mut preface = [0_u8; 16];
        channel.read_exact(&mut preface).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while control.active_flows() != 1 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(super::super::idle::has_active_attachments(
            0,
            control.active_flows(),
            0
        ));

        control
            .inject_failure(AccessFlowRelayFailure {
                kind: AccessFlowRelayFailureKind::ListenerAccept,
                route: None,
            })
            .await;
        assert_eq!(state.relay_fatal(), Some(RelayFatalKind::RuntimeFailure));
        let status = control.status().await;
        assert_eq!(status.state, AccessFlowRelayStateName::Failed);
        assert!(!status.ready);
        assert!(!supervisor.is_finished());

        client.write_all(b"still-live").await.unwrap();
        let mut received = [0_u8; 10];
        channel.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"still-live");
        control.close_admission().await;
        control.close_admission().await;
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Failed
        );

        drop(client);
        drop(channel);
        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        assert!(supervisor.await.unwrap().is_err());
        assert_eq!(control.active_flows(), 0);
    }

    #[tokio::test]
    async fn startup_manager_failure_publishes_typed_fatal_and_failed_status() {
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let config = test_config(listen.to_string(), "/missing/access-flow.sock".into());
        let (control, commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));

        let result = run_relay_supervisor(
            config,
            IdentityPresentation::Disabled,
            Vec::new(),
            state.clone(),
            control.clone(),
            commands,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(state.relay_fatal(), Some(RelayFatalKind::ManagerFailure));
        let status = control.status().await;
        assert_eq!(status.state, AccessFlowRelayStateName::Failed);
        assert!(!status.ready);
    }

    #[tokio::test]
    async fn shutdown_error_is_published_before_completion_acknowledgement() {
        let config = test_config("127.0.0.1:3128".into(), "/missing/access-flow.sock".into());
        let (control, _commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let (completed, acknowledged) = oneshot::channel();
        let finish_state = state.clone();
        let finish_control = control.clone();
        let finish = tokio::spawn(async move {
            finish_shutdown_command(
                Err(AccessFlowRelayFailure {
                    kind: AccessFlowRelayFailureKind::ListenerAccept,
                    route: None,
                }),
                &finish_state,
                &finish_control,
                completed,
            )
            .await
        });

        acknowledged.await.unwrap();
        assert_eq!(
            state.relay_fatal(),
            Some(RelayFatalKind::RuntimeFailure),
            "fatal must be visible before shutdown acknowledgement"
        );
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Failed
        );
        assert!(finish.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn prepare_bind_failure_is_atomic_across_routes() {
        let dir = tempfile::tempdir().unwrap();
        let first_endpoint = dir.path().join("first.sock");
        let second_endpoint = dir.path().join("second.sock");
        let _first_endpoint_listener =
            std::os::unix::net::UnixListener::bind(&first_endpoint).unwrap();
        let _second_endpoint_listener =
            std::os::unix::net::UnixListener::bind(&second_endpoint).unwrap();

        let first_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let first_listen = first_reservation.local_addr().unwrap();
        drop(first_reservation);
        let second_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listen = second_reservation.local_addr().unwrap();

        let mut config = test_config(
            first_listen.to_string(),
            first_endpoint.display().to_string(),
        );
        config.routes.push(AccessFlowRelayRoute {
            name: "https".into(),
            listen: second_listen.to_string(),
            allowed_destination_ports: vec![second_listen.port()],
            transport: AccessFlowRelayTransport::Unix {
                path: second_endpoint.display().to_string(),
            },
        });
        let compiled = config
            .compile(crate::config::AccessFlowRelayValidationMode::Agent)
            .unwrap();
        probe_unix_endpoints(&compiled).unwrap();
        let relay = AccessFlowRelay::new(
            compiled.plan,
            UnixAccessFlowConnector::new(),
            Arc::new(RelayObserver {
                active_flows: Arc::new(AtomicUsize::new(0)),
            }),
            relay_resource_budget(false, 0).unwrap(),
        )
        .unwrap();

        assert!(
            relay
                .prepare(Arc::new(RelayCancellation::default()))
                .await
                .is_err()
        );
        std::net::TcpListener::bind(first_listen)
            .expect("failed prepare must release listeners bound for earlier routes");
    }

    #[test]
    fn endpoint_probe_rejects_non_socket_and_symlink_without_owner_mode_policy() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let compiled = test_config("127.0.0.1:3128".into(), missing.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent)
            .unwrap();
        assert!(probe_unix_endpoints(&compiled).is_err());

        let regular = dir.path().join("regular");
        std::fs::write(&regular, b"not a socket").unwrap();
        let compiled = test_config("127.0.0.1:3128".into(), regular.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent)
            .unwrap();
        assert!(probe_unix_endpoints(&compiled).is_err());

        let socket = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut permissions = std::fs::metadata(&socket).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o777);
        std::fs::set_permissions(&socket, permissions).unwrap();
        let compiled = test_config("127.0.0.1:3128".into(), socket.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent)
            .unwrap();
        probe_unix_endpoints(&compiled).unwrap();

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        let compiled = test_config("127.0.0.1:3128".into(), link.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent)
            .unwrap();
        assert!(probe_unix_endpoints(&compiled).is_err());
    }
}
