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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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

use super::relay_transport::{PendingRelayTransportReload, RelayTransportRuntime};
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
    ReloadSecurity,
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
    security_healthy: Arc<AtomicBool>,
    reload_in_progress: AtomicBool,
    phase: AtomicU8,
    command_tx: mpsc::Sender<RelayCommand>,
    startup_cancellation: RelayCancellation,
    drain_timeout: Duration,
    #[cfg(test)]
    activation_pause: Mutex<Option<ActivationPause>>,
    #[cfg(test)]
    reload_pause: Mutex<Option<ReloadWorkerPause>>,
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
                security_healthy: Arc::new(AtomicBool::new(false)),
                reload_in_progress: AtomicBool::new(false),
                phase: AtomicU8::new(RELAY_PHASE_PREPARING),
                command_tx,
                startup_cancellation,
                drain_timeout,
                #[cfg(test)]
                activation_pause: Mutex::new(None),
                #[cfg(test)]
                reload_pause: Mutex::new(None),
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
            && self.security_healthy.load(Ordering::Acquire)
    }

    pub(super) fn drain_timeout(&self) -> Duration {
        self.drain_timeout
    }

    pub(super) async fn close_admission(&self) {
        let phase = self.phase.swap(RELAY_PHASE_CLOSING, Ordering::AcqRel);
        self.accepting.store(false, Ordering::Release);
        self.security_healthy.store(false, Ordering::Release);
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

    pub(super) fn initiate_security_reload(self: &Arc<Self>) -> Result<(), ()> {
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
        if self
            .command_tx
            .try_send(RelayCommand::ReloadSecurity)
            .is_err()
        {
            self.reload_in_progress.store(false, Ordering::Release);
            tracing::debug!(
                category = "security_material",
                "access flow relay trust reload was not started"
            );
            return Err(());
        }
        Ok(())
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

    #[cfg(test)]
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
    async fn pause_blocking_reload(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, wait_for_reached) = std::sync::mpsc::channel();
        let (resume, wait_for_resume) = std::sync::mpsc::channel();
        let previous = self.reload_pause.lock().await.replace(ReloadWorkerPause {
            reached,
            resume: wait_for_resume,
        });
        assert!(previous.is_none(), "reload pause already installed");
        (wait_for_reached, resume)
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
        )
        .context("compile access flow relay configuration")?;
    probe_unix_endpoints(&compiled)?;
    let transport_reserve = RelayTransportRuntime::resource_reserve(&compiled.plan)
        .context("project access flow relay transport reload resources")?;
    let budget = relay_resource_budget(
        state.bridge_enabled,
        services.len(),
        transport_reserve.descriptors,
        transport_reserve.memory_bytes,
    )?;
    let transport =
        RelayTransportRuntime::prepare(&compiled.plan, Arc::clone(&control.security_healthy))
            .context("prepare access flow relay transport")?;
    let relay = AccessFlowRelay::new(
        compiled.plan,
        transport.connector(),
        Arc::new(RelayObserver {
            active_flows: Arc::clone(&control.active_flows),
        }),
        budget,
    )
    .context("access flow relay resource preflight")?;
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
    transport
        .activate_prepared()
        .await
        .context("activate access flow relay transport")?;
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
        Some(RelayCommand::ReloadSecurity) => {
            finish_security_reload(control, ReloadFinish::Rejected, Err(()));
            None
        }
        Some(RelayCommand::CloseAdmission(completed)) => {
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
            RelayCommand::ReloadSecurity => {
                finish_security_reload(control, ReloadFinish::Rejected, Err(()));
            }
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
                finish_pending_reload(
                    control,
                    &mut pending_reload,
                    ReloadFinish::Shutdown,
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
                    Some(RelayCommand::ReloadSecurity) => {
                        if pending_reload.is_some() {
                            tracing::debug!(
                                category = "security_material",
                                "access flow relay concurrent trust reload was rejected"
                            );
                        } else {
                            #[cfg(test)]
                            let reload_pause = control.reload_pause.lock().await.take();
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
                            match reload {
                                Ok(Some(reload)) => {
                                    pending_reload = Some(reload);
                                    tracing::debug!(
                                        category = "security_material",
                                        "access flow relay trust reload started"
                                    );
                                }
                                Ok(None) => finish_security_reload(
                                    control,
                                    ReloadFinish::Completed,
                                    Ok(()),
                                ),
                                Err(_) => finish_security_reload(
                                    control,
                                    ReloadFinish::Completed,
                                    Err(()),
                                ),
                            }
                        }
                    }
                    Some(RelayCommand::CloseAdmission(completed)) => {
                        transport.close();
                        finish_pending_reload(
                            control,
                            &mut pending_reload,
                            ReloadFinish::Shutdown,
                        ).await;
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
                        ).await;
                        control.accepting.store(false, Ordering::Release);
                        let _ = running.shutdown(Instant::now()).await;
                        return Err(anyhow::anyhow!("access flow relay control channel closed"));
                    }
                    #[cfg(test)]
                    Some(RelayCommand::InjectFailure { failure, observed }) => {
                        transport.close();
                        finish_pending_reload(
                            control,
                            &mut pending_reload,
                            ReloadFinish::Shutdown,
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
) {
    if let Some(pending) = pending.take() {
        let result = pending.complete().await.map_err(|_| ());
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
            RelayCommand::ReloadSecurity => {
                finish_security_reload(control, ReloadFinish::Rejected, Err(()));
            }
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
        AccessFlowRelayPresentation, AccessFlowRelayRoute, AccessFlowRelayTransport,
        AccessFlowRelayTrust, LoggingConfig, RestartPolicy, ServiceConfig,
    };
    #[cfg(target_os = "linux")]
    use access_flow::{
        AccessFlowAcceptor, AccessFlowAdmission, AccessFlowAdmissionInput,
        AccessFlowPresentationMode,
    };
    #[cfg(target_os = "linux")]
    use access_flow_conformance::load_tls_pki_fixture;
    use access_flow_relay::{AccessFlowConnector, AccessFlowRelayFailureKind};
    #[cfg(target_os = "linux")]
    use access_flow_tls::{
        TlsAccessFlowHandshakeTimeout, TlsAccessFlowServerAdapter, TlsAccessFlowServerChannel,
        TlsAccessFlowServerLimits, TlsAccessFlowTcpListener,
    };
    use std::collections::BTreeMap;
    #[cfg(target_os = "linux")]
    use std::convert::Infallible;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(target_os = "linux")]
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
    struct ExpectProductTlsPreface {
        destination: access_flow::AccessFlowDestination,
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
        assert_eq!(relay_config.max_connections, 96);
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
            let AccessFlowRelayTransport::TlsTcp { trust, .. } = &mut route.transport else {
                panic!("shipped TLS relay contains a non-TLS route");
            };
            let AccessFlowRelayTrust::PemBundle { path } = trust;
            *path = trust_path.display().to_string();
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
                )
                .unwrap();
            let transport_reserve =
                RelayTransportRuntime::resource_reserve(&compiled.plan).unwrap();
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
            TlsAccessFlowServerLimits::new(8, 8, 16, 32, 1).unwrap(),
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
                    trust: AccessFlowRelayTrust::PemBundle {
                        path: trust_path.display().to_string(),
                    },
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
                    &ExpectProductTlsPreface { destination },
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
            TlsAccessFlowServerLimits::new(8, 8, 16, 32, 1).unwrap(),
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
                    trust: AccessFlowRelayTrust::PemBundle {
                        path: trust_path.display().to_string(),
                    },
                },
            }],
        };
        let compiled = config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
                ),
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
                    trust: AccessFlowRelayTrust::PemBundle {
                        path: trust_path.display().to_string(),
                    },
                },
            }],
        };
        let presentation = IdentityPresentation::Bearer(
            access_identity::SensitiveBearer::new(TEST_ACCESS_FLOW_BEARER).unwrap(),
        );
        let (control, commands) = RelayControl::configured(&config);
        let (reload_reached, resume_reload) = control.pause_blocking_reload().await;
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
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || reload_reached.recv()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(!control.is_ready());
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
            "shutdown completed without joining the paused reload task"
        );
        resume_reload.send(()).unwrap();
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
