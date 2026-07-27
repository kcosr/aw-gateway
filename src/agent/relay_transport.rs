use crate::config::CompiledAccessFlowRelayEndpoint;
use access_async_contracts::{AccessCancellation, BoxAccessFuture};
use access_flow_relay::{
    AccessFlowChannelFailure, AccessFlowChannelResourceCost, AccessFlowConnectContext,
    AccessFlowConnector, AccessFlowRelayPlan,
};
use access_flow_tls::{
    GENERATION_CONTROL_BYTES, MAX_DER_CERTIFICATE_BYTES, MAX_TRUST_ANCHOR_BYTES, MAX_TRUST_ANCHORS,
    TlsAccessFlowClientEndpoint, TlsAccessFlowClientLimits, TlsAccessFlowConnector,
    TlsAccessFlowGeneration, TlsAccessFlowPreparedClientEndpoint, TlsAccessFlowStream,
    TlsAccessFlowTrust,
};
use access_flow_unix::{UnixAccessFlowConnector, UnixAccessFlowStream};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;

const CERTIFICATE_BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
const CERTIFICATE_END: &[u8] = b"-----END CERTIFICATE-----";
const MAX_TRUST_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_TRUST_PATH_BYTES: usize = 4096;
const MAX_TRUST_PATH_COMPONENTS: usize = 256;
const TRUST_LOAD_DESCRIPTOR_ENVELOPE: u64 = 2;

/// Fixed, path-free product transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RelayTransportError {
    Unavailable,
    UntrustedSource,
    ResourceLimit,
    InvalidMaterial,
    InvalidPlan,
    ReloadInProgress,
    Coordinator,
    ShuttingDown,
}

impl fmt::Display for RelayTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "Access Flow trust source is unavailable",
            Self::UntrustedSource => "Access Flow trust source is not trusted",
            Self::ResourceLimit => "Access Flow trust source exceeds a resource bound",
            Self::InvalidMaterial => "Access Flow trust material is invalid",
            Self::InvalidPlan => "Access Flow transport plan is invalid",
            Self::ReloadInProgress => "Access Flow transport reload is already in progress",
            Self::Coordinator => "Access Flow transport coordinator failed",
            Self::ShuttingDown => "Access Flow transport is shutting down",
        })
    }
}

impl Error for RelayTransportError {}

#[derive(Clone)]
struct TlsRouteSource {
    address: access_flow_tls::TlsAccessFlowAddress,
    server_name: access_flow_tls::TlsAccessFlowServerName,
    trust_path: PathBuf,
}

struct RelayTransportGeneration {
    id: TlsAccessFlowGeneration,
    tls_endpoints: Box<[TlsAccessFlowPreparedClientEndpoint]>,
}

#[derive(Clone)]
enum RelayTransportState {
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
    publication: Mutex<()>,
    shutdown_started: AtomicBool,
    unix: UnixAccessFlowConnector,
    tls: TlsAccessFlowConnector,
}

/// Product-owned lifecycle and atomic trust-generation coordinator.
#[derive(Clone)]
pub(super) struct RelayTransportRuntime {
    inner: Arc<RelayTransportInner>,
}

impl RelayTransportRuntime {
    pub(super) async fn activate(
        plan: &AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
        readiness: Arc<AtomicBool>,
    ) -> Result<Self, RelayTransportError> {
        readiness.store(false, Ordering::Release);
        let sources = collect_tls_sources(plan)?;
        let initial = load_generation(sources.clone(), 1).await?;
        let (state, _) = watch::channel(RelayTransportState::Healthy(Arc::new(initial)));
        let runtime = Self {
            inner: Arc::new(RelayTransportInner {
                sources: sources.into_boxed_slice(),
                state,
                readiness,
                next_generation: AtomicU64::new(2),
                reload_in_progress: AtomicBool::new(false),
                publication: Mutex::new(()),
                shutdown_started: AtomicBool::new(false),
                unix: UnixAccessFlowConnector::new(),
                tls: TlsAccessFlowConnector::with_system_resolver(
                    TlsAccessFlowClientLimits::default(),
                ),
            }),
        };
        runtime.inner.readiness.store(true, Ordering::Release);
        Ok(runtime)
    }

    pub(super) fn connector(&self) -> RelayTransportConnector {
        RelayTransportConnector {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(super) fn reload_descriptor_reserve(
        plan: &AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint>,
    ) -> Result<u64, RelayTransportError> {
        if collect_tls_sources(plan)?.is_empty() {
            Ok(0)
        } else {
            Ok(TRUST_LOAD_DESCRIPTOR_ENVELOPE)
        }
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
        self.inner.preserve_generation_as_unhealthy();
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
                return Err(RelayTransportError::ResourceLimit);
            }
        };
        let sources = self.inner.sources.to_vec();
        let worker = tokio::task::spawn_blocking(move || {
            worker_started();
            load_generation_blocking(&sources, generation)
        });
        Ok(Some(PendingRelayTransportReload {
            inner: Arc::clone(&self.inner),
            worker: Some(worker),
        }))
    }

    #[cfg(test)]
    fn healthy(&self) -> bool {
        self.inner.readiness.load(Ordering::Acquire)
    }

    pub(super) fn close(&self) {
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
    worker: Option<tokio::task::JoinHandle<Result<RelayTransportGeneration, RelayTransportError>>>,
}

impl PendingRelayTransportReload {
    /// Joins and publishes this reload. Cancelling the wait leaves the worker owned here.
    pub(super) async fn wait(&mut self) -> Result<(), RelayTransportError> {
        let loaded = {
            let worker = self
                .worker
                .as_mut()
                .ok_or(RelayTransportError::Coordinator)?;
            worker
                .await
                .unwrap_or(Err(RelayTransportError::Coordinator))
        };
        self.worker = None;
        self.publish(loaded)
    }

    /// Final join used by terminal shutdown after the runtime is closed.
    pub(super) async fn complete(mut self) -> Result<(), RelayTransportError> {
        self.wait().await
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
                Ok(generation) => {
                    self.inner
                        .publish_generation_as_healthy(Arc::new(generation));
                    Ok(())
                }
                Err(error) => Err(error),
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
    fn preserve_generation_as_unhealthy(&self) {
        self.preserve_generation_as_unhealthy_with(|| {});
    }

    fn preserve_generation_as_unhealthy_with(&self, after_readiness: impl FnOnce()) {
        let current = match self.state.borrow().clone() {
            RelayTransportState::Healthy(generation)
            | RelayTransportState::Unhealthy(generation) => generation,
            RelayTransportState::Closed => return,
        };
        self.readiness.store(false, Ordering::Release);
        after_readiness();
        self.state
            .send_replace(RelayTransportState::Unhealthy(current));
    }

    fn publish_generation_as_healthy(&self, generation: Arc<RelayTransportGeneration>) {
        self.publish_generation_as_healthy_with(generation, || {});
    }

    fn publish_generation_as_healthy_with(
        &self,
        generation: Arc<RelayTransportGeneration>,
        after_state: impl FnOnce(),
    ) {
        self.state
            .send_replace(RelayTransportState::Healthy(generation));
        after_state();
        self.readiness.store(true, Ordering::Release);
    }

    fn publish_closed(&self) {
        self.publish_closed_with(|| {});
    }

    fn publish_closed_with(&self, after_readiness: impl FnOnce()) {
        self.readiness.store(false, Ordering::Release);
        after_readiness();
        self.state.send_replace(RelayTransportState::Closed);
    }

    fn healthy_generation(&self) -> Option<Arc<RelayTransportGeneration>> {
        match self.state.borrow().clone() {
            RelayTransportState::Healthy(generation) => Some(generation),
            RelayTransportState::Unhealthy(_) | RelayTransportState::Closed => None,
        }
    }

    fn retained_generation(&self) -> Option<Arc<RelayTransportGeneration>> {
        match self.state.borrow().clone() {
            RelayTransportState::Healthy(generation)
            | RelayTransportState::Unhealthy(generation) => Some(generation),
            RelayTransportState::Closed => None,
        }
    }

    fn generation_is_current(&self, expected: TlsAccessFlowGeneration) -> bool {
        matches!(
            &*self.state.borrow(),
            RelayTransportState::Healthy(generation) if generation.id == expected
        )
    }
}

/// Exact product connector over the mutually exclusive Unix and TLS adapters.
#[derive(Clone)]
pub(super) struct RelayTransportConnector {
    inner: Arc<RelayTransportInner>,
}

impl fmt::Debug for RelayTransportConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayTransportConnector")
    }
}

/// Product stream sum that does not expose either physical adapter upstream.
pub(super) enum RelayTransportStream {
    Unix(UnixAccessFlowStream),
    Tls(TlsAccessFlowStream),
}

impl fmt::Debug for RelayTransportStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unix(_) => "RelayTransportStream::Unix",
            Self::Tls(_) => "RelayTransportStream::Tls",
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
            Self::Tls(stream) => Pin::new(stream).poll_read(context, buffer),
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
            Self::Tls(stream) => Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(context),
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
                        .map(RelayTransportStream::Tls)
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
            CompiledAccessFlowRelayEndpoint::TlsTcp { tls_index, .. } => {
                let generation = self
                    .inner
                    .retained_generation()
                    .ok_or(AccessFlowChannelFailure::Unavailable)?;
                let endpoint = generation
                    .tls_endpoints
                    .get(*tls_index)
                    .ok_or(AccessFlowChannelFailure::InvalidEndpoint)?;
                let base = self.inner.tls.resource_projection(endpoint)?;
                let maximum_generation = (MAX_TRUST_ANCHOR_BYTES as u64)
                    .checked_add(GENERATION_CONTROL_BYTES)
                    .ok_or(AccessFlowChannelFailure::ResourceExhausted)?;
                let candidate_bytes = maximum_generation
                    .checked_add(MAX_TRUST_SOURCE_BYTES as u64)
                    .ok_or(AccessFlowChannelFailure::ResourceExhausted)?;
                AccessFlowChannelResourceCost::new(
                    base.retained_endpoint_bytes
                        .checked_add(candidate_bytes)
                        .ok_or(AccessFlowChannelFailure::ResourceExhausted)?,
                    base.connecting_descriptors,
                    base.active_descriptors,
                    base.connecting_bytes,
                    base.active_bytes
                        .checked_add(maximum_generation)
                        .ok_or(AccessFlowChannelFailure::ResourceExhausted)?,
                )
            }
        }
    }
}

struct GenerationCancellation<'a> {
    expected: TlsAccessFlowGeneration,
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
            trust_path,
        } = route.endpoint()
        else {
            continue;
        };
        let source = TlsRouteSource {
            address: address.clone(),
            server_name: server_name.clone(),
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

async fn load_generation(
    sources: Vec<TlsRouteSource>,
    generation: u64,
) -> Result<RelayTransportGeneration, RelayTransportError> {
    tokio::task::spawn_blocking(move || load_generation_blocking(&sources, generation))
        .await
        .map_err(|_| RelayTransportError::Coordinator)?
}

fn load_generation_blocking(
    sources: &[TlsRouteSource],
    generation: u64,
) -> Result<RelayTransportGeneration, RelayTransportError> {
    load_generation_blocking_with_hook(sources, generation, |_| {})
}

fn load_generation_blocking_with_hook(
    sources: &[TlsRouteSource],
    generation: u64,
    mut after_source: impl FnMut(usize),
) -> Result<RelayTransportGeneration, RelayTransportError> {
    let id =
        TlsAccessFlowGeneration::new(generation).map_err(|_| RelayTransportError::ResourceLimit)?;
    let mut loaded_sources = BTreeMap::<PathBuf, LoadedTrustSource>::new();
    for source in sources {
        if !loaded_sources.contains_key(&source.trust_path) {
            let loaded = load_trust_source(&source.trust_path)?;
            loaded_sources.insert(source.trust_path.clone(), loaded);
            after_source(loaded_sources.len() - 1);
        }
    }
    for (path, source) in &loaded_sources {
        source.revalidate(path)?;
    }
    let mut endpoints = Vec::with_capacity(sources.len());
    for source in sources {
        let anchors = loaded_sources
            .get(&source.trust_path)
            .ok_or(RelayTransportError::Coordinator)?
            .anchors
            .clone();
        let trust = TlsAccessFlowTrust::new(id, anchors).map_err(map_trust_contract_error)?;
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
        id,
        tls_endpoints: endpoints.into_boxed_slice(),
    })
}

fn map_trust_contract_error(
    error: access_flow_tls::TlsAccessFlowContractError,
) -> RelayTransportError {
    use access_flow_tls::TlsAccessFlowContractError;
    match error {
        TlsAccessFlowContractError::CertificateCountExceeded
        | TlsAccessFlowContractError::CertificateSizeExceeded
        | TlsAccessFlowContractError::CertificateAggregateExceeded
        | TlsAccessFlowContractError::ResourceOverflow => RelayTransportError::ResourceLimit,
        _ => RelayTransportError::InvalidMaterial,
    }
}

#[cfg(unix)]
struct LoadedTrustSource {
    anchors: Vec<Vec<u8>>,
    snapshot: StableMetadata,
}

#[cfg(unix)]
impl LoadedTrustSource {
    fn revalidate(&self, path: &Path) -> Result<(), RelayTransportError> {
        let ((), current) = with_secure_trust_file(path, |_| Ok(()))?;
        if current != self.snapshot {
            return Err(RelayTransportError::UntrustedSource);
        }
        Ok(())
    }
}

#[cfg(not(unix))]
struct LoadedTrustSource {
    anchors: Vec<Vec<u8>>,
}

#[cfg(not(unix))]
impl LoadedTrustSource {
    fn revalidate(&self, _path: &Path) -> Result<(), RelayTransportError> {
        Err(RelayTransportError::Unavailable)
    }
}

#[cfg(test)]
fn load_trust_anchors(path: &Path) -> Result<Vec<Vec<u8>>, RelayTransportError> {
    let loaded = load_trust_source(path)?;
    loaded.revalidate(path)?;
    Ok(loaded.anchors)
}

#[cfg(all(test, unix))]
fn load_trust_anchors_with_hook(
    path: &Path,
    after_read: impl FnOnce(),
) -> Result<Vec<Vec<u8>>, RelayTransportError> {
    let loaded = load_trust_source_with_hook(path, after_read)?;
    loaded.revalidate(path)?;
    Ok(loaded.anchors)
}

fn load_trust_source(path: &Path) -> Result<LoadedTrustSource, RelayTransportError> {
    load_trust_source_with_hook(path, || {})
}

#[cfg(unix)]
fn load_trust_source_with_hook(
    path: &Path,
    after_read: impl FnOnce(),
) -> Result<LoadedTrustSource, RelayTransportError> {
    use std::io::Read as _;

    let (bytes, snapshot) = with_secure_trust_file(path, move |file| {
        let opened_size = usize::try_from(
            file.metadata()
                .map_err(|_| RelayTransportError::Unavailable)?
                .len(),
        )
        .map_err(|_| RelayTransportError::ResourceLimit)?;
        let read_limit = MAX_TRUST_SOURCE_BYTES
            .checked_add(1)
            .ok_or(RelayTransportError::ResourceLimit)?;
        let mut bytes = Vec::with_capacity(opened_size.min(MAX_TRUST_SOURCE_BYTES));
        file.by_ref()
            .take(read_limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| RelayTransportError::Unavailable)?;
        if bytes.len() > MAX_TRUST_SOURCE_BYTES {
            return Err(RelayTransportError::ResourceLimit);
        }
        after_read();
        if bytes.len() != opened_size {
            return Err(RelayTransportError::UntrustedSource);
        }
        Ok(bytes)
    })?;
    Ok(LoadedTrustSource {
        anchors: parse_trust_pem(&bytes)?,
        snapshot,
    })
}

#[cfg(unix)]
fn with_secure_trust_file<T>(
    path: &Path,
    operation: impl FnOnce(&mut std::fs::File) -> Result<T, RelayTransportError>,
) -> Result<(T, StableMetadata), RelayTransportError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    if path.as_os_str().as_bytes().len() > MAX_TRUST_PATH_BYTES {
        return Err(RelayTransportError::ResourceLimit);
    }
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(RelayTransportError::UntrustedSource);
    }
    let names = components
        .map(|component| match component {
            Component::Normal(name) => {
                CString::new(name.as_bytes()).map_err(|_| RelayTransportError::UntrustedSource)
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => Err(RelayTransportError::UntrustedSource),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if names.len() > MAX_TRUST_PATH_COMPONENTS {
        return Err(RelayTransportError::ResourceLimit);
    }
    let (leaf, ancestors) = names
        .split_last()
        .ok_or(RelayTransportError::UntrustedSource)?;
    let effective_uid = unsafe { libc::geteuid() };
    let root = CString::new("/").expect("filesystem root contains no NUL");
    let root_fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    let mut directory = owned_fd(root_fd)?;
    verify_trusted_directory(directory.as_raw_fd(), effective_uid)?;
    for ancestor in ancestors {
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                ancestor.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        let next = owned_fd(descriptor)?;
        verify_trusted_directory(next.as_raw_fd(), effective_uid)?;
        directory = next;
    }

    let probed = probe_leaf(directory.as_raw_fd(), leaf)?;
    verify_trust_file(&probed, effective_uid)?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    let descriptor: OwnedFd = owned_fd(descriptor)?;
    let opened = descriptor_metadata(descriptor.as_raw_fd())?;
    verify_trust_file(&opened, effective_uid)?;
    if object_metadata(&opened) != object_metadata(&probed) {
        return Err(RelayTransportError::UntrustedSource);
    }
    let mut file = std::fs::File::from(descriptor);
    let opened_stable = stable_metadata(&file)?;
    let result = operation(&mut file)?;
    let post = descriptor_metadata(file.as_raw_fd())?;
    verify_trust_file(&post, effective_uid)?;
    let final_probe = probe_leaf(directory.as_raw_fd(), leaf)?;
    if stable_metadata(&file)? != opened_stable
        || object_metadata(&post) != object_metadata(&opened)
        || object_metadata(&final_probe) != object_metadata(&opened)
    {
        return Err(RelayTransportError::UntrustedSource);
    }
    Ok((result, opened_stable))
}

#[cfg(not(unix))]
fn load_trust_source_with_hook(
    _path: &Path,
    _after_read: impl FnOnce(),
) -> Result<LoadedTrustSource, RelayTransportError> {
    Err(RelayTransportError::Unavailable)
}

#[cfg(unix)]
fn owned_fd(descriptor: libc::c_int) -> Result<std::os::fd::OwnedFd, RelayTransportError> {
    use std::os::fd::FromRawFd as _;
    if descriptor < 0 {
        return Err(RelayTransportError::Unavailable);
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn descriptor_metadata(descriptor: std::os::fd::RawFd) -> Result<libc::stat, RelayTransportError> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(RelayTransportError::Unavailable);
    }
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(unix)]
fn probe_leaf(
    directory: std::os::fd::RawFd,
    leaf: &std::ffi::CStr,
) -> Result<libc::stat, RelayTransportError> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory,
            leaf.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(RelayTransportError::Unavailable);
    }
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(unix)]
fn verify_trusted_directory(
    descriptor: std::os::fd::RawFd,
    effective_uid: libc::uid_t,
) -> Result<(), RelayTransportError> {
    let metadata = descriptor_metadata(descriptor)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR
        || !trusted_owner(metadata.st_uid, effective_uid)
        || metadata.st_mode & 0o022 != 0
    {
        return Err(RelayTransportError::UntrustedSource);
    }
    Ok(())
}

#[cfg(unix)]
fn verify_trust_file(
    metadata: &libc::stat,
    effective_uid: libc::uid_t,
) -> Result<(), RelayTransportError> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || !trusted_owner(metadata.st_uid, effective_uid)
        || metadata.st_mode & 0o022 != 0
        || metadata.st_nlink != 1
    {
        return Err(RelayTransportError::UntrustedSource);
    }
    let size = usize::try_from(metadata.st_size).map_err(|_| RelayTransportError::ResourceLimit)?;
    if size == 0 {
        return Err(RelayTransportError::InvalidMaterial);
    }
    if size > MAX_TRUST_SOURCE_BYTES {
        return Err(RelayTransportError::ResourceLimit);
    }
    Ok(())
}

#[cfg(unix)]
const fn trusted_owner(owner: libc::uid_t, effective_uid: libc::uid_t) -> bool {
    owner == 0 || owner == effective_uid
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct ObjectMetadata {
    device: libc::dev_t,
    inode: libc::ino_t,
    mode: libc::mode_t,
    owner: libc::uid_t,
    links: libc::nlink_t,
    size: libc::off_t,
}

#[cfg(unix)]
fn object_metadata(metadata: &libc::stat) -> ObjectMetadata {
    ObjectMetadata {
        device: metadata.st_dev,
        inode: metadata.st_ino,
        mode: metadata.st_mode,
        owner: metadata.st_uid,
        links: metadata.st_nlink,
        size: metadata.st_size,
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct StableMetadata {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
fn stable_metadata(file: &std::fs::File) -> Result<StableMetadata, RelayTransportError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file
        .metadata()
        .map_err(|_| RelayTransportError::Unavailable)?;
    Ok(StableMetadata {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        links: metadata.nlink(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

fn parse_trust_pem(input: &[u8]) -> Result<Vec<Vec<u8>>, RelayTransportError> {
    let mut remaining = input;
    let mut anchors = Vec::new();
    while !remaining.trim_ascii().is_empty() {
        let (block, rest) = take_exact_pem_block(remaining)?;
        let (label, der) =
            pem_rfc7468::decode_vec(block).map_err(|_| RelayTransportError::InvalidMaterial)?;
        if label != "CERTIFICATE" {
            return Err(RelayTransportError::InvalidMaterial);
        }
        if der.is_empty() {
            return Err(RelayTransportError::InvalidMaterial);
        }
        if der.len() > MAX_DER_CERTIFICATE_BYTES {
            return Err(RelayTransportError::ResourceLimit);
        }
        anchors.push(der);
        if anchors.len() > MAX_TRUST_ANCHORS {
            return Err(RelayTransportError::ResourceLimit);
        }
        remaining = rest;
    }
    if anchors.is_empty() {
        return Err(RelayTransportError::InvalidMaterial);
    }
    Ok(anchors)
}

fn take_exact_pem_block(input: &[u8]) -> Result<(&[u8], &[u8]), RelayTransportError> {
    let trimmed = input.trim_ascii_start();
    if !trimmed.starts_with(CERTIFICATE_BEGIN) {
        return Err(RelayTransportError::InvalidMaterial);
    }
    let end_offset = trimmed
        .windows(CERTIFICATE_END.len())
        .position(|window| window == CERTIFICATE_END)
        .ok_or(RelayTransportError::InvalidMaterial)?;
    let block_end = end_offset
        .checked_add(CERTIFICATE_END.len())
        .ok_or(RelayTransportError::ResourceLimit)?;
    Ok((&trimmed[..block_end], &trimmed[block_end..]))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use access_flow_relay::{AccessFlowRoute, AccessFlowRouteName};
    use access_flow_tls::{TlsAccessFlowAddress, TlsAccessFlowServerName};
    use access_flow_unix::{NormalizedUnixSocketPath, UnixAccessFlowEndpoint};
    use access_identity::{IdentityPresentation, SensitiveBearer};
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::num::{NonZeroU16, NonZeroUsize};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
    use std::time::{Duration, Instant};

    const TEST_ROOT_PEM: &str = "\
-----BEGIN CERTIFICATE-----\n\
MIIBZDCCAQqgAwIBAgIUVeNPvjXF+esAR5JSWSDHPhHi8Z8wCgYIKoZIzj0EAwIw\n\
FzEVMBMGA1UEAwwMYWNsLXByb3h5IENBMCAXDTc1MDEwMTAwMDAwMFoYDzQwOTYw\n\
MTAxMDAwMDAwWjAXMRUwEwYDVQQDDAxhY2wtcHJveHkgQ0EwWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAASSq7ztpOLW2yTnbT6B7tdXn2E37SCt7/WeOajZV3mUDpvH\n\
lpLGD6uz16wTm75vtZ6aoLpTq7iE4pzTO9jOwftTozIwMDAdBgNVHQ4EFgQUKOgN\n\
bJ2u/7pEok6/UT9IFfajHPYwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNI\n\
ADBFAiB005pgAL7CLsHpHJFXEEgDG/fmG91oI1vRO/ZFSVufDQIhAOYNaysbiwJR\n\
c+E0ChYtUrWyHuDFX+/4kDlyJh3LeI70\n\
-----END CERTIFICATE-----\n";

    struct NeverCancelled;

    impl AccessCancellation for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }

        fn cancelled(&self) -> BoxAccessFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }

    fn trusted_tempdir() -> tempfile::TempDir {
        let home = std::env::var_os("HOME").expect("test user home directory");
        let directory = tempfile::Builder::new()
            .prefix(".relay-transport-test-")
            .tempdir_in(home)
            .expect("trusted temporary directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private temporary directory");
        directory
    }

    fn write_trust(directory: &Path, contents: &[u8]) -> PathBuf {
        let path = directory.join("trust.pem");
        std::fs::write(&path, contents).expect("write trust");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set trust mode");
        path
    }

    fn presentation() -> IdentityPresentation {
        IdentityPresentation::Bearer(
            SensitiveBearer::new(b"abcdefghijklmnopqrstuvwxyzABCDEF").expect("test bearer"),
        )
    }

    fn tls_plan(
        trust_path: PathBuf,
        address: &str,
    ) -> AccessFlowRelayPlan<CompiledAccessFlowRelayEndpoint> {
        let endpoint = CompiledAccessFlowRelayEndpoint::TlsTcp {
            tls_index: 0,
            address: TlsAccessFlowAddress::parse(address).expect("test TLS address"),
            server_name: TlsAccessFlowServerName::parse("localhost").expect("test server name"),
            trust_path,
        };
        AccessFlowRelayPlan::new(
            vec![
                AccessFlowRoute::new(
                    AccessFlowRouteName::new("tls").expect("route name"),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
                    vec![NonZeroU16::new(80).expect("port")],
                    endpoint,
                )
                .expect("TLS route"),
            ],
            presentation(),
            Duration::from_secs(5),
            NonZeroUsize::new(8).expect("connections"),
            NonZeroUsize::new(4096).expect("buffer"),
        )
        .expect("TLS plan")
    }

    fn tls_sources(first: PathBuf, second: PathBuf) -> Vec<TlsRouteSource> {
        [first, second]
            .into_iter()
            .enumerate()
            .map(|(index, trust_path)| TlsRouteSource {
                address: TlsAccessFlowAddress::parse(&format!("127.0.0.1:{}", 7443 + index))
                    .expect("test TLS address"),
                server_name: TlsAccessFlowServerName::parse("localhost").expect("test server name"),
                trust_path,
            })
            .collect()
    }

    fn unix_endpoint(path: &Path) -> CompiledAccessFlowRelayEndpoint {
        CompiledAccessFlowRelayEndpoint::Unix(UnixAccessFlowEndpoint::new(
            NormalizedUnixSocketPath::new(path.to_str().expect("UTF-8 path"))
                .expect("normalized Unix path"),
        ))
    }

    async fn complete_reload(runtime: &RelayTransportRuntime) -> Result<(), RelayTransportError> {
        match runtime.begin_reload()? {
            Some(pending) => pending.complete().await,
            None => Ok(()),
        }
    }

    #[test]
    fn strict_pem_accepts_certificates_and_rejects_junk_or_other_labels() {
        let anchors = parse_trust_pem(TEST_ROOT_PEM.as_bytes()).expect("valid root");
        assert_eq!(anchors.len(), 1);
        let invalid_inputs = [
            b"junk\n".to_vec(),
            b"-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n".to_vec(),
            format!("{TEST_ROOT_PEM}junk").into_bytes(),
            b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".to_vec(),
        ];
        for invalid in invalid_inputs {
            assert_eq!(
                parse_trust_pem(&invalid),
                Err(RelayTransportError::InvalidMaterial)
            );
        }
    }

    #[test]
    fn strict_pem_enforces_certificate_count_and_size() {
        let oversized = pem_rfc7468::encode_string(
            "CERTIFICATE",
            pem_rfc7468::LineEnding::LF,
            &vec![7_u8; MAX_DER_CERTIFICATE_BYTES + 1],
        )
        .expect("encode oversized certificate");
        assert_eq!(
            parse_trust_pem(oversized.as_bytes()),
            Err(RelayTransportError::ResourceLimit)
        );

        let mut over_count = String::new();
        for index in 0..=MAX_TRUST_ANCHORS {
            over_count.push_str(
                &pem_rfc7468::encode_string(
                    "CERTIFICATE",
                    pem_rfc7468::LineEnding::LF,
                    &[u8::try_from(index).expect("bounded index")],
                )
                .expect("encode certificate"),
            );
        }
        assert_eq!(
            parse_trust_pem(over_count.as_bytes()),
            Err(RelayTransportError::ResourceLimit)
        );
    }

    #[test]
    fn secure_loader_accepts_stable_single_link_public_trust() {
        let directory = trusted_tempdir();
        let path = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let anchors = load_trust_anchors(&path).expect("stable trust source");
        assert_eq!(anchors.len(), 1);
    }

    #[test]
    fn secure_loader_rejects_leaf_symlink_hardlink_and_writable_mode() {
        let directory = trusted_tempdir();
        let path = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let link = directory.path().join("link.pem");
        symlink(&path, &link).expect("trust symlink");
        assert!(matches!(
            load_trust_anchors(&link),
            Err(RelayTransportError::Unavailable | RelayTransportError::UntrustedSource)
        ));

        let hardlink = directory.path().join("hardlink.pem");
        std::fs::hard_link(&path, &hardlink).expect("trust hard link");
        assert_eq!(
            load_trust_anchors(&path),
            Err(RelayTransportError::UntrustedSource)
        );
        std::fs::remove_file(&hardlink).expect("remove hard link");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("unsafe trust mode");
        assert_eq!(
            load_trust_anchors(&path),
            Err(RelayTransportError::UntrustedSource)
        );
    }

    #[test]
    fn secure_loader_rejects_symlinked_or_writable_ancestor() {
        let directory = trusted_tempdir();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).expect("real directory");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let trust = write_trust(&real, TEST_ROOT_PEM.as_bytes());
        let link = directory.path().join("linked");
        symlink(&real, &link).expect("ancestor symlink");
        assert!(matches!(
            load_trust_anchors(&link.join("trust.pem")),
            Err(RelayTransportError::Unavailable | RelayTransportError::UntrustedSource)
        ));

        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o777))
            .expect("unsafe ancestor mode");
        assert_eq!(
            load_trust_anchors(&trust),
            Err(RelayTransportError::UntrustedSource)
        );
    }

    #[test]
    fn secure_loader_detects_same_descriptor_mutation() {
        let directory = trusted_tempdir();
        let path = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let mutation_path = path.clone();
        assert_eq!(
            load_trust_anchors_with_hook(&path, move || {
                let mut bytes = TEST_ROOT_PEM.as_bytes().to_vec();
                bytes[40] ^= 1;
                std::fs::write(mutation_path, bytes).expect("mutate trust");
            }),
            Err(RelayTransportError::UntrustedSource)
        );
    }

    #[test]
    fn secure_loader_enforces_raw_source_bound_and_regular_type() {
        let directory = trusted_tempdir();
        let oversized = write_trust(directory.path(), &vec![b' '; MAX_TRUST_SOURCE_BYTES + 1]);
        assert_eq!(
            load_trust_anchors(&oversized),
            Err(RelayTransportError::ResourceLimit)
        );
        assert_eq!(
            load_trust_anchors(directory.path()),
            Err(RelayTransportError::UntrustedSource)
        );
    }

    #[test]
    fn secure_loader_enforces_path_memory_and_component_bounds() {
        let oversized = PathBuf::from(format!("/{}", "a".repeat(MAX_TRUST_PATH_BYTES)));
        assert_eq!(
            load_trust_anchors(&oversized),
            Err(RelayTransportError::ResourceLimit)
        );

        let too_deep = PathBuf::from(format!(
            "/{}",
            std::iter::repeat_n("a", MAX_TRUST_PATH_COMPONENTS + 1)
                .collect::<Vec<_>>()
                .join("/")
        ));
        assert_eq!(
            load_trust_anchors(&too_deep),
            Err(RelayTransportError::ResourceLimit)
        );
    }

    #[test]
    fn generation_revalidates_every_source_after_all_sources_are_read() {
        let directory = trusted_tempdir();
        let first = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let second = directory.path().join("second.pem");
        std::fs::write(&second, TEST_ROOT_PEM).expect("write second trust");
        std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o644))
            .expect("set second trust mode");
        let mutation_path = first.clone();
        let sources = tls_sources(first, second);

        let result = load_generation_blocking_with_hook(&sources, 2, move |index| {
            if index == 1 {
                let mut still_valid = TEST_ROOT_PEM.as_bytes().to_vec();
                still_valid.push(b'\n');
                std::fs::write(&mutation_path, still_valid)
                    .expect("mutate first source after second read");
            }
        });

        assert!(matches!(result, Err(RelayTransportError::UntrustedSource)));
    }

    #[tokio::test]
    async fn failed_reload_is_atomic_unhealthy_and_later_reload_recovers() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let plan = tls_plan(trust.clone(), "127.0.0.1:7443");
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&plan, Arc::clone(&readiness))
            .await
            .expect("initial activation");
        assert!(runtime.healthy());
        assert!(readiness.load(Ordering::Acquire));
        let first = runtime
            .inner
            .healthy_generation()
            .expect("initial generation")
            .id;

        std::fs::write(&trust, b"invalid").expect("invalidate trust");
        assert_eq!(
            complete_reload(&runtime).await,
            Err(RelayTransportError::InvalidMaterial)
        );
        assert!(!runtime.healthy());
        assert!(!readiness.load(Ordering::Acquire));
        let retained = runtime
            .inner
            .retained_generation()
            .expect("retained generation");
        assert_eq!(retained.id, first);

        std::fs::write(&trust, TEST_ROOT_PEM).expect("restore trust");
        complete_reload(&runtime).await.expect("recover trust");
        assert!(runtime.healthy());
        assert!(readiness.load(Ordering::Acquire));
        assert_ne!(
            runtime
                .inner
                .healthy_generation()
                .expect("recovered generation")
                .id,
            first
        );
    }

    #[tokio::test]
    async fn unhealthy_state_rejects_unix_and_tls_without_fallback() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let plan = tls_plan(trust.clone(), "127.0.0.1:7443");
        let runtime = RelayTransportRuntime::activate(&plan, Arc::new(AtomicBool::new(false)))
            .await
            .expect("initial activation");
        std::fs::write(&trust, b"invalid").expect("invalidate trust");
        assert!(complete_reload(&runtime).await.is_err());
        let connector = runtime.connector();
        let cancellation = NeverCancelled;
        let context =
            AccessFlowConnectContext::new(Instant::now() + Duration::from_secs(1), &cancellation);
        assert!(matches!(
            connector
                .connect(
                    &unix_endpoint(&directory.path().join("missing.sock")),
                    context
                )
                .await,
            Err(AccessFlowChannelFailure::Unavailable)
        ));
        let context =
            AccessFlowConnectContext::new(Instant::now() + Duration::from_secs(1), &cancellation);
        assert!(matches!(
            connector
                .connect(plan.routes()[0].endpoint(), context)
                .await,
            Err(AccessFlowChannelFailure::Unavailable)
        ));
    }

    #[tokio::test]
    async fn synchronous_reload_gate_is_fail_closed_and_clone_can_recover() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let plan = tls_plan(trust, "127.0.0.1:7443");
        assert_eq!(
            RelayTransportRuntime::reload_descriptor_reserve(&plan),
            Ok(TRUST_LOAD_DESCRIPTOR_ENVELOPE)
        );
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&plan, Arc::clone(&readiness))
            .await
            .expect("initial activation");
        let initial = runtime
            .inner
            .healthy_generation()
            .expect("initial generation")
            .id;

        let pending = runtime
            .begin_reload()
            .expect("begin reload")
            .expect("TLS reload worker");
        assert!(!readiness.load(Ordering::Acquire));
        assert!(!runtime.healthy());
        assert_eq!(
            runtime
                .inner
                .retained_generation()
                .expect("retained generation")
                .id,
            initial
        );

        let owned = runtime.clone();
        tokio::spawn(async move { pending.complete().await })
            .await
            .expect("reload task")
            .expect("reload recovery");
        drop(owned);
        assert!(readiness.load(Ordering::Acquire));
        assert!(runtime.healthy());
    }

    #[tokio::test]
    async fn pending_reload_owns_started_worker_until_terminal_join() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let plan = tls_plan(trust, "127.0.0.1:7443");
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&plan, Arc::clone(&readiness))
            .await
            .expect("initial activation");
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let mut pending = runtime
            .begin_reload_with_hook(move || {
                started_tx.send(()).expect("report worker start");
                release_rx.recv().expect("release blocking worker");
            })
            .expect("begin reload")
            .expect("TLS reload worker");

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking worker actually started");
        assert!(!readiness.load(Ordering::Acquire));
        assert_eq!(
            runtime.begin_reload().unwrap_err(),
            RelayTransportError::ReloadInProgress
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pending.wait())
                .await
                .is_err(),
            "select-style wait remains pending while the worker is paused"
        );

        runtime.close();
        release_tx.send(()).expect("release blocking worker");
        assert_eq!(
            pending.complete().await,
            Err(RelayTransportError::ShuttingDown)
        );
        assert!(!readiness.load(Ordering::Acquire));
        assert!(matches!(
            &*runtime.inner.state.borrow(),
            RelayTransportState::Closed
        ));
    }

    #[tokio::test]
    async fn readiness_publication_orders_each_state_transition() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let plan = tls_plan(trust, "127.0.0.1:7443");
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&plan, Arc::clone(&readiness))
            .await
            .expect("initial activation");
        let generation = runtime
            .inner
            .healthy_generation()
            .expect("initial generation");

        runtime.inner.preserve_generation_as_unhealthy_with(|| {
            assert!(!readiness.load(Ordering::Acquire));
            assert!(matches!(
                &*runtime.inner.state.borrow(),
                RelayTransportState::Healthy(_)
            ));
        });
        assert!(matches!(
            &*runtime.inner.state.borrow(),
            RelayTransportState::Unhealthy(_)
        ));

        runtime
            .inner
            .publish_generation_as_healthy_with(generation, || {
                assert!(matches!(
                    &*runtime.inner.state.borrow(),
                    RelayTransportState::Healthy(_)
                ));
                assert!(!readiness.load(Ordering::Acquire));
            });
        assert!(readiness.load(Ordering::Acquire));

        runtime.inner.publish_closed_with(|| {
            assert!(!readiness.load(Ordering::Acquire));
            assert!(matches!(
                &*runtime.inner.state.borrow(),
                RelayTransportState::Healthy(_)
            ));
        });
        assert!(matches!(
            &*runtime.inner.state.borrow(),
            RelayTransportState::Closed
        ));
    }

    #[tokio::test]
    async fn failed_reload_cancels_tls_connect_already_in_progress() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("TCP listener");
        let address = listener.local_addr().expect("listener address");
        let plan = tls_plan(trust.clone(), &address.to_string());
        let runtime = RelayTransportRuntime::activate(&plan, Arc::new(AtomicBool::new(false)))
            .await
            .expect("initial activation");
        let connector = runtime.connector();
        let cancellation = NeverCancelled;
        let context =
            AccessFlowConnectContext::new(Instant::now() + Duration::from_secs(10), &cancellation);
        let connect = connector.connect(plan.routes()[0].endpoint(), context);
        tokio::pin!(connect);
        let accept = listener.accept();
        tokio::pin!(accept);
        let accepted_stream = tokio::select! {
            accepted = &mut accept => {
                accepted.expect("accepted TLS TCP").0
            }
            result = &mut connect => panic!("TLS setup finished before reload: {result:?}"),
        };

        std::fs::write(&trust, b"invalid").expect("invalidate trust");
        let pending = runtime
            .begin_reload()
            .expect("begin reload")
            .expect("TLS reload worker");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), &mut connect)
                .await
                .expect("generation cancellation"),
            Err(AccessFlowChannelFailure::Cancelled)
        ));
        assert_eq!(
            pending.complete().await,
            Err(RelayTransportError::InvalidMaterial)
        );
        drop(accepted_stream);
    }

    #[tokio::test]
    async fn tls_projection_reserves_candidate_and_retired_active_generation() {
        let directory = trusted_tempdir();
        let trust = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let plan = tls_plan(trust, "127.0.0.1:7443");
        let runtime = RelayTransportRuntime::activate(&plan, Arc::new(AtomicBool::new(false)))
            .await
            .expect("initial activation");
        let connector = runtime.connector();
        let endpoint = plan.routes()[0].endpoint();
        let projected = connector
            .resource_projection(endpoint)
            .expect("product projection");
        let generation_bytes = MAX_TRUST_ANCHOR_BYTES as u64 + GENERATION_CONTROL_BYTES;
        let candidate_bytes = generation_bytes + MAX_TRUST_SOURCE_BYTES as u64;
        let CompiledAccessFlowRelayEndpoint::TlsTcp { tls_index, .. } = endpoint else {
            panic!("TLS endpoint expected");
        };
        let generation = runtime
            .inner
            .retained_generation()
            .expect("retained generation");
        let base = runtime
            .inner
            .tls
            .resource_projection(&generation.tls_endpoints[*tls_index])
            .expect("base projection");
        assert_eq!(
            projected.retained_endpoint_bytes,
            base.retained_endpoint_bytes + candidate_bytes
        );
        assert_eq!(projected.active_bytes, base.active_bytes + generation_bytes);
    }

    #[tokio::test]
    async fn close_is_terminal_and_empty_tls_set_reload_is_noop() {
        let directory = trusted_tempdir();
        let endpoint = unix_endpoint(&directory.path().join("relay.sock"));
        let plan = AccessFlowRelayPlan::new(
            vec![
                AccessFlowRoute::new(
                    AccessFlowRouteName::new("unix").expect("route name"),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18081),
                    vec![NonZeroU16::new(80).expect("port")],
                    endpoint,
                )
                .expect("Unix route"),
            ],
            presentation(),
            Duration::from_secs(5),
            NonZeroUsize::new(8).expect("connections"),
            NonZeroUsize::new(4096).expect("buffer"),
        )
        .expect("Unix plan");
        assert_eq!(
            RelayTransportRuntime::reload_descriptor_reserve(&plan),
            Ok(0)
        );
        let readiness = Arc::new(AtomicBool::new(false));
        let runtime = RelayTransportRuntime::activate(&plan, Arc::clone(&readiness))
            .await
            .expect("Unix-only activation");
        assert!(readiness.load(Ordering::Acquire));
        assert!(runtime.begin_reload().expect("Unix-only reload").is_none());
        assert!(readiness.load(Ordering::Acquire));
        assert!(runtime.healthy());
        runtime.close();
        assert!(!runtime.healthy());
        assert!(!readiness.load(Ordering::Acquire));
        assert_eq!(
            runtime.begin_reload().map(|_| ()),
            Err(RelayTransportError::ShuttingDown)
        );
    }

    #[test]
    fn mutation_fixture_really_changes_descriptor_metadata() {
        let directory = trusted_tempdir();
        let path = write_trust(directory.path(), TEST_ROOT_PEM.as_bytes());
        let before = std::fs::metadata(&path).expect("before");
        std::fs::write(&path, b"changed").expect("change");
        let after = std::fs::metadata(&path).expect("after");
        assert_eq!(before.dev(), after.dev());
        assert_eq!(before.ino(), after.ino());
        assert!(
            before.len() != after.len()
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
                || before.ctime() != after.ctime()
                || before.ctime_nsec() != after.ctime_nsec()
        );
    }
}
