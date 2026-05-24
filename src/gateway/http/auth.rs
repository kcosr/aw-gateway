use super::AppState;
use super::response::{HttpError, operation_error_response};
use crate::config::HttpAuthType;
use crate::gateway::ops::OperationError;
use crate::paths::{self, UserContext};
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
