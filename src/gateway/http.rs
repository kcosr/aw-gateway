use super::ops::{
    CanonicalLaunchVarValue, GatewayOperation, GatewayOperationResult, LaunchPassthroughArgs,
    OperationExecutionOptions, OperationMode, OutputSelection, SuppliedLaunchVars,
    execute_gateway_operation_cancelable_with_context, execute_gateway_operation_with_context,
};
use super::{
    PreparedExecution, SessionOutcome, prepare_launch_execution_with_config,
    prepare_run_execution_with_config,
};
use crate::config::GatewayConfig;
use crate::context::{RuntimeContext, deserialize_context_object};
use crate::gateway::ops::ExecutionOutcome;
use crate::runtime::{ContainerPtySession, ContainerPtySize};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, watch};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

mod auth;
mod output_projection;
mod response;

use auth::{authorize, authorize_action, constant_time_eq};
use output_projection::{OutputFormat, OutputFormats};
use response::{
    ErrorCode, HttpError, execution_response, metadata_result_response, operation_error_response,
};

const PTY_ATTACH_LEASE_TTL: Duration = Duration::from_secs(30);
const PTY_ATTACH_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_TERMINAL_COLS: u16 = 10;
const MAX_TERMINAL_COLS: u16 = 500;
const MIN_TERMINAL_ROWS: u16 = 2;
const MAX_TERMINAL_ROWS: u16 = 300;
const MAX_WS_TEXT_BYTES: usize = 64 * 1024;
const MAX_WS_BINARY_BYTES: usize = 256 * 1024;
const PTY_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const PTY_SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const WAIT_SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct AppState {
    config_path: Option<PathBuf>,
    config: Arc<GatewayConfig>,
    pty_leases: Arc<PtyLeaseManager>,
    pty_shutdown: PtyShutdown,
    wait_shutdown: WaitShutdown,
}

pub(super) async fn serve(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = super::load_config(config_path.clone())?;
    if !cfg.http.enabled {
        anyhow::bail!("http listener is disabled in config");
    }
    let addr = cfg.http.listen_addr()?;
    let pty_leases = Arc::new(PtyLeaseManager::default());
    let pty_shutdown = PtyShutdown::default();
    let wait_shutdown = WaitShutdown::default();
    let app = router(AppState {
        config_path,
        config: Arc::new(cfg),
        pty_leases: pty_leases.clone(),
        pty_shutdown: pty_shutdown.clone(),
        wait_shutdown: wait_shutdown.clone(),
    });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(listen = %addr, "http listener started");
    let shutdown_leases = pty_leases.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("http shutdown signal received; giving active wait operations grace");
            wait_shutdown
                .cancel_active_after_grace(WAIT_SHUTDOWN_GRACE, WAIT_SHUTDOWN_WAIT_TIMEOUT)
                .await;
            tracing::info!("http shutdown signal received; canceling active pty sessions");
            pty_shutdown
                .cancel_active_and_wait(PTY_SHUTDOWN_WAIT_TIMEOUT)
                .await;
            tracing::info!("http shutdown canceling prepared pty leases");
            shutdown_leases.cancel_all().await;
            tracing::info!("http shutdown pty cancellation complete");
        })
        .await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/status/all", get(status_all))
        .route("/api/v1/targets", get(targets))
        .route("/api/v1/up", post(up))
        .route("/api/v1/stop", post(stop))
        .route("/api/v1/remove", post(remove))
        .route("/api/v1/launches", get(launches))
        .route("/api/v1/launches/{name}", get(launch_show))
        .route("/api/v1/launches/{name}/run", post(launch_run))
        .route("/api/v1/run", post(run))
        .route("/api/v1/pty/{pty_id}", get(pty_attach))
        .fallback(not_found)
        .with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[derive(Default)]
struct PtyLeaseManager {
    leases: Mutex<BTreeMap<String, PreparedPtyLease>>,
}

struct PreparedPtyLease {
    attach_token: String,
    terminal: ContainerPtySize,
    execution: PreparedExecution,
}

#[derive(Clone)]
struct PtyShutdown {
    sender: watch::Sender<bool>,
    active: Arc<AtomicUsize>,
}

impl Default for PtyShutdown {
    fn default() -> Self {
        let (sender, _) = watch::channel(false);
        Self {
            sender,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl PtyShutdown {
    fn subscribe(&self) -> watch::Receiver<bool> {
        self.sender.subscribe()
    }

    fn track_active(&self) -> ActivePtySession {
        self.active.fetch_add(1, Ordering::Relaxed);
        ActivePtySession {
            active: self.active.clone(),
        }
    }

    async fn cancel_active_and_wait(&self, timeout: Duration) {
        tracing::info!("broadcasting pty shutdown");
        let _ = self.sender.send(true);
        let deadline = tokio::time::Instant::now() + timeout;
        while self.active.load(Ordering::Relaxed) > 0 {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    active = self.active.load(Ordering::Relaxed),
                    "timed out waiting for active pty sessions to finish"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

struct ActivePtySession {
    active: Arc<AtomicUsize>,
}

impl Drop for ActivePtySession {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct WaitShutdown {
    token: CancellationToken,
    active: Arc<AtomicUsize>,
}

impl Default for WaitShutdown {
    fn default() -> Self {
        Self {
            token: CancellationToken::new(),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl WaitShutdown {
    fn register(&self) -> (CancellationToken, ActiveWaitOperation) {
        self.active.fetch_add(1, Ordering::Relaxed);
        (
            self.token.child_token(),
            ActiveWaitOperation {
                active: self.active.clone(),
            },
        )
    }

    async fn cancel_active_after_grace(&self, grace: Duration, wait_timeout: Duration) {
        let grace_deadline = tokio::time::Instant::now() + grace;
        while self.active.load(Ordering::Relaxed) > 0 {
            if tokio::time::Instant::now() >= grace_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.active.load(Ordering::Relaxed) == 0 {
            return;
        }

        tracing::info!(
            active = self.active.load(Ordering::Relaxed),
            "canceling active http wait operations after shutdown grace"
        );
        self.token.cancel();
        let wait_deadline = tokio::time::Instant::now() + wait_timeout;
        while self.active.load(Ordering::Relaxed) > 0 {
            if tokio::time::Instant::now() >= wait_deadline {
                tracing::warn!(
                    active = self.active.load(Ordering::Relaxed),
                    "timed out waiting for active http wait operations to finish"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

struct ActiveWaitOperation {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveWaitOperation {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

struct OperationCancelGuard {
    token: CancellationToken,
}

impl Drop for OperationCancelGuard {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

#[derive(Serialize)]
struct PtyLeaseCreated {
    ok: bool,
    mode: &'static str,
    status: &'static str,
    pty_id: String,
    attach_token: String,
    session_id: Option<String>,
    attach_url: String,
}

impl PtyLeaseManager {
    async fn insert(
        self: &Arc<Self>,
        execution: PreparedExecution,
        terminal: ContainerPtySize,
    ) -> anyhow::Result<PtyLeaseCreated> {
        let pty_id = format!("pty_{}", super::token::random_hex_token()?);
        let attach_token = format!("awpt_{}", super::token::random_hex_token()?);
        let session_id = execution.ready().session_id.clone();
        let lease = PreparedPtyLease {
            attach_token: attach_token.clone(),
            terminal,
            execution,
        };
        self.leases.lock().await.insert(pty_id.clone(), lease);
        let manager = self.clone();
        let expiry_id = pty_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PTY_ATTACH_LEASE_TTL).await;
            manager.expire(&expiry_id).await;
        });
        Ok(PtyLeaseCreated {
            ok: true,
            mode: "pty",
            status: "prepared",
            pty_id: pty_id.clone(),
            attach_token,
            session_id,
            attach_url: format!("/api/v1/pty/{pty_id}"),
        })
    }

    async fn contains(&self, pty_id: &str) -> bool {
        self.leases.lock().await.contains_key(pty_id)
    }

    async fn consume_if_token(&self, pty_id: &str, attach_token: &str) -> Option<PreparedPtyLease> {
        let mut leases = self.leases.lock().await;
        let matches = leases.get(pty_id).is_some_and(|lease| {
            constant_time_eq(lease.attach_token.as_bytes(), attach_token.as_bytes())
        });
        if matches { leases.remove(pty_id) } else { None }
    }

    async fn expire(&self, pty_id: &str) {
        let lease = self.leases.lock().await.remove(pty_id);
        if let Some(lease) = lease {
            finish_pty_lease(
                lease,
                Ok(ExecutionOutcome::new(130)),
                SessionOutcome::Canceled,
            )
            .await;
        }
    }

    async fn cancel_all(&self) {
        let leases = {
            let mut guard = self.leases.lock().await;
            std::mem::take(&mut *guard)
        };
        for (_, lease) in leases {
            finish_pty_lease(
                lease,
                Ok(ExecutionOutcome::new(130)),
                SessionOutcome::Canceled,
            )
            .await;
        }
    }
}

async fn finish_pty_lease(
    lease: PreparedPtyLease,
    result: anyhow::Result<ExecutionOutcome>,
    outcome: SessionOutcome,
) {
    if let Err(err) = lease.execution.finish(result, outcome).await {
        tracing::warn!(error = %err, "pty session cleanup failed");
    }
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    handle_metadata(state, headers, "status", || {
        let query = parse_status_query(query.as_deref())?;
        Ok((
            GatewayOperation::Status {
                target: query.target,
                session_id: query.session_id,
            },
            query.context,
        ))
    })
    .await
}

async fn status_all(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    handle_metadata(state, headers, "status", || {
        let query = parse_status_query(query.as_deref())?;
        if query.target.is_some() || query.session_id.is_some() {
            return Err(HttpError::invalid_request(
                "status/all query only accepts context.<key> parameters",
            ));
        }
        Ok((GatewayOperation::StatusAll, query.context))
    })
    .await
}

async fn targets(State(state): State<AppState>, headers: HeaderMap) -> Response {
    handle_metadata(state, headers, "targets", || {
        Ok((GatewayOperation::Targets, RuntimeContext::empty()))
    })
    .await
}

async fn up(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_metadata(state, headers, "up", || {
        let request: UpRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        Ok((
            GatewayOperation::Up {
                target: request.target,
                session_id: request.session_id,
            },
            request.context.unwrap_or_default(),
        ))
    })
    .await
}

async fn stop(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_metadata(state, headers, "stop", || {
        let request: LifecycleRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        Ok((
            GatewayOperation::Stop {
                target: request.target,
                session_id: request.session_id,
            },
            request.context.unwrap_or_default(),
        ))
    })
    .await
}

async fn remove(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_metadata(state, headers, "remove", || {
        let request: LifecycleRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        Ok((
            GatewayOperation::Remove {
                target: request.target,
                session_id: request.session_id,
            },
            request.context.unwrap_or_default(),
        ))
    })
    .await
}

async fn launches(State(state): State<AppState>, headers: HeaderMap) -> Response {
    handle_metadata(state, headers, "launches", || {
        Ok((GatewayOperation::Launches, RuntimeContext::empty()))
    })
    .await
}

async fn launch_show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    handle_metadata(state, headers, "launch", || {
        Ok((
            GatewayOperation::LaunchShow { name },
            RuntimeContext::empty(),
        ))
    })
    .await
}

async fn launch_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let request: LaunchRunRequest = match authorize_action(&state, &headers, "launch").await {
        Ok(()) => match parse_body(&body, ErrorCode::InvalidLaunchVar) {
            Ok(request) => request,
            Err(err) => return err.into_response(),
        },
        Err(err) => return err.into_response(),
    };
    let context = request.context.clone().unwrap_or_default();
    if is_pty_mode(request.mode.as_deref()) {
        return prepare_pty_launch(state, name, request).await;
    }
    let execution = match execution_request_options(
        request.mode.as_deref(),
        request.output.as_deref(),
        request.output_format.as_ref(),
    ) {
        Ok(execution) => execution,
        Err(err) => return err.into_response(),
    };
    let operation = GatewayOperation::Launch {
        name,
        session_id: request.session_id,
        vars: request.vars.unwrap_or_default(),
        args: request.args.unwrap_or_default(),
        options: execution.options,
    };
    execute_http_execution(
        state,
        operation,
        execution.output_formats,
        execution.options.mode,
        context,
    )
    .await
}

async fn run(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request: RunRequest = match authorize_action(&state, &headers, "run").await {
        Ok(()) => match parse_body(&body, ErrorCode::InvalidRequest) {
            Ok(request) => request,
            Err(err) => return err.into_response(),
        },
        Err(err) => return err.into_response(),
    };
    if let Err(err) = validate_run_command(&request.command) {
        return err.into_response();
    }
    let context = request.context.clone().unwrap_or_default();
    if is_pty_mode(request.mode.as_deref()) {
        return prepare_pty_run(state, request).await;
    }
    let execution = match execution_request_options(
        request.mode.as_deref(),
        request.output.as_deref(),
        request.output_format.as_ref(),
    ) {
        Ok(execution) => execution,
        Err(err) => return err.into_response(),
    };
    let operation = GatewayOperation::Run {
        target: request.target,
        session_id: request.session_id,
        cwd: request.cwd,
        command: request.command,
        options: execution.options,
    };
    execute_http_execution(
        state,
        operation,
        execution.output_formats,
        execution.options.mode,
        context,
    )
    .await
}

async fn pty_attach(
    State(state): State<AppState>,
    Path(pty_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.pty_leases.contains(&pty_id).await {
        return HttpError::not_found("pty lease not found").into_response();
    }
    ws.on_upgrade(move |socket| {
        handle_pty_socket(state.pty_leases, state.pty_shutdown, pty_id, socket)
    })
}

async fn handle_pty_socket(
    leases: Arc<PtyLeaseManager>,
    pty_shutdown: PtyShutdown,
    pty_id: String,
    mut socket: WebSocket,
) {
    let _active = pty_shutdown.track_active();
    let auth = read_pty_auth_frame(&mut socket, pty_shutdown.subscribe()).await;
    let token = match auth {
        Ok(token) => token,
        Err(err) => {
            let _ = send_ws_error(&mut socket, err.code, err.message).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let Some(lease) = leases.consume_if_token(&pty_id, &token).await else {
        let _ = send_ws_error(&mut socket, ErrorCode::Unauthorized, "unauthorized").await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    run_attached_pty(lease, socket, pty_shutdown.subscribe()).await;
}

async fn read_pty_auth_frame(
    socket: &mut WebSocket,
    mut shutdown: watch::Receiver<bool>,
) -> Result<String, HttpError> {
    match tokio::time::timeout(PTY_ATTACH_AUTH_TIMEOUT, async {
        loop {
            if *shutdown.borrow() {
                return Err(HttpError::unauthorized("pty attach canceled by shutdown"));
            }
            tokio::select! {
                message = socket.recv() => {
                    match message {
                        Some(Ok(Message::Text(text))) => return parse_pty_auth(&text),
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {
                            return Err(HttpError::unauthorized("first pty message must be auth"));
                        }
                        Some(Err(err)) => return Err(HttpError::unauthorized(err.to_string())),
                        None => return Err(HttpError::unauthorized("pty attach closed before auth")),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Err(HttpError::unauthorized("pty attach canceled by shutdown"));
                    }
                }
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(HttpError::unauthorized("pty attach auth timed out")),
    }
}

async fn run_attached_pty(
    lease: PreparedPtyLease,
    mut socket: WebSocket,
    mut shutdown: watch::Receiver<bool>,
) {
    let ready = lease.execution.ready().clone();
    let launch = lease.execution.launch_name().map(str::to_string);
    let mut pty = match lease.execution.spawn_pty(lease.terminal) {
        Ok(pty) => pty,
        Err(err) => {
            tracing::warn!(error = %err, "failed to spawn pty child");
            let _ = send_ws_error(
                &mut socket,
                ErrorCode::OperationFailed,
                "failed to start interactive session",
            )
            .await;
            let _ = socket.send(Message::Close(None)).await;
            finish_pty_lease(
                lease,
                Err(err.context("failed to spawn pty child")),
                SessionOutcome::Failure,
            )
            .await;
            return;
        }
    };

    let ready_payload = serde_json::json!({
        "type": "ready",
        "session_id": ready.session_id,
        "target": ready.target,
        "target_mode": ready.mode,
        "launch": launch,
    });
    if socket
        .send(Message::Text(ready_payload.to_string().into()))
        .await
        .is_err()
    {
        let _ = pty.terminate().await;
        drop(pty);
        finish_pty_lease(
            lease,
            Ok(ExecutionOutcome::new(130)),
            SessionOutcome::Canceled,
        )
        .await;
        return;
    }

    let mut outcome = SessionOutcome::Canceled;
    let mut result = Ok(ExecutionOutcome::new(130));
    let mut output_open = true;
    if *shutdown.borrow() {
        tracing::info!("pty session started after shutdown; terminating immediately");
        cancel_pty_session(&pty).await;
        drop(pty);
        finish_pty_lease(lease, result, outcome).await;
        return;
    }
    loop {
        tokio::select! {
            output = pty.output.recv(), if output_open => {
                match output {
                    Some(bytes) => {
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
                            cancel_pty_session(&pty).await;
                            break;
                        }
                    }
                    None => {
                        output_open = false;
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!("pty session observed http shutdown; terminating");
                    cancel_pty_session(&pty).await;
                    break;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        if bytes.len() > MAX_WS_BINARY_BYTES {
                            let _ = send_ws_error(&mut socket, ErrorCode::InvalidRequest, "pty input message is too large").await;
                            continue;
                        }
                        if pty.input.send(bytes.to_vec()).await.is_err() {
                            let _ = pty.terminate().await;
                            let (exit_result, exit_outcome) = finish_pty_exit(&mut socket, &mut pty).await;
                            result = exit_result;
                            outcome = exit_outcome;
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match parse_pty_control(&text) {
                            Ok(control) => match control {
                                PtyClientControl::Resize { .. } => {
                                    match control.resize_size() {
                                        Ok(Some(size)) => {
                                            let _ = pty.resize.send(size).await;
                                        }
                                        Ok(None) => {}
                                        Err(err) => {
                                            let _ = send_ws_error(&mut socket, err.code, err.message).await;
                                        }
                                    }
                                }
                                PtyClientControl::Close => {
                                    cancel_pty_session(&pty).await;
                                    break;
                                }
                                PtyClientControl::Auth { .. } => {
                                    let _ = send_ws_error(&mut socket, ErrorCode::InvalidRequest, "auth is only valid as the first pty message").await;
                                }
                            },
                            Err(err) => {
                                let _ = send_ws_error(&mut socket, err.code, err.message).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        cancel_pty_session(&pty).await;
                        break;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Err(_)) => {
                        cancel_pty_session(&pty).await;
                        break;
                    }
                }
            }
            exit = &mut pty.exit => {
                (result, outcome) = handle_pty_exit(&mut socket, &mut pty.output, exit).await;
                break;
            }
        }
    }
    drop(pty);
    finish_pty_lease(lease, result, outcome).await;
}

async fn cancel_pty_session(pty: &ContainerPtySession) {
    tracing::info!("canceling pty session");
    let _ = pty.terminate().await;
    tracing::info!("pty session cancel request complete");
}

async fn drain_pty_output(
    socket: &mut WebSocket,
    output: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<(), axum::Error> {
    loop {
        match tokio::time::timeout(PTY_EXIT_DRAIN_TIMEOUT, output.recv()).await {
            Ok(Some(bytes)) => socket.send(Message::Binary(bytes.into())).await?,
            Ok(None) | Err(_) => return Ok(()),
        }
    }
}

async fn finish_pty_exit(
    socket: &mut WebSocket,
    pty: &mut ContainerPtySession,
) -> (anyhow::Result<ExecutionOutcome>, SessionOutcome) {
    match (&mut pty.exit).await {
        Ok(exit) => handle_pty_exit(socket, &mut pty.output, Ok(exit)).await,
        Err(err) => handle_pty_exit(socket, &mut pty.output, Err(err)).await,
    }
}

async fn handle_pty_exit(
    socket: &mut WebSocket,
    output: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    exit: Result<anyhow::Result<i32>, tokio::task::JoinError>,
) -> (anyhow::Result<ExecutionOutcome>, SessionOutcome) {
    match exit {
        Ok(Ok(code)) => {
            let _ = drain_pty_output(socket, output).await;
            let outcome = if code == 0 {
                SessionOutcome::Success
            } else {
                SessionOutcome::Failure
            };
            let payload = serde_json::json!({
                "type": "exit",
                "exit_code": code,
                "outcome": if code == 0 { "success" } else { "failure" },
            });
            let _ = socket.send(Message::Text(payload.to_string().into())).await;
            (Ok(ExecutionOutcome::new(code)), outcome)
        }
        Ok(Err(err)) => {
            let _ = drain_pty_output(socket, output).await;
            tracing::warn!(error = %err, "pty wait failed");
            let _ = send_ws_error(
                socket,
                ErrorCode::OperationFailed,
                "interactive session failed",
            )
            .await;
            (Err(err), SessionOutcome::Failure)
        }
        Err(err) => {
            let _ = drain_pty_output(socket, output).await;
            let err = anyhow::Error::from(err);
            tracing::warn!(error = %err, "pty wait task failed");
            let _ = send_ws_error(socket, ErrorCode::OperationFailed, "pty wait task failed").await;
            (Err(err), SessionOutcome::Failure)
        }
    }
}

async fn prepare_pty_run(state: AppState, request: RunRequest) -> Response {
    if let Err(err) = reject_pty_output(request.output.as_ref(), request.output_format.as_ref()) {
        return err.into_response();
    }
    let terminal = match terminal_size(request.terminal.as_ref()) {
        Ok(terminal) => terminal,
        Err(err) => return err.into_response(),
    };
    let prepared = match prepare_run_execution_with_config(
        (*state.config).clone(),
        request.target,
        request.session_id,
        request.cwd,
        request.command,
        request.context.unwrap_or_default(),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(err) => return operation_error_response(err),
    };
    match state.pty_leases.insert(prepared, terminal).await {
        Ok(created) => (StatusCode::CREATED, Json(created)).into_response(),
        Err(err) => HttpError::operation_failed(err.to_string()).into_response(),
    }
}

async fn prepare_pty_launch(state: AppState, name: String, request: LaunchRunRequest) -> Response {
    if let Err(err) = reject_pty_output(request.output.as_ref(), request.output_format.as_ref()) {
        return err.into_response();
    }
    let terminal = match terminal_size(request.terminal.as_ref()) {
        Ok(terminal) => terminal,
        Err(err) => return err.into_response(),
    };
    let prepared = match prepare_launch_execution_with_config(
        (*state.config).clone(),
        &name,
        request.session_id,
        request.vars.unwrap_or_default(),
        request.args.unwrap_or_default(),
        request.context.unwrap_or_default(),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(err) => return operation_error_response(err),
    };
    match state.pty_leases.insert(prepared, terminal).await {
        Ok(created) => (StatusCode::CREATED, Json(created)).into_response(),
        Err(err) => HttpError::operation_failed(err.to_string()).into_response(),
    }
}

async fn execute_http_execution(
    state: AppState,
    operation: GatewayOperation,
    output_formats: OutputFormats,
    mode: OperationMode,
    context: RuntimeContext,
) -> Response {
    if mode == OperationMode::Wait {
        return execute_http_wait(state, operation, output_formats, context).await;
    }
    execute_http_execution_direct(state.config_path, operation, output_formats, context).await
}

async fn execute_http_wait(
    state: AppState,
    operation: GatewayOperation,
    output_formats: OutputFormats,
    context: RuntimeContext,
) -> Response {
    let (cancel, active) = state.wait_shutdown.register();
    let guard = OperationCancelGuard {
        token: cancel.clone(),
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _active = active;
        let response = execute_http_execution_direct_cancelable(
            state.config_path,
            operation,
            output_formats,
            cancel,
            context,
        )
        .await;
        let _ = tx.send(response);
    });
    let response = match rx.await {
        Ok(response) => response,
        Err(_) => {
            HttpError::operation_failed("operation task ended without a response").into_response()
        }
    };
    drop(guard);
    response
}

async fn execute_http_execution_direct(
    config_path: Option<PathBuf>,
    operation: GatewayOperation,
    output_formats: OutputFormats,
    context: RuntimeContext,
) -> Response {
    match execute_gateway_operation_with_context(config_path, operation, context).await {
        Ok(GatewayOperationResult::Run(outcome)) | Ok(GatewayOperationResult::Launch(outcome)) => {
            execution_response(outcome, output_formats)
        }
        Ok(_) => {
            HttpError::operation_failed("operation returned an unexpected result").into_response()
        }
        Err(err) => operation_error_response(err),
    }
}

async fn execute_http_execution_direct_cancelable(
    config_path: Option<PathBuf>,
    operation: GatewayOperation,
    output_formats: OutputFormats,
    cancel: CancellationToken,
    context: RuntimeContext,
) -> Response {
    match execute_gateway_operation_cancelable_with_context(config_path, operation, cancel, context)
        .await
    {
        Ok(GatewayOperationResult::Run(outcome)) | Ok(GatewayOperationResult::Launch(outcome)) => {
            execution_response(outcome, output_formats)
        }
        Ok(_) => {
            HttpError::operation_failed("operation returned an unexpected result").into_response()
        }
        Err(err) => operation_error_response(err),
    }
}

async fn not_found(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !uri.path().starts_with("/api/v1/") {
        return HttpError::not_found("route not found").into_response();
    }
    match authorize(&state, &headers).await {
        Ok(()) => HttpError::not_found("route not found").into_response(),
        Err(err) => err.into_response(),
    }
}

async fn handle_metadata(
    state: AppState,
    headers: HeaderMap,
    action: &'static str,
    operation: impl FnOnce() -> Result<(GatewayOperation, RuntimeContext), HttpError>,
) -> Response {
    let (operation, context) = match authorize_action(&state, &headers, action).await {
        Ok(()) => match operation() {
            Ok(operation) => operation,
            Err(err) => return err.into_response(),
        },
        Err(err) => return err.into_response(),
    };
    match execute_gateway_operation_with_context(state.config_path, operation, context).await {
        Ok(result) => metadata_result_response(result),
        Err(err) => operation_error_response(err),
    }
}

#[derive(Debug)]
struct HttpExecutionOptions {
    options: OperationExecutionOptions,
    output_formats: OutputFormats,
}

fn parse_body<T: for<'de> Deserialize<'de>>(
    body: &[u8],
    semantic_code: ErrorCode,
) -> Result<T, HttpError> {
    serde_json::from_slice(body).map_err(|err| {
        if semantic_code == ErrorCode::InvalidLaunchVar && is_launch_args_error(&err) {
            return HttpError::new(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidLaunchArgs,
                err.to_string(),
            );
        }
        if semantic_code == ErrorCode::InvalidLaunchVar && is_launch_var_error(&err) {
            HttpError::new(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidLaunchVar,
                err.to_string(),
            )
        } else {
            HttpError::invalid_request(err.to_string())
        }
    })
}

#[derive(Debug, Default)]
struct ParsedStatusQuery {
    target: Option<String>,
    session_id: Option<String>,
    context: RuntimeContext,
}

fn parse_status_query(raw: Option<&str>) -> Result<ParsedStatusQuery, HttpError> {
    let mut query = ParsedStatusQuery::default();
    let mut context = BTreeMap::new();
    let mut seen_context = BTreeSet::new();
    let mut saw_target = false;
    let mut saw_session_id = false;
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let key = key.into_owned();
        let value = value.into_owned();
        match key.as_str() {
            "target" => {
                if saw_target {
                    return Err(HttpError::invalid_request(
                        "duplicate status query parameter \"target\"",
                    ));
                }
                saw_target = true;
                query.target = Some(value);
            }
            "session_id" => {
                if saw_session_id {
                    return Err(HttpError::invalid_request(
                        "duplicate status query parameter \"session_id\"",
                    ));
                }
                saw_session_id = true;
                query.session_id = Some(value);
            }
            _ => {
                let Some(context_key) = key.strip_prefix("context.") else {
                    return Err(HttpError::invalid_request(format!(
                        "unknown status query parameter {key:?}"
                    )));
                };
                if context_key.is_empty() {
                    return Err(HttpError::invalid_request(
                        "context query parameters must use context.<key>=value",
                    ));
                }
                if !seen_context.insert(context_key.to_string()) {
                    return Err(HttpError::invalid_request(format!(
                        "duplicate context key {context_key:?}"
                    )));
                }
                context.insert(context_key.to_string(), value);
            }
        }
    }
    query.context = RuntimeContext::from_map(context);
    Ok(query)
}

fn deserialize_optional_runtime_context<'de, D>(
    deserializer: D,
) -> Result<Option<RuntimeContext>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptionalRuntimeContextVisitor)
}

struct OptionalRuntimeContextVisitor;

impl<'de> Visitor<'de> for OptionalRuntimeContextVisitor {
    type Value = Option<RuntimeContext>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("null or a JSON object with string values")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_context_object(deserializer)
            .map(RuntimeContext::from_map)
            .map(Some)
    }
}

fn is_launch_args_error(err: &serde_json::Error) -> bool {
    err.to_string().contains("invalid launch args") || err.to_string().contains("launch args must")
}

fn is_launch_var_error(err: &serde_json::Error) -> bool {
    let text = err.to_string();
    text.contains("launch variable") || text.contains("launch var")
}

fn execution_request_options(
    mode: Option<&str>,
    output: Option<&[String]>,
    output_format: Option<&BTreeMap<String, String>>,
) -> Result<HttpExecutionOptions, HttpError> {
    let mode = match mode.unwrap_or("wait") {
        "wait" => OperationMode::Wait,
        "detach" => OperationMode::Detach,
        _ => {
            return Err(HttpError::invalid_mode(
                "mode must be \"wait\", \"detach\", or \"pty\"",
            ));
        }
    };
    if mode == OperationMode::Detach && output.is_some() {
        return Err(HttpError::invalid_output(
            "output is only supported for wait mode",
        ));
    }
    if mode == OperationMode::Detach && output_format.is_some() {
        return Err(HttpError::invalid_output(
            "output_format is only supported for wait mode",
        ));
    }
    let output = output_selection(output)?;
    let output_formats = validate_output_formats(output_format, output)?;
    Ok(HttpExecutionOptions {
        options: OperationExecutionOptions { mode, output },
        output_formats,
    })
}

fn output_selection(output: Option<&[String]>) -> Result<OutputSelection, HttpError> {
    let Some(output) = output else {
        return Ok(OutputSelection::BOTH);
    };
    if output.is_empty() {
        return Err(HttpError::invalid_output("output must not be empty"));
    }
    let mut seen = BTreeSet::new();
    let mut selection = OutputSelection {
        stdout: false,
        stderr: false,
    };
    for stream in output {
        if !seen.insert(stream.as_str()) {
            return Err(HttpError::invalid_output(format!(
                "duplicate output stream {stream:?}"
            )));
        }
        match stream.as_str() {
            "stdout" => selection.stdout = true,
            "stderr" => selection.stderr = true,
            _ => {
                return Err(HttpError::invalid_output(format!(
                    "unknown output stream {stream:?}"
                )));
            }
        }
    }
    Ok(selection)
}

fn validate_output_formats(
    output_format: Option<&BTreeMap<String, String>>,
    selection: OutputSelection,
) -> Result<OutputFormats, HttpError> {
    let mut formats = OutputFormats::TEXT;
    let Some(output_format) = output_format else {
        return Ok(formats);
    };
    for (stream, format) in output_format {
        let (selected, slot) = match stream.as_str() {
            "stdout" => (selection.stdout, &mut formats.stdout),
            "stderr" => (selection.stderr, &mut formats.stderr),
            _ => {
                return Err(HttpError::invalid_output(format!(
                    "unknown output_format stream {stream:?}"
                )));
            }
        };
        if !selected {
            return Err(HttpError::invalid_output(format!(
                "output_format stream {stream:?} is not selected"
            )));
        }
        let parsed = match format.as_str() {
            "text" => OutputFormat::Text,
            "json" => OutputFormat::Json,
            _ => {
                return Err(HttpError::invalid_output(format!(
                    "unknown output format {format:?} for stream {stream:?}"
                )));
            }
        };
        *slot = parsed;
    }
    Ok(formats)
}

fn is_pty_mode(mode: Option<&str>) -> bool {
    mode == Some("pty")
}

fn validate_run_command(command: &[String]) -> Result<(), HttpError> {
    if command.is_empty() {
        return Err(HttpError::invalid_request("command must not be empty"));
    }
    if command.iter().any(|arg| arg.is_empty()) {
        return Err(HttpError::invalid_request(
            "command elements must not be empty",
        ));
    }
    Ok(())
}

fn reject_pty_output(
    output: Option<&Vec<String>>,
    output_format: Option<&BTreeMap<String, String>>,
) -> Result<(), HttpError> {
    if output.is_some() {
        return Err(HttpError::invalid_output(
            "output is only supported for wait mode",
        ));
    }
    if output_format.is_some() {
        return Err(HttpError::invalid_output(
            "output_format is only supported for wait mode",
        ));
    }
    Ok(())
}

fn terminal_size(terminal: Option<&TerminalRequest>) -> Result<ContainerPtySize, HttpError> {
    let terminal =
        terminal.ok_or_else(|| HttpError::invalid_request("terminal is required for pty mode"))?;
    Ok(ContainerPtySize {
        cols: validate_dimension(
            "terminal.cols",
            terminal.cols,
            MIN_TERMINAL_COLS,
            MAX_TERMINAL_COLS,
        )?,
        rows: validate_dimension(
            "terminal.rows",
            terminal.rows,
            MIN_TERMINAL_ROWS,
            MAX_TERMINAL_ROWS,
        )?,
        pixel_width: terminal.cell_width_px.unwrap_or_default(),
        pixel_height: terminal.cell_height_px.unwrap_or_default(),
    })
}

fn validate_dimension(
    name: &'static str,
    value: u16,
    min: u16,
    max: u16,
) -> Result<u16, HttpError> {
    if value < min || value > max {
        return Err(HttpError::invalid_request(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PtyClientControl {
    Auth {
        token: String,
    },
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
    },
    Close,
}

fn parse_pty_auth(text: &str) -> Result<String, HttpError> {
    let control = parse_pty_control(text)?;
    match control {
        PtyClientControl::Auth { token } if !token.is_empty() => Ok(token),
        PtyClientControl::Auth { .. } => Err(HttpError::unauthorized("attach token is required")),
        _ => Err(HttpError::unauthorized("first pty message must be auth")),
    }
}

fn parse_pty_control(text: &str) -> Result<PtyClientControl, HttpError> {
    if text.len() > MAX_WS_TEXT_BYTES {
        return Err(HttpError::invalid_request(
            "pty control message is too large",
        ));
    }
    serde_json::from_str(text)
        .map_err(|err| HttpError::invalid_request(format!("invalid pty control message: {err}")))
}

impl PtyClientControl {
    fn resize_size(&self) -> Result<Option<ContainerPtySize>, HttpError> {
        match self {
            Self::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            } => Ok(Some(ContainerPtySize {
                cols: validate_dimension("cols", *cols, MIN_TERMINAL_COLS, MAX_TERMINAL_COLS)?,
                rows: validate_dimension("rows", *rows, MIN_TERMINAL_ROWS, MAX_TERMINAL_ROWS)?,
                pixel_width: cell_width_px.unwrap_or_default(),
                pixel_height: cell_height_px.unwrap_or_default(),
            })),
            _ => Ok(None),
        }
    }
}

async fn send_ws_error(
    socket: &mut WebSocket,
    code: ErrorCode,
    message: impl Into<String>,
) -> Result<(), axum::Error> {
    let payload = serde_json::json!({
        "type": "error",
        "code": code.as_str(),
        "message": message.into(),
    });
    socket.send(Message::Text(payload.to_string().into())).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpRequest {
    target: Option<String>,
    session_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_runtime_context")]
    context: Option<RuntimeContext>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleRequest {
    target: Option<String>,
    session_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_runtime_context")]
    context: Option<RuntimeContext>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    target: Option<String>,
    session_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_runtime_context")]
    context: Option<RuntimeContext>,
    cwd: Option<String>,
    command: Vec<String>,
    mode: Option<String>,
    terminal: Option<TerminalRequest>,
    output: Option<Vec<String>>,
    output_format: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRunRequest {
    session_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_runtime_context")]
    context: Option<RuntimeContext>,
    #[serde(default, deserialize_with = "deserialize_launch_vars")]
    vars: Option<SuppliedLaunchVars>,
    #[serde(default, deserialize_with = "deserialize_launch_args")]
    args: Option<LaunchPassthroughArgs>,
    mode: Option<String>,
    terminal: Option<TerminalRequest>,
    output: Option<Vec<String>>,
    output_format: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalRequest {
    cols: u16,
    rows: u16,
    cell_width_px: Option<u16>,
    cell_height_px: Option<u16>,
}

fn deserialize_launch_vars<'de, D>(deserializer: D) -> Result<Option<SuppliedLaunchVars>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(LaunchVarsOptionVisitor)
}

fn deserialize_launch_args<'de, D>(
    deserializer: D,
) -> Result<Option<LaunchPassthroughArgs>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(LaunchArgsOptionVisitor)
}

struct LaunchArgsOptionVisitor;

impl<'de> Visitor<'de> for LaunchArgsOptionVisitor {
    type Value = Option<LaunchPassthroughArgs>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a launch args array")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(de::Error::custom(
            "invalid launch args: args must be an array",
        ))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<String>::deserialize(deserializer)
            .map_err(|err| de::Error::custom(format!("invalid launch args: {err}")))?;
        LaunchPassthroughArgs::from_strings(values)
            .map(Some)
            .map_err(de::Error::custom)
    }
}

struct LaunchVarsOptionVisitor;

impl<'de> Visitor<'de> for LaunchVarsOptionVisitor {
    type Value = Option<SuppliedLaunchVars>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a launch variable object")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(de::Error::custom("launch variables must be an object"))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LaunchVarsVisitor).map(Some)
    }
}

struct LaunchVarsVisitor;

impl<'de> Visitor<'de> for LaunchVarsVisitor {
    type Value = SuppliedLaunchVars;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a launch variable object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut vars = SuppliedLaunchVars::default();
        while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
            let value = match value {
                serde_json::Value::String(value) => {
                    CanonicalLaunchVarValue::from_json_string(value)
                }
                serde_json::Value::Bool(value) => CanonicalLaunchVarValue::from_json_bool(value),
                serde_json::Value::Number(value) => {
                    CanonicalLaunchVarValue::from_json_number(&key, value.as_i64(), value.as_f64())
                        .map_err(de::Error::custom)?
                }
                serde_json::Value::Null
                | serde_json::Value::Array(_)
                | serde_json::Value::Object(_) => {
                    return Err(de::Error::custom(format!(
                        "invalid launch variable {key:?}: value must be string, boolean, or number"
                    )));
                }
            };
            vars.insert(key, value).map_err(de::Error::custom)?;
        }
        Ok(vars)
    }
}

#[cfg(test)]
mod tests;
