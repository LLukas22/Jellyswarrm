//! Enforce read-only API-key permissions before incoming requests reach handlers.
use crate::{auth::UserAuthorizationService, protocol::identity::JellyfinAuthorization};
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tracing::error;
pub(crate) async fn enforce_read_only_api_keys(
    State(state): State<Arc<UserAuthorizationService>>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }

    // Only the credential matters here; resolving the caller's user would cost a lookup.
    let url = match crate::http::preprocessing::request_url(request.uri()) {
        Ok(url) => url,
        Err(error) => {
            error!("Failed to enforce API key access: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let auth = JellyfinAuthorization::from_headers_url(request.headers(), &url);
    let Some(token) = auth.as_ref().and_then(JellyfinAuthorization::token_ref) else {
        return next.run(request).await;
    };

    match state.is_read_only_api_key(token).await {
        Ok(true) => StatusCode::FORBIDDEN.into_response(),
        Ok(false) => next.run(request).await,
        Err(error) => {
            error!("Failed to check API key access: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
