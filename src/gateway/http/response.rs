use super::super::ops::{ExecutionOutcome, GatewayOperationResult, OperationError};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

pub(super) fn metadata_result_response(result: GatewayOperationResult) -> Response {
    match result {
        GatewayOperationResult::Targets(value) => success_data(value),
        GatewayOperationResult::Status(value) => success_data(value),
        GatewayOperationResult::StatusAll(value) => success_data(value),
        GatewayOperationResult::Up(value) => success_data(value),
        GatewayOperationResult::Launches(value) => success_data(value),
        GatewayOperationResult::LaunchShow(value) => success_data(value),
        GatewayOperationResult::Stop(value) => success_data(value),
        GatewayOperationResult::Remove(value) => success_data(value),
        _ => HttpError::operation_failed("operation returned an unexpected result").into_response(),
    }
}

pub(super) fn success_data<T: Serialize>(data: T) -> Response {
    (StatusCode::OK, Json(json!({ "ok": true, "data": data }))).into_response()
}

pub(super) fn execution_response(outcome: ExecutionOutcome) -> Response {
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

pub(super) fn operation_error_response(err: OperationError) -> Response {
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
        OperationError::AgentNotReady { .. }
        | OperationError::ContainerNotFound { .. }
        | OperationError::ContainerLabelMismatch { .. }
        | OperationError::OperationFailed { .. } => HttpError::operation_failed(message),
    }
    .into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorCode {
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
pub(super) struct HttpError {
    pub(super) status: StatusCode,
    pub(super) code: ErrorCode,
    pub(super) message: String,
}

impl HttpError {
    pub(super) fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(super) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized, message)
    }

    pub(super) fn disabled_action(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::DisabledAction, message)
    }

    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    pub(super) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, message)
    }

    pub(super) fn invalid_mode(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidMode, message)
    }

    pub(super) fn invalid_output(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidOutput, message)
    }

    pub(super) fn operation_failed(message: impl Into<String>) -> Self {
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
