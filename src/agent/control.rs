use crate::agent_control::{
    AuthRequirement, ControlEnvelope, ControlFailure, ControlRequest, ControlRequestId,
    ControlSuccess, DecodedControlEnvelope, ReapResult, SessionHoldResult, ShutdownResult,
};
use crate::fileutil;
use anyhow::Context;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{Duration, sleep};

use super::idle::reap_processes;
use super::service::stop_services;
use super::socket::{apply_path_owner, unlink_socket_if_present, validate_control_peer};
use super::state::AgentState;
use super::status::status_payload;

const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONTROL_REQUEST_BYTES: usize = 64 * 1024;

pub(super) async fn run_control_socket(state: Arc<AgentState>, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fileutil::ensure_private_dir(parent)?;
        apply_path_owner(parent, state.socket_owner)?;
    }
    unlink_socket_if_present(path).await?;
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    apply_path_owner(path, state.socket_owner)?;
    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                shutdown_agent(state).await;
                return Ok(());
            }
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_control_connection(state, stream).await {
                        tracing::warn!(error = %err, "control connection failed");
                    }
                });
            }
        }
    }
}

pub(super) async fn wait_for_shutdown_signal(state: Arc<AgentState>) -> anyhow::Result<()> {
    shutdown_signal().await?;
    shutdown_agent(state).await;
    Ok(())
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("install SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("wait for Ctrl-C")?;
            }
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("wait for Ctrl-C")?;
    }
    Ok(())
}

pub(super) async fn shutdown_agent(state: Arc<AgentState>) {
    state.shutting_down.store(true, Ordering::SeqCst);
    state.accepting_bridge.store(false, Ordering::SeqCst);
    stop_services(&state).await;
}

async fn handle_control_connection(
    state: Arc<AgentState>,
    stream: UnixStream,
) -> anyhow::Result<()> {
    validate_control_peer(&stream, state.socket_owner.map(|owner| owner.uid))?;
    let mut reader = BufReader::new(stream);
    let line =
        match tokio::time::timeout(CONTROL_READ_TIMEOUT, read_control_request(&mut reader)).await {
            Ok(Ok(line)) => line,
            Ok(Err(err)) if err.to_string().contains("exceeds") => {
                let response = ControlFailure::new(
                    serde_json::Value::Null,
                    "request_too_large",
                    "control request is too large",
                );
                write_control_response(reader.into_inner(), response).await?;
                return Ok(());
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => anyhow::bail!("timed out reading control request"),
        };
    let Some(line) = line else {
        anyhow::bail!("empty control request");
    };
    let request: serde_json::Value = match serde_json::from_slice(&line) {
        Ok(request) => request,
        Err(err) => {
            let response =
                ControlFailure::new(serde_json::Value::Null, "parse_error", err.to_string());
            write_control_response(reader.into_inner(), response).await?;
            return Ok(());
        }
    };
    let envelope = match ControlEnvelope::decode(&request) {
        DecodedControlEnvelope::Request(envelope) => envelope,
        DecodedControlEnvelope::UnknownMethod(id) => {
            let response = ControlFailure::new(id, "unknown_method", "unknown control method");
            write_control_response(reader.into_inner(), response).await?;
            return Ok(());
        }
    };
    let ControlEnvelope { id, request } = envelope;
    if let Some(response) = unauthorized_if_needed(&state, &request, &id) {
        write_control_response(reader.into_inner(), response).await?;
        return Ok(());
    }
    match request {
        ControlRequest::Status => {
            let response = ControlSuccess::new(id, status_payload(&state).await);
            write_control_response(reader.into_inner(), response).await?;
        }
        ControlRequest::SessionHold(_) => {
            let mut stream = reader.into_inner();
            write_control_response_ref(
                &mut stream,
                ControlSuccess::new(id, SessionHoldResult { held: true }),
            )
            .await?;
            hold_control_session(state, stream).await;
            return Ok(());
        }
        ControlRequest::Shutdown(_) => {
            state.shutting_down.store(true, Ordering::SeqCst);
            state.accepting_bridge.store(false, Ordering::SeqCst);
            stop_services(&state).await;
            let response = ControlSuccess::new(
                id,
                ShutdownResult {
                    shutting_down: true,
                },
            );
            write_control_response(reader.into_inner(), response).await?;
            tokio::spawn(async {
                sleep(Duration::from_millis(10)).await;
                std::process::exit(0);
            });
        }
        ControlRequest::ReapNow(_) => {
            let result = state
                .idle_cleanup
                .as_ref()
                .map(|config| reap_processes(config, &BTreeSet::new(), true))
                .unwrap_or(ReapResult {
                    dry_run: true,
                    would_terminate: Vec::new(),
                    preserved: Vec::new(),
                });
            state.idle_state.lock().await.last_reap_result = Some(result.clone());
            let response = ControlSuccess::new(id, result);
            write_control_response(reader.into_inner(), response).await?;
        }
    };
    Ok(())
}

async fn read_control_request(
    reader: &mut BufReader<UnixStream>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if line.len() + end > MAX_CONTROL_REQUEST_BYTES {
            anyhow::bail!("control request exceeds {MAX_CONTROL_REQUEST_BYTES} bytes");
        }
        line.extend_from_slice(&available[..end]);
        reader.consume(end);
        if line.ends_with(b"\n") {
            return Ok(Some(line));
        }
    }
}

pub(super) fn unauthorized_if_needed(
    state: &AgentState,
    request: &ControlRequest,
    id: &ControlRequestId,
) -> Option<ControlFailure> {
    if request.auth_requirement() == AuthRequirement::None {
        return None;
    }
    let expected = state.control_token.as_deref()?;
    if request.token() == Some(expected) {
        return None;
    }
    Some(ControlFailure::new(
        id.clone(),
        "unauthorized",
        "control token is required",
    ))
}

async fn write_control_response<T: Serialize>(
    mut stream: UnixStream,
    response: T,
) -> anyhow::Result<()> {
    write_control_response_ref(&mut stream, response).await
}

async fn write_control_response_ref<T: Serialize>(
    stream: &mut UnixStream,
    response: T,
) -> anyhow::Result<()> {
    stream
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn hold_control_session(state: Arc<AgentState>, mut stream: UnixStream) {
    state.active_sessions.fetch_add(1, Ordering::SeqCst);
    let mut buffer = [0_u8; 1024];
    loop {
        tokio::select! {
            read = stream.read(&mut buffer) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            _ = sleep(Duration::from_secs(1)), if state.shutting_down.load(Ordering::SeqCst) => {
                break;
            }
        }
    }
    state.active_sessions.fetch_sub(1, Ordering::SeqCst);
}
