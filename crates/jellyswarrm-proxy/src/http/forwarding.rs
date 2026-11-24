//! Catch-all adapter: forward any client request without a dedicated route to
//! one backend through `application::forwarding`.
use crate::{
    application::{
        self, error::ApplicationError, media_services::MediaServices, request::input::routing_error,
    },
    protocol::routes::{playback_session_action, PlaybackSessionAction},
    translation::response_processor::ResponseProcessingProfile,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use percent_encoding::percent_decode_str;
use tracing::{debug, error};

#[axum::debug_handler]
pub(crate) async fn forward(
    State(state): State<MediaServices>,
    req: Request,
) -> Result<Response, ApplicationError> {
    // `Router::nest` already stripped the URL prefix from `req.uri()`.
    let path = req.uri().path();
    let action = playback_session_action(req.method(), path);
    if is_unhandled_session_route(path, action) {
        return Ok(StatusCode::NOT_IMPLEMENTED.into_response());
    }
    let response_profile = ResponseProcessingProfile::for_proxy_request(req.method(), path);
    if let Some(response) = super::assets::serve(&req) {
        return Ok(response.unwrap_or_else(IntoResponse::into_response));
    }
    // Without a token only well-known anonymous endpoints may reach a backend;
    // anything else (scanner probes, unknown paths) is answered locally (#224).
    match super::public_routes::gate(req.uri(), req.headers(), &state.user_authorization)
        .await
        .map_err(ApplicationError::Storage)?
    {
        super::public_routes::Gate::Forward => {}
        super::public_routes::Gate::NotFound => {
            debug!(
                path,
                "Rejected unauthenticated request outside the public route list"
            );
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        super::public_routes::Gate::Unauthorized => {
            debug!(path, "Rejected request with an unknown token");
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        }
    }

    let prepared = prepare(&state, req, action).await.inspect_err(|error| {
        error!("Failed to prepare forwarded request: {error}");
    })?;
    let response = application::forwarding::forward(&state, prepared, response_profile).await?;
    Ok(response.map(Body::from).into_response())
}

async fn prepare(
    state: &MediaServices,
    req: Request,
    action: Option<PlaybackSessionAction>,
) -> Result<application::forwarding::PreparedForward, ApplicationError> {
    let input = super::incoming::collect_operation(req, state, action)
        .await
        .map_err(routing_error)?;
    application::forwarding::prepare(state, input, action).await
}

/// `/Sessions/*` belongs to the proxy (client sessions, remote control); only
/// playback reports reach a backend. This guard, not the route table, catches
/// percent-encoded and mixed-case spellings that bypass the explicit routes.
fn is_unhandled_session_route(path: &str, action: Option<PlaybackSessionAction>) -> bool {
    let decoded = percent_decode_str(path).decode_utf8_lossy();
    action.is_none()
        && decoded
            .split('/')
            .find(|part| !part.is_empty())
            .is_some_and(|part| part.eq_ignore_ascii_case("sessions"))
}
