use crate::config::CompiledAccessFlowRelayEndpoint;
use access_async_contracts::{AccessCancellation, BoxAccessFuture};
use access_flow_relay::{
    AccessFlowChannelFailure, AccessFlowChannelResourceCost, AccessFlowConnectContext,
    AccessFlowConnector, AccessFlowRelayPlan,
};
use access_flow_tls::{
    ACTIVE_CHANNEL_BYTES, CLIENT_DNS_ADDRESS_BYTES, CLIENT_PEER_CHAIN_BYTES, HANDSHAKE_BYTES,
    TlsAccessFlowClientEndpoint, TlsAccessFlowClientLimits, TlsAccessFlowConnector,
    TlsAccessFlowPreparedClientEndpoint, TlsAccessFlowStream,
};
use access_flow_unix::{UnixAccessFlowConnector, UnixAccessFlowStream};
use access_tls_trust::{
    CUSTOM_COMPONENT_RETAINED_CEILING, EFFECTIVE_PLAN_CONTROL_BYTES, GENERATION_CONTROL_BYTES,
    MAX_CUSTOM_DER_BYTES, MAX_CUSTOM_PEM_BYTES, MAX_SYSTEM_DER_BYTES, PreparedTlsTrustCandidate,
    SYSTEM_COMPONENT_RETAINED_CEILING, TRUST_SOURCE_WORKSPACE_CONTROL_BYTES, TlsClientTrustMode,
    TlsClientTrustPlan, TlsTrustFileSource, TlsTrustGeneration, TlsTrustLoadError,
    TlsTrustLoadLimits, TlsTrustLoadWorkspace, TlsTrustRevalidationProgress,
    WORKSPACE_SCRATCH_BYTES,
};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_PUBLISHED_TRUST_GENERATIONS: usize = 8;
#[cfg(test)]
const TEST_TLS_TRUST_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(test)]
const TEST_TLS_TRUST_DESCRIPTOR_BUDGET: u64 = 32;
const TLS_ENDPOINT_CONTROL_BYTES: u64 = 8 * 1024;
const TRUST_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const TRUST_CANCELLATION_JOIN_GRACE: Duration = Duration::from_millis(10);
const TRUST_CANDIDATE_BASE_TIMEOUT: Duration = Duration::from_secs(60);
const TRUST_CANDIDATE_MAX_TIMEOUT: Duration = Duration::from_secs(180);

/// Fixed, path-free product transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RelayTransportError {
    SystemStoreEmpty,
    SystemStoreUnavailable,
    UntrustedSource,
    InvalidMaterial,
    SourceResourceLimit,
    InvalidPlan,
    Cancelled,
    Internal,
    TrustGenerationLimit,
    ReloadInProgress,
    Unavailable,
    ShuttingDown,
}

impl RelayTransportError {
    fn keeps_active_generation_ready(self) -> bool {
        matches!(
            self,
            Self::SystemStoreEmpty
                | Self::SystemStoreUnavailable
                | Self::Cancelled
                | Self::TrustGenerationLimit
                | Self::ReloadInProgress
        )
    }

    pub(super) const fn status_code(self) -> &'static str {
        match self {
            Self::SystemStoreEmpty => "system_store_empty",
            Self::SystemStoreUnavailable => "system_store_unavailable",
            Self::UntrustedSource => "untrusted_source",
            Self::InvalidMaterial => "invalid_material",
            Self::SourceResourceLimit => "trust_source_resource_limit",
            Self::InvalidPlan => "invalid_plan",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
            Self::TrustGenerationLimit => "trust_generation_limit",
            Self::ReloadInProgress => "trust_reload_blocked",
            Self::Unavailable => "unavailable",
            Self::ShuttingDown => "shutting_down",
        }
    }
}

impl fmt::Display for RelayTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SystemStoreEmpty => "Access Flow system trust store is empty",
            Self::SystemStoreUnavailable => "Access Flow system trust store is unavailable",
            Self::UntrustedSource => "Access Flow trust source is not trusted",
            Self::InvalidMaterial => "Access Flow trust material is invalid",
            Self::SourceResourceLimit => "Access Flow trust source exceeds a resource bound",
            Self::InvalidPlan => "Access Flow transport plan is invalid",
            Self::Cancelled => "Access Flow trust reload was cancelled",
            Self::Internal => "Access Flow transport coordinator failed",
            Self::TrustGenerationLimit => "Access Flow trust generation limit was reached",
            Self::ReloadInProgress => "Access Flow transport reload is already in progress",
            Self::Unavailable => "Access Flow transport is unavailable",
            Self::ShuttingDown => "Access Flow transport is shutting down",
        })
    }
}

impl Error for RelayTransportError {}

impl From<TlsTrustLoadError> for RelayTransportError {
    fn from(error: TlsTrustLoadError) -> Self {
        match error {
            TlsTrustLoadError::SystemStoreEmpty => Self::SystemStoreEmpty,
            TlsTrustLoadError::SystemStoreUnavailable => Self::SystemStoreUnavailable,
            TlsTrustLoadError::UntrustedSource => Self::UntrustedSource,
            TlsTrustLoadError::InvalidMaterial => Self::InvalidMaterial,
            TlsTrustLoadError::ResourceLimit => Self::SourceResourceLimit,
            TlsTrustLoadError::InvalidPlan => Self::InvalidPlan,
            TlsTrustLoadError::Cancelled => Self::Cancelled,
            TlsTrustLoadError::Internal => Self::Internal,
        }
    }
}

#[derive(Clone)]
struct TlsRouteSource {
    address: access_flow_tls::TlsAccessFlowAddress,
    server_name: access_flow_tls::TlsAccessFlowServerName,
    mode: TlsClientTrustMode,
    plan: TlsClientTrustPlan,
    trust_path: Option<PathBuf>,
}

pub(super) struct RelayTransportGeneration {
    id: TlsTrustGeneration,
    tls_endpoints: Box<[TlsAccessFlowPreparedClientEndpoint]>,
    trust_candidate: Option<PreparedTlsTrustCandidate>,
}

impl RelayTransportGeneration {
    fn retained_bytes(&self) -> u64 {
        self.trust_candidate
            .as_ref()
            .map(|candidate| candidate.resource_projection().retained_bytes())
            .unwrap_or(0)
    }
}

#[derive(Clone)]
enum RelayTransportState {
    Pending,
    Healthy(Arc<RelayTransportGeneration>),
    Unhealthy(Arc<RelayTransportGeneration>),
    Closed,
}

struct RelayTransportInner {
    sources: Box<[TlsRouteSource]>,
    state: watch::Sender<RelayTransportState>,
    readiness: Arc<AtomicBool>,
    next_generation: AtomicU64,
    reload_in_progress: AtomicBool,
    blocked_operation: AtomicBool,
    trust_budget: OnceLock<RelayTransportTrustBudget>,
    publication: Mutex<()>,
    retained_generations: Mutex<Vec<Weak<RelayTransportGeneration>>>,
    shutdown_started: AtomicBool,
    shutdown_cancellation: CancellationToken,
    unix: UnixAccessFlowConnector,
    tls: TlsAccessFlowConnector,
}

/// Product-owned lifecycle and atomic trust-generation coordinator.
#[derive(Clone)]
pub(super) struct RelayTransportRuntime {
    inner: Arc<RelayTransportInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RelayTransportResourceReserve {
    pub(super) descriptors: u64,
    pub(super) memory_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RelayTransportTrustBudget {
    pub(super) descriptors: u64,
    pub(super) memory_bytes: u64,
}

impl RelayTransportRuntime {
    pub(super) fn prepare(
        plan: &AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
        readiness: Arc<AtomicBool>,
    ) -> Result<Self, RelayTransportError> {
        readiness.store(false, Ordering::Release);
        let sources = collect_tls_sources(plan)?;
        project_candidate_loader_peak(&sources)?;
        let (state, _) = watch::channel(RelayTransportState::Pending);
        Ok(Self {
            inner: Arc::new(RelayTransportInner {
                sources: sources.into_boxed_slice(),
                state,
                readiness,
                next_generation: AtomicU64::new(2),
                reload_in_progress: AtomicBool::new(false),
                blocked_operation: AtomicBool::new(false),
                trust_budget: OnceLock::new(),
                publication: Mutex::new(()),
                retained_generations: Mutex::new(Vec::new()),
                shutdown_started: AtomicBool::new(false),
                shutdown_cancellation: CancellationToken::new(),
                unix: UnixAccessFlowConnector::new(),
                tls: TlsAccessFlowConnector::with_system_resolver(
                    TlsAccessFlowClientLimits::default(),
                ),
            }),
        })
    }

    #[cfg(test)]
    pub(super) async fn activate(
        plan: &AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
        readiness: Arc<AtomicBool>,
    ) -> Result<Self, RelayTransportError> {
        let runtime = Self::prepare(plan, readiness)?;
        runtime.configure_trust_budget(RelayTransportTrustBudget {
            descriptors: TEST_TLS_TRUST_DESCRIPTOR_BUDGET,
            memory_bytes: TEST_TLS_TRUST_BUDGET_BYTES,
        })?;
        runtime.activate_prepared().await?;
        Ok(runtime)
    }

    pub(super) fn configure_trust_budget(
        &self,
        budget: RelayTransportTrustBudget,
    ) -> Result<(), RelayTransportError> {
        let candidate_memory = project_candidate_loader_peak(&self.inner.sources)?;
        let candidate_descriptors = projected_candidate_descriptors(&self.inner.sources)?;
        if !self.inner.sources.is_empty()
            && (budget.descriptors == 0
                || budget.memory_bytes == 0
                || candidate_memory > budget.memory_bytes
                || candidate_descriptors > budget.descriptors)
        {
            return Err(RelayTransportError::SourceResourceLimit);
        }
        self.inner
            .trust_budget
            .set(budget)
            .map_err(|_| RelayTransportError::InvalidPlan)
    }

    pub(super) async fn activate_prepared(&self) -> Result<(), RelayTransportError> {
        if self.inner.blocked_operation.load(Ordering::Acquire) {
            return Err(RelayTransportError::ReloadInProgress);
        }
        let budget = self.inner.trust_budget()?;
        let initial = match load_generation(
            self.inner.sources.to_vec(),
            1,
            budget,
            self.inner.shutdown_cancellation.clone(),
        )
        .await
        {
            GenerationLoadOutcome::Finished(result) => result?,
            GenerationLoadOutcome::TimedOut(watcher) => {
                self.inner.install_blocked_operation(watcher);
                return Err(RelayTransportError::Cancelled);
            }
        };
        let _publication = self
            .inner
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.shutdown_started.load(Ordering::Acquire) {
            return Err(RelayTransportError::ShuttingDown);
        }
        if !matches!(&*self.inner.state.borrow(), RelayTransportState::Pending) {
            return Err(RelayTransportError::Internal);
        }
        self.inner
            .publish_generation_as_healthy(Arc::new(initial))?;
        Ok(())
    }

    pub(super) fn connector(&self) -> RelayTransportConnector {
        RelayTransportConnector {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(super) fn reload_blocked(&self) -> bool {
        self.inner.blocked_operation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn install_test_blocked_operation(&self, watcher: tokio::task::JoinHandle<()>) {
        self.inner.install_blocked_operation(watcher);
    }

    pub(super) fn resource_reserve(
        plan: &AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
    ) -> Result<RelayTransportResourceReserve, RelayTransportError> {
        let sources = collect_tls_sources(plan)?;
        Ok(RelayTransportResourceReserve {
            descriptors: projected_candidate_descriptors(&sources)?,
            memory_bytes: project_candidate_loader_peak(&sources)?,
        })
    }

    pub(super) fn begin_reload(
        &self,
    ) -> Result<Option<PendingRelayTransportReload>, RelayTransportError> {
        self.begin_reload_inner(|| {})
    }

    #[cfg(test)]
    pub(super) fn begin_reload_with_hook(
        &self,
        worker_started: impl FnOnce() + Send + 'static,
    ) -> Result<Option<PendingRelayTransportReload>, RelayTransportError> {
        self.begin_reload_inner(worker_started)
    }

    fn begin_reload_inner(
        &self,
        worker_started: impl FnOnce() + Send + 'static,
    ) -> Result<Option<PendingRelayTransportReload>, RelayTransportError> {
        let _publication = self
            .inner
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.shutdown_started.load(Ordering::Acquire) {
            return Err(RelayTransportError::ShuttingDown);
        }
        if matches!(&*self.inner.state.borrow(), RelayTransportState::Pending) {
            return Err(RelayTransportError::Unavailable);
        }
        if self.inner.blocked_operation.load(Ordering::Acquire) {
            return Err(RelayTransportError::ReloadInProgress);
        }
        if self.inner.sources.is_empty() {
            return Ok(None);
        }
        if self
            .inner
            .reload_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(RelayTransportError::ReloadInProgress);
        }
        let retained_bytes = self.inner.retained_generation_bytes()?;
        let candidate_peak = project_candidate_loader_peak(&self.inner.sources)?;
        let budget = self.inner.trust_budget()?;
        let Some(working_limit) = budget.memory_bytes.checked_sub(retained_bytes) else {
            self.inner
                .reload_in_progress
                .store(false, Ordering::Release);
            return Err(RelayTransportError::TrustGenerationLimit);
        };
        if candidate_peak > working_limit {
            self.inner
                .reload_in_progress
                .store(false, Ordering::Release);
            return Err(RelayTransportError::TrustGenerationLimit);
        }
        let generation = match self.inner.next_generation.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| current.checked_add(1),
        ) {
            Ok(generation) => generation,
            Err(_) => {
                self.inner
                    .reload_in_progress
                    .store(false, Ordering::Release);
                return Err(RelayTransportError::TrustGenerationLimit);
            }
        };
        let sources = self.inner.sources.to_vec();
        let shutdown_cancellation = self.inner.shutdown_cancellation.clone();
        let worker = tokio::spawn(async move {
            let hook = tokio::task::spawn_blocking(worker_started);
            if hook.await.is_err() {
                return GenerationLoadOutcome::Finished(Err(RelayTransportError::Internal));
            }
            load_generation(
                sources,
                generation,
                RelayTransportTrustBudget {
                    descriptors: budget.descriptors,
                    memory_bytes: working_limit,
                },
                shutdown_cancellation,
            )
            .await
        });
        Ok(Some(PendingRelayTransportReload {
            inner: Arc::clone(&self.inner),
            worker: Some(worker),
        }))
    }

    pub(super) fn close(&self) {
        self.inner.shutdown_cancellation.cancel();
        let _publication = self
            .inner
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.inner.shutdown_started.swap(true, Ordering::AcqRel) {
            self.inner.publish_closed();
        }
    }
}

impl fmt::Debug for RelayTransportRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayTransportRuntime")
    }
}

/// One owned reload worker. It must be joined before being dropped.
#[must_use = "pending transport reload workers must be joined"]
pub(super) struct PendingRelayTransportReload {
    inner: Arc<RelayTransportInner>,
    worker: Option<tokio::task::JoinHandle<GenerationLoadOutcome>>,
}

impl PendingRelayTransportReload {
    pub(super) async fn wait(&mut self) -> Result<(), RelayTransportError> {
        let outcome = {
            let worker = self.worker.as_mut().ok_or(RelayTransportError::Internal)?;
            worker.await.unwrap_or(GenerationLoadOutcome::Finished(Err(
                RelayTransportError::Internal,
            )))
        };
        self.worker = None;
        match outcome {
            GenerationLoadOutcome::Finished(loaded) => self.publish(loaded),
            GenerationLoadOutcome::TimedOut(watcher) => {
                self.inner.install_blocked_operation(watcher);
                Err(RelayTransportError::Cancelled)
            }
        }
    }

    pub(super) async fn complete(mut self) -> Result<(), RelayTransportError> {
        self.wait().await
    }

    pub(super) async fn complete_by(
        mut self,
        deadline: Instant,
    ) -> Result<(), RelayTransportError> {
        let mut worker = self.worker.take().ok_or(RelayTransportError::Internal)?;
        match tokio::time::timeout_at(deadline, &mut worker).await {
            Ok(outcome) => match outcome.unwrap_or(GenerationLoadOutcome::Finished(Err(
                RelayTransportError::Internal,
            ))) {
                GenerationLoadOutcome::Finished(loaded) => self.publish(loaded),
                GenerationLoadOutcome::TimedOut(watcher) => {
                    self.inner.install_blocked_operation(watcher);
                    Err(RelayTransportError::Cancelled)
                }
            },
            Err(_) => {
                self.inner.shutdown_cancellation.cancel();
                tokio::task::yield_now().await;
                if worker.is_finished() {
                    let _ = worker.await;
                    self.inner
                        .reload_in_progress
                        .store(false, Ordering::Release);
                } else {
                    self.inner.install_blocked_candidate(worker);
                }
                Err(RelayTransportError::Cancelled)
            }
        }
    }

    fn publish(
        &self,
        loaded: Result<RelayTransportGeneration, RelayTransportError>,
    ) -> Result<(), RelayTransportError> {
        let _publication = self
            .inner
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = if self.inner.shutdown_started.load(Ordering::Acquire) {
            Err(RelayTransportError::ShuttingDown)
        } else {
            match loaded {
                Ok(generation) => self
                    .inner
                    .publish_generation_as_healthy(Arc::new(generation)),
                Err(error) => {
                    if !error.keeps_active_generation_ready() {
                        self.inner.preserve_generation_as_unhealthy();
                    }
                    Err(error)
                }
            }
        };
        self.inner
            .reload_in_progress
            .store(false, Ordering::Release);
        result
    }
}

impl fmt::Debug for PendingRelayTransportReload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingRelayTransportReload")
    }
}

impl RelayTransportInner {
    fn trust_budget(&self) -> Result<RelayTransportTrustBudget, RelayTransportError> {
        self.trust_budget
            .get()
            .copied()
            .ok_or(RelayTransportError::InvalidPlan)
    }

    fn install_blocked_operation(self: &Arc<Self>, watcher: tokio::task::JoinHandle<()>) {
        self.blocked_operation.store(true, Ordering::Release);
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let _ = watcher.await;
            inner.blocked_operation.store(false, Ordering::Release);
            inner.reload_in_progress.store(false, Ordering::Release);
        });
    }

    fn install_blocked_candidate(
        self: &Arc<Self>,
        watcher: tokio::task::JoinHandle<GenerationLoadOutcome>,
    ) {
        self.blocked_operation.store(true, Ordering::Release);
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            if let Ok(GenerationLoadOutcome::TimedOut(operation)) = watcher.await {
                let _ = operation.await;
            }
            inner.blocked_operation.store(false, Ordering::Release);
            inner.reload_in_progress.store(false, Ordering::Release);
        });
    }
    fn retained_generation_bytes(&self) -> Result<u64, RelayTransportError> {
        let mut retained = self
            .retained_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retained.retain(|generation| generation.strong_count() > 0);
        retained
            .iter()
            .filter_map(Weak::upgrade)
            .try_fold(0_u64, |total, generation| {
                total.checked_add(generation.retained_bytes())
            })
            .ok_or(RelayTransportError::TrustGenerationLimit)
    }

    fn preserve_generation_as_unhealthy(&self) {
        let current = match self.state.borrow().clone() {
            RelayTransportState::Healthy(generation)
            | RelayTransportState::Unhealthy(generation) => generation,
            RelayTransportState::Pending | RelayTransportState::Closed => return,
        };
        self.readiness.store(false, Ordering::Release);
        self.state
            .send_replace(RelayTransportState::Unhealthy(current));
    }

    fn publish_generation_as_healthy(
        &self,
        generation: Arc<RelayTransportGeneration>,
    ) -> Result<(), RelayTransportError> {
        let mut retained = self
            .retained_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retained.retain(|generation| generation.strong_count() > 0);
        let retained_bytes = retained
            .iter()
            .filter_map(Weak::upgrade)
            .try_fold(0_u64, |total, generation| {
                total.checked_add(generation.retained_bytes())
            })
            .ok_or(RelayTransportError::TrustGenerationLimit)?;
        if retained.len() >= MAX_PUBLISHED_TRUST_GENERATIONS
            || retained_bytes
                .checked_add(generation.retained_bytes())
                .is_none_or(|bytes| {
                    self.trust_budget
                        .get()
                        .is_none_or(|budget| bytes > budget.memory_bytes)
                })
        {
            return Err(RelayTransportError::TrustGenerationLimit);
        }
        retained.push(Arc::downgrade(&generation));
        drop(retained);
        self.state
            .send_replace(RelayTransportState::Healthy(generation));
        self.readiness.store(true, Ordering::Release);
        Ok(())
    }

    fn publish_closed(&self) {
        self.readiness.store(false, Ordering::Release);
        self.state.send_replace(RelayTransportState::Closed);
    }

    fn healthy_generation(&self) -> Option<Arc<RelayTransportGeneration>> {
        match self.state.borrow().clone() {
            RelayTransportState::Healthy(generation) => Some(generation),
            RelayTransportState::Pending
            | RelayTransportState::Unhealthy(_)
            | RelayTransportState::Closed => None,
        }
    }

    fn generation_is_current(&self, expected: TlsTrustGeneration) -> bool {
        matches!(
            &*self.state.borrow(),
            RelayTransportState::Healthy(generation) if generation.id == expected
        )
    }
}

#[derive(Clone)]
pub(super) struct RelayTransportConnector {
    inner: Arc<RelayTransportInner>,
}

impl fmt::Debug for RelayTransportConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayTransportConnector")
    }
}

pub(super) enum RelayTransportStream {
    Unix(UnixAccessFlowStream),
    Tls {
        stream: TlsAccessFlowStream,
        _generation: Arc<RelayTransportGeneration>,
    },
}

impl fmt::Debug for RelayTransportStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unix(_) => "RelayTransportStream::Unix",
            Self::Tls { .. } => "RelayTransportStream::Tls",
        })
    }
}

impl AsyncRead for RelayTransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tls { stream, .. } => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for RelayTransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_write(context, buffer),
            Self::Tls { stream, .. } => Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls { stream, .. } => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls { stream, .. } => Pin::new(stream).poll_shutdown(context),
        }
    }
}

impl AccessFlowConnector for RelayTransportConnector {
    type Endpoint = CompiledAccessFlowRelayEndpoint;
    type Stream = RelayTransportStream;

    fn connect<'a>(
        &'a self,
        endpoint: &'a Self::Endpoint,
        context: AccessFlowConnectContext<'a>,
    ) -> BoxAccessFuture<'a, Result<Self::Stream, AccessFlowChannelFailure>> {
        Box::pin(async move {
            let generation = self
                .inner
                .healthy_generation()
                .ok_or(AccessFlowChannelFailure::Unavailable)?;
            let gate = GenerationCancellation {
                expected: generation.id,
                state: self.inner.state.subscribe(),
                caller: context.cancellation(),
            };
            let gated_context = AccessFlowConnectContext::new(context.deadline(), &gate);
            let stream = match endpoint {
                CompiledAccessFlowRelayEndpoint::Unix(endpoint) => self
                    .inner
                    .unix
                    .connect(endpoint, gated_context)
                    .await
                    .map(RelayTransportStream::Unix),
                CompiledAccessFlowRelayEndpoint::TlsTcp { tls_index, .. } => {
                    let endpoint = generation
                        .tls_endpoints
                        .get(*tls_index)
                        .ok_or(AccessFlowChannelFailure::InvalidEndpoint)?;
                    self.inner
                        .tls
                        .connect(endpoint, gated_context)
                        .await
                        .map(|stream| RelayTransportStream::Tls {
                            stream,
                            _generation: Arc::clone(&generation),
                        })
                }
            }?;
            if self.inner.generation_is_current(generation.id) {
                Ok(stream)
            } else {
                drop(stream);
                Err(AccessFlowChannelFailure::Cancelled)
            }
        })
    }

    fn resource_projection(
        &self,
        endpoint: &Self::Endpoint,
    ) -> Result<AccessFlowChannelResourceCost, AccessFlowChannelFailure> {
        match endpoint {
            CompiledAccessFlowRelayEndpoint::Unix(endpoint) => {
                self.inner.unix.resource_projection(endpoint)
            }
            CompiledAccessFlowRelayEndpoint::TlsTcp { .. } => {
                let connecting_bytes = ACTIVE_CHANNEL_BYTES
                    .checked_add(HANDSHAKE_BYTES)
                    .and_then(|bytes| bytes.checked_add(CLIENT_PEER_CHAIN_BYTES))
                    .and_then(|bytes| bytes.checked_add(CLIENT_DNS_ADDRESS_BYTES))
                    .ok_or(AccessFlowChannelFailure::ResourceExhausted)?;
                let active_bytes = ACTIVE_CHANNEL_BYTES
                    .checked_add(CLIENT_PEER_CHAIN_BYTES)
                    .ok_or(AccessFlowChannelFailure::ResourceExhausted)?;
                AccessFlowChannelResourceCost::new(
                    TLS_ENDPOINT_CONTROL_BYTES,
                    1,
                    1,
                    connecting_bytes,
                    active_bytes,
                )
            }
        }
    }
}

struct GenerationCancellation<'a> {
    expected: TlsTrustGeneration,
    state: watch::Receiver<RelayTransportState>,
    caller: &'a dyn AccessCancellation,
}

impl GenerationCancellation<'_> {
    fn generation_is_current(&self) -> bool {
        matches!(
            &*self.state.borrow(),
            RelayTransportState::Healthy(generation) if generation.id == self.expected
        )
    }
}

impl AccessCancellation for GenerationCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.caller.is_cancelled() || !self.generation_is_current()
    }

    fn cancelled(&self) -> BoxAccessFuture<'_, ()> {
        let mut state = self.state.clone();
        Box::pin(async move {
            loop {
                if self.caller.is_cancelled()
                    || !matches!(
                        &*state.borrow(),
                        RelayTransportState::Healthy(generation)
                            if generation.id == self.expected
                    )
                {
                    return;
                }
                tokio::select! {
                    biased;
                    () = self.caller.cancelled() => return,
                    changed = state.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
            }
        })
    }
}

fn collect_tls_sources(
    plan: &AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
) -> Result<Vec<TlsRouteSource>, RelayTransportError> {
    let mut indexed = BTreeMap::new();
    for route in plan.routes() {
        let CompiledAccessFlowRelayEndpoint::TlsTcp {
            tls_index,
            address,
            server_name,
            trust_mode,
            trust_plan,
            trust_path,
        } = route.endpoint()
        else {
            continue;
        };
        let source = TlsRouteSource {
            address: address.clone(),
            server_name: server_name.clone(),
            mode: *trust_mode,
            plan: *trust_plan,
            trust_path: trust_path.clone(),
        };
        if indexed.insert(*tls_index, source).is_some() {
            return Err(RelayTransportError::InvalidPlan);
        }
    }
    let mut sources = Vec::with_capacity(indexed.len());
    for expected in 0..indexed.len() {
        sources.push(
            indexed
                .remove(&expected)
                .ok_or(RelayTransportError::InvalidPlan)?,
        );
    }
    Ok(sources)
}

fn projected_candidate_descriptors(sources: &[TlsRouteSource]) -> Result<u64, RelayTransportError> {
    if sources.is_empty() {
        return Ok(0);
    }
    let custom_sources = sources
        .iter()
        .filter_map(|source| source.plan.custom_source())
        .collect::<BTreeSet<_>>()
        .len();
    u64::try_from(custom_sources)
        .ok()
        .and_then(|count| count.checked_add(8))
        .ok_or(RelayTransportError::SourceResourceLimit)
}

fn project_candidate_loader_peak(sources: &[TlsRouteSource]) -> Result<u64, RelayTransportError> {
    if sources.is_empty() {
        return Ok(0);
    }
    let plan_count = u64::try_from(
        sources
            .iter()
            .map(|source| source.plan)
            .collect::<BTreeSet<_>>()
            .len(),
    )
    .map_err(|_| RelayTransportError::SourceResourceLimit)?;
    let custom_count = u64::try_from(
        sources
            .iter()
            .filter_map(|source| source.plan.custom_source())
            .collect::<BTreeSet<_>>()
            .len(),
    )
    .map_err(|_| RelayTransportError::SourceResourceLimit)?;
    let system_required = sources.iter().any(|source| {
        matches!(
            source.mode,
            TlsClientTrustMode::System | TlsClientTrustMode::SystemPlusCustom
        )
    });
    let retained_base = GENERATION_CONTROL_BYTES
        .checked_add(
            plan_count
                .checked_mul(EFFECTIVE_PLAN_CONTROL_BYTES)
                .ok_or(RelayTransportError::SourceResourceLimit)?,
        )
        .ok_or(RelayTransportError::SourceResourceLimit)?;
    let workspace_base = WORKSPACE_SCRATCH_BYTES
        .checked_add(
            custom_count
                .checked_mul(TRUST_SOURCE_WORKSPACE_CONTROL_BYTES)
                .ok_or(RelayTransportError::SourceResourceLimit)?,
        )
        .and_then(|bytes| {
            bytes.checked_add(if custom_count > 0 {
                MAX_CUSTOM_PEM_BYTES as u64
            } else {
                0
            })
        })
        .ok_or(RelayTransportError::SourceResourceLimit)?;
    let retained_before_custom = retained_base
        .checked_add(if system_required {
            SYSTEM_COMPONENT_RETAINED_CEILING
        } else {
            0
        })
        .ok_or(RelayTransportError::SourceResourceLimit)?;
    let retained_complete = retained_before_custom
        .checked_add(
            custom_count
                .checked_mul(CUSTOM_COMPONENT_RETAINED_CEILING)
                .ok_or(RelayTransportError::SourceResourceLimit)?,
        )
        .ok_or(RelayTransportError::SourceResourceLimit)?;
    let system_step = if system_required {
        retained_base
            .checked_add(workspace_base)
            .and_then(|bytes| bytes.checked_add(SYSTEM_COMPONENT_RETAINED_CEILING))
            .and_then(|bytes| bytes.checked_add(MAX_SYSTEM_DER_BYTES as u64))
            .ok_or(RelayTransportError::SourceResourceLimit)?
    } else {
        0
    };
    let custom_step = if custom_count > 0 {
        retained_complete
            .checked_add(workspace_base)
            .and_then(|bytes| bytes.checked_add(MAX_CUSTOM_DER_BYTES as u64))
            .ok_or(RelayTransportError::SourceResourceLimit)?
    } else {
        0
    };
    retained_complete
        .checked_add(workspace_base)
        .map(|complete| complete.max(system_step).max(custom_step))
        .ok_or(RelayTransportError::SourceResourceLimit)
}

async fn load_generation(
    sources: Vec<TlsRouteSource>,
    generation: u64,
    budget: RelayTransportTrustBudget,
    shutdown_cancellation: CancellationToken,
) -> GenerationLoadOutcome {
    match load_generation_inner(&sources, generation, budget, shutdown_cancellation).await {
        Ok(generation) => GenerationLoadOutcome::Finished(Ok(generation)),
        Err(GenerationLoadFailure::Error(error)) => GenerationLoadOutcome::Finished(Err(error)),
        Err(GenerationLoadFailure::TimedOut(watcher)) => GenerationLoadOutcome::TimedOut(watcher),
    }
}

enum GenerationLoadOutcome {
    Finished(Result<RelayTransportGeneration, RelayTransportError>),
    TimedOut(tokio::task::JoinHandle<()>),
}

enum GenerationLoadFailure {
    Error(RelayTransportError),
    TimedOut(tokio::task::JoinHandle<()>),
}

impl From<RelayTransportError> for GenerationLoadFailure {
    fn from(error: RelayTransportError) -> Self {
        Self::Error(error)
    }
}

impl From<TlsTrustLoadError> for GenerationLoadFailure {
    fn from(error: TlsTrustLoadError) -> Self {
        Self::Error(error.into())
    }
}

enum WorkspaceOperation {
    LoadSystem,
    LoadCustom(TlsTrustFileSource),
    Revalidate,
    #[cfg(test)]
    Block(Arc<std::sync::Barrier>),
    #[cfg(test)]
    Mark(Arc<AtomicBool>),
    #[cfg(test)]
    WaitForCancellation {
        started: Arc<AtomicBool>,
        observed: Arc<AtomicBool>,
    },
}

async fn load_generation_inner(
    sources: &[TlsRouteSource],
    generation: u64,
    budget: RelayTransportTrustBudget,
    shutdown_cancellation: CancellationToken,
) -> Result<RelayTransportGeneration, GenerationLoadFailure> {
    if sources.is_empty() {
        return Ok(RelayTransportGeneration {
            id: TlsTrustGeneration::new(generation)?,
            tls_endpoints: Box::new([]),
            trust_candidate: None,
        });
    }
    let candidate_started = Instant::now();
    let custom_count = sources
        .iter()
        .filter_map(|source| source.plan.custom_source())
        .collect::<BTreeSet<_>>()
        .len();
    let candidate_timeout = candidate_timeout(custom_count)?;
    let candidate_deadline = candidate_started + candidate_timeout;
    let generation = TlsTrustGeneration::new(generation)?;
    let plans = sources
        .iter()
        .map(|source| source.plan)
        .collect::<BTreeSet<_>>();
    let limits = TlsTrustLoadLimits::new(budget.memory_bytes, budget.descriptors)?;
    let mut workspace = TlsTrustLoadWorkspace::new(generation, plans, limits)?;
    if sources.iter().any(|source| {
        matches!(
            source.mode,
            TlsClientTrustMode::System | TlsClientTrustMode::SystemPlusCustom
        )
    }) {
        let (next, _) = supervise_workspace_operation(
            workspace,
            WorkspaceOperation::LoadSystem,
            candidate_deadline,
            &shutdown_cancellation,
        )
        .await?;
        workspace = next;
    }
    let mut custom_sources = BTreeMap::new();
    for source in sources {
        let Some(path) = &source.trust_path else {
            continue;
        };
        custom_sources
            .entry(
                source
                    .plan
                    .custom_source()
                    .ok_or(RelayTransportError::InvalidPlan)?,
            )
            .or_insert_with(|| path.clone());
    }
    for path in custom_sources.into_values() {
        let source = TlsTrustFileSource::new(path)?;
        let (next, _) = supervise_workspace_operation(
            workspace,
            WorkspaceOperation::LoadCustom(source),
            candidate_deadline,
            &shutdown_cancellation,
        )
        .await?;
        workspace = next;
    }
    loop {
        let (next, progress) = supervise_workspace_operation(
            workspace,
            WorkspaceOperation::Revalidate,
            candidate_deadline,
            &shutdown_cancellation,
        )
        .await?;
        workspace = next;
        if progress == Some(TlsTrustRevalidationProgress::Complete) {
            break;
        }
    }
    if shutdown_cancellation.is_cancelled() || Instant::now() >= candidate_deadline {
        return Err(RelayTransportError::Cancelled.into());
    }
    let candidate = workspace.finalize()?;
    let mut endpoints = Vec::with_capacity(sources.len());
    for source in sources {
        let trust = candidate
            .prepared(&source.plan)
            .ok_or(RelayTransportError::Internal)?;
        let endpoint = TlsAccessFlowClientEndpoint::new(
            source.address.clone(),
            source.server_name.clone(),
            trust,
        )
        .activate()
        .map_err(|_| RelayTransportError::InvalidMaterial)?;
        endpoints.push(endpoint);
    }
    Ok(RelayTransportGeneration {
        id: generation,
        tls_endpoints: endpoints.into_boxed_slice(),
        trust_candidate: Some(candidate),
    })
}

async fn supervise_workspace_operation(
    workspace: TlsTrustLoadWorkspace,
    operation: WorkspaceOperation,
    candidate_deadline: Instant,
    shutdown_cancellation: &CancellationToken,
) -> Result<(TlsTrustLoadWorkspace, Option<TlsTrustRevalidationProgress>), GenerationLoadFailure> {
    if shutdown_cancellation.is_cancelled() || Instant::now() >= candidate_deadline {
        return Err(RelayTransportError::Cancelled.into());
    }
    let operation_deadline = (Instant::now() + TRUST_OPERATION_TIMEOUT).min(candidate_deadline);
    let cancellation = OperationCancellation {
        operation: CancellationToken::new(),
        shutdown: shutdown_cancellation.clone(),
    };
    let worker_cancellation = cancellation.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        let mut workspace = workspace;
        let result = match operation {
            WorkspaceOperation::LoadSystem => {
                workspace.load_system(&worker_cancellation).map(|()| None)
            }
            WorkspaceOperation::LoadCustom(source) => workspace
                .load_custom(&source, &worker_cancellation)
                .map(|()| None),
            WorkspaceOperation::Revalidate => {
                workspace.revalidate_next(&worker_cancellation).map(Some)
            }
            #[cfg(test)]
            WorkspaceOperation::Block(barrier) => {
                barrier.wait();
                Ok(None)
            }
            #[cfg(test)]
            WorkspaceOperation::Mark(started) => {
                started.store(true, Ordering::Release);
                Ok(None)
            }
            #[cfg(test)]
            WorkspaceOperation::WaitForCancellation { started, observed } => {
                started.store(true, Ordering::Release);
                while !worker_cancellation.is_cancelled() {
                    std::thread::yield_now();
                }
                observed.store(true, Ordering::Release);
                Err(TlsTrustLoadError::Cancelled)
            }
        };
        (workspace, result)
    });
    let completion = tokio::select! {
        result = tokio::time::timeout_at(operation_deadline, &mut worker) => Some(result),
        () = shutdown_cancellation.cancelled() => None,
    };
    match completion {
        Some(Ok(Ok((workspace, result)))) => Ok((workspace, result?)),
        Some(Ok(Err(_))) => Err(RelayTransportError::Internal.into()),
        Some(Err(_)) | None => {
            cancellation.operation.cancel();
            if tokio::time::timeout(TRUST_CANCELLATION_JOIN_GRACE, &mut worker)
                .await
                .is_ok()
            {
                return Err(RelayTransportError::Cancelled.into());
            }
            let watcher = tokio::spawn(async move {
                let _ = worker.await;
            });
            Err(GenerationLoadFailure::TimedOut(watcher))
        }
    }
}

fn candidate_timeout(custom_count: usize) -> Result<Duration, RelayTransportError> {
    let custom_allowance = Duration::from_millis(
        u64::try_from(custom_count)
            .map_err(|_| RelayTransportError::SourceResourceLimit)?
            .checked_mul(250)
            .ok_or(RelayTransportError::SourceResourceLimit)?,
    );
    Ok(TRUST_CANDIDATE_BASE_TIMEOUT
        .checked_add(custom_allowance)
        .unwrap_or(TRUST_CANDIDATE_MAX_TIMEOUT)
        .min(TRUST_CANDIDATE_MAX_TIMEOUT))
}

#[derive(Clone)]
struct OperationCancellation {
    operation: CancellationToken,
    shutdown: CancellationToken,
}

impl AccessCancellation for OperationCancellation {
    fn is_cancelled(&self) -> bool {
        self.operation.is_cancelled() || self.shutdown.is_cancelled()
    }

    fn cancelled(&self) -> BoxAccessFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                () = self.operation.cancelled() => {}
                () = self.shutdown.cancelled() => {}
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AccessFlowRelayConfig, AccessFlowRelayPresentation, AccessFlowRelayRoute,
        AccessFlowRelayTransport,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    const TEST_ROOT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
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

    fn insecure_workspace() -> TlsTrustLoadWorkspace {
        let generation = TlsTrustGeneration::new(1).unwrap();
        let plan = TlsClientTrustPlan::new(TlsClientTrustMode::Insecure, None).unwrap();
        let limits = TlsTrustLoadLimits::new(TEST_TLS_TRUST_BUDGET_BYTES, 8).unwrap();
        TlsTrustLoadWorkspace::new(generation, [plan], limits).unwrap()
    }

    #[test]
    fn candidate_deadline_is_bounded_and_scales_with_custom_sources() {
        assert_eq!(candidate_timeout(0).unwrap(), Duration::from_secs(60));
        assert_eq!(candidate_timeout(16).unwrap(), Duration::from_secs(64));
        assert_eq!(candidate_timeout(512).unwrap(), Duration::from_secs(180));
    }

    #[tokio::test]
    async fn expired_candidate_does_not_start_a_blocking_operation() {
        let started = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();
        let failure = match supervise_workspace_operation(
            insecure_workspace(),
            WorkspaceOperation::Mark(Arc::clone(&started)),
            Instant::now(),
            &shutdown,
        )
        .await
        {
            Ok(_) => panic!("expired candidate deadline was accepted"),
            Err(failure) => failure,
        };
        let GenerationLoadFailure::Error(RelayTransportError::Cancelled) = failure else {
            panic!("expired candidate returned the wrong failure");
        };
        assert!(!started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn timed_out_noninterruptible_operation_returns_an_owned_watcher() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let shutdown = CancellationToken::new();
        let failure = match supervise_workspace_operation(
            insecure_workspace(),
            WorkspaceOperation::Block(Arc::clone(&barrier)),
            Instant::now() + Duration::from_millis(20),
            &shutdown,
        )
        .await
        {
            Ok(_) => panic!("noninterruptible operation did not time out"),
            Err(failure) => failure,
        };
        let GenerationLoadFailure::TimedOut(watcher) = failure else {
            panic!("noninterruptible operation returned the wrong failure");
        };
        barrier.wait();
        watcher.await.unwrap();
    }

    #[tokio::test]
    async fn close_cancellation_is_observed_and_cooperative_worker_is_joined() {
        let started = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();
        let operation = supervise_workspace_operation(
            insecure_workspace(),
            WorkspaceOperation::WaitForCancellation {
                started: Arc::clone(&started),
                observed: Arc::clone(&observed),
            },
            Instant::now() + Duration::from_secs(1),
            &shutdown,
        );
        tokio::pin!(operation);
        tokio::select! {
            _ = &mut operation => panic!("cooperative worker ended before cancellation"),
            () = async {
                while !started.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
        shutdown.cancel();
        let failure = operation.await.unwrap_err();
        assert!(matches!(
            failure,
            GenerationLoadFailure::Error(RelayTransportError::Cancelled)
        ));
        assert!(observed.load(Ordering::Acquire));
    }

    fn insecure_plan() -> AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint> {
        let config = AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "2s".into(),
            max_connections: 8,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_IDENTITY_TOKEN".into(),
            },
            routes: vec![AccessFlowRelayRoute {
                name: "https".into(),
                listen: "127.0.0.1:3129".into(),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: "proxy.example.test:7443".into(),
                    server_name: "proxy.example.test".into(),
                    trust: TlsClientTrustMode::Insecure,
                    ca_certificate: None,
                },
            }],
        };
        config
            .compile_with_presentation(
                crate::config::AccessFlowRelayValidationMode::Agent,
                access_identity::IdentityPresentation::Bearer(
                    access_identity::SensitiveBearer::new(b"abcdefghijklmnopqrstuvwxyzABCDEF")
                        .unwrap(),
                ),
                None,
            )
            .unwrap()
            .plan
    }

    fn tls_plan(
        modes: &[(TlsClientTrustMode, Option<String>)],
    ) -> AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint> {
        let routes = modes
            .iter()
            .enumerate()
            .map(|(index, (trust, ca_certificate))| AccessFlowRelayRoute {
                name: format!("tls-{index}"),
                listen: format!("127.0.0.1:{}", 32000 + index),
                allowed_destination_ports: vec![443],
                transport: AccessFlowRelayTransport::TlsTcp {
                    address: "127.0.0.1:7443".into(),
                    server_name: "proxy.example.test".into(),
                    trust: *trust,
                    ca_certificate: ca_certificate.clone(),
                },
            })
            .collect();
        AccessFlowRelayConfig {
            setup_timeout: "2s".into(),
            drain_timeout: "2s".into(),
            max_connections: 8,
            copy_buffer_bytes_per_direction: 4096,
            start_after_services: Vec::new(),
            presentation: AccessFlowRelayPresentation::BearerEnvironment {
                variable: "AW_IDENTITY_TOKEN".into(),
            },
            routes,
        }
        .compile_with_presentation(
            crate::config::AccessFlowRelayValidationMode::Agent,
            access_identity::IdentityPresentation::Bearer(
                access_identity::SensitiveBearer::new(b"abcdefghijklmnopqrstuvwxyzABCDEF").unwrap(),
            ),
            None,
        )
        .unwrap()
        .plan
    }

    #[test]
    fn system_only_one_plan_has_exact_design_peak() {
        let plan = tls_plan(&[(TlsClientTrustMode::System, None)]);
        let reserve = RelayTransportRuntime::resource_reserve(&plan).unwrap();
        assert_eq!(reserve.memory_bytes, 58_855_424);
        let accepted =
            RelayTransportRuntime::prepare(&plan, Arc::new(AtomicBool::new(false))).unwrap();
        accepted
            .configure_trust_budget(RelayTransportTrustBudget {
                descriptors: reserve.descriptors,
                memory_bytes: reserve.memory_bytes,
            })
            .unwrap();
        let rejected =
            RelayTransportRuntime::prepare(&plan, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(
            rejected.configure_trust_budget(RelayTransportTrustBudget {
                descriptors: reserve.descriptors,
                memory_bytes: reserve.memory_bytes - 1,
            }),
            Err(RelayTransportError::SourceResourceLimit)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn system_and_composite_modes_activate_reload_and_mix_atomically() {
        let dir = tempfile::Builder::new()
            .prefix(".relay-shared-trust-modes-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let trust_path = dir.path().join("roots.pem");
        std::fs::write(&trust_path, TEST_ROOT_PEM).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let trust_path = trust_path.display().to_string();

        for modes in [
            vec![(TlsClientTrustMode::System, None)],
            vec![(
                TlsClientTrustMode::SystemPlusCustom,
                Some(trust_path.clone()),
            )],
            vec![
                (TlsClientTrustMode::System, None),
                (TlsClientTrustMode::Custom, Some(trust_path.clone())),
                (
                    TlsClientTrustMode::SystemPlusCustom,
                    Some(trust_path.clone()),
                ),
                (TlsClientTrustMode::Insecure, None),
            ],
        ] {
            let readiness = Arc::new(AtomicBool::new(false));
            let runtime =
                RelayTransportRuntime::activate(&tls_plan(&modes), Arc::clone(&readiness))
                    .await
                    .unwrap();
            let initial = runtime.inner.healthy_generation().unwrap();
            reload(&runtime).await.unwrap();
            let reloaded = runtime.inner.healthy_generation().unwrap();
            assert_ne!(initial.id, reloaded.id);
            assert!(readiness.load(Ordering::Acquire));
            assert_eq!(reloaded.tls_endpoints.len(), modes.len());
        }
    }

    async fn reload(runtime: &RelayTransportRuntime) -> Result<(), RelayTransportError> {
        runtime
            .begin_reload()?
            .expect("TLS trust reload")
            .complete()
            .await
    }

    #[tokio::test]
    async fn insecure_only_reload_stays_ready_and_publishes_a_new_generation() {
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&insecure_plan(), Arc::clone(&readiness))
            .await
            .unwrap();
        let initial = runtime.inner.healthy_generation().unwrap();
        let mut pending = runtime.begin_reload().unwrap().unwrap();
        assert!(readiness.load(Ordering::Acquire));
        pending.wait().await.unwrap();
        assert!(readiness.load(Ordering::Acquire));
        let current = runtime.inner.healthy_generation().unwrap();
        assert_ne!(initial.id, current.id);
    }

    #[tokio::test]
    async fn eight_retained_generations_reject_then_recover_after_retirement() {
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&insecure_plan(), Arc::clone(&readiness))
            .await
            .unwrap();
        let mut leases = vec![runtime.inner.healthy_generation().unwrap()];
        for _ in 1..MAX_PUBLISHED_TRUST_GENERATIONS {
            reload(&runtime).await.unwrap();
            leases.push(runtime.inner.healthy_generation().unwrap());
        }
        assert_eq!(
            reload(&runtime).await,
            Err(RelayTransportError::TrustGenerationLimit)
        );
        assert!(readiness.load(Ordering::Acquire));
        leases.remove(0);
        reload(&runtime).await.unwrap();
        assert!(readiness.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn startup_blocked_operation_keeps_not_ready_and_excludes_activation() {
        let readiness = Arc::new(AtomicBool::new(false));
        let plan = insecure_plan();
        let runtime = RelayTransportRuntime::prepare(&plan, Arc::clone(&readiness)).unwrap();
        runtime
            .configure_trust_budget(RelayTransportTrustBudget {
                descriptors: TEST_TLS_TRUST_DESCRIPTOR_BUDGET,
                memory_bytes: TEST_TLS_TRUST_BUDGET_BYTES,
            })
            .unwrap();
        let (release, wait_for_release) = tokio::sync::oneshot::channel::<()>();
        runtime.install_test_blocked_operation(tokio::spawn(async move {
            let _ = wait_for_release.await;
        }));
        assert_eq!(
            runtime.activate_prepared().await,
            Err(RelayTransportError::ReloadInProgress)
        );
        assert!(!readiness.load(Ordering::Acquire));
        assert_eq!(
            runtime.activate_prepared().await,
            Err(RelayTransportError::ReloadInProgress)
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.reload_blocked() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        runtime.activate_prepared().await.unwrap();
        assert!(readiness.load(Ordering::Acquire));
    }
}
