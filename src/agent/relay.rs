use access_async_contracts::{AccessCancellation, BoxAccessFuture};
#[cfg(test)]
use access_flow_relay::ConnectionCloseCategory;
use access_flow_relay::{
    AccessFlowRelay, AccessFlowRelayEvent, AccessFlowRelayEventKind, AccessFlowRelayFailure,
    AccessFlowRelayObserver, AccessFlowRelayResourceBudget, RunningAccessFlowRelay,
};
use access_identity::IdentityPresentation;
use anyhow::Context;
use std::os::unix::fs::FileTypeExt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Instant;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use crate::agent_control::{
    AccessFlowRelayRouteStatus, AccessFlowRelayStateName, AccessFlowRelayStatus,
};
use crate::config::{
    AccessFlowRelayConfig, CompiledAccessFlowRelayConfig, CompiledAccessFlowRelayEndpoint,
};

use super::relay_transport::{
    PendingRelayTransportReload, RelayTransportRuntime, RelayTransportTrustBudget,
};
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
    ReloadSecurity(PendingRelayTransportReload),
    CloseAdmission {
        deadline: Instant,
        completed: oneshot::Sender<()>,
    },
    Shutdown {
        deadline: Instant,
        completed: oneshot::Sender<()>,
    },
    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    InjectFailure {
        failure: AccessFlowRelayFailure,
        observed: oneshot::Sender<()>,
    },
}

#[derive(Debug)]
pub(super) struct RelayControl {
    state: Mutex<AccessFlowRelayStateName>,
    routes: Box<[(String, Option<access_tls_trust::TlsClientTrustMode>)]>,
    active_flows: Arc<AtomicUsize>,
    accepting: AtomicBool,
    security_healthy: Arc<AtomicBool>,
    security_failure: StdMutex<Option<super::relay_transport::RelayTransportError>>,
    reload_in_progress: AtomicBool,
    phase: AtomicU8,
    lifecycle: StdMutex<()>,
    command_tx: mpsc::Sender<RelayCommand>,
    transport: OnceLock<RelayTransportRuntime>,
    startup_cancellation: RelayCancellation,
    drain_timeout: Duration,
    #[cfg(test)]
    activation_pause: Mutex<Option<ActivationPause>>,
    #[cfg(test)]
    reload_pause: StdMutex<Option<ReloadWorkerPause>>,
    #[cfg(test)]
    reload_command_pause: Mutex<Option<ActivationPause>>,
    #[cfg(test)]
    close_reload_join_reached: Mutex<Option<oneshot::Sender<()>>>,
    #[cfg(test)]
    close_reload_join_completed: Mutex<Option<oneshot::Sender<()>>>,
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
                routes: config
                    .routes
                    .iter()
                    .map(|route| {
                        let trust_mode = match route.transport {
                            crate::config::AccessFlowRelayTransport::Unix { .. } => None,
                            crate::config::AccessFlowRelayTransport::TlsTcp { trust, .. } => {
                                Some(trust)
                            }
                        };
                        (route.name.clone(), trust_mode)
                    })
                    .collect(),
                active_flows: Arc::new(AtomicUsize::new(0)),
                accepting: AtomicBool::new(false),
                security_healthy: Arc::new(AtomicBool::new(false)),
                security_failure: StdMutex::new(None),
                reload_in_progress: AtomicBool::new(false),
                phase: AtomicU8::new(RELAY_PHASE_PREPARING),
                lifecycle: StdMutex::new(()),
                command_tx,
                transport: OnceLock::new(),
                startup_cancellation,
                drain_timeout,
                #[cfg(test)]
                activation_pause: Mutex::new(None),
                #[cfg(test)]
                reload_pause: StdMutex::new(None),
                #[cfg(test)]
                reload_command_pause: Mutex::new(None),
                #[cfg(test)]
                close_reload_join_reached: Mutex::new(None),
                #[cfg(test)]
                close_reload_join_completed: Mutex::new(None),
            }),
            RelayCommandReceiver(command_rx),
        )
    }

    pub(super) async fn status(&self) -> AccessFlowRelayStatus {
        let state = *self.state.lock().await;
        let accepting = self.phase.load(Ordering::Acquire) == RELAY_PHASE_ACTIVE
            && self.accepting.load(Ordering::Acquire)
            && self.security_healthy.load(Ordering::Acquire);
        AccessFlowRelayStatus {
            state,
            ready: state == AccessFlowRelayStateName::Accepting && accepting,
            active_flows: self.active_flows.load(Ordering::Acquire),
            routes: self
                .routes
                .iter()
                .map(|(name, trust_mode)| AccessFlowRelayRouteStatus {
                    name: name.clone(),
                    accepting,
                    trust_mode: *trust_mode,
                })
                .collect(),
            trust_failure: if self
                .transport
                .get()
                .is_some_and(RelayTransportRuntime::reload_blocked)
            {
                Some("trust_reload_blocked".to_string())
            } else {
                self.security_failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .map(|error| error.status_code().to_string())
            },
        }
    }

    pub(super) fn active_flows(&self) -> usize {
        self.active_flows.load(Ordering::Acquire)
    }

    pub(super) fn is_ready(&self) -> bool {
        self.phase.load(Ordering::Acquire) == RELAY_PHASE_ACTIVE
            && self.accepting.load(Ordering::Acquire)
            && self.security_healthy.load(Ordering::Acquire)
    }

    pub(super) fn drain_timeout(&self) -> Duration {
        self.drain_timeout
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) async fn close_admission(&self) {
        let deadline = Instant::now()
            .checked_add(self.drain_timeout)
            .unwrap_or_else(Instant::now);
        self.close_admission_by(deadline).await;
    }

    pub(super) async fn close_admission_by(&self, deadline: Instant) {
        let phase = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let phase = self.phase.swap(RELAY_PHASE_CLOSING, Ordering::AcqRel);
            self.accepting.store(false, Ordering::Release);
            self.security_healthy.store(false, Ordering::Release);
            phase
        };
        let mut state = self.state.lock().await;
        if *state != AccessFlowRelayStateName::Failed {
            *state = AccessFlowRelayStateName::Draining;
        }
        drop(state);
        if phase == RELAY_PHASE_PREPARING {
            self.startup_cancellation.cancel();
        }
        if let Some(transport) = self.transport.get() {
            transport.close();
        }
        let (completed, wait) = oneshot::channel();
        if self
            .command_tx
            .send(RelayCommand::CloseAdmission {
                deadline,
                completed,
            })
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
    }

    pub(super) fn initiate_security_reload(self: &Arc<Self>) -> Result<(), ()> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.phase.load(Ordering::Acquire) != RELAY_PHASE_ACTIVE {
            tracing::debug!(
                category = "security_material",
                "access flow relay trust reload was rejected"
            );
            return Err(());
        }
        if self
            .reload_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::debug!(
                category = "security_material",
                "access flow relay concurrent trust reload was rejected"
            );
            return Err(());
        }
        let command = match self.command_tx.try_reserve() {
            Ok(command) => command,
            Err(_) => {
                self.reload_in_progress.store(false, Ordering::Release);
                tracing::debug!(
                    category = "security_material",
                    "access flow relay trust reload was not started"
                );
                return Err(());
            }
        };
        let Some(transport) = self.transport.get() else {
            self.reload_in_progress.store(false, Ordering::Release);
            tracing::debug!(
                category = "security_material",
                "access flow relay trust reload was not started"
            );
            return Err(());
        };
        #[cfg(test)]
        let reload_pause = self
            .reload_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        #[cfg(test)]
        let reload = if let Some(reload_pause) = reload_pause {
            transport.begin_reload_with_hook(move || {
                let _ = reload_pause.reached.send(());
                let _ = reload_pause.resume.recv();
            })
        } else {
            transport.begin_reload()
        };
        #[cfg(not(test))]
        let reload = transport.begin_reload();
        let pending = match reload {
            Ok(Some(pending)) => pending,
            Ok(None) => {
                drop(command);
                finish_security_reload(self, ReloadFinish::Completed, Ok(()));
                return Ok(());
            }
            Err(error) => {
                drop(command);
                self.set_security_failure(Some(error));
                finish_security_reload(self, ReloadFinish::Rejected, Err(()));
                return Err(());
            }
        };
        command.send(RelayCommand::ReloadSecurity(pending));
        Ok(())
    }

    pub(super) async fn shutdown(&self, deadline: Instant) {
        {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let phase = self.phase.swap(RELAY_PHASE_CLOSING, Ordering::AcqRel);
            self.accepting.store(false, Ordering::Release);
            self.security_healthy.store(false, Ordering::Release);
            if phase == RELAY_PHASE_PREPARING {
                self.startup_cancellation.cancel();
            }
        }
        if let Some(transport) = self.transport.get() {
            transport.close();
        }
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
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

    fn set_security_failure(&self, error: Option<super::relay_transport::RelayTransportError>) {
        *self
            .security_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = error;
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    async fn pause_before_activation(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, wait_for_reached) = oneshot::channel();
        let (resume, wait_for_resume) = oneshot::channel();
        let previous = self.activation_pause.lock().await.replace(ActivationPause {
            reached,
            resume: wait_for_resume,
        });
        assert!(previous.is_none(), "activation pause already installed");
        (wait_for_reached, resume)
    }

    #[cfg(test)]
    async fn wait_for_activation_pause(&self) {
        let Some(pause) = self.activation_pause.lock().await.take() else {
            return;
        };
        let _ = pause.reached.send(());
        let _ = pause.resume.await;
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn pause_blocking_reload(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, wait_for_reached) = std::sync::mpsc::channel();
        let (resume, wait_for_resume) = std::sync::mpsc::channel();
        let previous = self
            .reload_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(ReloadWorkerPause {
                reached,
                resume: wait_for_resume,
            });
        assert!(previous.is_none(), "reload pause already installed");
        (wait_for_reached, resume)
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    async fn pause_before_reload_command(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, wait_for_reached) = oneshot::channel();
        let (resume, wait_for_resume) = oneshot::channel();
        let previous = self
            .reload_command_pause
            .lock()
            .await
            .replace(ActivationPause {
                reached,
                resume: wait_for_resume,
            });
        assert!(previous.is_none(), "reload command pause already installed");
        (wait_for_reached, resume)
    }

    #[cfg(test)]
    async fn wait_for_reload_command_pause(&self) {
        let Some(pause) = self.reload_command_pause.lock().await.take() else {
            return;
        };
        let _ = pause.reached.send(());
        let _ = pause.resume.await;
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    async fn observe_close_before_reload_join(&self) -> oneshot::Receiver<()> {
        let (reached, wait_for_reached) = oneshot::channel();
        let previous = self.close_reload_join_reached.lock().await.replace(reached);
        assert!(
            previous.is_none(),
            "close-before-reload-join observer already installed"
        );
        wait_for_reached
    }

    #[cfg(test)]
    async fn publish_close_before_reload_join(&self) {
        if let Some(reached) = self.close_reload_join_reached.lock().await.take() {
            let _ = reached.send(());
        }
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    async fn observe_close_after_reload_join(&self) -> oneshot::Receiver<()> {
        let (completed, wait_for_completed) = oneshot::channel();
        let previous = self
            .close_reload_join_completed
            .lock()
            .await
            .replace(completed);
        assert!(
            previous.is_none(),
            "close-after-reload-join observer already installed"
        );
        wait_for_completed
    }

    #[cfg(test)]
    async fn publish_close_after_reload_join(&self) {
        if let Some(completed) = self.close_reload_join_completed.lock().await.take() {
            let _ = completed.send(());
        }
    }
}

pub(super) struct RelayCommandReceiver(mpsc::Receiver<RelayCommand>);

#[cfg(test)]
#[derive(Debug)]
struct ActivationPause {
    reached: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

#[cfg(test)]
#[derive(Debug)]
struct ReloadWorkerPause {
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupDisposition {
    Ready,
    Cancelled,
    ShutdownCompleted,
}

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
            AccessFlowRelayEventKind::ConnectionClosed => {
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
            close_category = ?event.close_category,
            active_flows = event.active_flows,
            "access flow relay event"
        );
    }
}

pub(super) async fn run_relay_supervisor(
    config: AccessFlowRelayConfig,
    presentation: IdentityPresentation,
    execution_context: Option<String>,
    services: Vec<Arc<ManagedService>>,
    state: Arc<AgentState>,
    control: Arc<RelayControl>,
    mut commands: RelayCommandReceiver,
) -> anyhow::Result<()> {
    let result = run_relay(
        &config,
        presentation,
        execution_context.as_deref(),
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
    execution_context: Option<&str>,
    services: &[Arc<ManagedService>],
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<()> {
    match wait_for_start_dependencies(config, services, state, control, commands).await? {
        StartupDisposition::Ready => {}
        StartupDisposition::Cancelled => {
            return finish_cancelled_start(control, commands).await;
        }
        StartupDisposition::ShutdownCompleted => return Ok(()),
    }
    let compiled = config
        .compile_with_presentation(
            crate::config::AccessFlowRelayValidationMode::Agent,
            presentation,
            execution_context,
        )
        .context("compile access flow relay configuration")?;
    for route in &config.routes {
        if matches!(
            route.transport,
            crate::config::AccessFlowRelayTransport::TlsTcp {
                trust: access_tls_trust::TlsClientTrustMode::Insecure,
                ..
            }
        ) {
            tracing::warn!(
                category = "security_material",
                route = %route.name,
                "Access Flow TLS route does not authenticate the remote server"
            );
        }
    }
    probe_unix_endpoints(&compiled)?;
    let transport_reserve = RelayTransportRuntime::resource_reserve(&compiled.plan)
        .context("project access flow relay transport reload resources")?;
    let agent_budget = relay_resource_budget(state.bridge_enabled, services.len(), 0, 0)?;
    let relay_budget = relay_resource_budget(
        state.bridge_enabled,
        services.len(),
        transport_reserve.descriptors,
        transport_reserve.memory_bytes,
    )?;
    let transport =
        RelayTransportRuntime::prepare(&compiled.plan, Arc::clone(&control.security_healthy))
            .context("prepare access flow relay transport")?;
    control
        .transport
        .set(transport.clone())
        .map_err(|_| anyhow::anyhow!("access flow relay transport was already installed"))?;
    let relay = AccessFlowRelay::new(
        compiled.plan,
        transport.connector(),
        Arc::new(RelayObserver {
            active_flows: Arc::clone(&control.active_flows),
        }),
        relay_budget,
    )
    .context("access flow relay resource preflight")?;
    let relay_projection = relay.resource_projection();
    let trust_budget = relay_transport_trust_budget(agent_budget, relay_projection)?;
    transport
        .configure_trust_budget(trust_budget)
        .context("configure Access Flow TLS trust residual")?;
    let projected_descriptors = relay
        .resource_projection()
        .total_descriptors
        .checked_add(transport_reserve.descriptors)
        .context("access flow relay descriptor projection overflow")?;
    let projected_memory_bytes = relay
        .resource_projection()
        .total_memory_bytes
        .checked_add(transport_reserve.memory_bytes)
        .context("access flow relay memory projection overflow")?;
    tracing::info!(
        descriptors = projected_descriptors,
        memory_bytes = projected_memory_bytes,
        "access flow relay resource preflight passed"
    );
    match transport.activate_prepared().await {
        Ok(()) => {}
        Err(error) if ordered_startup_cancelled(state, control) => {
            tracing::debug!(
                error = %error,
                "access flow relay transport activation cancelled by shutdown"
            );
            return finish_cancelled_start(control, commands).await;
        }
        Err(error) => {
            return Err(error).context("activate access flow relay transport");
        }
    }
    let prepared = match relay
        .prepare(Arc::new(control.startup_cancellation.clone()))
        .await
    {
        Ok(prepared) => prepared,
        Err(err) if ordered_startup_cancelled(state, control) => {
            tracing::debug!(error = %err, "access flow relay prepare cancelled by shutdown");
            return finish_cancelled_start(control, commands).await;
        }
        Err(err) => return Err(err).context("prepare access flow relay"),
    };
    #[cfg(test)]
    control.wait_for_activation_pause().await;
    let running = match prepared.activate().await {
        Ok(running) => running,
        Err(err) if ordered_startup_cancelled(state, control) => {
            tracing::debug!(error = %err, "access flow relay activation cancelled by shutdown");
            return finish_cancelled_start(control, commands).await;
        }
        Err(err) => return Err(err).context("activate access flow relay"),
    };
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
    run_active_relay(
        running,
        transport,
        compiled.drain_timeout,
        state,
        control,
        commands,
    )
    .await
}

async fn wait_for_start_dependencies(
    config: &AccessFlowRelayConfig,
    services: &[Arc<ManagedService>],
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<StartupDisposition> {
    loop {
        if state.shutting_down.load(Ordering::Acquire)
            || control.startup_cancellation.is_cancelled()
        {
            return Ok(StartupDisposition::Cancelled);
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
            return Ok(StartupDisposition::Ready);
        }
        tokio::select! {
            command = commands.recv() => {
                if let Some(disposition) = handle_start_command(command, control).await {
                    return Ok(disposition);
                }
            }
            () = sleep(Duration::from_millis(250)) => {}
        }
    }
}

async fn handle_start_command(
    command: Option<RelayCommand>,
    control: &RelayControl,
) -> Option<StartupDisposition> {
    match command {
        Some(RelayCommand::ReloadSecurity(pending)) => {
            if let Some(transport) = control.transport.get() {
                transport.close();
            }
            let _ = pending.complete().await;
            finish_security_reload(control, ReloadFinish::Rejected, Err(()));
            None
        }
        Some(RelayCommand::CloseAdmission { completed, .. }) => {
            control.startup_cancellation.cancel();
            control.accepting.store(false, Ordering::Release);
            control.set_state(AccessFlowRelayStateName::Draining).await;
            let _ = completed.send(());
            Some(StartupDisposition::Cancelled)
        }
        Some(RelayCommand::Shutdown { completed, .. }) => {
            control.startup_cancellation.cancel();
            control.accepting.store(false, Ordering::Release);
            control.set_state(AccessFlowRelayStateName::Stopped).await;
            let _ = completed.send(());
            Some(StartupDisposition::ShutdownCompleted)
        }
        #[cfg(test)]
        Some(RelayCommand::InjectFailure { observed, .. }) => {
            let _ = observed.send(());
            None
        }
        None => Some(StartupDisposition::Cancelled),
    }
}

fn ordered_startup_cancelled(state: &AgentState, control: &RelayControl) -> bool {
    state.shutting_down.load(Ordering::Acquire) || control.startup_cancellation.is_cancelled()
}

async fn finish_cancelled_start(
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<()> {
    control.accepting.store(false, Ordering::Release);
    while let Some(command) = commands.recv().await {
        match command {
            RelayCommand::ReloadSecurity(pending) => {
                if let Some(transport) = control.transport.get() {
                    transport.close();
                }
                let _ = pending.complete().await;
                finish_security_reload(control, ReloadFinish::Rejected, Err(()));
            }
            RelayCommand::CloseAdmission { completed, .. } => {
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
    transport: RelayTransportRuntime,
    configured_drain_timeout: Duration,
    state: &AgentState,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
) -> anyhow::Result<()> {
    let mut pending_reload = None;
    loop {
        tokio::select! {
            failure = running.wait_for_failure() => {
                transport.close();
                let reload_deadline = Instant::now()
                    .checked_add(configured_drain_timeout)
                    .unwrap_or_else(Instant::now);
                finish_pending_reload(
                    control,
                    &mut pending_reload,
                    ReloadFinish::Shutdown,
                    reload_deadline,
                ).await;
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
                    Some(RelayCommand::ReloadSecurity(reload)) => {
                        #[cfg(test)]
                        control.wait_for_reload_command_pause().await;
                        if pending_reload.is_some() {
                            tracing::debug!(
                                category = "security_material",
                                "access flow relay concurrent trust reload was rejected"
                            );
                            let result = reload.complete().await.map_err(|_| ());
                            finish_security_reload(
                                control,
                                ReloadFinish::Rejected,
                                result,
                            );
                        } else {
                            pending_reload = Some(reload);
                            tracing::debug!(
                                category = "security_material",
                                "access flow relay trust reload started"
                            );
                        }
                    }
                    Some(RelayCommand::CloseAdmission {
                        deadline,
                        completed,
                    }) => {
                        transport.close();
                        #[cfg(test)]
                        control.publish_close_before_reload_join().await;
                        finish_pending_reload(
                            control,
                            &mut pending_reload,
                            ReloadFinish::Shutdown,
                            deadline,
                        ).await;
                        #[cfg(test)]
                        control.publish_close_after_reload_join().await;
                        control.accepting.store(false, Ordering::Release);
                        control.set_state(AccessFlowRelayStateName::Draining).await;
                        running.close_admission().await;
                        let _ = completed.send(());
                    }
                    Some(RelayCommand::Shutdown { deadline, completed }) => {
                        transport.close();
                        finish_pending_reload(
                            control,
                            &mut pending_reload,
                            ReloadFinish::Shutdown,
                            deadline,
                        ).await;
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
                        transport.close();
                        finish_pending_reload(
                            control,
                            &mut pending_reload,
                            ReloadFinish::Shutdown,
                            Instant::now(),
                        ).await;
                        control.accepting.store(false, Ordering::Release);
                        let _ = running.shutdown(Instant::now()).await;
                        return Err(anyhow::anyhow!("access flow relay control channel closed"));
                    }
                    #[cfg(test)]
                    Some(RelayCommand::InjectFailure { failure, observed }) => {
                        transport.close();
                        let reload_deadline = Instant::now()
                            .checked_add(configured_drain_timeout)
                            .unwrap_or_else(Instant::now);
                        finish_pending_reload(
                            control,
                            &mut pending_reload,
                            ReloadFinish::Shutdown,
                            reload_deadline,
                        ).await;
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
            result = wait_for_reload(&mut pending_reload), if pending_reload.is_some() => {
                control.set_security_failure(result.as_ref().err().copied());
                let pending = pending_reload
                    .take()
                    .expect("completed reload remains owned by the relay loop");
                drop(pending);
                finish_security_reload(
                    control,
                    ReloadFinish::Completed,
                    result.map_err(|_| ()),
                );
            }
        }
    }
}

async fn wait_for_reload(
    pending: &mut Option<PendingRelayTransportReload>,
) -> Result<(), super::relay_transport::RelayTransportError> {
    pending
        .as_mut()
        .expect("reload wait is guarded by task presence")
        .wait()
        .await
}

async fn finish_pending_reload(
    control: &RelayControl,
    pending: &mut Option<PendingRelayTransportReload>,
    finish: ReloadFinish,
    deadline: Instant,
) {
    if let Some(pending) = pending.take() {
        let result = pending.complete_by(deadline.into()).await.map_err(|_| ());
        finish_security_reload(control, finish, result);
    }
}

#[derive(Clone, Copy)]
enum ReloadFinish {
    Completed,
    Rejected,
    Shutdown,
}

fn finish_security_reload(control: &RelayControl, finish: ReloadFinish, result: Result<(), ()>) {
    control.reload_in_progress.store(false, Ordering::Release);
    match (finish, result) {
        (ReloadFinish::Completed, Ok(())) => tracing::debug!(
            category = "security_material",
            "access flow relay trust reload completed"
        ),
        (ReloadFinish::Completed, Err(())) => tracing::warn!(
            category = "security_material",
            "access flow relay trust reload failed"
        ),
        (ReloadFinish::Rejected, _) => tracing::debug!(
            category = "security_material",
            "access flow relay trust reload was rejected"
        ),
        (ReloadFinish::Shutdown, _) => tracing::debug!(
            category = "security_material",
            "access flow relay trust reload joined during shutdown"
        ),
    }
}

async fn finish_shutdown_command(
    result: Result<(), AccessFlowRelayFailure>,
    state: &AgentState,
    control: &RelayControl,
    completed: oneshot::Sender<()>,
) -> anyhow::Result<()> {
    control.accepting.store(false, Ordering::Release);
    control.security_healthy.store(false, Ordering::Release);
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
    control.security_healthy.store(false, Ordering::Release);
    control.phase.store(RELAY_PHASE_CLOSING, Ordering::Release);
    control.set_state(AccessFlowRelayStateName::Failed).await;
    running.close_admission().await;
    state.publish_relay_fatal(RelayFatalKind::RuntimeFailure);
    if let Some(observed) = observed {
        let _ = observed.send(());
    }
    await_failed_relay_shutdown(
        running,
        configured_drain_timeout,
        control,
        commands,
        failure,
    )
    .await
}

async fn await_failed_relay_shutdown(
    running: RunningAccessFlowRelay,
    configured_drain_timeout: Duration,
    control: &RelayControl,
    commands: &mut mpsc::Receiver<RelayCommand>,
    failure: access_flow_relay::AccessFlowRelayFailure,
) -> anyhow::Result<()> {
    while let Some(command) = commands.recv().await {
        match command {
            RelayCommand::ReloadSecurity(pending) => {
                if let Some(transport) = control.transport.get() {
                    transport.close();
                }
                let _ = pending.complete().await;
                finish_security_reload(control, ReloadFinish::Rejected, Err(()));
            }
            RelayCommand::CloseAdmission { completed, .. } => {
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
        let CompiledAccessFlowRelayEndpoint::Unix(endpoint) = route.endpoint() else {
            continue;
        };
        let path = endpoint.path().as_str();
        let metadata = std::fs::symlink_metadata(path).with_context(|| {
            format!(
                "access flow route {:?} endpoint is unavailable",
                route.name().as_str()
            )
        })?;
        let file_type = metadata.file_type();
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
    transport_reload_descriptors: u64,
    transport_reload_memory_bytes: u64,
) -> anyhow::Result<AccessFlowRelayResourceBudget> {
    relay_resource_budget_with_nofile(
        bridge_enabled,
        service_count,
        transport_reload_descriptors,
        transport_reload_memory_bytes,
        soft_nofile_limit()?,
    )
}

fn relay_transport_trust_budget(
    agent_budget: AccessFlowRelayResourceBudget,
    relay_projection: access_flow_relay::AccessFlowRelayResourceProjection,
) -> anyhow::Result<RelayTransportTrustBudget> {
    Ok(RelayTransportTrustBudget {
        descriptors: agent_budget
            .descriptors
            .checked_sub(relay_projection.total_descriptors)
            .context("Access Flow TLS trust descriptor residual is exhausted")?,
        memory_bytes: agent_budget
            .memory_bytes
            .checked_sub(relay_projection.total_memory_bytes)
            .context("Access Flow TLS trust memory residual is exhausted")?,
    })
}

fn relay_resource_budget_with_nofile(
    bridge_enabled: bool,
    service_count: usize,
    transport_reload_descriptors: u64,
    transport_reload_memory_bytes: u64,
    soft_nofile: u64,
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
    let descriptor_reserve = descriptor_reserve
        .checked_add(transport_reload_descriptors)
        .ok_or_else(|| anyhow::anyhow!("transport reload descriptor reserve overflow"))?;
    let descriptors = soft_nofile
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
        .and_then(|value| value.checked_add(transport_reload_memory_bytes))
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
        AccessFlowRelayPresentation, AccessFlowRelayRoute, AccessFlowRelayTransport, LoggingConfig,
        RestartPolicy, ServiceConfig,
    };
    #[cfg(target_os = "linux")]
    use access_flow::{
        ACCESS_FLOW_V2_HEADER_LEN, AccessFlowAcceptor, AccessFlowAdmission,
        AccessFlowAdmissionInput, AccessFlowPreface, AccessFlowPresentationMode,
    };
    #[cfg(target_os = "linux")]
    use access_flow_conformance::load_tls_pki_fixture;
    #[cfg(target_os = "linux")]
    use access_flow_relay::AccessFlowConnector;
    use access_flow_relay::AccessFlowRelayFailureKind;
    #[cfg(target_os = "linux")]
    use access_flow_tls::{
        EstablishedTlsAccessFlowChannel, TlsAccessFlowCertificateChain, TlsAccessFlowGeneration,
        TlsAccessFlowHandshakeTimeout, TlsAccessFlowPrivateKey, TlsAccessFlowServerAdapter,
        TlsAccessFlowServerChannel, TlsAccessFlowServerIdentity, TlsAccessFlowServerLimits,
        TlsAccessFlowTcpListener,
    };
    use access_tls_trust::TlsClientTrustMode;
    use std::collections::BTreeMap;
    #[cfg(target_os = "linux")]
    use std::convert::Infallible;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TEST_ACCESS_FLOW_BEARER: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEF";

    #[cfg(unix)]
    const TEST_ACCESS_FLOW_ROOT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBeDCCASqgAwIBAgIUXLfYBhGLaC2YWMIvB0aPnyCXmZMwBQYDK2VwMCgxJjAk
BgNVBAMMHUFXIEFjY2VzcyBGbG93IFJUMDEgVGVzdCBSb290MB4XDTI2MDcyNjE5
NDMwN1oXDTM2MDcyMzE5NDMwN1owKDEmMCQGA1UEAwwdQVcgQWNjZXNzIEZsb3cg
UlQwMSBUZXN0IFJvb3QwKjAFBgMrZXADIQBpAdFVn/HrfItwIx/XktXtNOZRrLFE
bRD4FW2ahSmyWaNmMGQwHwYDVR0jBBgwFoAUEXrimwcSAhT4Ae6XbVXVkbSfUUgw
EgYDVR0TAQH/BAgwBgEB/wIBADAOBgNVHQ8BAf8EBAMCAQYwHQYDVR0OBBYEFBF6
4psHEgIU+AHul21V1ZG0n1FIMAUGAytlcANBAFpX6ZvogOz9Sd4QpaxfhacxJKGu
O6IBKa79z07RBsJ3vyWrw6+ytc5B2vUiZTDhocxsDzNCyZPnHB1Iq7iIFwQ=
-----END CERTIFICATE-----
"#;

    #[cfg(target_os = "linux")]
    const RT06_OLD_ROOT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBZTCCARegAwIBAgIUHnYz2yzFmxU84z4ABlhv4/F5LnUwBQYDK2VwMCAxHjAc
BgNVBAMMFUFXIFJUMDYgb2xkIFRlc3QgUm9vdDAeFw0yNjA3MjcxOTMzMzlaFw0z
NjA3MjQxOTMzMzlaMCAxHjAcBgNVBAMMFUFXIFJUMDYgb2xkIFRlc3QgUm9vdDAq
MAUGAytlcAMhAPUsEAtTONdXaX3PsLg5m0op+gAwISmupvtb/GVBAgElo2MwYTAd
BgNVHQ4EFgQUsWvtfXQjVjyfbGvzXd0JCgSnuekwHwYDVR0jBBgwFoAUsWvtfXQj
VjyfbGvzXd0JCgSnuekwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYw
BQYDK2VwA0EA2WCGFxW+nw1huar98jP1LU/rlUiMXc0Y+sdVNky1XogaVauI27bw
aMoOFB2KM9Y8mqmBE5+pvbaZqs1K+3q0CQ==
-----END CERTIFICATE-----
"#;

    #[cfg(target_os = "linux")]
    const RT06_OLD_LEAF_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBkTCCAUOgAwIBAgIUS43yMrf0ZlNMIl39s8iWQ4t7NoowBQYDK2VwMCAxHjAc
BgNVBAMMFUFXIFJUMDYgb2xkIFRlc3QgUm9vdDAeFw0yNjA3MjcxOTMzMzlaFw0z
NjA3MjQxOTMzMzlaMBsxGTAXBgNVBAMMEGFjY2Vzcy1mbG93LnRlc3QwKjAFBgMr
ZXADIQDrRiLdZ9qPU78tWMi3+gijyuM48SdzKkY2Wr8uVRw6KaOBkzCBkDAMBgNV
HRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAb
BgNVHREEFDASghBhY2Nlc3MtZmxvdy50ZXN0MB0GA1UdDgQWBBSYAao1VncdIvxC
DhAc7z2EiWNDuTAfBgNVHSMEGDAWgBSxa+19dCNWPJ9sa/Nd3QkKBKe56TAFBgMr
ZXADQQD2FEj18diKXaEVYUfinchEwZa8t3sjuIp6ui+mmftXkT0443S9eb1Ct9CT
NoTp/2CPPfeduH0D2vBiXXJ/i38D
-----END CERTIFICATE-----
"#;

    #[cfg(target_os = "linux")]
    const RT06_OLD_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIOhH2hCkQS3bz8y1WrZ+LGUyiKQyohqR9ClcHFJcfjti
-----END PRIVATE KEY-----
"#;

    #[cfg(target_os = "linux")]
    const RT06_NEW_ROOT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBZTCCARegAwIBAgIUTrszQRtyCdvLzg5vMnXv6xCQpOkwBQYDK2VwMCAxHjAc
BgNVBAMMFUFXIFJUMDYgbmV3IFRlc3QgUm9vdDAeFw0yNjA3MjcxOTMzMzlaFw0z
NjA3MjQxOTMzMzlaMCAxHjAcBgNVBAMMFUFXIFJUMDYgbmV3IFRlc3QgUm9vdDAq
MAUGAytlcAMhAK3re6bsPSfxWXKa1in9ar92OMQAVEyD7hpwdzpzZexOo2MwYTAd
BgNVHQ4EFgQUOMJ7vKAJgWNf954L9eDdQe1EuBUwHwYDVR0jBBgwFoAUOMJ7vKAJ
gWNf954L9eDdQe1EuBUwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYw
BQYDK2VwA0EAfjK+q0Y1GdjI5gha7N1LAOn9H9WS1HNpKixzOdP1DyLdR6xcCcz5
gKqPfMOfkoKTGyuJBQnR1TnGE/59yzx7BA==
-----END CERTIFICATE-----
"#;

    #[cfg(target_os = "linux")]
    const RT06_NEW_LEAF_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBkTCCAUOgAwIBAgIUCaf3ckcsqB+oIkcSoL3v/SoDHmYwBQYDK2VwMCAxHjAc
BgNVBAMMFUFXIFJUMDYgbmV3IFRlc3QgUm9vdDAeFw0yNjA3MjcxOTMzMzlaFw0z
NjA3MjQxOTMzMzlaMBsxGTAXBgNVBAMMEGFjY2Vzcy1mbG93LnRlc3QwKjAFBgMr
ZXADIQB9fZytzxpxniLELx0FBzP6cPdVbZWuUfluRcJOw8OH0aOBkzCBkDAMBgNV
HRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAb
BgNVHREEFDASghBhY2Nlc3MtZmxvdy50ZXN0MB0GA1UdDgQWBBTmGWSfT6geN23E
oMjeUZWoj0ATjzAfBgNVHSMEGDAWgBQ4wnu8oAmBY1/3ngv14N1B7US4FTAFBgMr
ZXADQQDo1KDk1uS6l9jqYad26iNByl5JSlergecmSPOtYp/sUEu3dMWF846gY4om
iJPat6lRzLetfgUpaIOP5Tw8kmoF
-----END CERTIFICATE-----
"#;

    #[cfg(target_os = "linux")]
    const RT06_NEW_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIGRXBokZ2/yO2kASVZKtUVGnOwIM7kZJKJgugoeMCRxd
-----END PRIVATE KEY-----
"#;

    #[cfg(target_os = "linux")]
    struct ExpectProductTlsPreface {
        destination: access_flow::AccessFlowDestination,
        execution_context: Option<&'static str>,
    }

    #[cfg(target_os = "linux")]
    impl AccessFlowAdmission<TlsAccessFlowServerChannel> for ExpectProductTlsPreface {
        type Facts = ();
        type Error = Infallible;

        fn admit<'a>(
            &'a self,
            input: AccessFlowAdmissionInput<'a, TlsAccessFlowServerChannel>,
        ) -> BoxAccessFuture<'a, Result<Self::Facts, Self::Error>> {
            assert_eq!(input.destination, self.destination);
            assert_eq!(
                input
                    .execution_context
                    .map(access_execution_context::ExecutionContext::as_str),
                self.execution_context
            );
            let IdentityPresentation::Bearer(bearer) = input.presentation else {
                panic!("product TLS route did not send the required bearer");
            };
            bearer.expose(|actual| assert_eq!(actual, TEST_ACCESS_FLOW_BEARER));
            assert!(input.channel_facts.mark_admitted());
            Box::pin(async { Ok(()) })
        }
    }

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

    #[cfg(target_os = "linux")]
    fn rt06_server_adapter(
        certificate_pem: &str,
        private_key_pem: &str,
        generation: u64,
    ) -> TlsAccessFlowServerAdapter {
        let (certificate_label, certificate) =
            pem_rfc7468::decode_vec(certificate_pem.as_bytes()).unwrap();
        assert_eq!(certificate_label, "CERTIFICATE");
        let (key_label, private_key) = pem_rfc7468::decode_vec(private_key_pem.as_bytes()).unwrap();
        assert_eq!(key_label, "PRIVATE KEY");
        let identity = TlsAccessFlowServerIdentity::new(
            TlsAccessFlowGeneration::new(generation).unwrap(),
            TlsAccessFlowCertificateChain::new(vec![certificate]).unwrap(),
            TlsAccessFlowPrivateKey::new(private_key).unwrap(),
        );
        TlsAccessFlowServerAdapter::new(
            identity,
            TlsAccessFlowHandshakeTimeout::new(Duration::from_secs(2)).unwrap(),
            TlsAccessFlowServerLimits::new(8, None).unwrap(),
        )
        .unwrap()
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

    fn dependency_service(name: &str) -> Arc<ManagedService> {
        Arc::new(ManagedService::new(
            ServiceConfig {
                name: name.into(),
                required: true,
                user: "root".into(),
                command: vec!["sleep".into(), "infinity".into()],
                cwd: None,
                restart: RestartPolicy::Never,
                restart_backoff: None,
                restart_backoff_max: None,
                startup_timeout: None,
                shutdown_timeout: None,
                depends_on: Vec::new(),
                env: BTreeMap::new(),
                health_check: None,
            },
            PathBuf::from("/tmp"),
            LoggingConfig::default(),
        ))
    }

    #[test]
    fn resource_budget_subtracts_frozen_non_relay_memory() {
        let no_bridge = relay_resource_budget(false, 4, 0, 0).unwrap();
        let bridge = relay_resource_budget(true, 4, 0, 0).unwrap();
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
    fn resource_budget_reserves_transport_reload_descriptors() {
        let baseline = relay_resource_budget(false, 0, 0, 0).unwrap();
        let with_tls_reload = relay_resource_budget(false, 0, 2, 1024).unwrap();
        assert_eq!(with_tls_reload.descriptors, baseline.descriptors - 2);
        assert_eq!(with_tls_reload.memory_bytes, baseline.memory_bytes - 1024);
        assert!(relay_resource_budget(false, 0, u64::MAX, 0).is_err());
        assert!(relay_resource_budget(false, 0, 0, u64::MAX).is_err());
    }

    #[test]
    fn system_trust_exact_peak_fits_both_product_residuals_and_oversubscription_rejects() {
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 64,
            copy_buffer_bytes_per_direction: 16 * 1024,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: "127.0.0.1:3129".into(),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: "127.0.0.1:7443".into(),
                    server_name: "access-flow.test".into(),
                    trust: TlsClientTrustMode::System,
                    ca_certificate: None,
                },
            }],
        };
        let compile = || {
            config
                .compile_with_presentation(
                    crate::config::AccessFlowRelayValidationMode::Agent,
                    IdentityPresentation::Bearer(
                        access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
                    ),
                    None,
                )
                .unwrap()
        };
        let compiled = compile();
        let reserve = RelayTransportRuntime::resource_reserve(&compiled.plan).unwrap();
        assert_eq!(reserve.memory_bytes, 58_855_424);

        for bridge_enabled in [false, true] {
            let compiled = compile();
            let agent_budget =
                relay_resource_budget_with_nofile(bridge_enabled, 0, 0, 0, 4096).unwrap();
            let relay_budget = relay_resource_budget_with_nofile(
                bridge_enabled,
                0,
                reserve.descriptors,
                reserve.memory_bytes,
                4096,
            )
            .unwrap();
            let transport =
                RelayTransportRuntime::prepare(&compiled.plan, Arc::new(AtomicBool::new(false)))
                    .unwrap();
            let relay = AccessFlowRelay::new(
                compiled.plan,
                transport.connector(),
                Arc::new(RelayObserver {
                    active_flows: Arc::new(AtomicUsize::new(0)),
                }),
                relay_budget,
            )
            .unwrap();
            let residual =
                relay_transport_trust_budget(agent_budget, relay.resource_projection()).unwrap();
            assert!(residual.memory_bytes >= 58_855_424);
            transport.configure_trust_budget(residual).unwrap();

            let rejected = compile();
            let oversubscribed =
                RelayTransportRuntime::prepare(&rejected.plan, Arc::new(AtomicBool::new(false)))
                    .unwrap();
            assert!(
                oversubscribed
                    .configure_trust_budget(RelayTransportTrustBudget {
                        descriptors: residual.descriptors,
                        memory_bytes: reserve.memory_bytes - 1,
                    })
                    .is_err()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn blocked_trust_operation_preserves_readiness_excludes_reload_and_recovers() {
        let source = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: listen.to_string(),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: "127.0.0.1:7443".into(),
                    server_name: "access-flow.test".into(),
                    trust: TlsClientTrustMode::Insecure,
                    ca_certificate: None,
                },
            }],
        };
        let (control, commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp/aw-gateway-blocked-trust-test"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Bearer(
                access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
            ),
            None,
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

        let (release, wait_for_release) = oneshot::channel::<()>();
        control
            .transport
            .get()
            .unwrap()
            .install_test_blocked_operation(tokio::spawn(async move {
                let _ = wait_for_release.await;
            }));
        assert!(control.is_ready());
        let blocked = control.status().await;
        assert!(blocked.ready);
        assert_eq!(
            blocked.trust_failure.as_deref(),
            Some("trust_reload_blocked")
        );
        assert!(control.initiate_security_reload().is_err());
        assert!(control.is_ready());

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control
                .transport
                .get()
                .is_some_and(RelayTransportRuntime::reload_blocked)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        control.initiate_security_reload().unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.reload_in_progress.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(control.is_ready());
        assert_eq!(control.status().await.trust_failure, None);

        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        supervisor.await.unwrap().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_deadline_detaches_and_accounts_noninterruptible_reload() {
        let source = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "50ms".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: listen.to_string(),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: "127.0.0.1:7443".into(),
                    server_name: "access-flow.test".into(),
                    trust: TlsClientTrustMode::Insecure,
                    ca_certificate: None,
                },
            }],
        };
        let (control, commands) = RelayControl::configured(&config);
        let (reload_reached, resume_reload) = control.pause_blocking_reload();
        let state = Arc::new(AgentState::new(
            PathBuf::from("/tmp/aw-gateway-reload-deadline-test"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Bearer(
                access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
            ),
            None,
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
        control.initiate_security_reload().unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || reload_reached.recv()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        let deadline = Instant::now() + Duration::from_millis(50);
        tokio::time::timeout(
            Duration::from_millis(250),
            control.close_admission_by(deadline),
        )
        .await
        .expect("admission close exceeded the requested deadline");
        let transport = control.transport.get().unwrap();
        assert!(transport.reload_blocked());
        assert_eq!(
            control.status().await.trust_failure.as_deref(),
            Some("trust_reload_blocked")
        );
        control.shutdown(deadline).await;
        tokio::time::timeout(Duration::from_millis(250), supervisor)
            .await
            .expect("relay shutdown exceeded the original requested deadline")
            .unwrap()
            .unwrap();

        resume_reload.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while transport.reload_blocked() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shipped_tls_agent_plan_fits_supported_product_topologies() {
        let mut authored: crate::config::ContainerAgentFile = toml::from_str(include_str!(
            "../../examples/docker/container-agent-access-flow-tls.toml"
        ))
        .unwrap();
        authored.validate().unwrap();
        let relay_config = authored
            .container_agent
            .access_flow_relay
            .as_mut()
            .expect("shipped TLS relay");
        assert_eq!(relay_config.routes.len(), 2);
        assert_eq!(relay_config.max_connections, 64);
        assert_eq!(relay_config.copy_buffer_bytes_per_direction, 16 * 1024);

        let dir = tempfile::Builder::new()
            .prefix(".relay-product-preflight-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("roots.pem");
        std::fs::write(&trust_path, TEST_ACCESS_FLOW_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for route in &mut relay_config.routes {
            let AccessFlowRelayTransport::TlsTcp { ca_certificate, .. } = &mut route.transport
            else {
                panic!("shipped TLS relay contains a non-TLS route");
            };
            *ca_certificate = Some(trust_path.display().to_string());
        }
        let relay_config = relay_config.clone();
        let bearer = vec![b'B'; access_identity::MAX_BEARER_LEN];

        for (bridge_enabled, service_count) in [(false, 0), (true, 0), (true, 2)] {
            let compiled = relay_config
                .compile_with_presentation(
                    crate::config::AccessFlowRelayValidationMode::Agent,
                    IdentityPresentation::Bearer(
                        access_identity::SensitiveBearer::new(&bearer).unwrap(),
                    ),
                    None,
                )
                .unwrap();
            let transport_reserve =
                RelayTransportRuntime::resource_reserve(&compiled.plan).unwrap();
            let agent_budget =
                relay_resource_budget_with_nofile(bridge_enabled, service_count, 0, 0, 4096)
                    .unwrap();
            let budget = relay_resource_budget_with_nofile(
                bridge_enabled,
                service_count,
                transport_reserve.descriptors,
                transport_reserve.memory_bytes,
                4096,
            )
            .unwrap();
            let transport =
                RelayTransportRuntime::prepare(&compiled.plan, Arc::new(AtomicBool::new(false)))
                    .unwrap();
            let relay = AccessFlowRelay::new(
                compiled.plan,
                transport.connector(),
                Arc::new(RelayObserver {
                    active_flows: Arc::new(AtomicUsize::new(0)),
                }),
                budget,
            )
            .unwrap();
            let trust_budget =
                relay_transport_trust_budget(agent_budget, relay.resource_projection()).unwrap();
            let non_relay_memory = if bridge_enabled {
                BRIDGE_NON_RELAY_MEMORY_BYTES
            } else {
                NO_BRIDGE_NON_RELAY_MEMORY_BYTES
            };
            assert_eq!(
                trust_budget.memory_bytes,
                TOTAL_AGENT_MEMORY_PREFLIGHT_BYTES
                    - non_relay_memory
                    - PER_SERVICE_NON_RELAY_MEMORY_BYTES * service_count as u64
                    - relay.resource_projection().total_memory_bytes
            );
            assert!(trust_budget.memory_bytes >= transport_reserve.memory_bytes);
            transport.configure_trust_budget(trust_budget).unwrap();
            transport.activate_prepared().await.unwrap();
            let projection = relay.resource_projection();
            assert!(projection.total_descriptors <= budget.descriptors);
            assert!(projection.total_memory_bytes <= budget.memory_bytes);
            if bridge_enabled && service_count == 2 {
                assert!(
                    budget.memory_bytes - projection.total_memory_bytes >= 8 * 1024 * 1024,
                    "shipped bridge topology has less than 8 MiB relay preflight margin"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rt06_tls_agent_resource_boundary_uses_exclusive_connection_phases() {
        let mut authored: crate::config::ContainerAgentFile = toml::from_str(include_str!(
            "../../examples/docker/container-agent-access-flow-tls.toml"
        ))
        .unwrap();
        authored.validate().unwrap();
        let relay_config = authored
            .container_agent
            .access_flow_relay
            .as_mut()
            .expect("shipped TLS relay");
        assert_eq!(relay_config.routes.len(), 2);
        assert_eq!(relay_config.copy_buffer_bytes_per_direction, 16 * 1024);
        relay_config.max_connections = 128;
        let projected_flows = u64::try_from(relay_config.max_connections).unwrap();

        let dir = tempfile::Builder::new()
            .prefix(".relay-rt06-boundary-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("roots.pem");
        std::fs::write(&trust_path, TEST_ACCESS_FLOW_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for route in &mut relay_config.routes {
            let AccessFlowRelayTransport::TlsTcp { ca_certificate, .. } = &mut route.transport
            else {
                panic!("RT06 boundary relay contains a non-TLS route");
            };
            *ca_certificate = Some(trust_path.display().to_string());
        }

        let bearer = vec![b'B'; access_identity::MAX_BEARER_LEN];
        let compiled = relay_config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(&bearer).unwrap(),
                ),
                None,
            )
            .unwrap();
        let transport_reserve = RelayTransportRuntime::resource_reserve(&compiled.plan).unwrap();
        let agent_budget = relay_resource_budget_with_nofile(false, 1, 0, 0, 4096).unwrap();
        let budget = relay_resource_budget_with_nofile(
            false,
            1,
            transport_reserve.descriptors,
            transport_reserve.memory_bytes,
            4096,
        )
        .unwrap();
        let transport =
            RelayTransportRuntime::prepare(&compiled.plan, Arc::new(AtomicBool::new(false)))
                .unwrap();
        let relay = AccessFlowRelay::new(
            compiled.plan,
            transport.connector(),
            Arc::new(RelayObserver {
                active_flows: Arc::new(AtomicUsize::new(0)),
            }),
            budget,
        )
        .expect("phase-aware RT06 resource projection must fit the product budget");
        transport
            .configure_trust_budget(
                relay_transport_trust_budget(agent_budget, relay.resource_projection()).unwrap(),
            )
            .unwrap();
        let projection = relay.resource_projection();
        assert!(projection.total_descriptors <= budget.descriptors);
        assert!(projection.total_memory_bytes <= budget.memory_bytes);
        for active_component in [
            projection.flow_task_session_bytes,
            projection.copy_buffer_bytes,
            projection.connector_active_bytes,
        ] {
            assert_eq!(
                active_component % projected_flows,
                0,
                "the Gateway RT06 active-flow component must divide exactly by the flow ceiling"
            );
        }
        let minimum_active_bytes_per_flow = projection
            .flow_task_session_bytes
            .checked_add(projection.copy_buffer_bytes)
            .and_then(|value| value.checked_add(projection.connector_active_bytes))
            .and_then(|value| value.checked_div(projected_flows))
            .expect("the Gateway RT06 active-flow projection must fit u64");
        assert!(
            minimum_active_bytes_per_flow
                .checked_mul(projected_flows)
                .is_some_and(|value| value <= projection.total_memory_bytes),
            "the Gateway RT06 active-flow floor must fit its total relay projection"
        );
        if let Some(output) = std::env::var_os("AW_GATEWAY_RT06_PROJECTION_OUTPUT") {
            use std::fs::OpenOptions;
            use std::io::Write;

            let output = PathBuf::from(output);
            assert!(
                output.is_absolute() && output.file_name().is_some(),
                "AW_GATEWAY_RT06_PROJECTION_OUTPUT must name a new absolute output file"
            );
            let projected_memory_bytes = projection
                .total_memory_bytes
                .checked_add(transport_reserve.memory_bytes)
                .expect("the Gateway RT06 memory projection must fit u64");
            let projected_descriptors = projection
                .total_descriptors
                .checked_add(transport_reserve.descriptors)
                .expect("the Gateway RT06 descriptor projection must fit u64");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)
                .expect("the Gateway RT06 projection output must not already exist");
            writeln!(
                file,
                "name\tvalue\n\
                 gateway_projected_bytes\t{projected_memory_bytes}\n\
                 gateway_projected_descriptors\t{projected_descriptors}\n\
                 gateway_minimum_active_bytes_per_flow\t{minimum_active_bytes_per_flow}"
            )
            .expect("the Gateway RT06 projection output must be writable");
            file.sync_all()
                .expect("the Gateway RT06 projection output must be durable");
        }

        let obsolete_summed_phase_bytes = projection
            .total_memory_bytes
            .checked_add(projection.setup_bytes.min(projection.copy_buffer_bytes))
            .unwrap();
        assert!(
            obsolete_summed_phase_bytes > projection.total_memory_bytes,
            "the RT06 projection must not sum mutually exclusive connection phases"
        );
    }

    #[tokio::test]
    async fn relay_resource_projection_accounts_for_retained_bearer() {
        let config = test_config("127.0.0.1:3128".into(), "/tmp/access-flow.sock".into());
        let compiled = config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(b"abcdefghijklmnopqrstuvwxyzABCDEF")
                        .unwrap(),
                ),
                None,
            )
            .unwrap();
        let transport =
            RelayTransportRuntime::activate(&compiled.plan, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap();
        let relay = AccessFlowRelay::new(
            compiled.plan,
            transport.connector(),
            Arc::new(RelayObserver {
                active_flows: Arc::new(AtomicUsize::new(0)),
            }),
            relay_resource_budget(false, 0, 0, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(relay.resource_projection().presentation_bytes, 32);
        assert!(
            relay.resource_projection().total_memory_bytes
                >= relay.resource_projection().presentation_bytes
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn product_tls_route_sends_awaf_preface_and_bidirectional_application_bytes() {
        let dir = tempfile::Builder::new()
            .prefix(".relay-product-tls-test-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("access-flow-root.pem");
        std::fs::write(&trust_path, TEST_ACCESS_FLOW_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let tls_tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let tls_address = tls_tcp.local_addr().unwrap();
        let mut tls_listener = TlsAccessFlowTcpListener::from_std(tls_tcp).unwrap();
        let fixture = load_tls_pki_fixture().unwrap();
        let server_name = fixture.server_name().host().dns_name().unwrap().to_string();
        let (_, _, server_identity) = fixture.into_parts();
        let tls_server = TlsAccessFlowServerAdapter::new(
            server_identity,
            TlsAccessFlowHandshakeTimeout::new(Duration::from_secs(2)).unwrap(),
            TlsAccessFlowServerLimits::new(8, None).unwrap(),
        )
        .unwrap();

        let local = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let listen = local.local_addr().unwrap();
        drop(local);
        let destination =
            access_flow::AccessFlowDestination::new(std::net::Ipv4Addr::LOCALHOST, listen.port())
                .unwrap();
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: listen.to_string(),
                allowed_destination_ports: vec![listen.port()],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: tls_address.to_string(),
                    server_name,
                    trust: TlsClientTrustMode::Custom,
                    ca_certificate: Some(trust_path.display().to_string()),
                },
            }],
        };
        let presentation = IdentityPresentation::Bearer(
            access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
        );
        let (control, commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            dir.path().join("state"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));

        let server = tokio::spawn(async move {
            let cancellation = RelayCancellation::default();
            let channel = tls_server
                .accept(&mut tls_listener, &cancellation)
                .await
                .unwrap()
                .establish(&cancellation)
                .await
                .unwrap();
            let acceptor =
                AccessFlowAcceptor::new(AccessFlowPresentationMode::Required, [destination.port()])
                    .unwrap();
            let accepted = acceptor
                .accept(
                    channel,
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                    &ExpectProductTlsPreface {
                        destination,
                        execution_context: Some("internal"),
                    },
                )
                .await
                .unwrap();
            let mut stream = accepted.into_parts().io;
            let mut request = [0_u8; 16];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"client-to-server");
            stream.write_all(b"server-to-client").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let mut supervisor = tokio::spawn(run_relay_supervisor(
            config,
            presentation,
            Some("internal".into()),
            Vec::new(),
            state,
            control.clone(),
            commands,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut supervisor => {
                    panic!("relay supervisor failed before readiness: {result:?}");
                }
                () = async {
                    while !control.is_ready() {
                        sleep(Duration::from_millis(10)).await;
                    }
                } => {}
            }
        })
        .await
        .unwrap();

        let mut client = tokio::net::TcpStream::connect(listen).await.unwrap();
        client.write_all(b"client-to-server").await.unwrap();
        let mut response = [0_u8; 16];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"server-to-client");
        client.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();

        control.close_admission().await;
        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Stopped
        );
        assert_eq!(control.active_flows(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn established_tls_streams_survive_multiple_successful_trust_reloads() {
        let dir = tempfile::Builder::new()
            .prefix(".relay-generation-overlap-test-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("access-flow-root.pem");
        std::fs::write(&trust_path, TEST_ACCESS_FLOW_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let tls_tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let tls_address = tls_tcp.local_addr().unwrap();
        let mut tls_listener = TlsAccessFlowTcpListener::from_std(tls_tcp).unwrap();
        let fixture = load_tls_pki_fixture().unwrap();
        let server_name = fixture.server_name().host().dns_name().unwrap().to_string();
        let (_, _, server_identity) = fixture.into_parts();
        let tls_server = TlsAccessFlowServerAdapter::new(
            server_identity,
            TlsAccessFlowHandshakeTimeout::new(Duration::from_secs(2)).unwrap(),
            TlsAccessFlowServerLimits::new(8, None).unwrap(),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let cancellation = RelayCancellation::default();
            let mut retained = Vec::new();
            for _ in 0..3 {
                retained.push(
                    tls_server
                        .accept(&mut tls_listener, &cancellation)
                        .await
                        .unwrap()
                        .establish(&cancellation)
                        .await
                        .unwrap(),
                );
            }
            retained
        });

        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: "127.0.0.1:3129".into(),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: tls_address.to_string(),
                    server_name,
                    trust: TlsClientTrustMode::Custom,
                    ca_certificate: Some(trust_path.display().to_string()),
                },
            }],
        };
        let compiled = config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
                ),
                None,
            )
            .unwrap();
        let runtime =
            RelayTransportRuntime::activate(&compiled.plan, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap();
        let connector = runtime.connector();
        let cancellation = RelayCancellation::default();
        let mut streams = Vec::new();
        for generation in 0..3 {
            let context = access_flow_relay::AccessFlowConnectContext::new(
                Instant::now() + Duration::from_secs(2),
                &cancellation,
            );
            streams.push(
                connector
                    .connect(compiled.plan.routes()[0].endpoint(), context)
                    .await
                    .unwrap(),
            );
            if generation < 2 {
                runtime
                    .begin_reload()
                    .unwrap()
                    .expect("TLS reload worker")
                    .complete()
                    .await
                    .unwrap();
            }
        }
        let retained_server_channels = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained_server_channels.len(), 3);
        for stream in &mut streams {
            stream.write_all(b"x").await.unwrap();
            stream.flush().await.unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn tls_trust_overlap_and_cutover_select_current_roots_and_preserve_old_stream() {
        #[derive(Clone, Copy)]
        enum ServerGeneration {
            Old,
            New,
        }

        let dir = tempfile::Builder::new()
            .prefix(".relay-real-trust-overlap-test-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("access-flow-roots.pem");
        std::fs::write(&trust_path, RT06_OLD_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let tls_tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let tls_address = tls_tcp.local_addr().unwrap();
        let mut tls_listener = TlsAccessFlowTcpListener::from_std(tls_tcp).unwrap();
        let old_server = rt06_server_adapter(RT06_OLD_LEAF_PEM, RT06_OLD_KEY_PEM, 1);
        let new_server = rt06_server_adapter(RT06_NEW_LEAF_PEM, RT06_NEW_KEY_PEM, 2);
        let (server_tx, mut server_rx) = mpsc::channel::<(
            ServerGeneration,
            oneshot::Sender<Option<EstablishedTlsAccessFlowChannel>>,
        )>(1);
        let server = tokio::spawn(async move {
            while let Some((generation, completed)) = server_rx.recv().await {
                let cancellation = RelayCancellation::default();
                let accepted = match generation {
                    ServerGeneration::Old => {
                        old_server.accept(&mut tls_listener, &cancellation).await
                    }
                    ServerGeneration::New => {
                        new_server.accept(&mut tls_listener, &cancellation).await
                    }
                };
                let channel = match accepted {
                    Ok(accepted) => accepted.establish(&cancellation).await.ok(),
                    Err(_) => None,
                };
                let _ = completed.send(channel);
            }
        });

        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 8,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: "127.0.0.1:3129".into(),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: tls_address.to_string(),
                    server_name: "access-flow.test".into(),
                    trust: TlsClientTrustMode::Custom,
                    ca_certificate: Some(trust_path.display().to_string()),
                },
            }],
        };
        let compiled = config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
                ),
                None,
            )
            .unwrap();
        let runtime =
            RelayTransportRuntime::activate(&compiled.plan, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap();
        let connector = runtime.connector();
        let endpoint = compiled.plan.routes()[0].endpoint();

        macro_rules! connect_with {
            ($generation:expr) => {{
                let (completed, wait) = oneshot::channel();
                server_tx.send(($generation, completed)).await.unwrap();
                let cancellation = RelayCancellation::default();
                let context = access_flow_relay::AccessFlowConnectContext::new(
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                );
                let client = connector.connect(endpoint, context).await;
                let server = tokio::time::timeout(Duration::from_secs(2), wait)
                    .await
                    .unwrap()
                    .unwrap();
                (client, server)
            }};
        }

        let (old_client, old_channel) = connect_with!(ServerGeneration::Old);
        let mut old_client = old_client.expect("old-only generation accepts old server");
        let old_channel = old_channel.expect("old server completed old-only handshake");

        std::fs::write(
            &trust_path,
            format!("{RT06_OLD_ROOT_PEM}{RT06_NEW_ROOT_PEM}"),
        )
        .unwrap();
        runtime
            .begin_reload()
            .unwrap()
            .expect("overlap reload")
            .complete()
            .await
            .unwrap();
        let (new_overlap_client, new_overlap_server) = connect_with!(ServerGeneration::New);
        assert!(new_overlap_client.is_ok());
        assert!(new_overlap_server.is_some());
        drop(new_overlap_client);
        drop(new_overlap_server);
        let (old_overlap_client, old_overlap_server) = connect_with!(ServerGeneration::Old);
        assert!(old_overlap_client.is_ok());
        assert!(old_overlap_server.is_some());
        drop(old_overlap_client);
        drop(old_overlap_server);

        std::fs::write(&trust_path, RT06_NEW_ROOT_PEM).unwrap();
        runtime
            .begin_reload()
            .unwrap()
            .expect("new-only reload")
            .complete()
            .await
            .unwrap();
        let (new_client, new_channel) = connect_with!(ServerGeneration::New);
        assert!(new_client.is_ok());
        assert!(new_channel.is_some());
        drop(new_client);
        drop(new_channel);
        let (removed_old_client, removed_old_channel) = connect_with!(ServerGeneration::Old);
        assert!(
            removed_old_client.is_err(),
            "new connection accepted a server signed only by the removed old root"
        );
        assert!(
            removed_old_channel.is_none(),
            "removed old server unexpectedly completed its TLS handshake"
        );

        let destination =
            access_flow::AccessFlowDestination::new(std::net::Ipv4Addr::LOCALHOST, 443).unwrap();
        AccessFlowPreface::new(
            destination,
            IdentityPresentation::Bearer(
                access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
            ),
            None,
        )
        .write_to(&mut old_client)
        .await
        .unwrap();
        let admission_cancellation = RelayCancellation::default();
        let accepted = AccessFlowAcceptor::new(
            AccessFlowPresentationMode::Required,
            [std::num::NonZeroU16::new(443).unwrap()],
        )
        .unwrap()
        .accept(
            old_channel,
            Instant::now() + Duration::from_secs(2),
            &admission_cancellation,
            &ExpectProductTlsPreface {
                destination,
                execution_context: None,
            },
        )
        .await
        .unwrap();
        let mut old_channel = accepted.into_parts().io;
        old_client
            .write_all(b"old-generation-still-live")
            .await
            .unwrap();
        let mut retained = [0_u8; 25];
        old_channel.read_exact(&mut retained).await.unwrap();
        assert_eq!(&retained, b"old-generation-still-live");
        old_channel.write_all(b"drained").await.unwrap();
        let mut drained = [0_u8; 7];
        old_client.read_exact(&mut drained).await.unwrap();
        assert_eq!(&drained, b"drained");

        runtime.close();
        drop(server_tx);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn integrated_relay_saturation_rejects_n_plus_one_and_recovers_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("access-flow.sock");
        let endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        endpoint_listener.set_nonblocking(true).unwrap();
        let endpoint_listener = tokio::net::UnixListener::from_std(endpoint_listener).unwrap();
        let source = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let mut config = test_config(listen.to_string(), endpoint.display().to_string());
        config.max_connections = 1;
        let (control, commands) = RelayControl::configured(&config);
        let state = Arc::new(AgentState::new(
            dir.path().join("state"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Disabled,
            None,
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

        let mut first = tokio::net::TcpStream::connect(listen).await.unwrap();
        let (mut first_channel, _) = endpoint_listener.accept().await.unwrap();
        let mut first_preface = [0_u8; ACCESS_FLOW_V2_HEADER_LEN];
        first_channel.read_exact(&mut first_preface).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.active_flows() != 1 {
                assert!(control.active_flows() <= 1);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let mut saturated = tokio::net::TcpStream::connect(listen).await.unwrap();
        let mut eof = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(250), saturated.read(&mut eof))
                .await
                .expect("saturated connection was not rejected immediately")
                .unwrap(),
            0
        );
        assert_eq!(control.active_flows(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), endpoint_listener.accept())
                .await
                .is_err(),
            "saturated connection reached the transport connector"
        );

        first.shutdown().await.unwrap();
        drop(first);
        drop(first_channel);
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.active_flows() != 0 {
                assert!(control.active_flows() <= 1);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let recovered = tokio::net::TcpStream::connect(listen).await.unwrap();
        let (mut recovered_channel, _) = endpoint_listener.accept().await.unwrap();
        let mut recovered_preface = [0_u8; ACCESS_FLOW_V2_HEADER_LEN];
        recovered_channel
            .read_exact(&mut recovered_preface)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.active_flows() != 1 {
                assert!(control.active_flows() <= 1);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(recovered);
        drop(recovered_channel);

        control.close_admission().await;
        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(control.active_flows(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stable_trust_mutation_during_reload_gates_unix_and_tls_until_recovery() {
        let dir = tempfile::Builder::new()
            .prefix(".relay-mixed-mutation-test-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("access-flow-root.pem");
        std::fs::write(&trust_path, RT06_OLD_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let unix_path = dir.path().join("access-flow.sock");
        let unix_listener = std::os::unix::net::UnixListener::bind(&unix_path).unwrap();
        unix_listener.set_nonblocking(true).unwrap();
        let unix_listener = tokio::net::UnixListener::from_std(unix_listener).unwrap();
        let tls_tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let tls_address = tls_tcp.local_addr().unwrap();

        let http_source = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let http_listen = http_source.local_addr().unwrap();
        drop(http_source);
        let https_source = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let https_listen = https_source.local_addr().unwrap();
        drop(https_source);
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![
                AccessFlowRelayRoute {
                    name: "http".into(),
                    listen: http_listen.to_string(),
                    allowed_destination_ports: vec![http_listen.port()],
                    transport: AccessFlowRelayTransport::Unix {
                        path: unix_path.display().to_string(),
                    },
                },
                AccessFlowRelayRoute {
                    name: "https".into(),
                    listen: https_listen.to_string(),
                    allowed_destination_ports: vec![https_listen.port()],
                    transport: AccessFlowRelayTransport::TlsTcp {
                        address: tls_address.to_string(),
                        server_name: "access-flow.test".into(),
                        trust: TlsClientTrustMode::Custom,
                        ca_certificate: Some(trust_path.display().to_string()),
                    },
                },
            ],
        };
        let (control, commands) = RelayControl::configured(&config);
        let (reload_reached, resume_reload) = control.pause_blocking_reload();
        let state = Arc::new(AgentState::new(
            dir.path().join("state"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Bearer(
                access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
            ),
            None,
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

        control.initiate_security_reload().unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || reload_reached.recv()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        std::fs::write(&trust_path, b"stable invalid trust material").unwrap();
        assert!(
            control.is_ready(),
            "staged reload must preserve the current generation until failure is known"
        );

        resume_reload.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.reload_in_progress.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!control.is_ready());

        std::fs::write(&trust_path, RT06_OLD_ROOT_PEM).unwrap();
        control.initiate_security_reload().unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.reload_in_progress.load(Ordering::Acquire) || !control.is_ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let recovered_unix = tokio::net::TcpStream::connect(http_listen).await.unwrap();
        let (mut recovered_unix_channel, _) = unix_listener.accept().await.unwrap();
        let mut unix_preface = [0_u8; 48];
        recovered_unix_channel
            .read_exact(&mut unix_preface)
            .await
            .unwrap();
        drop(recovered_unix);
        drop(recovered_unix_channel);

        let mut tls_listener = TlsAccessFlowTcpListener::from_std(tls_tcp).unwrap();
        let tls_server = rt06_server_adapter(RT06_OLD_LEAF_PEM, RT06_OLD_KEY_PEM, 1);
        let tls_server_task = tokio::spawn(async move {
            let cancellation = RelayCancellation::default();
            let channel = tls_server
                .accept(&mut tls_listener, &cancellation)
                .await
                .unwrap()
                .establish(&cancellation)
                .await
                .unwrap();
            let destination = access_flow::AccessFlowDestination::new(
                std::net::Ipv4Addr::LOCALHOST,
                https_listen.port(),
            )
            .unwrap();
            let accepted = AccessFlowAcceptor::new(
                AccessFlowPresentationMode::Required,
                [std::num::NonZeroU16::new(https_listen.port()).unwrap()],
            )
            .unwrap()
            .accept(
                channel,
                Instant::now() + Duration::from_secs(2),
                &cancellation,
                &ExpectProductTlsPreface {
                    destination,
                    execution_context: None,
                },
            )
            .await
            .unwrap();
            let mut stream = accepted.into_parts().io;
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, [b'R']);
        });
        let mut recovered_tls = tokio::net::TcpStream::connect(https_listen).await.unwrap();
        recovered_tls.write_all(b"R").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), tls_server_task)
            .await
            .unwrap()
            .unwrap();
        drop(recovered_tls);

        control.close_admission().await;
        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ordered_shutdown_joins_an_in_progress_blocking_tls_reload() {
        let dir = tempfile::Builder::new()
            .prefix(".relay-reload-shutdown-test-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("access-flow-root.pem");
        std::fs::write(&trust_path, TEST_ACCESS_FLOW_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let remote = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let remote_address = remote.local_addr().unwrap();
        let local = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let listen = local.local_addr().unwrap();
        drop(local);
        let fixture = load_tls_pki_fixture().unwrap();
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "1s".into(),
            max_connections: 4,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_ACCESS_FLOW_TEST_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: listen.to_string(),
                allowed_destination_ports: vec![listen.port()],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: remote_address.to_string(),
                    server_name: fixture.server_name().host().dns_name().unwrap().to_string(),
                    trust: TlsClientTrustMode::Custom,
                    ca_certificate: Some(trust_path.display().to_string()),
                },
            }],
        };
        let presentation = IdentityPresentation::Bearer(
            access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
        );
        let (control, commands) = RelayControl::configured(&config);
        let (reload_reached, resume_reload) = control.pause_blocking_reload();
        let (command_reached, resume_command) = control.pause_before_reload_command().await;
        let close_before_join = control.observe_close_before_reload_join().await;
        let mut close_after_join = control.observe_close_after_reload_join().await;
        let state = Arc::new(AgentState::new(
            dir.path().join("state"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        let mut supervisor = tokio::spawn(run_relay_supervisor(
            config,
            presentation,
            None,
            Vec::new(),
            state,
            control.clone(),
            commands,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut supervisor => {
                    panic!("relay supervisor failed before readiness: {result:?}");
                }
                () = async {
                    while !control.is_ready() {
                        sleep(Duration::from_millis(10)).await;
                    }
                } => {}
            }
        })
        .await
        .unwrap();
        control.initiate_security_reload().unwrap();
        tokio::time::timeout(Duration::from_secs(1), command_reached)
            .await
            .unwrap()
            .unwrap();
        assert!(
            control.is_ready(),
            "staged trust reload must preserve current-generation readiness"
        );
        assert!(
            control.initiate_security_reload().is_err(),
            "concurrent reload was not coalesced"
        );

        let closing_control = control.clone();
        let close = tokio::spawn(async move {
            closing_control.close_admission().await;
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.phase.load(Ordering::Acquire) != RELAY_PHASE_CLOSING {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !close.is_finished(),
            "admission close passed a reload awaiting command consumption"
        );
        resume_command.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), close_before_join)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || reload_reached.recv()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(
            !close.is_finished(),
            "admission close completed after reaching but without awaiting the paused reload join"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut close_after_join)
                .await
                .is_err(),
            "admission close crossed the reload join while its worker was paused"
        );
        resume_reload.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), close_after_join)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), close)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.reload_in_progress.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(control.initiate_security_reload().is_err());

        control
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
        tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Stopped
        );
        drop(remote);
    }

    #[test]
    fn observer_delta_accounting_ignores_rejections_and_snapshot_interleaving() {
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
            AccessFlowRelayEventKind::ConnectionRejected,
            Some(ConnectionCloseCategory::Saturated),
            2,
        ));
        observer.observe(observed_event(
            AccessFlowRelayEventKind::ConnectionRejected,
            Some(ConnectionCloseCategory::Cancelled),
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn supervisor_keeps_live_flow_until_drain_deadline_then_forces_it() {
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
            None,
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
        let mut preface = [0_u8; ACCESS_FLOW_V2_HEADER_LEN];
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

        let shutdown_control = control.clone();
        let shutdown = tokio::spawn(async move {
            shutdown_control
                .shutdown(Instant::now() + Duration::from_millis(200))
                .await;
        });
        sleep(Duration::from_millis(30)).await;
        assert!(!shutdown.is_finished());
        client.write_all(b"pre-deadline").await.unwrap();
        let mut pre_deadline = [0_u8; 12];
        channel.read_exact(&mut pre_deadline).await.unwrap();
        assert_eq!(&pre_deadline, b"pre-deadline");
        assert_eq!(control.active_flows(), 1);

        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
        supervisor.await.unwrap().unwrap();
        assert_eq!(control.active_flows(), 0);
        let mut eof = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), channel.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Stopped
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn slow_relay_dependent_stop_does_not_consume_the_flow_drain_window() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("access-flow.sock");
        let endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        endpoint_listener.set_nonblocking(true).unwrap();
        let endpoint_listener = tokio::net::UnixListener::from_std(endpoint_listener).unwrap();
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let mut config = test_config(listen.to_string(), endpoint.display().to_string());
        config.drain_timeout = "200ms".into();
        let (control, commands) = RelayControl::configured(&config);
        let dependent = Arc::new(ManagedService::new(
            ServiceConfig {
                name: "relay-dependent".into(),
                required: true,
                user: "root".into(),
                command: vec!["sleep".into(), "infinity".into()],
                cwd: None,
                restart: RestartPolicy::Never,
                restart_backoff: None,
                restart_backoff_max: None,
                startup_timeout: None,
                shutdown_timeout: Some("1s".into()),
                depends_on: vec![crate::config::ACCESS_FLOW_RELAY_NODE.into()],
                env: BTreeMap::new(),
                health_check: None,
            },
            dir.path().to_path_buf(),
            LoggingConfig::default(),
        ));
        let state = Arc::new(AgentState::new(
            dir.path().join("state"),
            None,
            false,
            None,
            None,
            Some(control.clone()),
        ));
        *state.services.lock().await = vec![Arc::clone(&dependent)];
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Disabled,
            None,
            Vec::new(),
            Arc::clone(&state),
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
        let mut preface = [0_u8; ACCESS_FLOW_V2_HEADER_LEN];
        channel.read_exact(&mut preface).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.active_flows() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let child_guard = dependent.child.lock().await;
        let shutdown_state = Arc::clone(&state);
        let shutdown =
            tokio::spawn(
                async move { super::super::lifecycle::shutdown_agent(shutdown_state).await },
            );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !dependent.stopping.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        sleep(Duration::from_millis(150)).await;
        assert!(!shutdown.is_finished());
        drop(child_guard);

        sleep(Duration::from_millis(75)).await;
        assert!(
            !shutdown.is_finished(),
            "relay-dependent service stop consumed the flow drain window"
        );
        client.write_all(b"fresh-window").await.unwrap();
        let mut received = [0_u8; 12];
        channel.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"fresh-window");

        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
        supervisor.await.unwrap().unwrap();
        assert_eq!(control.active_flows(), 0);
    }

    #[cfg(target_os = "linux")]
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
            None,
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
        let mut preface = [0_u8; ACCESS_FLOW_V2_HEADER_LEN];
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ordered_shutdown_cancellation_during_activation_stops_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("access-flow.sock");
        let _endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
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
        let (activation_reached, resume_activation) = control.pause_before_activation().await;
        let supervisor = tokio::spawn(run_relay_supervisor(
            config,
            IdentityPresentation::Disabled,
            None,
            Vec::new(),
            state.clone(),
            control.clone(),
            commands,
        ));

        tokio::time::timeout(Duration::from_secs(2), activation_reached)
            .await
            .unwrap()
            .unwrap();
        let closing_control = control.clone();
        let close = tokio::spawn(async move {
            closing_control.close_admission().await;
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !control.startup_cancellation.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        resume_activation.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), close)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !supervisor.is_finished(),
            "ordered close must keep the supervisor available for shutdown"
        );

        control
            .shutdown(Instant::now() + Duration::from_millis(100))
            .await;
        tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.relay_fatal(), None);
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Stopped
        );
    }

    #[tokio::test]
    async fn direct_shutdown_during_dependency_wait_terminates_supervisor() {
        let mut config = test_config("127.0.0.1:3128".into(), "/unused/access-flow.sock".into());
        config.start_after_services = vec!["dependency".into()];
        let dependency = dependency_service("dependency");
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
            None,
            vec![dependency],
            state.clone(),
            control.clone(),
            commands,
        ));
        tokio::task::yield_now().await;

        tokio::time::timeout(
            Duration::from_secs(1),
            control.shutdown(Instant::now() + Duration::from_millis(100)),
        )
        .await
        .expect("relay did not acknowledge direct startup shutdown");
        tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .expect("relay supervisor parked after direct startup shutdown")
            .unwrap()
            .unwrap();
        assert_eq!(state.relay_fatal(), None);
        assert_eq!(
            control.status().await.state,
            AccessFlowRelayStateName::Stopped
        );
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
            None,
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
            .compile(crate::config::AccessFlowRelayValidationMode::Agent, None)
            .unwrap();
        probe_unix_endpoints(&compiled).unwrap();
        let transport =
            RelayTransportRuntime::activate(&compiled.plan, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap();
        let relay = AccessFlowRelay::new(
            compiled.plan,
            transport.connector(),
            Arc::new(RelayObserver {
                active_flows: Arc::new(AtomicUsize::new(0)),
            }),
            relay_resource_budget(false, 0, 0, 0).unwrap(),
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
            .compile(crate::config::AccessFlowRelayValidationMode::Agent, None)
            .unwrap();
        assert!(probe_unix_endpoints(&compiled).is_err());

        let regular = dir.path().join("regular");
        std::fs::write(&regular, b"not a socket").unwrap();
        let compiled = test_config("127.0.0.1:3128".into(), regular.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent, None)
            .unwrap();
        assert!(probe_unix_endpoints(&compiled).is_err());

        let socket = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut permissions = std::fs::metadata(&socket).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o777);
        std::fs::set_permissions(&socket, permissions).unwrap();
        let compiled = test_config("127.0.0.1:3128".into(), socket.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent, None)
            .unwrap();
        probe_unix_endpoints(&compiled).unwrap();

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        let compiled = test_config("127.0.0.1:3128".into(), link.display().to_string())
            .compile(crate::config::AccessFlowRelayValidationMode::Agent, None)
            .unwrap();
        assert!(probe_unix_endpoints(&compiled).is_err());
    }
}
