use super::AppState;
use super::response::{HttpError, operation_error_response};
use crate::config::HttpAuthType;
use crate::gateway::ops::OperationError;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

const MAX_BEARER_TOKEN_BYTES: usize = 4096;

pub(super) async fn authorize_action(
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
pub(super) enum ActionAuthorizationError {
    Http(HttpError),
    Operation(OperationError),
}

impl IntoResponse for ActionAuthorizationError {
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

pub(super) async fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), HttpError> {
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
    let expected = state
        .config
        .http
        .auth
        .token
        .as_deref()
        .ok_or_else(|| HttpError::unauthorized("unauthorized"))?;
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
