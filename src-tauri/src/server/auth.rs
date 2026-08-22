//! Bearer-token authentication and the shared error shape for the inference
//! server.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use log::warn;
use serde::Serialize;

use super::ServerState;

/// Error body mirroring OpenAI's `{"error": {...}}` envelope, so a client
/// written against that API surfaces Handy's failures in its usual place.
#[derive(Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub message: String,
    pub r#type: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, r#type: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            error: ApiErrorBody {
                message: message.into(),
                r#type,
            },
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found_error", message)
    }

    pub fn unsupported_media(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            message,
        )
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "server_error", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

/// Reject every request that does not carry the configured bearer token.
///
/// The token is compared in constant time: a LAN attacker can issue requests
/// fast enough that an early-exit comparison leaks a usable timing signal.
pub async fn require_token(
    State(state): State<Arc<ServerState>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");

    if !constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        // Logged without the presented value: it is attacker-controlled and
        // would put credential-shaped strings into the user's log file.
        warn!(
            "Rejected unauthenticated request to {} {}",
            request.method(),
            request.uri().path()
        );
        return ApiError::unauthorized("Missing or invalid bearer token").into_response();
    }

    next.run(request).await
}

/// Length-independent byte comparison. Differing lengths still walk the longer
/// slice so the reply time does not reveal the token's length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let len = a.len().max(b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_semantics_of_plain_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
