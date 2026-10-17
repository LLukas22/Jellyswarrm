//! Enforce read-only API-key permissions before incoming requests reach handlers.
use crate::AppState;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::error;
pub(crate) async fn enforce_read_only_api_keys(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }

    let identity = match crate::http::preprocessing::resolve_request_identity_from_headers_uri(
        request.headers(),
        request.uri(),
        &state,
    )
    .await
    {
        Ok(identity) => identity,
        Err(error) => {
            error!("Failed to enforce API key access: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(token) = identity.auth.as_ref().and_then(|auth| auth.token_ref()) else {
        return next.run(request).await;
    };

    match state.user_authorization.is_read_only_api_key(token).await {
        Ok(true) => StatusCode::FORBIDDEN.into_response(),
        Ok(false) => next.run(request).await,
        Err(error) => {
            error!("Failed to check API key access: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
