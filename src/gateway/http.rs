use super::ops::{
    CanonicalLaunchVarValue, GatewayOperation, GatewayOperationResult, OperationExecutionOptions,
    OperationMode, OutputSelection, SuppliedLaunchVars, execute_gateway_operation,
};
use crate::config::GatewayConfig;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

mod auth;
mod output_projection;
mod response;

use auth::{authorize, authorize_action};
use output_projection::{OutputFormat, OutputFormats};
use response::{
    ErrorCode, HttpError, execution_response, metadata_result_response, operation_error_response,
};

#[derive(Clone)]
pub(super) struct AppState {
    config_path: Option<PathBuf>,
    config: Arc<GatewayConfig>,
}

pub(super) async fn serve(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = super::load_config(config_path.clone())?;
    if !cfg.http.enabled {
        anyhow::bail!("http listener is disabled in config");
    }
    let addr = cfg.http.listen_addr()?;
    let app = router(AppState {
        config_path,
        config: Arc::new(cfg),
    });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(listen = %addr, "http listener started");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<StatusQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    handle_metadata(state, headers, "status", || {
        let query = query.map_err(|_| HttpError::invalid_request("invalid status query"))?;
        Ok(GatewayOperation::Status {
            target: query.target.clone(),
            session_id: query.session_id.clone(),
        })
    })
    .await
}

async fn status_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    handle_metadata(state, headers, "status", || Ok(GatewayOperation::StatusAll)).await
}

async fn targets(State(state): State<AppState>, headers: HeaderMap) -> Response {
    handle_metadata(state, headers, "targets", || Ok(GatewayOperation::Targets)).await
}

async fn up(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_metadata(state, headers, "up", || {
        let request: UpRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        Ok(GatewayOperation::Up {
            target: request.target,
            session_id: request.session_id,
        })
    })
    .await
}

async fn stop(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_metadata(state, headers, "stop", || {
        let request: LifecycleRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        Ok(GatewayOperation::Stop {
            target: request.target,
            session_id: request.session_id,
        })
    })
    .await
}

async fn remove(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_metadata(state, headers, "remove", || {
        let request: LifecycleRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        Ok(GatewayOperation::Remove {
            target: request.target,
            session_id: request.session_id,
        })
    })
    .await
}

async fn launches(State(state): State<AppState>, headers: HeaderMap) -> Response {
    handle_metadata(state, headers, "launches", || {
        Ok(GatewayOperation::Launches)
    })
    .await
}

async fn launch_show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    handle_metadata(state, headers, "launch", || {
        Ok(GatewayOperation::LaunchShow { name })
    })
    .await
}

async fn launch_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    handle_execution(state, headers, "launch", || {
        let request: LaunchRunRequest = parse_body(&body, ErrorCode::InvalidLaunchVar)?;
        let execution = execution_request_options(
            request.mode.as_deref(),
            request.output.as_deref(),
            request.output_format.as_ref(),
        )?;
        Ok(HttpExecutionRequest {
            operation: GatewayOperation::Launch {
                name,
                session_id: request.session_id,
                vars: request.vars.unwrap_or_default(),
                options: execution.options,
            },
            output_formats: execution.output_formats,
        })
    })
    .await
}

async fn run(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_execution(state, headers, "run", || {
        let request: RunRequest = parse_body(&body, ErrorCode::InvalidRequest)?;
        if request.command.is_empty() {
            return Err(HttpError::invalid_request("command must not be empty"));
        }
        if request.command.iter().any(|arg| arg.is_empty()) {
            return Err(HttpError::invalid_request(
                "command elements must not be empty",
            ));
        }
        let execution = execution_request_options(
            request.mode.as_deref(),
            request.output.as_deref(),
            request.output_format.as_ref(),
        )?;
        Ok(HttpExecutionRequest {
            operation: GatewayOperation::Run {
                target: request.target,
                session_id: request.session_id,
                cwd: request.cwd,
                command: request.command,
                options: execution.options,
            },
            output_formats: execution.output_formats,
        })
    })
    .await
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
    operation: impl FnOnce() -> Result<GatewayOperation, HttpError>,
) -> Response {
    let operation = match authorize_action(&state, &headers, action).await {
        Ok(()) => match operation() {
            Ok(operation) => operation,
            Err(err) => return err.into_response(),
        },
        Err(err) => return err.into_response(),
    };
    match execute_gateway_operation(state.config_path, operation).await {
        Ok(result) => metadata_result_response(result),
        Err(err) => operation_error_response(err),
    }
}

async fn handle_execution(
    state: AppState,
    headers: HeaderMap,
    action: &'static str,
    operation: impl FnOnce() -> Result<HttpExecutionRequest, HttpError>,
) -> Response {
    let request = match authorize_action(&state, &headers, action).await {
        Ok(()) => match operation() {
            Ok(request) => request,
            Err(err) => return err.into_response(),
        },
        Err(err) => return err.into_response(),
    };
    match execute_gateway_operation(state.config_path, request.operation).await {
        Ok(GatewayOperationResult::Run(outcome)) | Ok(GatewayOperationResult::Launch(outcome)) => {
            execution_response(outcome, request.output_formats)
        }
        Ok(_) => {
            HttpError::operation_failed("operation returned an unexpected result").into_response()
        }
        Err(err) => operation_error_response(err),
    }
}

#[derive(Debug)]
struct HttpExecutionRequest {
    operation: GatewayOperation,
    output_formats: OutputFormats,
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
                "mode must be \"wait\" or \"detach\"",
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusQuery {
    target: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpRequest {
    target: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleRequest {
    target: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    target: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    command: Vec<String>,
    mode: Option<String>,
    output: Option<Vec<String>>,
    output_format: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRunRequest {
    session_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_launch_vars")]
    vars: Option<SuppliedLaunchVars>,
    mode: Option<String>,
    output: Option<Vec<String>>,
    output_format: Option<BTreeMap<String, String>>,
}

fn deserialize_launch_vars<'de, D>(deserializer: D) -> Result<Option<SuppliedLaunchVars>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(LaunchVarsOptionVisitor)
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
