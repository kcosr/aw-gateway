use super::ops::{
    ExecutionOutcome, GatewayOperation, GatewayOperationResult, OperationError,
    OperationExecutionOptions, OperationMode, OutputSelection, SuppliedLaunchVarValue,
    SuppliedLaunchVars, execute_gateway_operation,
};
use crate::config::{GatewayConfig, HttpAuthType};
use crate::paths::{self, UserContext};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

const MAX_BEARER_TOKEN_BYTES: usize = 4096;

#[derive(Clone)]
struct AppState {
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
        let options = operation_options(request.mode.as_deref(), request.output.as_deref())?;
        Ok(GatewayOperation::Launch {
            name,
            session_id: request.session_id,
            vars: request.vars.unwrap_or_default(),
            options,
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
        let options = operation_options(request.mode.as_deref(), request.output.as_deref())?;
        Ok(GatewayOperation::Run {
            target: request.target,
            session_id: request.session_id,
            cwd: request.cwd,
            command: request.command,
            options,
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
        Ok(GatewayOperationResult::Run(outcome)) | Ok(GatewayOperationResult::Launch(outcome)) => {
            execution_response(outcome)
        }
        Ok(_) => {
            HttpError::operation_failed("operation returned an unexpected result").into_response()
        }
        Err(err) => operation_error_response(err),
    }
}

async fn authorize_action(
    state: &AppState,
    headers: &HeaderMap,
    action: &'static str,
) -> Result<(), ActionAuthorizationError> {
    authorize(state, headers).await?;
    if !state
        .config
        .http
        .enabled_actions
        .iter()
        .any(|enabled| enabled == action)
    {
        return Err(
            OperationError::disabled_action(format!("http action {action:?} is disabled")).into(),
        );
    }
    Ok(())
}

#[derive(Debug)]
enum ActionAuthorizationError {
    Http(HttpError),
    Operation(OperationError),
}

impl ActionAuthorizationError {
    fn into_response(self) -> Response {
        match self {
            Self::Http(err) => err.into_response(),
            Self::Operation(err) => operation_error_response(err),
        }
    }
}

impl From<HttpError> for ActionAuthorizationError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

impl From<OperationError> for ActionAuthorizationError {
    fn from(err: OperationError) -> Self {
        Self::Operation(err)
    }
}

async fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), HttpError> {
    match state.config.http.auth.auth_type {
        HttpAuthType::None => Ok(()),
        HttpAuthType::Bearer => authorize_bearer(state, headers).await,
    }
}

async fn authorize_bearer(state: &AppState, headers: &HeaderMap) -> Result<(), HttpError> {
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HttpError::unauthorized("unauthorized"))?;
    let expected = read_bearer_token(state).await?;
    if !constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
        return Err(HttpError::unauthorized("unauthorized"));
    }
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() > MAX_BEARER_TOKEN_BYTES || right.len() > MAX_BEARER_TOKEN_BYTES {
        return false;
    }
    let mut diff = left.len() ^ right.len();
    for index in 0..MAX_BEARER_TOKEN_BYTES {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        diff |= usize::from(left_byte ^ right_byte);
    }
    diff == 0
}

async fn read_bearer_token(state: &AppState) -> Result<String, HttpError> {
    let token_file = state
        .config
        .http
        .auth
        .token_file
        .as_deref()
        .ok_or_else(|| HttpError::unauthorized("unauthorized"))?;
    let user = UserContext::current().map_err(|_| HttpError::unauthorized("unauthorized"))?;
    let path = paths::expand_home(&user.home, token_file);
    let metadata = tokio::fs::metadata(&path).await.map_err(|err| {
        tracing::warn!(path = %path.display(), error = %err, "failed to stat http bearer token file");
        HttpError::unauthorized("unauthorized")
    })?;
    validate_bearer_token_file(&path, &metadata)?;
    let token = tokio::fs::read_to_string(&path).await.map_err(|err| {
        tracing::warn!(path = %path.display(), error = %err, "failed to read http bearer token file");
        HttpError::unauthorized("unauthorized")
    })?;
    let token = token.trim().to_string();
    if token.is_empty() || token.len() > MAX_BEARER_TOKEN_BYTES {
        return Err(HttpError::unauthorized("unauthorized"));
    }
    Ok(token)
}

#[cfg(unix)]
fn validate_bearer_token_file(
    path: &std::path::Path,
    metadata: &std::fs::Metadata,
) -> Result<(), HttpError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o077 != 0 {
        tracing::warn!(
            path = %path.display(),
            "http bearer token file must not be group- or world-readable"
        );
        return Err(HttpError::unauthorized("unauthorized"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_bearer_token_file(
    _path: &std::path::Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), HttpError> {
    Ok(())
}

fn metadata_result_response(result: GatewayOperationResult) -> Response {
    match result {
        GatewayOperationResult::Targets(value) => success_data(value),
        GatewayOperationResult::Status(value) => success_data(value),
        GatewayOperationResult::StatusAll(value) => success_data(value),
        GatewayOperationResult::Up(value) => success_data(value),
        GatewayOperationResult::Launches(value) => success_data(value),
        GatewayOperationResult::LaunchShow(value) => success_data(value),
        _ => HttpError::operation_failed("operation returned an unexpected result").into_response(),
    }
}

fn success_data<T: Serialize>(data: T) -> Response {
    (StatusCode::OK, Json(json!({ "ok": true, "data": data }))).into_response()
}

fn execution_response(outcome: ExecutionOutcome) -> Response {
    match outcome {
        ExecutionOutcome::Captured {
            exit_code,
            stdout,
            stderr,
        } => match wait_payload(exit_code, stdout, stderr) {
            Ok(value) => (StatusCode::OK, Json(value)).into_response(),
            Err(err) => err.into_response(),
        },
        ExecutionOutcome::Detached { operation_id } => (
            StatusCode::ACCEPTED,
            Json(json!({
                "ok": true,
                "mode": "detach",
                "status": "accepted",
                "operation_id": operation_id,
            })),
        )
            .into_response(),
        ExecutionOutcome::Streamed { .. } => {
            HttpError::operation_failed("http operations must use wait or detach mode")
                .into_response()
        }
    }
}

fn wait_payload(
    exit_code: i32,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
) -> Result<serde_json::Value, HttpError> {
    let mut payload = serde_json::Map::new();
    payload.insert("ok".into(), json!(true));
    payload.insert("mode".into(), json!("wait"));
    payload.insert("exit_code".into(), json!(exit_code));
    if let Some(stdout) = stdout {
        let stdout = String::from_utf8(stdout)
            .map_err(|_| HttpError::operation_failed("captured stdout is not valid UTF-8"))?;
        payload.insert("stdout".into(), json!(stdout));
    }
    if let Some(stderr) = stderr {
        let stderr = String::from_utf8(stderr)
            .map_err(|_| HttpError::operation_failed("captured stderr is not valid UTF-8"))?;
        payload.insert("stderr".into(), json!(stderr));
    }
    Ok(serde_json::Value::Object(payload))
}

fn operation_error_response(err: OperationError) -> Response {
    let message = err.to_string();
    match err {
        OperationError::InvalidRequest { .. } | OperationError::InvalidSession { .. } => {
            HttpError::invalid_request(message)
        }
        OperationError::DisabledAction { .. } => HttpError::disabled_action(message),
        OperationError::UnknownLaunch { .. } => HttpError::not_found(message),
        OperationError::InvalidLaunchVariable { .. } => HttpError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidLaunchVar,
            message,
        ),
        OperationError::OperationFailed { .. } => HttpError::operation_failed(message),
    }
    .into_response()
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

fn operation_options(
    mode: Option<&str>,
    output: Option<&[String]>,
) -> Result<OperationExecutionOptions, HttpError> {
    let mode = match mode.unwrap_or("wait") {
        "wait" => OperationMode::Wait,
        "detach" => OperationMode::Detach,
        _ => {
            return Err(HttpError::invalid_mode(
                "mode must be \"wait\" or \"detach\"",
            ));
        }
    };
    Ok(OperationExecutionOptions {
        mode,
        output: output_selection(output)?,
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
struct RunRequest {
    target: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    command: Vec<String>,
    mode: Option<String>,
    output: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRunRequest {
    session_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_launch_vars")]
    vars: Option<SuppliedLaunchVars>,
    mode: Option<String>,
    output: Option<Vec<String>>,
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
                serde_json::Value::String(value) => SuppliedLaunchVarValue::String(value),
                serde_json::Value::Bool(value) => SuppliedLaunchVarValue::Boolean(value),
                serde_json::Value::Number(value) => {
                    if let Some(value) = value.as_i64() {
                        SuppliedLaunchVarValue::Integer(value)
                    } else {
                        let value = value.as_f64().ok_or_else(|| {
                            de::Error::custom(format!(
                                "invalid launch variable {key:?}: number must be finite"
                            ))
                        })?;
                        SuppliedLaunchVarValue::Float(value)
                    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorCode {
    Unauthorized,
    DisabledAction,
    NotFound,
    InvalidRequest,
    InvalidMode,
    InvalidOutput,
    InvalidLaunchVar,
    OperationFailed,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::DisabledAction => "disabled_action",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidMode => "invalid_mode",
            Self::InvalidOutput => "invalid_output",
            Self::InvalidLaunchVar => "invalid_launch_var",
            Self::OperationFailed => "operation_failed",
        }
    }
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    code: ErrorCode,
    message: String,
}

impl HttpError {
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized, message)
    }

    fn disabled_action(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::DisabledAction, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, message)
    }

    fn invalid_mode(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidMode, message)
    }

    fn invalid_output(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidOutput, message)
    }

    fn operation_failed(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::OperationFailed,
            message,
        )
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": {
                    "code": self.code.as_str(),
                    "message": self.message,
                },
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::GatewayStatus;
    use super::*;
    use crate::config::{HttpAuthConfig, HttpConfig};
    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::http::Request;
    use axum::http::header::AUTHORIZATION;
    use tower::ServiceExt;

    fn write_fake_runtime(path: &std::path::Path, script: &str) {
        std::fs::write(path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    fn fake_running_runtime_script(log: &std::path::Path) -> String {
        let user = UserContext::current().unwrap();
        format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  ps)
    cat <<'JSON'
[{{"Names":["ubuntu-dev"],"Image":"ubuntu/dev","State":"running","Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev","io.aw-gateway.image":"ubuntu/dev"}}}}]
JSON
    ;;
  exec)
    echo "$@" >> "{log}"
    echo "captured stdout"
    echo "captured stderr" >&2
    exit 23
    ;;
esac
exit 0
"#,
            user = user.user,
            uid = user.uid,
            log = log.display()
        )
    }

    fn http_operation_config(dir: &tempfile::TempDir, fake_runtime: &std::path::Path) -> PathBuf {
        let config = dir.path().join("gateway.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[http]
enabled = true
listen = "127.0.0.1:0"
enabled_actions = ["status", "targets", "launches", "run", "launch", "up"]

[runtime]
type = "podman"
program = "{program}"

[target_defaults.workspace]
path = "{workspace}"
state_dir = ".aw-gateway"
cleanup = "never"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev"
stop_when_idle = false

[launches.echo]
target = "default"
command = ["launch-command"]
"#,
                program = fake_runtime.display(),
                workspace = dir.path().join("workspace").display()
            ),
        )
        .unwrap();
        config
    }

    fn app_for_config(config: PathBuf) -> Router {
        let cfg = GatewayConfig::load(&config).unwrap();
        router(AppState {
            config_path: Some(config),
            config: Arc::new(cfg),
        })
    }

    async fn request_json(
        app: Router,
        method: &str,
        uri: &str,
        body: Body,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn response_json(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn post_json(app: Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
        request_json(app, "POST", uri, Body::from(body.to_string())).await
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        request_json(app, "GET", uri, Body::empty()).await
    }

    fn test_state(http: HttpConfig) -> AppState {
        let mut cfg: GatewayConfig =
            toml::from_str(crate::gateway::DEFAULT_GATEWAY_CONFIG).unwrap();
        cfg.http = http;
        AppState {
            config_path: None,
            config: Arc::new(cfg),
        }
    }

    #[cfg(unix)]
    fn set_private_file_mode(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn set_private_file_mode(_path: &std::path::Path) {}

    #[tokio::test]
    async fn bearer_auth_allows_matching_token_and_rejects_missing_or_wrong_header() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "secret-token\n").unwrap();
        set_private_file_mode(&token_file);
        let state = test_state(HttpConfig {
            enabled: true,
            listen: "127.0.0.1:0".into(),
            enabled_actions: vec!["status".into()],
            auth: HttpAuthConfig {
                auth_type: HttpAuthType::Bearer,
                token_file: Some(token_file.display().to_string()),
            },
        });

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer secret-token".parse().unwrap());
        authorize(&state, &headers).await.unwrap();

        let missing = authorize(&state, &HeaderMap::new()).await.unwrap_err();
        assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
        assert_eq!(missing.code, ErrorCode::Unauthorized);

        let mut wrong_scheme = HeaderMap::new();
        wrong_scheme.insert(AUTHORIZATION, "Basic secret-token".parse().unwrap());
        assert_eq!(
            authorize(&state, &wrong_scheme).await.unwrap_err().code,
            ErrorCode::Unauthorized
        );

        let mut wrong_token = HeaderMap::new();
        wrong_token.insert(AUTHORIZATION, "Bearer other".parse().unwrap());
        assert_eq!(
            authorize(&state, &wrong_token).await.unwrap_err().code,
            ErrorCode::Unauthorized
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bearer_auth_rejects_group_or_world_readable_token_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "secret-token\n").unwrap();
        let mut permissions = std::fs::metadata(&token_file).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&token_file, permissions).unwrap();
        let state = test_state(HttpConfig {
            enabled: true,
            listen: "127.0.0.1:0".into(),
            enabled_actions: vec!["status".into()],
            auth: HttpAuthConfig {
                auth_type: HttpAuthType::Bearer,
                token_file: Some(token_file.display().to_string()),
            },
        });

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer secret-token".parse().unwrap());
        let err = authorize(&state, &headers).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert_eq!(err.code, ErrorCode::Unauthorized);
    }

    #[tokio::test]
    async fn none_auth_allows_missing_authorization() {
        let state = test_state(HttpConfig {
            enabled: true,
            listen: "127.0.0.1:0".into(),
            enabled_actions: vec!["status".into()],
            auth: HttpAuthConfig::default(),
        });
        authorize(&state, &HeaderMap::new()).await.unwrap();
    }

    #[test]
    fn mode_and_output_parsing_accepts_wait_detach_and_rejects_bad_values() {
        let wait = operation_options(None, None).unwrap();
        assert_eq!(wait.mode, OperationMode::Wait);
        assert_eq!(wait.output, OutputSelection::BOTH);

        let output = vec!["stdout".to_string()];
        let detach = operation_options(Some("detach"), Some(&output)).unwrap();
        assert_eq!(detach.mode, OperationMode::Detach);
        assert_eq!(
            detach.output,
            OutputSelection {
                stdout: true,
                stderr: false,
            }
        );

        assert_eq!(
            operation_options(Some("stream"), None).unwrap_err().code,
            ErrorCode::InvalidMode
        );
        let duplicate = vec!["stdout".to_string(), "stdout".to_string()];
        assert_eq!(
            operation_options(Some("wait"), Some(&duplicate))
                .unwrap_err()
                .code,
            ErrorCode::InvalidOutput
        );
        let unknown = vec!["log".to_string()];
        assert_eq!(
            operation_options(Some("wait"), Some(&unknown))
                .unwrap_err()
                .code,
            ErrorCode::InvalidOutput
        );
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            operation_options(Some("wait"), Some(&empty))
                .unwrap_err()
                .code,
            ErrorCode::InvalidOutput
        );
    }

    #[test]
    fn launch_vars_parse_typed_values_and_reject_duplicates_and_structured_values() {
        let parsed: LaunchRunRequest = serde_json::from_str(
            r#"{"vars":{"repo":"https://example.test/repo.git","debug":true,"count":3,"ratio":1.5}}"#,
        )
        .unwrap();
        let vars = parsed.vars.unwrap();
        assert!(matches!(
            vars.get("repo"),
            Some(SuppliedLaunchVarValue::String(value))
                if value == "https://example.test/repo.git"
        ));
        assert!(matches!(
            vars.get("debug"),
            Some(SuppliedLaunchVarValue::Boolean(true))
        ));
        assert!(matches!(
            vars.get("count"),
            Some(SuppliedLaunchVarValue::Integer(3))
        ));
        assert!(matches!(
            vars.get("ratio"),
            Some(SuppliedLaunchVarValue::Float(value)) if *value == 1.5
        ));

        let duplicate =
            serde_json::from_str::<LaunchRunRequest>(r#"{"vars":{"repo":"a","repo":"b"}}"#)
                .unwrap_err()
                .to_string();
        assert!(
            duplicate.contains("duplicate launch variable"),
            "{duplicate}"
        );

        let object = serde_json::from_str::<LaunchRunRequest>(r#"{"vars":{"repo":{"url":"x"}}}"#)
            .unwrap_err()
            .to_string();
        assert!(object.contains("invalid launch variable"), "{object}");
    }

    #[test]
    fn execution_response_projects_wait_detach_and_invalid_utf8() {
        let response =
            execution_response(ExecutionOutcome::captured(7, Some(b"out".to_vec()), None));
        assert_eq!(response.status(), StatusCode::OK);

        let response = execution_response(ExecutionOutcome::detached("abc123".into()));
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let response = execution_response(ExecutionOutcome::captured(0, Some(vec![0xff]), None));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn operation_errors_map_launch_validation_to_invalid_launch_var() {
        let response = operation_error_response(OperationError::invalid_launch_variable(
            "missing required launch variable \"repo\"",
        ));
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_launch_var");

        let response = operation_error_response(OperationError::invalid_launch_variable(
            "unknown launch variable \"repo\"",
        ));
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_launch_var");
    }

    #[tokio::test]
    async fn operation_errors_map_all_typed_variants_to_http_codes() {
        let cases = vec![
            (
                OperationError::invalid_request("bad request"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "bad request",
            ),
            (
                OperationError::disabled_action("http action \"run\" is disabled"),
                StatusCode::FORBIDDEN,
                "disabled_action",
                "http action \"run\" is disabled",
            ),
            (
                OperationError::unknown_launch("unknown launch \"repo\""),
                StatusCode::NOT_FOUND,
                "not_found",
                "unknown launch \"repo\"",
            ),
            (
                OperationError::invalid_launch_variable("invalid enum launch variable \"mode\""),
                StatusCode::BAD_REQUEST,
                "invalid_launch_var",
                "invalid enum launch variable \"mode\"",
            ),
            (
                OperationError::invalid_session("invalid session id \"../bad\""),
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid session id \"../bad\"",
            ),
            (
                OperationError::operation_failed(anyhow::anyhow!("runtime failed")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "operation_failed",
                "runtime failed",
            ),
        ];

        for (err, expected_status, expected_code, expected_message) in cases {
            let response = operation_error_response(err);
            let (status, body) = response_json(response).await;
            assert_eq!(status, expected_status);
            assert_eq!(body["ok"], false);
            assert_eq!(body["error"]["code"], expected_code);
            assert_eq!(body["error"]["message"], expected_message);
        }
    }

    #[tokio::test]
    async fn operation_error_mapping_distinguishes_launch_var_from_unknown_launch() {
        let response = operation_error_response(OperationError::invalid_launch_variable(
            "unknown launch variable \"repo\"",
        ));
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_launch_var");

        let response =
            operation_error_response(OperationError::unknown_launch("unknown launch \"repo\""));
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn route_auth_and_disabled_action_return_json_errors() {
        let state = test_state(HttpConfig {
            enabled: true,
            listen: "127.0.0.1:0".into(),
            enabled_actions: vec!["targets".into()],
            auth: HttpAuthConfig::default(),
        });

        let response = authorize_action(&state, &HeaderMap::new(), "status")
            .await
            .unwrap_err();
        let ActionAuthorizationError::Operation(OperationError::DisabledAction { message }) =
            response
        else {
            panic!("expected disabled-action operation error");
        };
        assert_eq!(message, "http action \"status\" is disabled");

        let response = not_found(
            State(state),
            HeaderMap::new(),
            "/api/v1/missing".parse().unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_exposes_declared_routes_without_aliases() {
        let app = router(test_state(HttpConfig {
            enabled: true,
            listen: "127.0.0.1:0".into(),
            enabled_actions: vec!["targets".into()],
            auth: HttpAuthConfig::default(),
        }));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/statuses")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_wait_and_detach_routes_use_shared_operations() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
        let app = app_for_config(http_operation_config(&dir, &fake_runtime));

        let (status, body) = post_json(
            app.clone(),
            "/api/v1/run",
            r#"{"command":["run-wait"],"mode":"wait","output":["stdout"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["mode"], "wait");
        assert_eq!(body["exit_code"], 23);
        assert_eq!(body["stdout"], "captured stdout\n");
        assert!(body.get("stderr").is_none());

        let (status, body) = post_json(
            app,
            "/api/v1/run",
            r#"{"command":["run-detach"],"mode":"detach"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["ok"], true);
        assert_eq!(body["mode"], "detach");
        assert_eq!(body["status"], "accepted");
        assert!(body["operation_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn launch_wait_and_detach_routes_use_shared_operations() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
        let app = app_for_config(http_operation_config(&dir, &fake_runtime));

        let (status, body) = post_json(
            app.clone(),
            "/api/v1/launches/echo/run",
            r#"{"mode":"wait","output":["stderr"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["mode"], "wait");
        assert_eq!(body["exit_code"], 23);
        assert!(body.get("stdout").is_none());
        assert_eq!(body["stderr"], "captured stderr\n");

        let (status, body) =
            post_json(app, "/api/v1/launches/echo/run", r#"{"mode":"detach"}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["ok"], true);
        assert_eq!(body["mode"], "detach");
        assert_eq!(body["status"], "accepted");
        assert!(body["operation_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn metadata_routes_return_data_envelopes() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
        let app = app_for_config(http_operation_config(&dir, &fake_runtime));

        let (status, body) = get_json(app.clone(), "/api/v1/status?target=default").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["target"], "default");

        let (status, body) = get_json(app.clone(), "/api/v1/status/all").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"][0]["target"], "default");

        let (status, body) = get_json(app.clone(), "/api/v1/targets").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"][0]["target"], "default");

        let (status, body) = post_json(app.clone(), "/api/v1/up", r#"{"target":"default"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["target"], "default");

        let (status, body) = get_json(app.clone(), "/api/v1/launches").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"][0]["name"], "echo");

        let (status, body) = get_json(app, "/api/v1/launches/echo").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["name"], "echo");
        assert_eq!(body["data"]["command"][0], "launch-command");
    }

    #[tokio::test]
    async fn malformed_json_mode_output_and_launch_var_errors_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let fake_runtime = dir.path().join("runtime");
        let log = dir.path().join("runtime.log");
        write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
        let app = app_for_config(http_operation_config(&dir, &fake_runtime));

        let (status, body) = post_json(app.clone(), "/api/v1/run", r#"{"command":["x"]"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");

        let (status, body) = post_json(
            app.clone(),
            "/api/v1/run",
            r#"{"command":["x"],"mode":"stream"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_mode");

        let (status, body) = post_json(
            app.clone(),
            "/api/v1/run",
            r#"{"command":["x"],"output":["stdout","stdout"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_output");

        let (status, body) = post_json(
            app.clone(),
            "/api/v1/run",
            r#"{"command":["x"],"output":[]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_output");

        let (status, body) = post_json(
            app.clone(),
            "/api/v1/launches/echo/run",
            r#"{"vars":{"bogus":1}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_launch_var");

        let (status, body) = post_json(
            app,
            "/api/v1/launches/echo/run",
            r#"{"vars":{"repo":null}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_launch_var");
    }

    #[test]
    fn status_shape_can_be_wrapped_in_data_envelope() {
        let status = GatewayStatus {
            target: "default".into(),
            session_id: None,
            launch: None,
            mode: "fixed".into(),
            user: "alice".into(),
            image: "ubuntu/dev".into(),
            container: Some("ubuntu-dev".into()),
            container_pid: Some(123),
            active_sessions: 0,
            sessions: Vec::new(),
            agent_ready: false,
            ssh_socket: PathBuf::from("/tmp/ssh.sock"),
            status: "ready".into(),
            agent: None,
        };
        let response = success_data(status);
        assert_eq!(response.status(), StatusCode::OK);
    }
}
