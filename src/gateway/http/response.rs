use super::super::ops::{ExecutionOutcome, GatewayOperationResult, OperationError};
use super::output_projection::{OutputFormats, project_wait_payload};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use std::fmt;

const INTERNAL_OPERATION_FAILED_MESSAGE: &str = "internal operation failed";

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

pub(super) fn execution_response(outcome: ExecutionOutcome, formats: OutputFormats) -> Response {
    match outcome {
        ExecutionOutcome::Captured {
            exit_code,
            stdout,
            stderr,
        } => (
            StatusCode::OK,
            Json(project_wait_payload(exit_code, stdout, stderr, formats)),
        )
            .into_response(),
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
        ExecutionOutcome::Canceled { .. } => {
            HttpError::operation_failed("operation canceled").into_response()
        }
    }
}

pub(super) fn operation_error_response(err: OperationError) -> Response {
    match err {
        OperationError::InvalidRequest { message } | OperationError::InvalidSession { message } => {
            HttpError::invalid_request(message)
        }
        OperationError::DisabledAction { message } => HttpError::disabled_action(message),
        OperationError::UnknownLaunch { message } => HttpError::not_found(message),
        OperationError::InvalidLaunchVariable { message } => HttpError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidLaunchVar,
            message,
        ),
        OperationError::InvalidLaunchArgs { message } => HttpError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidLaunchArgs,
            message,
        ),
        OperationError::AgentNotReady { source }
        | OperationError::ContainerNotFound { source }
        | OperationError::ContainerLabelMismatch { source }
        | OperationError::OperationFailed { source } => {
            internal_operation_failed(source, "gateway HTTP operation failed")
        }
    }
    .into_response()
}

pub(super) fn internal_operation_error_response(
    error: impl fmt::Display,
    event: &'static str,
) -> Response {
    internal_operation_failed(error, event).into_response()
}

fn internal_operation_failed(error: impl fmt::Display, event: &'static str) -> HttpError {
    tracing::warn!(error = %error, "{event}");
    HttpError::operation_failed(INTERNAL_OPERATION_FAILED_MESSAGE)
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
    InvalidLaunchArgs,
    OperationFailed,
}

impl ErrorCode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::DisabledAction => "disabled_action",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidMode => "invalid_mode",
            Self::InvalidOutput => "invalid_output",
            Self::InvalidLaunchVar => "invalid_launch_var",
            Self::InvalidLaunchArgs => "invalid_launch_args",
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
